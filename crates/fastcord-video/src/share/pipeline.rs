//! The capture thread's side of a share: pictures in, access units out.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use fastcord_platform::capture::{Flow, FrameHandler, GpuFrame, GpuOutcome, Tick, VideoSample};

use super::{
    Counters, EncoderReport, MAX_ENCODED_FRAME_BYTES, MIN_BITRATE, OpenEncoder, OpenGpuEncoder,
    OpenedEncoder, PreviewSink, ShareConfig, ShareEnd, ShareEvent, ShareObserver, Shared,
    SinkOutcome, ZeroCopy,
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

/// Encoded rates as fractions of the capture rate: 30, 20 and 15 fps.
const PACE_LEVELS: [(u32, u32); 3] = [(1, 1), (2, 3), (1, 2)];
/// The rate steps down once encoding a picture takes this share of the capture
/// rate's frame time on average, and back up below `PACE_RECOVER`.
const PACE_BUSY: f64 = 0.6;
const PACE_RECOVER: f64 = 0.3;
/// The rate changes at most this often.
const PACE_HOLD: Duration = Duration::from_secs(2);
const RESOLUTION_HOLD: Duration = Duration::from_secs(5);
const RECOVERY_HOLD: Duration = Duration::from_secs(8);
const MAX_LOAD_LEVEL: usize = 4;

/// Trades frame rate for time when the machine is busy (a game holding the
/// GPU): a share that encodes fewer pictures smoothly beats one whose encoder
/// starves. Measures wall time per encode on the capture thread.
// ponytail: wall-clock encode time is the only load signal; add GPU queue
// depth or capture lateness if this misses contention.
pub(super) struct Pace {
    capture_fps: u32,
    level: usize,
    /// Smoothed seconds per encode.
    average: f64,
    next_admitted: Option<Instant>,
    changed_at: Option<Instant>,
    recovery_since: Option<Instant>,
}

impl Pace {
    pub(super) fn new(capture_fps: u32) -> Self {
        Self {
            capture_fps: capture_fps.max(1),
            level: 0,
            average: 0.0,
            next_admitted: None,
            changed_at: None,
            recovery_since: None,
        }
    }

    /// The encoded rate, or `None` at the full capture rate.
    pub(super) fn paced_fps(&self) -> Option<u32> {
        let (num, den) = PACE_LEVELS[self.level.min(PACE_LEVELS.len() - 1)];
        (self.level > 0).then(|| (self.capture_fps * num / den).max(1))
    }

    /// Whether the picture at `now` is encoded. At the full rate every
    /// picture is (the capture already paces them).
    pub(super) fn admit(&mut self, now: Instant) -> bool {
        let Some(fps) = self.paced_fps() else {
            self.next_admitted = None;
            return true;
        };
        let interval = Duration::from_secs(1) / fps;
        let tolerance = Duration::from_secs(1) / (4 * self.capture_fps);
        if let Some(due) = self.next_admitted {
            if due.saturating_duration_since(now) > tolerance {
                return false;
            }
            // Preserve fractional cadence against capture ticks (20 fps from
            // 30 fps alternates one and two ticks). Never replay missed slots.
            self.next_admitted = Some(if now.saturating_duration_since(due) >= interval {
                now + interval
            } else {
                due + interval
            });
        } else {
            self.next_admitted = Some(now + interval);
        }
        true
    }

    /// Records one encode that took `took`; true when the rate changed.
    pub(super) fn record(&mut self, took: Duration, now: Instant) -> bool {
        self.average += (took.as_secs_f64() - self.average) / 8.0;
        let frame = 1.0 / self.capture_fps as f64;
        let hold = if self.average < frame * PACE_RECOVER {
            let since = *self.recovery_since.get_or_insert(now);
            if now.saturating_duration_since(since) < RECOVERY_HOLD {
                return false;
            }
            RECOVERY_HOLD
        } else {
            self.recovery_since = None;
            if self.level >= 2 {
                RESOLUTION_HOLD
            } else {
                PACE_HOLD
            }
        };
        if self
            .changed_at
            .is_some_and(|at| now.saturating_duration_since(at) < hold)
        {
            return false;
        }
        let level = if self.average > frame * PACE_BUSY {
            (self.level + 1).min(MAX_LOAD_LEVEL)
        } else if self.average < frame * PACE_RECOVER {
            self.level.saturating_sub(1)
        } else {
            self.level
        };
        if level == self.level {
            return false;
        }
        self.level = level;
        self.recovery_since = None;
        self.next_admitted = None;
        self.changed_at = Some(now);
        true
    }
}

struct Running {
    encoder: Box<dyn VideoEncoder>,
    config: EncoderConfig,
    /// Opened for GPU pictures (and only fit for them).
    gpu: bool,
}

pub(super) struct Pipeline {
    shared: Arc<Shared>,
    config: ShareConfig,
    open: OpenEncoder,
    open_gpu: Option<OpenGpuEncoder>,
    /// Whether pictures may still be offered as GPU textures.
    zero_copy: bool,
    /// The last zero-copy state reported.
    reported_zero_copy: Option<ZeroCopy>,
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
    pace: Pace,
    /// One bounded CPU fallback surface; native conversion is preferred.
    scaled: Vec<u8>,
    #[cfg(test)]
    encode_duration: Option<Duration>,
}

impl Pipeline {
    pub(super) fn new(
        shared: Arc<Shared>,
        config: ShareConfig,
        open: OpenEncoder,
        open_gpu: Option<OpenGpuEncoder>,
        observer: ShareObserver,
        preview: Option<PreviewSink>,
        effect: Option<EffectHandle>,
    ) -> Self {
        // The preview and the effect read the picture bytes.
        let zero_copy = config.zero_copy
            && config.preference != BackendPreference::Software
            && preview.is_none()
            && effect.is_none();
        Self {
            zero_copy: zero_copy && open_gpu.is_some(),
            open_gpu,
            reported_zero_copy: None,
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
            pace: Pace::new(config.capture.frame_rate),
            scaled: Vec::new(),
            #[cfg(test)]
            encode_duration: None,
        }
    }

    /// One picture from the capture, at `now`.
    pub(super) fn picture(&mut self, sample: &VideoSample<'_>, now: Instant) -> Flow {
        self.picture_with_scaling(sample, now, false)
    }
    pub(super) fn picture_scaled(&mut self, sample: &VideoSample<'_>, now: Instant) -> Flow {
        self.picture_with_scaling(sample, now, true)
    }
    fn picture_with_scaling(
        &mut self,
        sample: &VideoSample<'_>,
        now: Instant,
        native_scaled: bool,
    ) -> Flow {
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
            Some(Ok(processed)) => self.deliver(&processed, now, native_scaled),
            Some(Err(error)) => {
                self.end(ShareEnd::Effect(error.to_string()));
                Flow::Stop
            }
            None => self.deliver(sample, now, native_scaled),
        };
        self.effected = effected;
        flow
    }

    /// One picture, after the effect: the preview sees it, then the encoder.
    fn deliver(&mut self, sample: &VideoSample<'_>, now: Instant, native_scaled: bool) -> Flow {
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
        if !self.pace.admit(now) {
            Counters::add(&counters.paced, 1);
            return Flow::Continue;
        }
        let size = if native_scaled {
            (sample.width(), sample.height())
        } else {
            self.output_size((sample.width(), sample.height()))
        };
        let result = if size == (sample.width(), sample.height()) {
            self.encode(sample, now)
        } else {
            let mut pixels = std::mem::take(&mut self.scaled);
            scale_nv12(sample, size, &mut pixels);
            let scaled =
                VideoSample::nv12(size.0, size.1, size.0 as usize, &pixels, sample.timestamp)
                    .expect("bounded even NV12 scaling");
            let result = self.encode(&scaled, now);
            self.scaled = pixels;
            result
        };
        match result {
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
        let started = Instant::now();
        if let Err(error) = running
            .encoder
            .encode(&image, timestamp, force, &mut self.output)
        {
            // The picture is lost; the next one starts a new encoder at an IDR.
            self.output.clear();
            return self.recover(&error);
        }
        self.paced(started.elapsed(), now);
        self.healthy = self.healthy.saturating_add(1);
        if self.healthy >= HEALTHY_PICTURES {
            self.failures = 0;
        }
        self.last_encode = Some(now);
        self.forward(now)
    }

    /// Feeds one encode's duration to the pace and publishes a rate change.
    fn paced(&mut self, took: Duration, now: Instant) {
        #[cfg(test)]
        let took = self.encode_duration.unwrap_or(took);
        if self.pace.record(took, now) {
            let fps = self.pace.paced_fps().unwrap_or(0);
            self.shared.counters.paced_fps.store(fps, Ordering::Relaxed);
        }
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
        if self.encoder.as_ref().is_some_and(|running| {
            (running.config.width, running.config.height) == size && !running.gpu
        }) {
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
        self.adopt(opened, config, false);
        Ok(())
    }

    /// Makes `opened` the running encoder and reports it.
    fn adopt(&mut self, opened: OpenedEncoder, config: EncoderConfig, gpu: bool) {
        let backend = opened.encoder.backend().to_string();
        let counters = &self.shared.counters;
        Counters::add(&counters.opened, 1);
        counters.width.store(config.width, Ordering::Relaxed);
        counters.height.store(config.height, Ordering::Relaxed);
        counters.bitrate.store(config.bitrate, Ordering::Relaxed);
        self.encoder = Some(Running {
            encoder: opened.encoder,
            config,
            gpu,
        });
        // The new encoder's first unit is an IDR; a request from before it
        // existed is answered by that unit.
        self.shared.keyframe.store(false, Ordering::Release);
        (self.observer)(ShareEvent::Encoder(EncoderReport {
            backend,
            width: config.width,
            height: config.height,
            bitrate: config.bitrate,
            skipped: opened.rejected.iter().map(ToString::to_string).collect(),
            fell_back: std::mem::take(&mut self.fell_back),
        }));
    }

    fn report_zero_copy(&mut self, state: ZeroCopy) {
        if self.reported_zero_copy.as_ref() != Some(&state) {
            self.reported_zero_copy = Some(state.clone());
            (self.observer)(ShareEvent::ZeroCopy(state));
        }
    }

    /// Abandons the zero-copy path for the rest of the share: the encoder
    /// opened for textures is dropped, the copy path opens its own at the next
    /// picture (an IDR), and the screen says why. Never ends the share.
    fn leave_zero_copy(&mut self, reason: String) {
        self.zero_copy = false;
        if self.encoder.as_ref().is_some_and(|running| running.gpu) {
            self.encoder = None;
        }
        self.output.clear();
        self.shared.keyframe.store(true, Ordering::Release);
        self.report_zero_copy(ZeroCopy::Copy(reason));
    }

    /// One picture that stayed on the GPU. Declined (and the copy path takes
    /// this same picture) when no hardware encoder takes it or one fails.
    fn picture_gpu(&mut self, frame: &GpuFrame<'_>, now: Instant) -> GpuOutcome {
        if self.ended {
            return GpuOutcome::Handled(Flow::Stop);
        }
        if !self.shared.armed() {
            let counters = &self.shared.counters;
            Counters::add(&counters.pictures, 1);
            Counters::add(&counters.gpu_pictures, 1);
            Counters::add(&counters.idle_pictures, 1);
            self.was_armed = false;
            return GpuOutcome::Handled(Flow::Continue);
        }
        let size = (frame.width(), frame.height());
        if let Err(reason) = self.ensure_gpu_encoder(frame, size) {
            self.leave_zero_copy(reason);
            return GpuOutcome::Declined;
        }
        if !self.was_armed {
            self.was_armed = true;
            self.shared.keyframe.store(true, Ordering::Release);
        }
        if !self.pace.admit(now) {
            let counters = &self.shared.counters;
            Counters::add(&counters.pictures, 1);
            Counters::add(&counters.gpu_pictures, 1);
            Counters::add(&counters.paced, 1);
            return GpuOutcome::Handled(Flow::Continue);
        }
        let force = self.shared.keyframe.swap(false, Ordering::AcqRel);
        let timestamp = self.timestamp(frame.timestamp());
        let running = self.encoder.as_mut().expect("opened above");
        self.output.clear();
        let started = Instant::now();
        if let Err(error) = running
            .encoder
            .encode_gpu(frame, timestamp, force, &mut self.output)
        {
            self.leave_zero_copy(format!("the GPU encoder failed: {error}"));
            return GpuOutcome::Declined;
        }
        self.paced(started.elapsed(), now);
        let counters = &self.shared.counters;
        Counters::add(&counters.pictures, 1);
        Counters::add(&counters.gpu_pictures, 1);
        self.healthy = self.healthy.saturating_add(1);
        if self.healthy >= HEALTHY_PICTURES {
            self.failures = 0;
        }
        self.last_encode = Some(now);
        match self.forward(now) {
            Ok(()) => GpuOutcome::Handled(Flow::Continue),
            Err(end) => {
                self.end(end);
                GpuOutcome::Handled(Flow::Stop)
            }
        }
    }

    /// Opens (or keeps) the encoder for GPU pictures of `size` and applies the
    /// requested bitrate in place; the error is the reason to use the copy
    /// path.
    fn ensure_gpu_encoder(&mut self, frame: &GpuFrame<'_>, size: (u32, u32)) -> Result<(), String> {
        let target = self.target_bitrate();
        let reusable = self.encoder.as_ref().is_some_and(|running| {
            running.gpu && (running.config.width, running.config.height) == size
        });
        if !reusable {
            let open = self
                .open_gpu
                .as_mut()
                .ok_or("no GPU encoder on this system")?;
            self.encoder = None;
            let config = EncoderConfig {
                width: size.0,
                height: size.1,
                frame_rate: self.config.capture.frame_rate,
                bitrate: target,
                keyframe_interval: self.config.keyframe_interval,
            };
            let opened = open(config, frame).map_err(|error| error.to_string())?;
            self.adopt(opened, config, true);
            self.report_zero_copy(ZeroCopy::Active);
        }
        let running = self.encoder.as_mut().expect("opened above");
        if running.config.bitrate != target {
            running
                .encoder
                .set_bitrate(target)
                .map_err(|error| error.to_string())?;
            running.config.bitrate = target;
            self.shared
                .counters
                .bitrate
                .store(target, Ordering::Relaxed);
        }
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
    fn output_size(&self, source: (u32, u32)) -> (u32, u32) {
        let (mut num, mut den) = match self.pace.level {
            3 => (3, 4),
            4 => (1, 2),
            _ => return source,
        };
        let shortest = source.0.min(source.1);
        if shortest <= 16 {
            return source;
        }
        if shortest * num / den < 16 {
            // Stop at the codec's minimum edge without stretching a thin
            // source into a different aspect ratio.
            num = 16;
            den = shortest;
        }
        fastcord_platform::capture::convert::fit_output(
            source.0,
            source.1,
            (source.0 * num / den).max(16),
            (source.1 * num / den).max(16),
        )
    }
    fn scaled_frame(&mut self, sample: &VideoSample<'_>) -> Flow {
        self.picture_scaled(sample, Instant::now())
    }
    fn wants_gpu(&self) -> bool {
        self.zero_copy && !self.ended
    }

    fn gpu_frame(&mut self, frame: &GpuFrame<'_>) -> GpuOutcome {
        self.picture_gpu(frame, Instant::now())
    }

    fn frame(&mut self, sample: &VideoSample<'_>) -> Flow {
        self.picture(sample, Instant::now())
    }

    fn tick(&mut self) -> Tick {
        self.tick_at(Instant::now())
    }
}

/// CPU fallback for already-NV12 sources (camera/macOS). Box averages luma
/// and each interleaved chroma component independently; no color conversion.
/// Input is capture-bounded, output never grows past one 1280x720 surface.
fn scale_nv12(sample: &VideoSample<'_>, size: (u32, u32), out: &mut Vec<u8>) {
    let (w, h) = (size.0 as usize, size.1 as usize);
    out.resize(w * h * 3 / 2, 0);
    let (y, uv) = sample.planes();
    for (src, dest, sw, sh, dw, dh, components) in [
        (
            y,
            0,
            sample.width() as usize,
            sample.height() as usize,
            w,
            h,
            1,
        ),
        (
            uv,
            w * h,
            sample.width() as usize / 2,
            sample.height() as usize / 2,
            w / 2,
            h / 2,
            2,
        ),
    ] {
        for row in 0..dh {
            for col in 0..dw {
                let (left, right) = (col * sw / dw, ((col + 1) * sw / dw).max(col * sw / dw + 1));
                let (top, bottom) = (row * sh / dh, ((row + 1) * sh / dh).max(row * sh / dh + 1));
                let count = ((right - left) * (bottom - top)) as u32;
                for component in 0..components {
                    let mut sum = 0u32;
                    for sy in top..bottom {
                        for sx in left..right {
                            sum +=
                                u32::from(src[sy * sample.stride() + sx * components + component]);
                        }
                    }
                    out[dest + (row * dw + col) * components + component] =
                        ((sum + count / 2) / count) as u8;
                }
            }
        }
    }
}

#[cfg(test)]
impl Pipeline {
    pub(super) fn set_encode_duration(&mut self, took: Duration) {
        self.encode_duration = Some(took);
    }
}

#[cfg(test)]
mod scaling_tests {
    use super::*;
    #[test]
    fn nv12_downscale_averages_pixels_and_preserves_chroma_and_padding() {
        let mut input = vec![255u8; 40 * 48];
        for y in 0..32 {
            for x in 0..32 {
                input[y * 40 + x] = (x + y) as u8;
            }
        }
        for y in 0..16 {
            for x in 0..16 {
                input[40 * 32 + y * 40 + x * 2] = 70;
                input[40 * 32 + y * 40 + x * 2 + 1] = 190;
            }
        }
        let sample = VideoSample::nv12(32, 32, 40, &input, Duration::ZERO).unwrap();
        let mut scaled = Vec::new();
        scale_nv12(&sample, (16, 16), &mut scaled);
        assert_eq!(scaled.len(), 16 * 16 * 3 / 2);
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(scaled[y * 16 + x], (2 * x + 2 * y + 1) as u8);
            }
        }
        for pair in scaled[256..].as_chunks::<2>().0 {
            assert_eq!(pair, &[70, 190]);
        }
        let capacity = scaled.capacity();
        scale_nv12(&sample, (16, 16), &mut scaled);
        assert_eq!(
            scaled.capacity(),
            capacity,
            "reuse the bounded fallback buffer"
        );
    }
}
