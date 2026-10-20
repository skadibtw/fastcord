//! The capture thread's side of a share: pictures in, access units out.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use fastcord_platform::capture::{Flow, FrameHandler, Tick, VideoSample};

use super::{
    Counters, EncoderReport, MAX_ENCODED_FRAME_BYTES, MIN_BITRATE, OpenEncoder, PreviewSink,
    ShareConfig, ShareEnd, ShareEvent, ShareObserver, Shared, SinkOutcome,
};
use crate::codec::{
    BackendKind, BackendPreference, CodecError, EncodedFrame, EncoderConfig, VideoEncoder,
};
use crate::effect::{EffectHandle, Stage};
use crate::frame::Nv12;

/// A keyframe the share asks of itself after a refused unit is at most this
/// often, so a congested link is not flooded with large IDR pictures.
const SELF_KEYFRAME_SPACING: Duration = Duration::from_millis(300);
/// Encoder failures in a row (without 30 good pictures between) that end the
/// share.
const MAX_ENCODER_FAILURES: u32 = 3;
const HEALTHY_PICTURES: u32 = 30;
/// Timestamps handed to the transport differ by at least this much.
const MIN_TIMESTAMP_STEP: Duration = Duration::from_millis(1);

struct Running {
    encoder: Box<dyn VideoEncoder>,
    config: EncoderConfig,
}

pub(super) struct Pipeline {
    shared: Arc<Shared>,
    config: ShareConfig,
    open: OpenEncoder,
    observer: ShareObserver,
    preview: Option<PreviewSink>,
    /// The camera's background effect, applied before preview and encoding.
    effect: Option<Stage>,
    /// The effect's output picture, reused.
    effected: Vec<u8>,
    encoder: Option<Running>,
    /// The backends the next open may use: the configured one, narrowed to
    /// software after a hardware encoder failed while running.
    preference: BackendPreference,
    /// The first picture's capture time; every timestamp is relative to it.
    base: Option<Duration>,
    last_timestamp: Option<Duration>,
    last_encode: Option<Instant>,
    last_forced: Option<Instant>,
    failures: u32,
    healthy: u32,
    /// The last picture was sent (not skipped as idle), so a stream that was
    /// paused or not yet sending restarts at a keyframe.
    was_armed: bool,
    /// The previous encoder failed while running, for the next report.
    fell_back: bool,
    output: Vec<EncodedFrame>,
    ended: bool,
}

impl Pipeline {
    pub(super) fn new(
        shared: Arc<Shared>,
        config: ShareConfig,
        open: OpenEncoder,
        observer: ShareObserver,
        preview: Option<PreviewSink>,
        effect: Option<EffectHandle>,
    ) -> Self {
        Self {
            shared,
            preference: config.preference,
            config,
            open,
            observer,
            preview,
            effect: effect.map(Stage::new),
            effected: Vec::new(),
            encoder: None,
            base: None,
            last_timestamp: None,
            last_encode: None,
            last_forced: None,
            failures: 0,
            healthy: 0,
            was_armed: false,
            fell_back: false,
            output: Vec::new(),
            ended: false,
        }
    }

    /// One picture from the capture, at `now`.
    pub(super) fn picture(&mut self, sample: &VideoSample<'_>, now: Instant) -> Flow {
        if self.ended {
            return Flow::Stop;
        }
        // The effect runs first, so the self-view and the stream always show
        // the same picture. If it cannot run, nothing unprotected is sent.
        let mut effected = std::mem::take(&mut self.effected);
        let stride = match self
            .effect
            .as_mut()
            .map(|stage| stage.apply(sample, &mut effected))
        {
            Some(Err(detail)) => {
                self.effected = effected;
                self.end(ShareEnd::Effect(detail));
                return Flow::Stop;
            }
            Some(Ok(Some(stride))) => Some(stride),
            _ => None,
        };
        let flow = match stride.map(|stride| {
            VideoSample::nv12(
                sample.width(),
                sample.height(),
                stride,
                &effected,
                sample.timestamp,
            )
        }) {
            Some(Ok(processed)) => self.deliver(&processed, now),
            Some(Err(error)) => {
                self.end(ShareEnd::Effect(error.to_string()));
                Flow::Stop
            }
            None => self.deliver(sample, now),
        };
        self.effected = effected;
        flow
    }

    /// One picture, after the effect: the preview sees it, then the encoder.
    fn deliver(&mut self, sample: &VideoSample<'_>, now: Instant) -> Flow {
        if let Some(preview) = self.preview.as_mut() {
            preview(sample);
        }
        let counters = &self.shared.counters;
        Counters::add(&counters.pictures, 1);
        if !self.shared.armed() {
            Counters::add(&counters.idle_pictures, 1);
            self.was_armed = false;
            return Flow::Continue;
        }
        if !self.was_armed {
            self.was_armed = true;
            self.shared.keyframe.store(true, Ordering::Release);
        }
        match self.encode(sample, now) {
            Ok(()) => Flow::Continue,
            Err(end) => {
                self.end(end);
                Flow::Stop
            }
        }
    }

    /// The capture's timer or a wake-up, at `now`: ask for the newest picture
    /// again when a keyframe is wanted or the screen has been static for the
    /// keepalive time.
    pub(super) fn tick_at(&mut self, now: Instant) -> Tick {
        if self.ended {
            return Tick::Stop;
        }
        if !self.shared.armed() {
            return Tick::Idle;
        }
        let keyframe_wanted = self.shared.keyframe.load(Ordering::Acquire);
        let stale = self
            .last_encode
            .is_none_or(|at| now.saturating_duration_since(at) >= self.config.idle_repeat);
        if keyframe_wanted || stale {
            Tick::Repeat
        } else {
            Tick::Idle
        }
    }

    fn end(&mut self, end: ShareEnd) {
        self.ended = true;
        self.shared.fail(end);
    }

    fn encode(&mut self, sample: &VideoSample<'_>, now: Instant) -> Result<(), ShareEnd> {
        let size = (sample.width(), sample.height());
        self.ensure_encoder(size)?;
        self.apply_bitrate(size)?;
        if self.encoder.is_none() {
            // The rate change failed and the encoder was discarded; the next
            // picture starts a new one at a keyframe.
            return Ok(());
        }
        let force = self.shared.keyframe.swap(false, Ordering::AcqRel);
        let timestamp = self.timestamp(sample.timestamp);
        let (luma, chroma) = sample.planes();
        let image = Nv12::new(
            size.0,
            size.1,
            luma,
            sample.stride(),
            chroma,
            sample.stride(),
        )
        .map_err(|error| ShareEnd::Encoder(error.to_string()))?;
        let running = self.encoder.as_mut().expect("opened above");
        self.output.clear();
        if let Err(error) = running
            .encoder
            .encode(&image, timestamp, force, &mut self.output)
        {
            // The picture is lost; the next one starts a new encoder at an IDR.
            self.output.clear();
            return self.recover(&error);
        }
        self.healthy = self.healthy.saturating_add(1);
        if self.healthy >= HEALTHY_PICTURES {
            self.failures = 0;
        }
        self.last_encode = Some(now);
        self.forward(now)
    }

    /// Hands the encoder's output to the transport.
    fn forward(&mut self, now: Instant) -> Result<(), ShareEnd> {
        let mut guard = self.shared.sink.lock();
        let Some(sink) = guard.as_mut() else {
            self.output.clear();
            return Ok(());
        };
        let mut refused = false;
        let mut closed = false;
        let counters = &self.shared.counters;
        for unit in self.output.drain(..) {
            Counters::add(&counters.encoded, 1);
            if unit.keyframe {
                Counters::add(&counters.keyframes, 1);
            }
            let bytes = unit.data.len();
            if closed {
                continue;
            }
            if bytes > MAX_ENCODED_FRAME_BYTES {
                Counters::add(&counters.dropped, 1);
                refused = true;
                continue;
            }
            match sink(unit) {
                SinkOutcome::Sent => {
                    Counters::add(&counters.sent, 1);
                    Counters::add(&counters.bytes, bytes as u64);
                }
                SinkOutcome::Dropped => {
                    Counters::add(&counters.dropped, 1);
                    refused = true;
                }
                SinkOutcome::Closed => closed = true,
            }
        }
        drop(guard);
        if closed {
            return Err(ShareEnd::SinkClosed);
        }
        if refused {
            self.want_keyframe(now);
        }
        Ok(())
    }

    /// Asks for a keyframe after a unit did not leave, at most every
    /// [`SELF_KEYFRAME_SPACING`].
    fn want_keyframe(&mut self, now: Instant) {
        if self
            .last_forced
            .is_none_or(|at| now.saturating_duration_since(at) >= SELF_KEYFRAME_SPACING)
        {
            self.last_forced = Some(now);
            self.shared.keyframe.store(true, Ordering::Release);
        }
    }

    /// Source time since the first picture, strictly increasing.
    fn timestamp(&mut self, source: Duration) -> Duration {
        let base = *self.base.get_or_insert(source);
        let relative = source.saturating_sub(base);
        let relative = match self.last_timestamp {
            Some(last) if relative <= last => last + MIN_TIMESTAMP_STEP,
            _ => relative,
        };
        self.last_timestamp = Some(relative);
        relative
    }

    fn target_bitrate(&self) -> u32 {
        self.shared
            .bitrate
            .load(Ordering::Acquire)
            .clamp(MIN_BITRATE, self.config.max_bitrate.max(MIN_BITRATE))
    }

    /// Opens an encoder for pictures of `size` unless the open one matches.
    fn ensure_encoder(&mut self, size: (u32, u32)) -> Result<(), ShareEnd> {
        if self
            .encoder
            .as_ref()
            .is_some_and(|running| (running.config.width, running.config.height) == size)
        {
            return Ok(());
        }
        self.open_encoder(size)
    }

    fn open_encoder(&mut self, size: (u32, u32)) -> Result<(), ShareEnd> {
        // Free the old encoder (and its hardware session) before the new one.
        self.encoder = None;
        let config = EncoderConfig {
            width: size.0,
            height: size.1,
            frame_rate: self.config.capture.frame_rate,
            bitrate: self.target_bitrate(),
            keyframe_interval: self.config.keyframe_interval,
        };
        let opened = (self.open)(config, self.preference)
            .map_err(|error| ShareEnd::Encoder(error.to_string()))?;
        let backend = opened.encoder.backend().to_string();
        let counters = &self.shared.counters;
        Counters::add(&counters.opened, 1);
        counters.width.store(size.0, Ordering::Relaxed);
        counters.height.store(size.1, Ordering::Relaxed);
        counters.bitrate.store(config.bitrate, Ordering::Relaxed);
        self.encoder = Some(Running {
            encoder: opened.encoder,
            config,
        });
        // The new encoder's first unit is an IDR; a request from before it
        // existed is answered by that unit.
        self.shared.keyframe.store(false, Ordering::Release);
        (self.observer)(ShareEvent::Encoder(EncoderReport {
            backend,
            width: size.0,
            height: size.1,
            bitrate: config.bitrate,
            skipped: opened.rejected.iter().map(ToString::to_string).collect(),
            fell_back: std::mem::take(&mut self.fell_back),
        }));
        Ok(())
    }

    /// Moves the running encoder to the requested bitrate: in place when it
    /// can, otherwise by reopening it (which starts at a keyframe).
    fn apply_bitrate(&mut self, size: (u32, u32)) -> Result<(), ShareEnd> {
        let target = self.target_bitrate();
        let Some(running) = self.encoder.as_mut() else {
            return Ok(());
        };
        if running.config.bitrate == target {
            return Ok(());
        }
        match running.encoder.set_bitrate(target) {
            Ok(()) => {
                running.config.bitrate = target;
                self.shared
                    .counters
                    .bitrate
                    .store(target, Ordering::Relaxed);
                Ok(())
            }
            Err(CodecError::Unsupported(_)) => self.open_encoder(size),
            Err(error) => self.recover(&error),
        }
    }

    /// A running encoder failed: discard it and carry on with a new one at the
    /// next picture (software if the failed one was hardware, unless hardware
    /// was demanded), or end the share after repeated failures.
    fn recover(&mut self, error: &CodecError) -> Result<(), ShareEnd> {
        self.failures += 1;
        self.healthy = 0;
        let was_hardware = self
            .encoder
            .take()
            .is_some_and(|running| running.encoder.backend().kind == BackendKind::Hardware);
        if self.failures > MAX_ENCODER_FAILURES {
            return Err(ShareEnd::Encoder(error.to_string()));
        }
        if was_hardware && self.preference == BackendPreference::Auto {
            self.preference = BackendPreference::Software;
        }
        self.fell_back = true;
        self.shared.keyframe.store(true, Ordering::Release);
        Ok(())
    }
}

impl FrameHandler for Pipeline {
    fn frame(&mut self, sample: &VideoSample<'_>) -> Flow {
        self.picture(sample, Instant::now())
    }

    fn tick(&mut self) -> Tick {
        self.tick_at(Instant::now())
    }
}
