//! The sending side of a screen share (SPEC §8.2, §8.3, §8.5; milestone 29).
//!
//! A [`ShareWorker`] runs one platform capture session. Its capture thread
//! owns the whole path for every picture: the capture session delivers a
//! bounded NV12 picture (at most 1280×720, at most 30 per second), the
//! encoder (opened lazily on that thread, hardware first) turns it into an H.264
//! access unit, and the [`FrameSink`] hands the unit to the transport. There is
//! no queue of its own: the capture session keeps only the newest picture, the
//! transport's queue is three frames, and a unit the transport refuses is
//! dropped, never held back.
//!
//! The transport's feedback arrives through [`ShareControl`]: a keyframe
//! request makes the next picture an IDR (and wakes the capture thread, so a
//! static screen answers at once), a new bitrate reaches the running encoder (or
//! reopens it where the encoder cannot change rate), and pausing slows the
//! capture to one picture a second until a viewer wants the stream again.
//! Nothing is encoded or sent before the transport has attached its sink and
//! reported that the server accepted H.264 from us.
//!
//! A source resize reopens the encoder at the new size on the capture thread; a
//! static screen is resent about once a second so receivers keep a live stream.

mod pipeline;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use fastcord_platform::capture::{
    CaptureConfig, CaptureController, CaptureError, CaptureSession, CaptureState, Conversion,
    ConversionPath, SourceId, StartOptions, StateObserver,
};
use parking_lot::Mutex;

use crate::codec::{BackendPreference, EncodedFrame, EncoderConfig, VideoEncoder};
use crate::select::{Attempt, OpenError};
use pipeline::Pipeline;

/// The largest encoded access unit passed on, matching the transport's bound.
pub const MAX_ENCODED_FRAME_BYTES: usize = 2 << 20;
/// The lowest bitrate requested of an encoder (the codec's own floor).
pub const MIN_BITRATE: u32 = 64_000;

/// How a share is captured and encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareConfig {
    /// The bounds of the picture and its rate.
    pub capture: CaptureConfig,
    /// The bitrate the encoder starts at, bits per second.
    pub bitrate: u32,
    /// The highest bitrate any request is clamped to.
    pub max_bitrate: u32,
    /// Frames between the encoder's own IDR pictures.
    pub keyframe_interval: u32,
    pub preference: BackendPreference,
    pub conversion: Conversion,
    /// A static screen is resent after this long without a picture.
    pub idle_repeat: Duration,
}

impl ShareConfig {
    /// SPEC §8.2: one 1280×720 layer at 30 fps starting at 2.5 Mbit/s.
    pub const SCREEN_SHARE_720P30: Self = Self {
        capture: CaptureConfig::SCREEN_SHARE_720P30,
        bitrate: 2_500_000,
        max_bitrate: 2_500_000,
        keyframe_interval: 150,
        preference: BackendPreference::Auto,
        conversion: Conversion::Auto,
        idle_repeat: Duration::from_secs(1),
    };

    fn validate(&self) -> Result<(), ShareError> {
        self.capture.validate().map_err(ShareError::Capture)?;
        let encoder = EncoderConfig {
            width: self.capture.width,
            height: self.capture.height,
            frame_rate: self.capture.frame_rate,
            bitrate: self.bitrate,
            keyframe_interval: self.keyframe_interval,
        };
        encoder.validate().map_err(ShareError::Codec)?;
        if self.max_bitrate < self.bitrate
            || self.idle_repeat < Duration::from_millis(100)
            || self.idle_repeat > Duration::from_secs(10)
        {
            return Err(ShareError::Codec(crate::CodecError::InvalidConfig(
                "bitrate ceiling below the start rate, or an unreasonable keepalive",
            )));
        }
        Ok(())
    }
}

/// A share could not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareError {
    Capture(CaptureError),
    Codec(crate::CodecError),
}

impl fmt::Display for ShareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(error) => error.fmt(f),
            Self::Codec(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ShareError {}

/// What the transport did with an access unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkOutcome {
    /// Accepted for sending.
    Sent,
    /// Refused (queue full, unit too large, or the stream is not ready): the
    /// stream must resume at a keyframe, which the share then produces.
    Dropped,
    /// The connection is gone: the share ends.
    Closed,
}

/// Takes finished access units on the capture thread; must not block.
pub type FrameSink = Box<dyn FnMut(EncodedFrame) -> SinkOutcome + Send>;

/// An opened encoder and the candidates that were tried before it.
pub struct OpenedEncoder {
    pub encoder: Box<dyn VideoEncoder>,
    pub rejected: Vec<Attempt>,
}

/// Opens an encoder for a size and preference. Runs on the capture thread,
/// which owns every encoder it opens.
pub type OpenEncoder =
    Box<dyn FnMut(EncoderConfig, BackendPreference) -> Result<OpenedEncoder, OpenError> + Send>;

/// The platform's H.264 encoders: Media Foundation on Windows (hardware first,
/// Microsoft's software transform as the fallback), VA-API hardware plus the
/// bundled OpenH264 software codec on Linux, and VideoToolbox on macOS once
/// milestone 31 lands. Platforms without a capture backend yet report no
/// encoder (SPEC milestones 30, 33, 34).
pub fn platform_encoder() -> OpenEncoder {
    #[cfg(windows)]
    {
        Box::new(|config, preference| {
            let selected = crate::media_foundation::open_encoder(config, preference)?;
            Ok(OpenedEncoder {
                encoder: Box::new(selected.codec),
                rejected: selected.rejected,
            })
        })
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(|config, preference| {
            let selected = crate::platform_h264::open_encoder(config, preference)?;
            Ok(OpenedEncoder {
                encoder: selected.codec,
                rejected: selected.rejected,
            })
        })
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Box::new(|_, preference| Err(OpenError::NoBackend(preference)))
    }
}

/// An encoder was opened (the first picture, a resize, a bitrate change an
/// encoder could not apply while running, or a fallback after a failure).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderReport {
    /// The backend as reported by the codec, for example
    /// `Media Foundation hardware: AMDh264Encoder on AMD Radeon RX 9070 XT`.
    pub backend: String,
    pub width: u32,
    pub height: u32,
    pub bitrate: u32,
    /// Candidates that failed to open before this one.
    pub skipped: Vec<String>,
    /// The encoder replaced one that failed while running.
    pub fell_back: bool,
}

/// Why a share ended. Reported exactly once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareEnd {
    /// Stopped by the caller.
    Stopped,
    /// The capture ended: the source closed, permission was refused, the OS
    /// failed.
    Capture(CaptureError),
    /// No encoder could be opened or it kept failing; the text is secret-free.
    Encoder(String),
    /// The transport closed.
    SinkClosed,
}

impl fmt::Display for ShareEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped => f.write_str("sharing stopped"),
            Self::Capture(error) => error.fmt(f),
            Self::Encoder(detail) => write!(f, "video encoding failed: {detail}"),
            Self::SinkClosed => f.write_str("the stream connection closed"),
        }
    }
}

/// Progress of a share, reported on the capture thread (or the thread that
/// stops it). Observers must not block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareEvent {
    /// The operating system accepted the capture.
    Capturing,
    /// Which converter produces the pictures.
    Converting(ConversionPath),
    /// The captured content changed size.
    SourceSize {
        width: u32,
        height: u32,
    },
    Encoder(EncoderReport),
    Ended(ShareEnd),
}

pub type ShareObserver = Arc<dyn Fn(ShareEvent) + Send + Sync>;

/// Counters of one share.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShareStats {
    /// Pictures the capture delivered.
    pub pictures: u64,
    /// Pictures ignored because the share was not sending (no sink yet, not
    /// negotiated, or paused by the server).
    pub idle_pictures: u64,
    /// Access units the encoder produced.
    pub encoded: u64,
    pub keyframes: u64,
    /// Units accepted by the transport, and their bytes.
    pub sent: u64,
    pub bytes: u64,
    /// Units the transport refused or that were too large.
    pub dropped: u64,
    /// Encoders opened (the first, resizes, rate changes, fallbacks).
    pub opened: u64,
    /// The encoder's current picture size and bitrate (0 before the first).
    pub width: u32,
    pub height: u32,
    pub bitrate: u32,
}

#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) pictures: AtomicU64,
    pub(crate) idle_pictures: AtomicU64,
    pub(crate) encoded: AtomicU64,
    pub(crate) keyframes: AtomicU64,
    pub(crate) sent: AtomicU64,
    pub(crate) bytes: AtomicU64,
    pub(crate) dropped: AtomicU64,
    pub(crate) opened: AtomicU64,
    pub(crate) width: AtomicU32,
    pub(crate) height: AtomicU32,
    pub(crate) bitrate: AtomicU32,
}

impl Counters {
    pub(crate) fn add(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }
}

/// While nobody wants the stream, pictures are taken this slowly.
const PAUSED_INTERVAL: Duration = Duration::from_secs(1);

/// State shared between the capture thread's pipeline and the controls.
pub(crate) struct Shared {
    pub(crate) keyframe: AtomicBool,
    pub(crate) bitrate: AtomicU32,
    pub(crate) active: AtomicBool,
    pub(crate) negotiated: AtomicBool,
    pub(crate) attached: AtomicBool,
    pub(crate) sink: Mutex<Option<FrameSink>>,
    pub(crate) counters: Counters,
    /// The first reason the pipeline ended the share, if it did.
    pub(crate) failure: Mutex<Option<ShareEnd>>,
    ended: AtomicBool,
    interval: Duration,
    controller: OnceLock<CaptureController>,
}

impl Shared {
    fn new(config: &ShareConfig) -> Self {
        Self {
            keyframe: AtomicBool::new(true),
            bitrate: AtomicU32::new(config.bitrate),
            active: AtomicBool::new(true),
            negotiated: AtomicBool::new(false),
            attached: AtomicBool::new(false),
            sink: Mutex::new(None),
            counters: Counters::default(),
            failure: Mutex::new(None),
            ended: AtomicBool::new(false),
            interval: config.capture.frame_interval(),
            controller: OnceLock::new(),
        }
    }

    pub(crate) fn wake(&self) {
        if let Some(controller) = self.controller.get() {
            controller.poke();
        }
    }

    /// Whether pictures should be encoded and sent right now.
    pub(crate) fn armed(&self) -> bool {
        self.attached.load(Ordering::Acquire)
            && self.negotiated.load(Ordering::Acquire)
            && self.active.load(Ordering::Acquire)
    }

    /// Records why the pipeline ended the share, keeping the first cause.
    pub(crate) fn fail(&self, end: ShareEnd) {
        self.failure.lock().get_or_insert(end);
    }
}

/// The transport's hand on a running share. Cheap to clone.
#[derive(Clone)]
pub struct ShareControl {
    shared: Arc<Shared>,
}

impl ShareControl {
    /// Gives the share the transport's sink. Until then nothing is encoded.
    /// The stream restarts at a keyframe.
    pub fn attach(&self, sink: FrameSink) {
        *self.shared.sink.lock() = Some(sink);
        self.shared.attached.store(true, Ordering::Release);
        self.shared.keyframe.store(true, Ordering::Release);
        self.shared.wake();
    }

    /// Whether the server accepted H.264 from us. Without it nothing is sent.
    pub fn set_negotiated(&self, negotiated: bool) {
        self.shared.negotiated.store(negotiated, Ordering::Release);
        if negotiated {
            self.shared.keyframe.store(true, Ordering::Release);
            self.shared.wake();
        }
    }

    /// Pauses (`false`) or resumes the stream at the server's request: while
    /// paused nothing is encoded and the capture slows to a picture a second;
    /// resuming asks for a keyframe at once.
    pub fn set_active(&self, active: bool) {
        self.shared.active.store(active, Ordering::Release);
        if let Some(controller) = self.shared.controller.get() {
            controller.set_min_interval(if active {
                self.shared.interval
            } else {
                PAUSED_INTERVAL
            });
        }
        if active {
            self.shared.keyframe.store(true, Ordering::Release);
            self.shared.wake();
        }
    }

    /// A receiver or the server asked for a keyframe: the next picture is an
    /// IDR, delivered at once even if the screen is static.
    pub fn request_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Release);
        self.shared.wake();
    }

    /// The bitrate to aim for, bits per second; clamped to the codec floor and
    /// the configured ceiling when applied.
    pub fn set_bitrate(&self, bitrate: u32) {
        self.shared.bitrate.store(bitrate, Ordering::Release);
    }

    pub fn stats(&self) -> ShareStats {
        let counters = &self.shared.counters;
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        ShareStats {
            pictures: get(&counters.pictures),
            idle_pictures: get(&counters.idle_pictures),
            encoded: get(&counters.encoded),
            keyframes: get(&counters.keyframes),
            sent: get(&counters.sent),
            bytes: get(&counters.bytes),
            dropped: get(&counters.dropped),
            opened: get(&counters.opened),
            width: counters.width.load(Ordering::Relaxed),
            height: counters.height.load(Ordering::Relaxed),
            bitrate: counters.bitrate.load(Ordering::Relaxed),
        }
    }
}

impl fmt::Debug for ShareControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ShareControl")
    }
}

/// A running share. Dropping it stops the capture thread (and with it the
/// encoder), waiting briefly for the native objects to be released.
pub struct ShareWorker {
    control: ShareControl,
    session: CaptureSession,
}

impl ShareWorker {
    /// Starts sharing `source` with the platform's encoders. Returns as soon as
    /// the capture thread exists; whether the OS accepted the capture is
    /// reported to `observer`.
    pub fn start(
        source: SourceId,
        config: ShareConfig,
        observer: ShareObserver,
    ) -> Result<Self, ShareError> {
        Self::start_with(source, config, observer, platform_encoder())
    }

    /// As [`start`](Self::start), with explicit encoders.
    pub fn start_with(
        source: SourceId,
        config: ShareConfig,
        observer: ShareObserver,
        open: OpenEncoder,
    ) -> Result<Self, ShareError> {
        config.validate()?;
        let shared = Arc::new(Shared::new(&config));
        let state_observer = state_observer(Arc::clone(&shared), Arc::clone(&observer));
        let handler_shared = Arc::clone(&shared);
        let session = fastcord_platform::capture::start(
            source,
            StartOptions {
                config: config.capture,
                conversion: config.conversion,
                observer: Some(state_observer),
            },
            move || Pipeline::new(handler_shared, config, open, observer),
        )
        .map_err(ShareError::Capture)?;
        let _ = shared.controller.set(session.controller());
        Ok(Self {
            control: ShareControl { shared },
            session,
        })
    }

    pub fn control(&self) -> ShareControl {
        self.control.clone()
    }

    /// Stops the share and waits for the capture thread, returning the capture's
    /// terminal error if it had failed.
    pub fn stop(self) -> Result<(), CaptureError> {
        self.session.stop()
    }
}

/// Folds the capture's states and the pipeline's own failure into share events;
/// the terminal one is sent exactly once.
fn state_observer(shared: Arc<Shared>, observer: ShareObserver) -> StateObserver {
    Arc::new(move |state: &CaptureState| {
        let event = match state {
            CaptureState::Starting => return,
            CaptureState::Capturing => ShareEvent::Capturing,
            CaptureState::Converting(path) => ShareEvent::Converting(*path),
            CaptureState::SourceSize { width, height } => ShareEvent::SourceSize {
                width: *width,
                height: *height,
            },
            CaptureState::Stopped => {
                ShareEvent::Ended(shared.failure.lock().take().unwrap_or(ShareEnd::Stopped))
            }
            CaptureState::Failed(error) => {
                shared.failure.lock().take();
                ShareEvent::Ended(ShareEnd::Capture(error.clone()))
            }
        };
        if matches!(event, ShareEvent::Ended(_)) && shared.ended.swap(true, Ordering::AcqRel) {
            return;
        }
        observer(event);
    })
}
