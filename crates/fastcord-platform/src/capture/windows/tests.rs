//! Capture runtime checks. The deterministic ones run everywhere Windows runs;
//! the hardware ones capture real windows and displays on this machine and are
//! run by hand with
//! `cargo test -p fastcord-platform --locked -- --ignored --nocapture --test-threads=1`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Foundation::{E_ACCESSDENIED, E_FAIL, RO_E_CLOSED};
use windows::core::Error;

use super::runtime::{Native, from_hns, monotonic_time, native_error, wait};
use super::test_window::{LEFT_RGB, RIGHT_RGB, TestWindow};
use super::wgc::SizeGovernor;
use crate::capture::convert::{Bgra, Converter, nv12_len};
use crate::capture::{
    CaptureConfig, CaptureError, CaptureSession, CaptureState, CaptureStatus, Conversion, Flow,
    FrameHandler, GpuFrame, GpuOutcome, SourceId, SourceKind, StartOptions, Tick, VideoSample,
    availability, list_sources, start,
};

const SETTLE: Duration = Duration::from_millis(300);

fn governor() -> SizeGovernor {
    SizeGovernor::new(CaptureConfig::SCREEN_SHARE_720P30)
}

#[test]
fn the_first_size_is_taken_at_once_and_a_change_waits_for_it_to_settle() {
    let mut governor = governor();
    let t0 = Duration::from_secs(1000);
    assert_eq!(governor.output((1920, 1080), t0), (1280, 720));
    assert_eq!(governor.due((1920, 1080)), None);
    // The window shrinks: pictures keep the old size (stretched) until the new
    // size has held for the settle time.
    let changed = t0 + Duration::from_millis(10);
    assert_eq!(governor.output((800, 600), changed), (1280, 720));
    assert_eq!(governor.due((800, 600)), Some(changed + SETTLE));
    assert_eq!(
        governor.output((800, 600), changed + Duration::from_millis(299)),
        (1280, 720)
    );
    assert_eq!(governor.output((800, 600), changed + SETTLE), (800, 600));
    assert_eq!(governor.due((800, 600)), None);
    // The new size stays.
    assert_eq!(
        governor.output((800, 600), changed + Duration::from_secs(5)),
        (800, 600)
    );
}

#[test]
fn a_size_that_keeps_changing_never_settles_and_returning_to_the_old_size_cancels_it() {
    let mut governor = governor();
    let t0 = Duration::from_secs(50);
    assert_eq!(governor.output((1000, 700), t0), (1000, 700));
    // A drag: every picture has a new size, so the clock restarts each time.
    for step in 1..=20u32 {
        let now = t0 + Duration::from_millis(100) * step;
        assert_eq!(
            governor.output((1000 + step * 4, 700 + step * 2), now),
            (1000, 700),
            "step {step}"
        );
    }
    // Back to the size in use: nothing is pending any more.
    let now = t0 + Duration::from_secs(3);
    assert_eq!(governor.output((1000, 700), now), (1000, 700));
    assert_eq!(governor.due((1000, 700)), None);
    // A pending size is only due for pictures that still have that size.
    assert_eq!(governor.output((600, 400), now), (1000, 700));
    assert_eq!(governor.due((600, 400)), Some(now + SETTLE));
    assert_eq!(governor.due((700, 400)), None);
}

fn collecting() -> (Arc<Mutex<Vec<CaptureState>>>, StartOptions) {
    let states = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&states);
    let mut options = StartOptions::new(CaptureConfig::SCREEN_SHARE_720P30);
    options.observer = Some(Arc::new(move |state| record.lock().push(state.clone())));
    (states, options)
}

#[test]
fn the_owner_thread_stops_on_request_and_publishes_its_terminal_state_after_cleanup() {
    let status = CaptureStatus::new(None);
    let native = Native::spawn("test-owner", status.clone(), |stop, _| {
        // Waits for the stop event like the capture loop does.
        assert_eq!(wait(&[stop.0], Duration::from_secs(10))?, Some(0));
        Ok(())
    })
    .unwrap();
    assert_eq!(status.snapshot().1, CaptureState::Starting);
    assert_eq!(native.stop(), Ok(()));
    assert_eq!(status.snapshot().1, CaptureState::Stopped);
}

#[test]
fn a_failing_owner_keeps_its_error_and_a_panicking_one_is_contained() {
    let status = CaptureStatus::new(None);
    let native = Native::spawn("test-owner", status.clone(), |_, _| {
        Err(CaptureError::PermissionDenied)
    })
    .unwrap();
    assert_eq!(native.stop(), Err(CaptureError::PermissionDenied));
    assert_eq!(
        status.snapshot().1,
        CaptureState::Failed(CaptureError::PermissionDenied)
    );

    let status = CaptureStatus::new(None);
    let native = Native::spawn("test-owner", status.clone(), |_, _| {
        panic!("native failure")
    })
    .unwrap();
    let error = CaptureError::Worker("the capture thread panicked");
    assert_eq!(native.stop(), Err(error.clone()));
    assert_eq!(status.snapshot().1, CaptureState::Failed(error));
}

#[test]
fn a_hung_owner_is_abandoned_after_the_timeout_instead_of_hanging_the_caller() {
    let status = CaptureStatus::new(None);
    let mut native = Native::spawn("test-owner", status.clone(), |_, _| {
        std::thread::sleep(Duration::from_millis(500));
        Ok(())
    })
    .unwrap();
    let started = Instant::now();
    assert_eq!(
        native.stop_within(Duration::from_millis(50)),
        Err(CaptureError::Worker(
            "the capture thread did not stop in time"
        ))
    );
    assert!(started.elapsed() < Duration::from_millis(400));
    // The detached thread still ends cleanly later.
    let deadline = Instant::now() + Duration::from_secs(5);
    while status.snapshot().1 != CaptureState::Stopped {
        assert!(Instant::now() < deadline, "the thread never finished");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_monotonic_clock_follows_real_time_and_never_goes_back() {
    let (first, started) = (monotonic_time().unwrap(), Instant::now());
    std::thread::sleep(Duration::from_millis(60));
    let (second, elapsed) = (monotonic_time().unwrap(), started.elapsed());
    assert!(second >= first);
    let measured = second - first;
    assert!(
        measured >= Duration::from_millis(55) && measured <= elapsed + Duration::from_millis(30),
        "{measured:?} against {elapsed:?}"
    );
    assert_eq!(from_hns(10_000_000), Duration::from_secs(1));
    assert_eq!(from_hns(-5), Duration::ZERO);
}

#[test]
fn native_failures_map_to_what_the_user_can_act_on() {
    assert_eq!(
        native_error("StartCapture", &Error::from_hresult(E_ACCESSDENIED)),
        CaptureError::PermissionDenied
    );
    assert_eq!(
        native_error("Recreate", &Error::from_hresult(RO_E_CLOSED)),
        CaptureError::SourceClosed
    );
    assert_eq!(
        native_error("CreateDevice", &Error::from_hresult(E_FAIL)),
        CaptureError::Native {
            operation: "CreateDevice",
            code: E_FAIL.0,
        }
    );
}

struct Idle;

impl FrameHandler for Idle {
    fn frame(&mut self, _: &VideoSample<'_>) -> Flow {
        Flow::Continue
    }

    fn tick(&mut self) -> Tick {
        Tick::Idle
    }
}

fn wait_for(deadline: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

#[test]
fn a_stale_window_handle_ends_the_capture_as_a_closed_source() {
    let (states, options) = collecting();
    let session = start(
        SourceId::from_raw(SourceKind::Window, 0x10),
        options,
        || Idle,
    )
    .unwrap();
    let status = session.status();
    assert!(wait_for(Duration::from_secs(10), || matches!(
        status.snapshot().1,
        CaptureState::Failed(_)
    )));
    // A machine without Windows.Graphics.Capture reports that instead; either
    // way nothing started and the failure names its cause.
    match status.snapshot().1 {
        CaptureState::Failed(CaptureError::SourceClosed | CaptureError::Unsupported(_)) => {}
        other => panic!("unexpected state {other:?}"),
    }
    assert!(!states.lock().contains(&CaptureState::Capturing));
    assert!(session.stop().is_err());
}

#[test]
fn invalid_configurations_are_refused_before_any_thread_starts() {
    let mut options = StartOptions::new(CaptureConfig {
        width: 1920,
        height: 1080,
        frame_rate: 30,
    });
    options.observer = None;
    assert!(matches!(
        start(SourceId::from_raw(SourceKind::Display, 1), options, || Idle),
        Err(CaptureError::InvalidConfig)
    ));
}

/// One picture kept by a test.
#[derive(Clone)]
struct Owned {
    width: u32,
    height: u32,
    stride: usize,
    bytes: Vec<u8>,
    timestamp: Duration,
}

/// Keeps every picture's geometry and timestamp, and the bytes of the first
/// few, then stops after `limit` pictures.
struct Collector {
    out: Arc<Mutex<Vec<Owned>>>,
    limit: usize,
    keep_bytes: usize,
    repeat: bool,
}

impl FrameHandler for Collector {
    fn frame(&mut self, sample: &VideoSample<'_>) -> Flow {
        let mut out = self.out.lock();
        let keep = out.len() < self.keep_bytes;
        let (luma, chroma) = sample.planes();
        out.push(Owned {
            width: sample.width(),
            height: sample.height(),
            stride: sample.stride(),
            bytes: if keep {
                [luma, chroma].concat()
            } else {
                Vec::new()
            },
            timestamp: sample.timestamp,
        });
        if out.len() >= self.limit {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }

    fn tick(&mut self) -> Tick {
        if self.repeat {
            Tick::Repeat
        } else {
            Tick::Idle
        }
    }
}

/// A running capture with the pictures and states it produced.
type Captured = (
    CaptureSession,
    Arc<Mutex<Vec<Owned>>>,
    Arc<Mutex<Vec<CaptureState>>>,
);

fn capture(
    source: SourceId,
    conversion: Conversion,
    limit: usize,
    keep_bytes: usize,
    repeat: bool,
) -> Captured {
    let out = Arc::new(Mutex::new(Vec::new()));
    let (states, mut options) = collecting();
    options.conversion = conversion;
    let handler_out = Arc::clone(&out);
    let session = start(source, options, move || Collector {
        out: handler_out,
        limit,
        keep_bytes,
        repeat,
    })
    .expect("the capture thread starts");
    (session, out, states)
}

/// The NV12 triple the CPU converter assigns to a solid color: the reference the
/// GPU output is held to.
fn expected_yuv([r, g, b]: [u8; 3]) -> [i32; 3] {
    let source: Vec<u8> = [b, g, r, 255].repeat(4);
    let image = Bgra::new(&source, 8, 2, 2).unwrap();
    let mut out = vec![0; nv12_len(2, 2)];
    Converter::default()
        .convert(&image, 2, 2, 2, &mut out)
        .unwrap();
    [i32::from(out[0]), i32::from(out[4]), i32::from(out[5])]
}

/// The mean Y, Cb and Cr of the block around (`x`, `y`) of picture `owned`.
fn mean_yuv(owned: &Owned, x: u32, y: u32) -> [i32; 3] {
    let luma_len = owned.stride * owned.height as usize;
    let (mut sum_y, mut sum_cb, mut sum_cr, mut count_y, mut count_c) = (0, 0, 0, 0, 0);
    for row in y - 4..y + 4 {
        for column in x - 4..x + 4 {
            sum_y += i32::from(owned.bytes[row as usize * owned.stride + column as usize]);
            count_y += 1;
        }
    }
    for row in (y - 4) / 2..(y + 4) / 2 {
        for pair in (x - 4) / 2..(x + 4) / 2 {
            let at = luma_len + row as usize * owned.stride + pair as usize * 2;
            sum_cb += i32::from(owned.bytes[at]);
            sum_cr += i32::from(owned.bytes[at + 1]);
            count_c += 1;
        }
    }
    [sum_y / count_y, sum_cb / count_c, sum_cr / count_c]
}

fn assert_close(actual: [i32; 3], expected: [i32; 3], what: &str) {
    for (index, (a, e)) in actual.iter().zip(expected).enumerate() {
        assert!(
            (a - e).abs() <= 4,
            "{what}: component {index} is {a}, expected {e} (±4); got {actual:?} for {expected:?}"
        );
    }
}

#[test]
#[ignore = "captures a real window; needs an interactive Windows desktop session"]
fn availability_reports_that_capture_works_on_this_machine() {
    availability().unwrap();
}

#[test]
#[ignore = "captures a real window; needs an interactive Windows desktop session"]
fn the_displays_of_this_machine_are_listed_primary_first_and_never_include_own_windows() {
    let own = TestWindow::open(400, 300, 80, 80);
    let sources = list_sources().unwrap();
    assert!(!sources.is_empty());
    let displays: Vec<_> = sources
        .iter()
        .filter(|source| source.id.kind() == SourceKind::Display)
        .collect();
    assert!(!displays.is_empty(), "{sources:?}");
    assert!(displays[0].primary, "the primary display comes first");
    assert!(displays.iter().all(|d| d.width > 0 && d.height > 0));
    assert!(displays[0].title.starts_with("Screen 1 - "));
    // Displays precede windows, and this process's own test window is excluded.
    let first_window = sources
        .iter()
        .position(|source| source.id.kind() == SourceKind::Window)
        .unwrap_or(sources.len());
    assert_eq!(first_window, displays.len());
    assert!(sources.iter().all(|source| source.id != own.id()));
    // Titles are private to the user's desktop: only counts and sizes are shown.
    println!(
        "{} displays, {} windows",
        displays.len(),
        sources.len() - displays.len()
    );
    for display in &displays {
        println!("{} ({}x{})", display.title, display.width, display.height);
    }
}

#[test]
#[ignore = "captures a real window; needs an interactive Windows desktop session"]
fn a_window_is_captured_as_bt709_nv12_by_the_gpu_and_the_cpu_converter() {
    for conversion in [Conversion::Gpu, Conversion::Cpu] {
        let window = TestWindow::open(640, 360, 120, 120);
        let (session, out, states) = capture(window.id(), conversion, 8, 8, false);
        assert!(
            wait_for(Duration::from_secs(10), || out.lock().len() >= 8),
            "{conversion:?}: pictures arrived: {}, states {:?}",
            out.lock().len(),
            states.lock()
        );
        let pictures = out.lock().clone();
        let last = pictures.last().unwrap();
        println!(
            "{conversion:?}: {}x{} stride {}",
            last.width, last.height, last.stride
        );
        assert!(last.width.is_multiple_of(2) && last.height.is_multiple_of(2));
        assert!(last.width <= 1280 && last.height <= 720);
        // The window is the client area plus a frame: a little larger than the
        // 640x360 painted area, never scaled.
        assert!((640..800).contains(&last.width), "{}", last.width);
        assert!((360..480).contains(&last.height), "{}", last.height);
        assert_eq!(
            last.bytes.len(),
            nv12_len(last.stride, last.height as usize)
        );
        // The left quarter is red, the right quarter blue (BT.709, limited).
        let (x_left, x_right, y) = (last.width / 4, last.width * 3 / 4, last.height * 6 / 10);
        assert_close(
            mean_yuv(last, x_left, y),
            expected_yuv(LEFT_RGB),
            "red half",
        );
        assert_close(
            mean_yuv(last, x_right, y),
            expected_yuv(RIGHT_RGB),
            "blue half",
        );
        assert!(session.stop().is_ok());
        assert_eq!(states.lock().last(), Some(&CaptureState::Stopped));
        assert!(states.lock().contains(&CaptureState::Capturing));
    }
}

#[test]
#[ignore = "captures a real display; needs an interactive Windows desktop session"]
fn the_primary_display_is_captured_at_the_720p30_baseline() {
    let window = TestWindow::open(500, 300, 60, 60);
    let sources = list_sources().unwrap();
    let display = sources.iter().find(|source| source.primary).unwrap();
    let (session, out, states) = capture(display.id, Conversion::Auto, 90, 1, false);
    let started = Instant::now();
    assert!(
        wait_for(Duration::from_secs(10), || out.lock().len() >= 60),
        "{} pictures, states {:?}",
        out.lock().len(),
        states.lock()
    );
    let elapsed = started.elapsed();
    let pictures = out.lock().clone();
    let (width, height) = (pictures[0].width, pictures[0].height);
    println!(
        "display {}x{} -> {width}x{height}: {} pictures in {elapsed:?}",
        display.width,
        display.height,
        pictures.len()
    );
    let expected = CaptureConfig::SCREEN_SHARE_720P30.fit(display.width, display.height);
    assert_eq!((width, height), expected);
    assert!(
        pictures
            .iter()
            .all(|p| (p.width, p.height) == (width, height))
    );
    // Paced to at most 30 fps, with no picture earlier than the pacing slack.
    let intervals: Vec<Duration> = pictures
        .windows(2)
        .map(|pair| pair[1].timestamp.saturating_sub(pair[0].timestamp))
        .collect();
    assert!(
        pictures
            .windows(2)
            .all(|pair| pair[1].timestamp > pair[0].timestamp)
    );
    let shortest = intervals.iter().min().copied().unwrap();
    println!("shortest interval {shortest:?}");
    assert!(shortest >= Duration::from_millis(24), "{shortest:?}");
    let rate = pictures.len() as f64 / elapsed.as_secs_f64();
    assert!(rate > 15.0 && rate < 31.0, "{rate:.1} pictures per second");
    drop(window);
    assert!(session.stop().is_ok());
}

#[test]
#[ignore = "captures a real window; needs an interactive Windows desktop session"]
fn a_resized_window_changes_the_output_size_once_it_settles_without_a_storm() {
    let window = TestWindow::open(640, 360, 140, 140);
    let (session, out, states) = capture(window.id(), Conversion::Auto, 10_000, 0, false);
    assert!(wait_for(Duration::from_secs(10), || out.lock().len() >= 5));
    let before = {
        let out = out.lock();
        (out[0].width, out[0].height)
    };
    // A drag: many intermediate sizes within the settle time.
    for step in 0..12 {
        window.resize(660 + step * 10, 380 + step * 6);
        std::thread::sleep(Duration::from_millis(40));
    }
    window.resize(900, 520);
    let settled = wait_for(Duration::from_secs(5), || {
        let out = out.lock();
        out.last()
            .is_some_and(|picture| picture.width > before.0 + 200)
    });
    assert!(
        settled,
        "the output never followed the new size; states {:?}",
        states.lock()
    );
    let out = out.lock();
    let mut sizes: Vec<(u32, u32)> = out.iter().map(|p| (p.width, p.height)).collect();
    sizes.dedup();
    println!("output sizes over the resize: {sizes:?}");
    assert!(
        sizes
            .iter()
            .all(|&(w, h)| w.is_multiple_of(2) && h.is_multiple_of(2) && w <= 1280 && h <= 720)
    );
    // The drag did not reopen the output at every step.
    assert!(sizes.len() <= 3, "{sizes:?}");
    assert!(
        states
            .lock()
            .iter()
            .any(|state| matches!(state, CaptureState::SourceSize { width, .. } if *width >= 900))
    );
    drop(out);
    assert!(session.stop().is_ok());
}

#[test]
#[ignore = "captures a real window; needs an interactive Windows desktop session"]
fn closing_the_source_ends_the_capture_and_says_why() {
    let mut window = TestWindow::open(500, 300, 160, 160);
    let (session, out, states) = capture(window.id(), Conversion::Auto, 10_000, 0, false);
    assert!(wait_for(Duration::from_secs(10), || out.lock().len() >= 3));
    window.close();
    let status = session.status();
    assert!(
        wait_for(Duration::from_secs(5), || matches!(
            status.snapshot().1,
            CaptureState::Failed(_)
        )),
        "states {:?}",
        states.lock()
    );
    assert_eq!(
        status.snapshot().1,
        CaptureState::Failed(CaptureError::SourceClosed)
    );
    assert_eq!(session.stop(), Err(CaptureError::SourceClosed));
}

#[test]
#[ignore = "captures a real window; needs an interactive Windows desktop session"]
fn a_static_window_is_delivered_again_on_request_with_fresh_timestamps() {
    let window = TestWindow::open(500, 300, 180, 180);
    window.set_animated(false);
    std::thread::sleep(Duration::from_millis(300));
    let (session, out, _states) = capture(window.id(), Conversion::Auto, 12, 1, true);
    assert!(
        wait_for(Duration::from_secs(10), || out.lock().len() >= 12),
        "only {} pictures",
        out.lock().len()
    );
    let pictures = out.lock().clone();
    assert!(
        pictures
            .windows(2)
            .all(|pair| pair[1].timestamp > pair[0].timestamp)
    );
    assert!(
        pictures
            .iter()
            .all(|p| (p.width, p.height) == (pictures[0].width, pictures[0].height))
    );
    // The repeats are the same pixels: the first picture's bytes were kept.
    assert!(!pictures[0].bytes.is_empty());
    assert!(session.stop().is_ok());
}

/// Reads zero-copy pictures back (test only) so their pixels can be checked.
struct GpuCollector {
    out: Arc<Mutex<Vec<Owned>>>,
    copies: Arc<Mutex<usize>>,
    limit: usize,
}

impl FrameHandler for GpuCollector {
    fn frame(&mut self, _: &VideoSample<'_>) -> Flow {
        *self.copies.lock() += 1;
        Flow::Continue
    }

    fn wants_gpu(&self) -> bool {
        true
    }

    fn gpu_frame(&mut self, frame: &GpuFrame<'_>) -> GpuOutcome {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
            D3D11_USAGE_STAGING,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: plain queries and a staging copy on the capture's device,
        // on the capture thread, inside the callback that owns the texture.
        unsafe {
            frame.texture().GetDesc(&mut desc);
            assert_eq!(desc.Format, DXGI_FORMAT_NV12);
            assert_eq!((desc.Width, desc.Height), (frame.width(), frame.height()));
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            frame
                .device()
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
            let staging = staging.unwrap();
            let context = frame.device().GetImmediateContext().unwrap();
            context.CopyResource(&staging, frame.texture());
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let stride = mapped.RowPitch as usize;
            let bytes = std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                nv12_len(stride, desc.Height as usize),
            )
            .to_vec();
            context.Unmap(&staging, 0);
            let mut out = self.out.lock();
            out.push(Owned {
                width: desc.Width,
                height: desc.Height,
                stride,
                bytes,
                timestamp: frame.timestamp(),
            });
            GpuOutcome::Handled(if out.len() >= self.limit {
                Flow::Stop
            } else {
                Flow::Continue
            })
        }
    }

    fn tick(&mut self) -> Tick {
        Tick::Idle
    }
}

#[test]
#[ignore = "captures a real window; needs an interactive Windows desktop session"]
fn a_window_reaches_the_handler_as_a_gpu_nv12_texture_with_the_same_colors() {
    let window = TestWindow::open(640, 360, 120, 120);
    let out = Arc::new(Mutex::new(Vec::new()));
    let copies = Arc::new(Mutex::new(0));
    let (states, options) = collecting();
    let (handler_out, handler_copies) = (Arc::clone(&out), Arc::clone(&copies));
    let session = start(window.id(), options, move || GpuCollector {
        out: handler_out,
        copies: handler_copies,
        limit: 8,
    })
    .unwrap();
    assert!(
        wait_for(Duration::from_secs(10), || out.lock().len() >= 8),
        "states {:?}",
        states.lock()
    );
    assert_eq!(*copies.lock(), 0, "no picture took the copy path");
    let pictures = out.lock().clone();
    let last = pictures.last().unwrap();
    let (x_left, x_right, y) = (last.width / 4, last.width * 3 / 4, last.height * 6 / 10);
    assert_close(
        mean_yuv(last, x_left, y),
        expected_yuv(LEFT_RGB),
        "red half",
    );
    assert_close(
        mean_yuv(last, x_right, y),
        expected_yuv(RIGHT_RGB),
        "blue half",
    );
    assert!(session.stop().is_ok());
}
