//! Native screen capture (SPEC §8.4, milestone 29): which displays and windows
//! can be shared, and a capture session that delivers their pixels as NV12
//! pictures bounded to the screen-share baseline.
//!
//! One session owns one dedicated native thread. Everything that touches the
//! operating system's capture objects, the GPU device and the frame handler
//! runs there; other threads only hold a [`CaptureSession`] (stop, wake, pace)
//! and read [`CaptureStatus`]. A [`FrameHandler`] borrows each picture for the
//! duration of its callback and must copy what it keeps. Dropping the session
//! stops and joins the thread, so no capture worker outlives a share.
//!
//! Windows.Graphics.Capture is the only backend so far (SPEC milestones 33 and
//! 34 add Linux and macOS); elsewhere every entry point reports
//! [`CaptureError::Unsupported`].

pub mod convert;
#[cfg(not(windows))]
mod unsupported;
#[cfg(windows)]
pub mod windows;

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;

#[cfg(not(windows))]
use unsupported as native;
#[cfg(windows)]
use windows as native;

/// Titles and labels are cut to this many Unicode scalar values.
pub const MAX_TITLE_CHARS: usize = 120;
/// Sources listed at once, so a pathological desktop cannot grow the picker.
pub const MAX_SOURCES: usize = 128;
/// WGC's frame pool is bounded separately from the scaled output. Supports a
/// 3840×2160 desktop and a 5120×1440 ultra-wide; larger sources fail visibly
/// rather than being cropped or allocating an unbounded native pool.
pub const MAX_NATIVE_DIMENSION: u32 = 8192;
/// Largest native area captured: 4096 × 2304.
pub const MAX_NATIVE_PIXELS: u64 = 4096 * 2304;
/// Native capture buffers in the OS frame pool: two for the system and the
/// frame being converted, one for the newest frame kept so it can be delivered
/// again without waiting for the screen to change.
pub const NATIVE_FRAME_COUNT: usize = 3;

/// The bounds of the single screen-share layer (SPEC §8.2): a source is scaled
/// down to fit `width` × `height` and frames are paced to `frame_rate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub frame_rate: u32,
}

impl CaptureConfig {
    /// The implemented baseline: 1280×720 at 30 fps.
    pub const SCREEN_SHARE_720P30: Self = Self {
        width: 1280,
        height: 720,
        frame_rate: 30,
    };

    pub fn validate(self) -> Result<(), CaptureError> {
        let edge = |edge: u32, max: u32| {
            (convert::MIN_OUTPUT_EDGE..=max).contains(&edge) && edge.is_multiple_of(2)
        };
        if !edge(self.width, 1280)
            || !edge(self.height, 720)
            || !(1..=30).contains(&self.frame_rate)
        {
            return Err(CaptureError::InvalidConfig);
        }
        Ok(())
    }

    /// The nominal time between pictures.
    pub fn frame_interval(self) -> Duration {
        Duration::from_secs(1) / self.frame_rate.max(1)
    }

    /// The picture size a source of `width` × `height` is delivered at.
    pub fn fit(self, width: u32, height: u32) -> (u32, u32) {
        convert::fit_output(width, height, self.width, self.height)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceKind {
    Display,
    Window,
}

/// An operating-system handle naming a display or window (an `HMONITOR` or
/// `HWND` on Windows). It is only meaningful on this machine, within this
/// login session, and may go stale: starting a capture re-validates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceId {
    kind: SourceKind,
    handle: u64,
}

impl SourceId {
    pub const fn from_raw(kind: SourceKind, handle: u64) -> Self {
        Self { kind, handle }
    }

    pub const fn kind(self) -> SourceKind {
        self.kind
    }

    pub const fn raw(self) -> u64 {
        self.handle
    }
}

/// Something the user can share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureSource {
    pub id: SourceId,
    /// A window's title or a display's label, at most [`MAX_TITLE_CHARS`]
    /// characters.
    pub title: String,
    /// Native size in pixels.
    pub width: u32,
    pub height: u32,
    /// The primary display.
    pub primary: bool,
}

/// One NV12 picture, valid only during the callback that receives it: a luma
/// plane of `height` rows followed directly by `ceil(height / 2)` rows of
/// interleaved chroma, every row `stride` bytes apart.
#[derive(Clone, Copy)]
pub struct VideoSample<'a> {
    width: u32,
    height: u32,
    stride: usize,
    bytes: &'a [u8],
    /// When the picture was captured, on the monotonic clock of the machine
    /// (time since boot). Only differences are meaningful.
    pub timestamp: Duration,
}

impl<'a> VideoSample<'a> {
    pub fn nv12(
        width: u32,
        height: u32,
        stride: usize,
        bytes: &'a [u8],
        timestamp: Duration,
    ) -> Result<Self, CaptureError> {
        if width == 0 || height == 0 || stride < width as usize {
            return Err(CaptureError::InvalidSample(
                "an NV12 picture needs nonzero size and a stride covering its rows",
            ));
        }
        let needed = stride
            .checked_mul(height as usize + height.div_ceil(2) as usize)
            .ok_or(CaptureError::InvalidSample("NV12 picture size overflows"))?;
        if bytes.len() < needed {
            return Err(CaptureError::InvalidSample(
                "an NV12 picture is shorter than its rows",
            ));
        }
        Ok(Self {
            width,
            height,
            stride,
            bytes,
            timestamp,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    /// The luma plane and the interleaved chroma plane, each starting at its
    /// first row.
    pub fn planes(&self) -> (&'a [u8], &'a [u8]) {
        self.bytes.split_at(self.stride * self.height as usize)
    }
}

impl fmt::Debug for VideoSample<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoSample")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("timestamp", &self.timestamp)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    InvalidConfig,
    /// Capture is not available on this operating system, build or machine.
    Unsupported(&'static str),
    /// The system refused to capture this source (a privacy setting, policy or
    /// protected content).
    PermissionDenied,
    /// The window or display disappeared, or the handle was already stale.
    SourceClosed,
    SourceTooLarge {
        width: u32,
        height: u32,
    },
    /// A native call failed with this platform status (an `HRESULT`).
    Native {
        operation: &'static str,
        code: i32,
    },
    InvalidSample(&'static str),
    /// The owner thread could not be started or ended abnormally.
    Worker(&'static str),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig => f.write_str(
                "capture supports even output sizes up to 1280x720 at 1-30 frames per second",
            ),
            Self::Unsupported(detail) | Self::InvalidSample(detail) | Self::Worker(detail) => {
                f.write_str(detail)
            }
            Self::PermissionDenied => f.write_str(
                "Windows refused to capture this source (a privacy setting, a policy, or a protected window)",
            ),
            Self::SourceClosed => f.write_str("the shared window or display closed"),
            Self::SourceTooLarge { width, height } => write!(
                f,
                "the source is {width}x{height}; at most 4096x2304 pixels (or 8192 on one side) can be captured"
            ),
            Self::Native { operation, code } => {
                write!(f, "{operation} failed ({:#010X})", *code as u32)
            }
        }
    }
}

impl std::error::Error for CaptureError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureState {
    Starting,
    /// The operating system accepted the capture. Pictures follow whenever the
    /// source changes.
    Capturing,
    /// The converter in use: reported when the capture starts and again if an
    /// automatic GPU conversion had to be abandoned for the CPU.
    Converting(ConversionPath),
    /// The captured content changed size (a window was resized).
    SourceSize {
        width: u32,
        height: u32,
    },
    /// Ended because it was asked to.
    Stopped,
    Failed(CaptureError),
}

/// Called on the capture thread whenever the state changes. It must not block.
pub type StateObserver = Arc<dyn Fn(&CaptureState) + Send + Sync>;

/// A newest-only status slot, not a growing event queue: the terminal state
/// stays observable even if nobody polled while the owner shut down.
#[derive(Clone)]
pub struct CaptureStatus {
    slot: Arc<Mutex<(u64, CaptureState)>>,
    // Only a backend notifies it; Windows is the only one so far.
    #[cfg_attr(not(windows), allow(dead_code))]
    observer: Option<StateObserver>,
}

impl CaptureStatus {
    fn new(observer: Option<StateObserver>) -> Self {
        Self {
            slot: Arc::new(Mutex::new((0, CaptureState::Starting))),
            observer,
        }
    }

    /// The revision (changed on every update) and latest state.
    pub fn snapshot(&self) -> (u64, CaptureState) {
        self.slot.lock().clone()
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn set(&self, state: CaptureState) {
        {
            let mut current = self.slot.lock();
            current.0 = current.0.wrapping_add(1);
            current.1 = state.clone();
        }
        if let Some(observer) = &self.observer {
            observer(&state);
        }
    }
}

/// What a capture session delivers to, on its capture thread.
pub trait FrameHandler {
    /// A new picture, at most one per frame interval.
    fn frame(&mut self, sample: &VideoSample<'_>) -> Flow;

    /// Called at least every few hundred milliseconds while nothing new
    /// arrives, and promptly after [`CaptureSession::poke`]: a static screen
    /// produces no pictures, yet keyframes and keep-alives are still due.
    /// Answering [`Tick::Repeat`] delivers the newest picture again.
    fn tick(&mut self) -> Tick;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// End the capture; the session reports [`CaptureState::Stopped`].
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tick {
    Idle,
    /// Deliver the newest picture again, stamped with the current time.
    Repeat,
    Stop,
}

/// How a backend turns captured pixels into the output picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Conversion {
    /// The GPU video processor, falling back to the CPU if it cannot be used.
    #[default]
    Auto,
    /// The GPU video processor only; failure ends the capture.
    Gpu,
    /// The CPU converter only.
    Cpu,
}

/// Which converter is producing the pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversionPath {
    /// The GPU video processor scales and converts; only the output picture
    /// is read back.
    Gpu,
    /// The surface is read back whole and converted on the CPU.
    Cpu,
}

#[derive(Clone)]
pub struct StartOptions {
    pub config: CaptureConfig,
    pub conversion: Conversion,
    pub observer: Option<StateObserver>,
}

impl StartOptions {
    pub fn new(config: CaptureConfig) -> Self {
        Self {
            config,
            conversion: Conversion::Auto,
            observer: None,
        }
    }
}

/// Settings the owner thread re-reads between pictures, and the way to wake it.
pub(crate) struct Control {
    min_interval_micros: AtomicU64,
    waker: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl Control {
    fn new(interval: Duration) -> Self {
        Self {
            min_interval_micros: AtomicU64::new(interval.as_micros() as u64),
            waker: OnceLock::new(),
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn min_interval(&self) -> Duration {
        Duration::from_micros(self.min_interval_micros.load(Ordering::Relaxed))
    }
}

/// What other threads may do to a running capture: pace it and wake it. Cheap
/// to clone; it keeps nothing of the capture itself alive.
#[derive(Clone)]
pub struct CaptureController(Arc<Control>);

impl CaptureController {
    /// The least time between delivered pictures; raise it while nobody needs
    /// the stream, lower it again to resume the configured rate.
    pub fn set_min_interval(&self, interval: Duration) {
        self.0.min_interval_micros.store(
            interval.as_micros().clamp(1, u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
    }

    /// Wakes the capture thread now, so the handler's `tick` runs without
    /// waiting for its timer.
    pub fn poke(&self) {
        if let Some(waker) = self.0.waker.get() {
            waker();
        }
    }
}

/// A running capture. Dropping it stops the capture thread and waits (briefly)
/// for it to release the operating system's objects.
pub struct CaptureSession {
    control: Arc<Control>,
    status: CaptureStatus,
    native: native::Native,
}

impl CaptureSession {
    pub fn status(&self) -> CaptureStatus {
        self.status.clone()
    }

    pub fn controller(&self) -> CaptureController {
        CaptureController(Arc::clone(&self.control))
    }

    /// Stops the capture and waits for the thread, returning its terminal
    /// error if it failed.
    pub fn stop(self) -> Result<(), CaptureError> {
        self.native.stop()
    }
}

/// The displays and windows that can be shared right now, displays first.
/// Never lists this process's own windows. May block briefly; call it off the
/// UI thread.
pub fn list_sources() -> Result<Vec<CaptureSource>, CaptureError> {
    native::list_sources()
}

/// Whether capture can work at all on this machine.
pub fn availability() -> Result<(), CaptureError> {
    native::availability()
}

/// Starts capturing `source` on a new native thread. Returns as soon as the
/// thread exists; whether the operating system accepted the capture (or
/// refused it) is reported through the observer and [`CaptureStatus`]. The
/// handler is built on the capture thread, so it may own objects that must not
/// leave it.
pub fn start<H: FrameHandler + 'static>(
    source: SourceId,
    options: StartOptions,
    handler: impl FnOnce() -> H + Send + 'static,
) -> Result<CaptureSession, CaptureError> {
    options.config.validate()?;
    let control = Arc::new(Control::new(options.config.frame_interval()));
    let status = CaptureStatus::new(options.observer.clone());
    let native = native::start(
        source,
        &options,
        Arc::clone(&control),
        status.clone(),
        Box::new(move || Box::new(handler()) as Box<dyn FrameHandler>),
    )?;
    // Wake-ups before this point have nobody to wake; the thread's first loop
    // pass runs the handler's `tick` anyway.
    let _ = control.waker.set(native.waker());
    Ok(CaptureSession {
        control,
        status,
        native,
    })
}

/// Builds the handler on the capture thread.
pub(crate) type HandlerFactory = Box<dyn FnOnce() -> Box<dyn FrameHandler> + Send>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_baseline_is_valid_and_bounds_are_enforced() {
        let base = CaptureConfig::SCREEN_SHARE_720P30;
        base.validate().unwrap();
        assert_eq!(base.frame_interval(), Duration::from_nanos(33_333_333));
        for invalid in [
            CaptureConfig {
                width: 1282,
                ..base
            },
            CaptureConfig {
                height: 722,
                ..base
            },
            CaptureConfig { width: 8, ..base },
            CaptureConfig { height: 15, ..base },
            CaptureConfig {
                frame_rate: 0,
                ..base
            },
            CaptureConfig {
                frame_rate: 31,
                ..base
            },
            CaptureConfig {
                width: 1279,
                ..base
            },
        ] {
            assert_eq!(invalid.validate(), Err(CaptureError::InvalidConfig));
        }
        assert_eq!(base.fit(3840, 2160), (1280, 720));
        assert_eq!(base.fit(640, 480), (640, 480));
    }

    #[test]
    fn samples_validate_their_layout_and_split_planes_at_the_luma_size() {
        // 4x3 picture, stride 8: 3 luma rows then 2 chroma rows.
        let bytes: Vec<u8> = (0..40).collect();
        let sample = VideoSample::nv12(4, 3, 8, &bytes, Duration::from_millis(5)).unwrap();
        let (luma, chroma) = sample.planes();
        assert_eq!(luma.len(), 24);
        assert_eq!(chroma[0], 24);
        assert_eq!(
            (sample.width(), sample.height(), sample.stride()),
            (4, 3, 8)
        );
        assert!(VideoSample::nv12(4, 3, 8, &bytes[..39], Duration::ZERO).is_err());
        assert!(VideoSample::nv12(4, 3, 3, &bytes, Duration::ZERO).is_err());
        assert!(VideoSample::nv12(0, 3, 8, &bytes, Duration::ZERO).is_err());
        assert!(VideoSample::nv12(4, 3, usize::MAX, &bytes, Duration::ZERO).is_err());
    }

    #[test]
    fn status_keeps_the_newest_state_and_notifies_the_observer_outside_its_lock() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let observer: StateObserver = Arc::new(move |state| {
            record.lock().push(state.clone());
        });
        let status = CaptureStatus::new(Some(observer));
        assert_eq!(status.snapshot(), (0, CaptureState::Starting));
        status.set(CaptureState::Capturing);
        status.set(CaptureState::Failed(CaptureError::SourceClosed));
        assert_eq!(
            status.snapshot(),
            (2, CaptureState::Failed(CaptureError::SourceClosed))
        );
        assert_eq!(
            *seen.lock(),
            [
                CaptureState::Capturing,
                CaptureState::Failed(CaptureError::SourceClosed)
            ]
        );
    }

    #[test]
    fn errors_explain_themselves_without_native_noise() {
        assert!(
            CaptureError::PermissionDenied
                .to_string()
                .contains("refused to capture")
        );
        assert_eq!(
            CaptureError::Native {
                operation: "StartCapture",
                code: -2147024891,
            }
            .to_string(),
            "StartCapture failed (0x80070005)"
        );
        assert!(
            CaptureError::SourceTooLarge {
                width: 7680,
                height: 4320
            }
            .to_string()
            .contains("7680x4320")
        );
    }
}
