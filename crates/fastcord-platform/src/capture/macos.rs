//! macOS screen and window capture through ScreenCaptureKit (SPEC §8.4,
//! milestone 34).
//!
//! ScreenCaptureKit scales the source and converts it to NV12 (`420v`,
//! BT.709 limited range) on the GPU, so no converter runs here. Its sample
//! handler copies the two planes of each new picture into a reused buffer on
//! its own serial dispatch queue and wakes the owner thread, which paces the
//! pictures and calls the [`FrameHandler`]. The first listing asks macOS for
//! Screen Recording permission; a refusal is [`CaptureError::PermissionDenied`].
//!
//! Unsafe code is confined to this module: it is the Objective-C and
//! CoreVideo FFI.
#![allow(unsafe_code)]

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID,
    kCGDisplayStreamYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};
use parking_lot::Mutex;

use super::camera::CameraDevice;
use super::convert;
use super::{
    CaptureConfig, CaptureError, CaptureSource, CaptureState, CaptureStatus, Control,
    ConversionPath, Flow, FrameHandler, HandlerFactory, MAX_SOURCES, MAX_TITLE_CHARS, SourceId,
    SourceKind, StartOptions, Tick, VideoSample,
};

/// How long a quiet capture waits before asking the handler for a tick.
const TICK: Duration = Duration::from_millis(250);
/// How long ScreenCaptureKit may take to answer a listing, start or stop.
const ANSWER: Duration = Duration::from_secs(10);
/// `SCStreamErrorUserDeclined`: Screen Recording permission is not granted.
const USER_DECLINED: isize = -3801;

enum Event {
    Frame,
    Stopped(CaptureError),
    Wake,
    Stop,
}

pub(super) struct Native {
    events: SyncSender<Event>,
    thread: Option<JoinHandle<Result<(), CaptureError>>>,
}

impl Native {
    pub(super) fn waker(&self) -> Box<dyn Fn() + Send + Sync> {
        let events = self.events.clone();
        Box::new(move || {
            let _ = events.try_send(Event::Wake);
        })
    }

    pub(super) fn stop(mut self) -> Result<(), CaptureError> {
        self.finish()
    }

    fn finish(&mut self) -> Result<(), CaptureError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        // The queue may be full of frame wake-ups; the stop must get through.
        let _ = self.events.send(Event::Stop);
        thread
            .join()
            .unwrap_or(Err(CaptureError::Worker("the capture thread panicked")))
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn ns_error(error: *mut NSError, operation: &'static str) -> CaptureError {
    // SAFETY: ScreenCaptureKit hands either null or a valid error.
    match unsafe { error.as_ref() } {
        Some(error) if error.code() == USER_DECLINED => CaptureError::PermissionDenied,
        Some(error) => CaptureError::Native {
            operation,
            code: error.code() as i32,
        },
        None => CaptureError::Worker("ScreenCaptureKit failed without an error"),
    }
}

/// The shareable displays and on-screen windows, from ScreenCaptureKit.
pub(crate) fn shareable() -> Result<Retained<SCShareableContent>, CaptureError> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: a non-null content is a valid object we now co-own.
            let content = unsafe { Retained::retain(content) };
            let _ = sender.send(match content {
                Some(content) => Ok(content),
                None => Err(ns_error(error, "SCShareableContent")),
            });
        },
    );
    // SAFETY: the block is valid for the call and copied by the framework.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &block,
        );
    }
    receiver
        .recv_timeout(ANSWER)
        .unwrap_or(Err(CaptureError::Worker("ScreenCaptureKit did not answer")))
}

/// Pixels per point of a display (2 on Retina).
fn scale_of(display: CGDirectDisplayID) -> f64 {
    let mode = CGDisplayCopyDisplayMode(display);
    let points = CGDisplayMode::width(mode.as_deref());
    let pixels = CGDisplayMode::pixel_width(mode.as_deref());
    if points == 0 || pixels == 0 {
        1.0
    } else {
        pixels as f64 / points as f64
    }
}

fn truncated(text: &str) -> String {
    text.chars().take(MAX_TITLE_CHARS).collect()
}

pub(super) fn list_sources() -> Result<Vec<CaptureSource>, CaptureError> {
    let content = shareable()?;
    let main = CGMainDisplayID();
    let own = std::process::id() as i32;
    let mut sources = Vec::new();
    // SAFETY: plain property reads on objects ScreenCaptureKit returned.
    unsafe {
        for (index, display) in content.displays().iter().enumerate() {
            let id = display.displayID();
            let scale = scale_of(id);
            let (width, height) = (
                (display.width() as f64 * scale) as u32,
                (display.height() as f64 * scale) as u32,
            );
            sources.push(CaptureSource {
                id: SourceId::from_raw(SourceKind::Display, u64::from(id)),
                title: format!("Screen {} ({width}×{height})", index + 1),
                width,
                height,
                primary: id == main,
            });
        }
        let scale = scale_of(main);
        for window in content.windows().iter() {
            if sources.len() >= MAX_SOURCES {
                break;
            }
            let app = window.owningApplication();
            if window.windowLayer() != 0
                || !window.isOnScreen()
                || app.as_ref().is_some_and(|app| app.processID() == own)
            {
                continue;
            }
            let title = window.title().map(|t| t.to_string()).unwrap_or_default();
            let frame = window.frame();
            if title.is_empty() || frame.size.width < 2.0 || frame.size.height < 2.0 {
                continue;
            }
            let label = match app.map(|app| app.applicationName().to_string()) {
                Some(name) if !name.is_empty() => format!("{name} – {title}"),
                _ => title,
            };
            sources.push(CaptureSource {
                id: SourceId::from_raw(SourceKind::Window, u64::from(window.windowID())),
                title: truncated(&label),
                width: (frame.size.width * scale) as u32,
                height: (frame.size.height * scale) as u32,
                primary: false,
            });
        }
    }
    sources.truncate(MAX_SOURCES);
    Ok(sources)
}

pub(super) fn availability() -> Result<(), CaptureError> {
    // ScreenCaptureKit exists on every supported macOS (12.3+, Info.plist);
    // permission is asked for by the listing.
    Ok(())
}

/// The newest picture the sample handler copied, waiting for the owner.
#[derive(Default)]
struct Latest {
    picture: Vec<u8>,
    width: u32,
    height: u32,
    timestamp: Duration,
    fresh: bool,
}

struct Ivars {
    latest: Arc<Mutex<Latest>>,
    events: SyncSender<Event>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `Output` does not
    // implement `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output_sample(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen && copy_picture(sample, &self.ivars().latest) {
                // A full queue already holds a wake-up.
                let _ = self.ivars().events.try_send(Event::Frame);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let ended = if error.code() == USER_DECLINED {
                CaptureError::PermissionDenied
            } else {
                // The window closed, the display went away, or the user
                // stopped the share from the system menu.
                CaptureError::SourceClosed
            };
            let _ = self.ivars().events.send(Event::Stopped(ended));
        }
    }
);

impl Output {
    fn new(latest: Arc<Mutex<Latest>>, events: SyncSender<Event>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { latest, events });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Monotonic time for picture stamps; only differences are meaningful.
fn now() -> Duration {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

/// Copies an NV12 sample into `latest` (stride = width). Idle samples carry no
/// picture and are skipped.
fn copy_picture(sample: &CMSampleBuffer, latest: &Mutex<Latest>) -> bool {
    // SAFETY: the sample is valid for the callback; the pixel buffer is locked
    // read-only while its planes are read, and every read stays inside the
    // plane rows CoreVideo reports.
    unsafe {
        let Some(buffer) = sample.image_buffer() else {
            return false;
        };
        if CVPixelBufferGetPixelFormatType(&buffer)
            != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        {
            return false;
        }
        if CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return false;
        }
        let width = CVPixelBufferGetWidth(&buffer) & !1;
        let height = CVPixelBufferGetHeight(&buffer) & !1;
        let mut latest = latest.lock();
        latest.picture.resize(convert::nv12_len(width, height), 0);
        let mut copied = width > 0 && height > 0;
        for (plane, rows, offset) in [(0, height, 0), (1, height / 2, width * height)] {
            let base = CVPixelBufferGetBaseAddressOfPlane(&buffer, plane).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, plane);
            if base.is_null() || stride < width {
                copied = false;
                break;
            }
            for row in 0..rows {
                let source = std::slice::from_raw_parts(base.add(row * stride), width);
                latest.picture[offset + row * width..offset + (row + 1) * width]
                    .copy_from_slice(source);
            }
        }
        CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly);
        if copied {
            latest.width = width as u32;
            latest.height = height as u32;
            latest.timestamp = now();
            latest.fresh = true;
        }
        copied
    }
}

/// Where a completion handler's outcome arrives.
pub(crate) type Answer = Receiver<Result<(), CaptureError>>;

/// Waits for a ScreenCaptureKit completion handler.
pub(crate) fn completion(operation: &'static str) -> (RcBlock<dyn Fn(*mut NSError)>, Answer) {
    let (sender, receiver) = mpsc::sync_channel(1);
    let block = RcBlock::new(move |error: *mut NSError| {
        let _ = sender.send(if error.is_null() {
            Ok(())
        } else {
            Err(ns_error(error, operation))
        });
    });
    (block, receiver)
}

pub(crate) fn answered(receiver: &Answer) -> Result<(), CaptureError> {
    receiver
        .recv_timeout(ANSWER)
        .unwrap_or(Err(CaptureError::Worker("ScreenCaptureKit did not answer")))
}

/// The filter for `source`, re-validated against what is shareable now.
fn filter(source: SourceId) -> Result<(Retained<SCContentFilter>, u32, u32), CaptureError> {
    let content = shareable()?;
    // SAFETY: property reads and initializers on objects ScreenCaptureKit returned.
    unsafe {
        match source.kind() {
            SourceKind::Display => {
                let display = content
                    .displays()
                    .iter()
                    .find(|display| u64::from(display.displayID()) == source.raw())
                    .ok_or(CaptureError::SourceClosed)?;
                let scale = scale_of(display.displayID());
                let filter = SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &NSArray::new(),
                );
                Ok((
                    filter,
                    (display.width() as f64 * scale) as u32,
                    (display.height() as f64 * scale) as u32,
                ))
            }
            SourceKind::Window => {
                let window = content
                    .windows()
                    .iter()
                    .find(|window| u64::from(window.windowID()) == source.raw())
                    .ok_or(CaptureError::SourceClosed)?;
                let scale = scale_of(CGMainDisplayID());
                let frame = window.frame();
                let filter = SCContentFilter::initWithDesktopIndependentWindow(
                    SCContentFilter::alloc(),
                    &window,
                );
                Ok((
                    filter,
                    (frame.size.width * scale) as u32,
                    (frame.size.height * scale) as u32,
                ))
            }
        }
    }
}

fn configuration(
    config: CaptureConfig,
    width: u32,
    height: u32,
) -> Retained<SCStreamConfiguration> {
    let (width, height) = config.fit(width, height);
    // SAFETY: plain property writes on a new configuration.
    unsafe {
        let configuration = SCStreamConfiguration::new();
        configuration.setWidth(width as usize);
        configuration.setHeight(height as usize);
        // A resized window is scaled into the picture rather than cropped.
        configuration.setScalesToFit(true);
        configuration.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        configuration.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
        configuration.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: config.frame_rate as i32,
            flags: objc2_core_media::CMTimeFlags::Valid,
            epoch: 0,
        });
        configuration.setQueueDepth(super::NATIVE_FRAME_COUNT as isize);
        configuration.setShowsCursor(true);
        configuration
    }
}

pub(super) fn start(
    source: SourceId,
    options: &StartOptions,
    control: Arc<Control>,
    status: CaptureStatus,
    handler: HandlerFactory,
) -> Result<Native, CaptureError> {
    let (events, receiver) = mpsc::sync_channel(4);
    let config = options.config;
    let output_events = events.clone();
    let thread = std::thread::Builder::new()
        .name("fastcord-capture".to_owned())
        .spawn(move || {
            let result = run(
                source,
                config,
                &control,
                &status,
                handler,
                output_events,
                &receiver,
            );
            status.set(match &result {
                Ok(()) => CaptureState::Stopped,
                Err(error) => CaptureState::Failed(error.clone()),
            });
            result
        })
        .map_err(|_| CaptureError::Worker("the capture thread could not be started"))?;
    Ok(Native {
        events,
        thread: Some(thread),
    })
}

fn run(
    source: SourceId,
    config: CaptureConfig,
    control: &Control,
    status: &CaptureStatus,
    handler: HandlerFactory,
    events: SyncSender<Event>,
    receiver: &Receiver<Event>,
) -> Result<(), CaptureError> {
    let (filter, width, height) = filter(source)?;
    let latest = Arc::new(Mutex::new(Latest::default()));
    let output = Output::new(Arc::clone(&latest), events);
    let queue = DispatchQueue::new("dev.fastcord.capture", None);
    // SAFETY: the stream, its output and its delegate stay alive until the
    // stream is stopped below; the sample handler runs on `queue` only.
    let stream = unsafe {
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &configuration(config, width, height),
            Some(ProtocolObject::from_ref(&*output)),
        );
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .map_err(|error| ns_error(Retained::as_ptr(&error).cast_mut(), "addStreamOutput"))?;
        let (block, started) = completion("startCapture");
        stream.startCaptureWithCompletionHandler(Some(&block));
        answered(&started)?;
        stream
    };
    status.set(CaptureState::Converting(ConversionPath::Gpu));
    status.set(CaptureState::Capturing);
    let result = deliver(control, handler(), &latest, receiver);
    // SAFETY: as above; the stop is waited for so no sample runs afterwards.
    unsafe {
        let (block, stopped) = completion("stopCapture");
        stream.stopCaptureWithCompletionHandler(Some(&block));
        let _ = answered(&stopped);
    }
    result
}

/// The owner loop: paces pictures, ticks the handler and waits for the end.
fn deliver(
    control: &Control,
    mut handler: Box<dyn FrameHandler>,
    latest: &Mutex<Latest>,
    receiver: &Receiver<Event>,
) -> Result<(), CaptureError> {
    let mut picture = Vec::new();
    let mut size = (0, 0);
    let mut delivered: Option<Instant> = None;
    let mut quiet_since = Instant::now();
    let send = |handler: &mut Box<dyn FrameHandler>, picture: &[u8], size: (u32, u32), at| {
        match VideoSample::nv12(size.0, size.1, size.0 as usize, picture, at) {
            Ok(sample) => handler.frame(&sample),
            Err(_) => Flow::Continue,
        }
    };
    loop {
        let interval = control.min_interval();
        let fresh = latest.lock().fresh;
        let wait = match (fresh, delivered) {
            (true, Some(at)) => interval.saturating_sub(at.elapsed()),
            _ => TICK.saturating_sub(quiet_since.elapsed()),
        };
        let mut woken = false;
        match receiver.recv_timeout(wait.max(Duration::from_millis(1))) {
            Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Ok(Event::Stopped(error)) => return Err(error),
            Ok(Event::Wake) => woken = true,
            Ok(Event::Frame) | Err(RecvTimeoutError::Timeout) => {}
        }
        if delivered.is_none_or(|at| at.elapsed() >= control.min_interval()) {
            let at = {
                let mut latest = latest.lock();
                if latest.fresh {
                    latest.fresh = false;
                    std::mem::swap(&mut picture, &mut latest.picture);
                    size = (latest.width, latest.height);
                    Some(latest.timestamp)
                } else {
                    None
                }
            };
            if let Some(at) = at {
                delivered = Some(Instant::now());
                quiet_since = Instant::now();
                if send(&mut handler, &picture, size, at) == Flow::Stop {
                    return Ok(());
                }
            }
        }
        if woken || quiet_since.elapsed() >= TICK {
            quiet_since = Instant::now();
            match handler.tick() {
                Tick::Idle => {}
                Tick::Repeat if !picture.is_empty() => {
                    if send(&mut handler, &picture, size, now()) == Flow::Stop {
                        return Ok(());
                    }
                }
                Tick::Repeat => {}
                Tick::Stop => return Ok(()),
            }
        }
    }
}

const CAMERA_UNSUPPORTED: CaptureError =
    CaptureError::Unsupported("the camera is only implemented for Windows so far");

pub(super) fn list_cameras() -> Result<Vec<CameraDevice>, CaptureError> {
    Err(CAMERA_UNSUPPORTED)
}

pub(super) fn start_camera(
    _device: Option<String>,
    _options: &StartOptions,
    _control: Arc<Control>,
    _status: CaptureStatus,
    _handler: HandlerFactory,
) -> Result<Native, CaptureError> {
    Err(CAMERA_UNSUPPORTED)
}
