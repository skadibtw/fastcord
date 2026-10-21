//! Milestone 32 on this machine: the zero-copy path is taken and its stream
//! decodes, a failing GPU encoder (or a device-removed error mid-stream) falls
//! back to the copy path without ending the share, and
//! `cpu_cost_per_frame...` records the CPU drop against the copy path. Run by
//! hand with
//! `cargo test -p fastcord-video --locked share::tests::zero_copy -- --ignored --nocapture --test-threads=1`.

use std::time::{Duration, Instant};

use fastcord_platform::capture::GpuFrame;
use fastcord_platform::capture::windows::test_window::TestWindow;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use super::*;
use crate::codec::{Backend, CodecError, VideoDecoder, VideoEncoder};
use crate::media_foundation::open_decoder;

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

/// User plus kernel CPU time of this whole process so far.
#[allow(unsafe_code)]
fn process_cpu() -> Duration {
    let (mut created, mut exited) = (FILETIME::default(), FILETIME::default());
    let (mut kernel, mut user) = (FILETIME::default(), FILETIME::default());
    // SAFETY: valid out pointers; the pseudo handle needs no closing.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    }
    .unwrap();
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
}

type Events = Arc<Mutex<Vec<ShareEvent>>>;

fn observer() -> (Events, ShareObserver) {
    let events: Events = Arc::default();
    let record = Arc::clone(&events);
    (events, Arc::new(move |event| record.lock().push(event)))
}

fn counting_sink(control: &ShareControl) -> Arc<Mutex<Vec<EncodedFrame>>> {
    let frames = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    control.attach(Box::new(move |unit| {
        let mut frames = sink.lock();
        if frames.len() < 600 {
            frames.push(unit);
        }
        SinkOutcome::Sent
    }));
    control.set_negotiated(true);
    frames
}

fn zero_copy_events(events: &Events) -> Vec<ZeroCopy> {
    events
        .lock()
        .iter()
        .filter_map(|event| match event {
            ShareEvent::ZeroCopy(state) => Some(state.clone()),
            _ => None,
        })
        .collect()
}

#[test]
#[ignore = "needs a GPU H.264 encoder and a Windows desktop session"]
fn the_zero_copy_path_encodes_a_window_without_a_readback() {
    let window = TestWindow::open(640, 360, 120, 120);
    let (events, observer) = observer();
    let worker =
        ShareWorker::start(window.id(), ShareConfig::SCREEN_SHARE_720P30, observer).unwrap();
    let control = worker.control();
    let frames = counting_sink(&control);
    assert!(
        wait_for(Duration::from_secs(10), || frames.lock().len() >= 30),
        "events {:?}",
        events.lock()
    );
    let stats = control.stats();
    println!("{stats:?}\n{:?}", zero_copy_events(&events));
    assert_eq!(zero_copy_events(&events), [ZeroCopy::Active]);
    assert!(stats.gpu_pictures > 0 && stats.gpu_pictures == stats.pictures);
    let report = events
        .lock()
        .iter()
        .find_map(|event| match event {
            ShareEvent::Encoder(report) => Some(report.clone()),
            _ => None,
        })
        .unwrap();
    println!("encoder: {}", report.backend);
    assert!(report.backend.contains("zero-copy") && report.backend.contains("hardware"));
    // What reached the transport is a valid stream of the painted window.
    let first = frames.lock()[0].clone();
    assert!(first.keyframe && crate::h264::inspect(&first.data).unwrap().idr);
    let mut decoder = open_decoder(BackendPreference::Software).unwrap().codec;
    let mut decoded = 0;
    let mut count = |_: crate::codec::DecodedFrame<'_>| decoded += 1;
    for unit in frames.lock().iter() {
        decoder
            .decode(&unit.data, unit.timestamp, &mut count)
            .unwrap();
    }
    decoder.flush(&mut count).unwrap();
    assert!(decoded >= 25, "{decoded} pictures decoded");
    worker.stop().unwrap();
}

/// Delegates to a real encoder, but its GPU path fails like a lost device
/// after `after` pictures.
struct Failing {
    inner: Box<dyn VideoEncoder>,
    after: u32,
}

impl VideoEncoder for Failing {
    fn backend(&self) -> &Backend {
        self.inner.backend()
    }

    fn config(&self) -> &EncoderConfig {
        self.inner.config()
    }

    fn encode(
        &mut self,
        frame: &crate::frame::Nv12<'_>,
        timestamp: Duration,
        keyframe: bool,
        out: &mut Vec<EncodedFrame>,
    ) -> Result<(), CodecError> {
        self.inner.encode(frame, timestamp, keyframe, out)
    }

    fn encode_gpu(
        &mut self,
        frame: &GpuFrame<'_>,
        timestamp: Duration,
        keyframe: bool,
        out: &mut Vec<EncodedFrame>,
    ) -> Result<(), CodecError> {
        if self.after == 0 {
            return Err(CodecError::Platform {
                operation: "IMFTransform::ProcessInput",
                code: DXGI_ERROR_DEVICE_REMOVED.0,
            });
        }
        self.after -= 1;
        self.inner.encode_gpu(frame, timestamp, keyframe, out)
    }

    fn flush(&mut self, out: &mut Vec<EncodedFrame>) -> Result<(), CodecError> {
        self.inner.flush(out)
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), CodecError> {
        self.inner.set_bitrate(bitrate)
    }
}

fn fall_back(opener: OpenGpuEncoder, expect_active_first: bool) {
    let window = TestWindow::open(640, 360, 120, 120);
    let (events, observer) = observer();
    let worker = ShareWorker::start_with_gpu(
        window.id(),
        ShareConfig::SCREEN_SHARE_720P30,
        observer,
        platform_encoder(),
        opener,
    )
    .unwrap();
    let control = worker.control();
    let frames = counting_sink(&control);
    assert!(
        wait_for(Duration::from_secs(10), || zero_copy_events(&events)
            .iter()
            .any(|state| matches!(state, ZeroCopy::Copy(_)))),
        "no fallback reported: {:?}",
        events.lock()
    );
    let states = zero_copy_events(&events);
    println!("{states:?}");
    assert_eq!(
        states.first() == Some(&ZeroCopy::Active),
        expect_active_first
    );
    // The share goes on, on the copy path, and restarts at a keyframe.
    let seen = frames.lock().len();
    assert!(wait_for(Duration::from_secs(10), || frames.lock().len() >= seen + 30));
    assert!(frames.lock()[seen..].iter().any(|unit| unit.keyframe));
    assert!(
        !events
            .lock()
            .iter()
            .any(|event| matches!(event, ShareEvent::Ended(_)))
    );
    worker.stop().unwrap();
}

#[test]
#[ignore = "captures a real window and encodes with Media Foundation; needs a Windows desktop session"]
fn a_gpu_encoder_that_cannot_open_leaves_the_share_on_the_copy_path() {
    fall_back(
        Box::new(|_, _| Err(OpenError::NoBackend(BackendPreference::Hardware))),
        false,
    );
}

#[test]
#[ignore = "needs a GPU H.264 encoder and a Windows desktop session"]
fn a_device_removed_error_mid_stream_falls_back_to_the_copy_path_without_ending_the_share() {
    let mut real = platform_gpu_encoder();
    fall_back(
        Box::new(move |config, frame| {
            let opened = real(config, frame)?;
            Ok(OpenedEncoder {
                encoder: Box::new(Failing {
                    inner: opened.encoder,
                    after: 20,
                }),
                rejected: opened.rejected,
            })
        }),
        true,
    );
}

/// CPU time per encoded frame of the copy path against the zero-copy path at
/// 720p30. The whole process is measured (capture thread, driver threads and
/// the test window painting), so the window alone is measured first and the
/// result is printed with and without that baseline.
#[test]
#[ignore = "benchmark: needs a GPU H.264 encoder and a Windows desktop session; prints numbers"]
fn cpu_cost_per_frame_of_the_copy_path_and_the_zero_copy_path_at_720p30() {
    const WARM_UP: Duration = Duration::from_secs(3);
    const MEASURE: Duration = Duration::from_secs(15);
    let window = TestWindow::open(1280, 720, 60, 60);
    std::thread::sleep(WARM_UP);
    let before = process_cpu();
    std::thread::sleep(MEASURE);
    let baseline = (process_cpu() - before).as_secs_f64() / MEASURE.as_secs_f64();
    println!(
        "window alone: {:.1} ms of CPU per second",
        baseline * 1000.0
    );
    for zero_copy in [false, true] {
        let (events, observer) = observer();
        let mut config = ShareConfig::SCREEN_SHARE_720P30;
        config.zero_copy = zero_copy;
        let worker = ShareWorker::start(window.id(), config, observer).unwrap();
        let control = worker.control();
        let _frames = counting_sink(&control);
        std::thread::sleep(WARM_UP);
        let (cpu, encoded) = (process_cpu(), control.stats().encoded);
        std::thread::sleep(MEASURE);
        let (cpu, encoded) = (
            (process_cpu() - cpu).as_secs_f64(),
            control.stats().encoded - encoded,
        );
        let stats = control.stats();
        let net = (cpu - baseline * MEASURE.as_secs_f64()).max(0.0);
        println!(
            "zero_copy={zero_copy}: {encoded} frames, {:.2} ms CPU per frame ({:.2} ms above the window alone), gpu pictures {}, zero-copy events {:?}",
            cpu * 1000.0 / encoded as f64,
            net * 1000.0 / encoded as f64,
            stats.gpu_pictures,
            zero_copy_events(&events),
        );
        assert!(encoded > 300, "only {encoded} frames");
        worker.stop().unwrap();
    }
}
