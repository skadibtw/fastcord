//! Engine-thread processing between the device rings and Opus.
//!
//! ```text
//! capture ring (device rate, device channels)
//!   -> downmix to mono -> resample to 48 kHz -> echo cancel -> 480-sample denoise -> gain
//!   -> 960-sample frames -> level meter -> sensitivity gate -> Opus
//! Opus packets -> decode 48 kHz stereo -> resample to device rate
//!   -> device channel layout -> playback ring (~40 ms target, 100 ms cap)
//! ```
//!
//! All buffers are sized when a stream opens; steady-state processing does
//! not allocate.
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::channels::{downmix_to_mono, stereo_to_device};
use crate::codec::{
    CHANNELS, EncodedPacket, FRAME_SAMPLES, MAX_FRAME_SAMPLES, SAMPLE_RATE, VoiceDecoder,
    VoiceEncoder,
};
use crate::denoise::{BLOCK_SAMPLES, MicrophoneDenoiser};
use crate::device::StreamFormat;
use crate::echo::{EchoCanceller, ReferenceTap};
use crate::error::AudioError;
use crate::jitter::JitterMixer;
use crate::level::{FLOOR_TENTHS_DB, GainStage, InputDiagnostics, InputGain, measure};
use crate::resample::RateConverter;
use crate::ring::{RingConsumer, RingProducer};

/// Hard cap of either ring (SPEC §7.1).
pub(crate) const RING_LIMIT_MS: u32 = 100;
/// Normal playback buffering target (SPEC §7.1).
pub(crate) const PLAYBACK_TARGET_MS: u32 = 40;
/// Maximum Opus decode attempts in one playback service pass.
const PLAYBACK_PACKET_BUDGET: usize = 16;

/// One encoded 20 ms microphone frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedFrame {
    pub packet: EncodedPacket,
    /// Frame number since capture started; the RTP timestamp advances by
    /// [`FRAME_SAMPLES`] per frame.
    pub sequence: u64,
}

/// One authenticated remote Opus packet offered to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFrame {
    pub ssrc: u32,
    pub sequence: u64,
    pub timestamp: u32,
    pub packet: EncodedPacket,
}

/// Engine-thread counters (lock-free, read by [`crate::EngineStats`]).
#[derive(Default)]
pub(crate) struct PipelineCounters {
    pub(crate) frames_encoded: AtomicU64,
    pub(crate) encode_errors: AtomicU64,
    /// Blocks given to a non-Off denoiser, including mode-switch priming and warm-up.
    pub(crate) denoise_blocks: AtomicU64,
    /// Blocks given to the echo canceller; unchanged while muted or off.
    pub(crate) echo_blocks: AtomicU64,
    /// Encoded frames dropped because the consumer was not keeping up.
    pub(crate) frames_dropped: AtomicU64,
    pub(crate) packets_decoded: AtomicU64,
    pub(crate) decode_errors: AtomicU64,
    /// 20 ms remote frames recovered from FEC or concealed (lost or late packets).
    pub(crate) plc_frames: AtomicU64,
    /// Input sensitivity: 0 transmits everything, otherwise frames quieter than
    /// `-n` dBFS are not sent (see [`VoiceGate`]).
    pub(crate) gate_below_full_scale: AtomicU8,
    /// 20 ms frames the sensitivity gate kept back.
    pub(crate) frames_gated: AtomicU64,
    /// [`InputGain::to_bits`]; 0 is 0 dB with automatic gain off.
    pub(crate) gain_bits: AtomicU32,
    /// A diagnostics consumer is attached: only then is the level measured.
    pub(crate) meter_enabled: AtomicBool,
    /// Loudest frame RMS / highest peak since the last read, as tenths of dB above the
    /// floor (0 = silence), so `fetch_max` and a zero default are correct.
    pub(crate) meter_rms: AtomicU32,
    pub(crate) meter_peak: AtomicU32,
    /// Frames measured and frames that reached full scale since diagnostics were enabled.
    pub(crate) meter_frames: AtomicU64,
    pub(crate) meter_clipped: AtomicU64,
    /// Total gain in force, tenths of dB offset by 1000.
    pub(crate) meter_gain: AtomicU32,
}

/// Offset that keeps tenths of dB unsigned in the meter atomics.
const METER_OFFSET: i32 = -(FLOOR_TENTHS_DB as i32);

impl PipelineCounters {
    /// Switches level measurement on or off; switching on starts the counts afresh.
    pub(crate) fn set_meter(&self, enabled: bool) {
        if enabled && !self.meter_enabled.load(Ordering::Relaxed) {
            self.meter_rms.store(0, Ordering::Relaxed);
            self.meter_peak.store(0, Ordering::Relaxed);
            self.meter_frames.store(0, Ordering::Relaxed);
            self.meter_clipped.store(0, Ordering::Relaxed);
            self.meter_gain
                .store(METER_OFFSET as u32, Ordering::Relaxed);
        }
        self.meter_enabled.store(enabled, Ordering::Release);
    }

    fn record_frame(&self, frame: &[f32]) {
        let (rms, peak) = measure(frame);
        self.meter_rms
            .fetch_max((i32::from(rms) + METER_OFFSET) as u32, Ordering::Relaxed);
        self.meter_peak
            .fetch_max((i32::from(peak) + METER_OFFSET) as u32, Ordering::Relaxed);
        if frame.iter().any(|s| s.abs() >= CLIP_LEVEL) {
            self.meter_clipped.fetch_add(1, Ordering::Relaxed);
        }
        self.meter_frames.fetch_add(1, Ordering::Release);
    }

    /// Level since the previous call (the maxima are consumed), without the echo part.
    pub(crate) fn take_input(&self) -> InputDiagnostics {
        let tenths = |value: u32| (value as i32 - METER_OFFSET) as i16;
        InputDiagnostics {
            frames: self.meter_frames.load(Ordering::Acquire),
            rms_tenths_db: tenths(self.meter_rms.swap(0, Ordering::Relaxed)),
            peak_tenths_db: tenths(self.meter_peak.swap(0, Ordering::Relaxed)),
            clipped_frames: self.meter_clipped.load(Ordering::Relaxed),
            gain_tenths_db: tenths(self.meter_gain.load(Ordering::Relaxed)),
            echo: None,
        }
    }
}

/// A sample at or above this level (-0.45 dBFS) is at full scale or at the limiter ceiling
/// (0.97) and counts as clipped.
const CLIP_LEVEL: f32 = 0.95;

/// Frames the gate stays open after the level last reached the threshold (200 ms).
const GATE_HANG_FRAMES: u8 = 10;
/// Level that keeps an open gate open, below the threshold that opens it.
const GATE_HYSTERESIS_DB: f32 = 3.0;

/// Voice-activity gate on 20 ms frames: opens at the threshold, closes only after the
/// level stayed below threshold minus the hysteresis for [`GATE_HANG_FRAMES`]. Pure
/// arithmetic on the owner thread; no allocation.
#[derive(Default)]
pub(crate) struct VoiceGate {
    open: bool,
    hang: u8,
}

impl VoiceGate {
    /// Whether `frame` is sent. `below_full_scale == 0` disables the gate.
    pub(crate) fn admit(&mut self, frame: &[f32], below_full_scale: u8) -> bool {
        if below_full_scale == 0 {
            self.open = false;
            return true;
        }
        let mean_square = frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32;
        let level = 10.0 * mean_square.max(1e-12).log10();
        let threshold = -f32::from(below_full_scale);
        if level >= threshold {
            self.open = true;
            self.hang = GATE_HANG_FRAMES;
        } else if self.open && level >= threshold - GATE_HYSTERESIS_DB {
            self.hang = GATE_HANG_FRAMES;
        } else if self.open {
            self.hang -= 1;
            self.open = self.hang > 0;
        }
        self.open
    }
}

/// Frames of `ms` milliseconds at `rate`, at least one.
pub(crate) fn frames_for(rate: u32, ms: u32) -> usize {
    (u64::from(rate) * u64::from(ms) / 1_000).max(1) as usize
}

const _: () = assert!(FRAME_SAMPLES.is_multiple_of(BLOCK_SAMPLES));

/// Gathers denoised blocks into 20 ms frames and encodes them.
struct FrameEncoder {
    frame: Box<[f32; FRAME_SAMPLES]>,
    filled: usize,
    encoder: VoiceEncoder,
    sequence: u64,
    gate: VoiceGate,
    gain: GainStage,
}

impl FrameEncoder {
    fn accept(
        &mut self,
        block: &[f32; BLOCK_SAMPLES],
        counters: &PipelineCounters,
        emit: &mut impl FnMut(CapturedFrame),
    ) {
        let slot = &mut self.frame[self.filled..self.filled + BLOCK_SAMPLES];
        for (out, &sample) in slot.iter_mut().zip(block) {
            *out = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        let gain = self.gain.process(
            slot,
            InputGain::from_bits(counters.gain_bits.load(Ordering::Relaxed)),
        );
        if counters.meter_enabled.load(Ordering::Relaxed) {
            let tenths = (gain * 10.0).round() as i32 + METER_OFFSET;
            counters
                .meter_gain
                .store(tenths.max(0) as u32, Ordering::Relaxed);
        }
        self.filled += BLOCK_SAMPLES;
        if self.filled < FRAME_SAMPLES {
            return;
        }
        self.filled = 0;
        if counters.meter_enabled.load(Ordering::Relaxed) {
            counters.record_frame(&self.frame[..]);
        }
        if !self.gate.admit(
            &self.frame[..],
            counters.gate_below_full_scale.load(Ordering::Relaxed),
        ) {
            counters.frames_gated.fetch_add(1, Ordering::Relaxed);
            self.sequence += 1;
            return;
        }
        let mut packet = EncodedPacket::empty();
        match self.encoder.encode_mono(&self.frame, &mut packet) {
            Ok(()) => {
                counters.frames_encoded.fetch_add(1, Ordering::Relaxed);
                emit(CapturedFrame {
                    packet,
                    sequence: self.sequence,
                });
            }
            Err(_) => {
                counters.encode_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Keep the capture clock even when a codec frame fails.
        self.sequence += 1;
    }
}

pub(crate) struct CapturePipeline {
    ring: RingConsumer,
    device: Box<[f32]>,
    mono: Box<[f32]>,
    converter: RateConverter,
    /// Resampled microphone samples gathered into one denoise block.
    block: [f32; BLOCK_SAMPLES],
    block_filled: usize,
    frames: FrameEncoder,
}

impl CapturePipeline {
    pub(crate) fn new(
        format: StreamFormat,
        ring: RingConsumer,
        bitrate: u32,
    ) -> Result<Self, AudioError> {
        let block = frames_for(format.rate, 10);
        Ok(Self {
            device: vec![0.0; block * format.channels].into_boxed_slice(),
            mono: vec![0.0; block].into_boxed_slice(),
            converter: RateConverter::new(format.rate, SAMPLE_RATE, 1, block)?,
            block: [0.0; BLOCK_SAMPLES],
            block_filled: 0,
            frames: FrameEncoder {
                frame: Box::new([0.0; FRAME_SAMPLES]),
                filled: 0,
                encoder: VoiceEncoder::new(bitrate)?,
                sequence: 0,
                gate: VoiceGate::default(),
                gain: GainStage::new(),
            },
            ring,
        })
    }

    /// Discards device samples without downmixing, resampling, encoding, or inference.
    pub(crate) fn discard(&mut self) {
        while self.ring.pop(&mut self.device) != 0 {}
        self.block.fill(0.0);
        self.block_filled = 0;
        self.frames.frame.fill(0.0);
        self.frames.filled = 0;
        self.frames.gate = VoiceGate::default();
        self.converter.reset();
    }

    /// Drains the capture ring, emitting every completed frame.
    pub(crate) fn run(
        &mut self,
        counters: &PipelineCounters,
        denoiser: &mut MicrophoneDenoiser,
        mut echo: Option<&mut EchoCanceller>,
        emit: &mut impl FnMut(CapturedFrame),
    ) {
        let Self {
            ring,
            device,
            mono,
            converter,
            block,
            block_filled,
            frames,
        } = self;
        let mut processed = [0.0; BLOCK_SAMPLES];
        loop {
            let samples = ring.pop(device);
            if samples == 0 {
                break;
            }
            let count = downmix_to_mono(&device[..samples], ring.channels(), mono);
            converter.process(&mono[..count], &mut |mut chunk: &[f32]| {
                while !chunk.is_empty() {
                    let take = (BLOCK_SAMPLES - *block_filled).min(chunk.len());
                    block[*block_filled..*block_filled + take].copy_from_slice(&chunk[..take]);
                    *block_filled += take;
                    chunk = &chunk[take..];
                    if *block_filled < BLOCK_SAMPLES {
                        continue;
                    }
                    *block_filled = 0;
                    if let Some(echo) = echo.as_deref_mut() {
                        counters.echo_blocks.fetch_add(1, Ordering::Relaxed);
                        let near = *block;
                        echo.process(&near, block);
                    }
                    denoiser.push(block);
                    while denoiser.pop(&mut processed) {
                        frames.accept(&processed, counters, emit);
                    }
                }
            });
        }
        // A worker thread answers after the block that it processed was pushed.
        while denoiser.pop(&mut processed) {
            frames.accept(&processed, counters, emit);
        }
    }
}

pub(crate) struct PlaybackPipeline {
    ring: RingProducer,
    target: usize,
    decoder: VoiceDecoder,
    pcm: Box<[f32]>,
    mixer: JitterMixer,
    mixed: Box<[f32; FRAME_SAMPLES * CHANNELS]>,
    converter: RateConverter,
    /// Converted stereo at the device rate not yet accepted by the ring.
    pending: Vec<f32>,
    pending_start: usize,
    enabled: bool,
    /// Echo-cancellation reference; present only while cancellation is on.
    tap: Option<ReferenceTap>,
    rate: u32,
}

impl PlaybackPipeline {
    pub(crate) fn new(format: StreamFormat, ring: RingProducer) -> Result<Self, AudioError> {
        let target_frames = frames_for(format.rate, PLAYBACK_TARGET_MS)
            .max(2 * format.period_frames.unwrap_or(0) as usize)
            .min(ring.limit() / format.channels);
        // One maximal packet plus the converter's staged chunk, at the device rate.
        let pending_frames =
            (MAX_FRAME_SAMPLES + 480) * format.rate as usize / SAMPLE_RATE as usize + 64;
        Ok(Self {
            target: target_frames * format.channels,
            decoder: VoiceDecoder::new()?,
            pcm: vec![0.0; MAX_FRAME_SAMPLES * CHANNELS].into_boxed_slice(),
            mixer: JitterMixer::new()?,
            mixed: Box::new([0.0; FRAME_SAMPLES * CHANNELS]),
            converter: RateConverter::new(SAMPLE_RATE, format.rate, CHANNELS, 480)?,
            pending: Vec::with_capacity(pending_frames * CHANNELS),
            pending_start: 0,
            enabled: true,
            tap: None,
            rate: format.rate,
            ring,
        })
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if !enabled {
            self.ring.clear();
            self.pending.clear();
            self.pending_start = 0;
            self.mixer.clear();
            if let Some(tap) = &mut self.tap {
                tap.clear();
            }
        }
    }

    /// Starts or stops copying played audio for echo cancellation.
    pub(crate) fn set_echo_reference(&mut self, active: bool) {
        if active == self.tap.is_some() {
            return;
        }
        self.tap = if active {
            ReferenceTap::new(self.rate, self.ring.limit() / self.ring.channels()).ok()
        } else {
            None
        };
    }

    /// Hands the echo canceller whatever the output device has played since the last call.
    pub(crate) fn feed_echo(&mut self, echo: &mut EchoCanceller, now: std::time::Instant) {
        let unread = self.ring.len() / self.ring.channels();
        if let Some(tap) = &mut self.tap {
            tap.advance(unread, now, echo);
        }
    }
    pub(crate) fn push_remote(&mut self, frame: RemoteFrame) {
        self.mixer.push(frame);
    }
    pub(crate) fn set_gain(&mut self, ssrc: u32, gain: f32) {
        self.mixer.set_gain(ssrc, gain);
    }

    /// Keeps the playback ring near its target, pulling packets from `next`
    /// only when more audio is needed.
    pub(crate) fn run(
        &mut self,
        counters: &PipelineCounters,
        next: &mut impl FnMut() -> Option<EncodedPacket>,
    ) {
        let mut attempts = 0;
        loop {
            self.flush_pending();
            if self.pending_start < self.pending.len() || self.ring.len() >= self.target {
                return;
            }
            if attempts == PLAYBACK_PACKET_BUDGET {
                return;
            }
            let Some(packet) = next() else {
                let active = self.mixer.mix(&mut self.mixed, std::time::Instant::now());
                counters
                    .plc_frames
                    .store(self.mixer.concealed_frames(), Ordering::Relaxed);
                if active {
                    self.pending_start = 0;
                    self.pending.clear();
                    let Self {
                        converter,
                        pending,
                        mixed,
                        ..
                    } = self;
                    converter.process(mixed.as_slice(), &mut |block: &[f32]| {
                        pending.extend_from_slice(block);
                    });
                    continue;
                }
                return;
            };
            attempts += 1;
            let frames = match self.decoder.decode(packet.as_bytes(), &mut self.pcm) {
                Ok(frames) => frames,
                Err(_) => {
                    counters.decode_errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            counters.packets_decoded.fetch_add(1, Ordering::Relaxed);
            self.pending_start = 0;
            let Self {
                converter,
                pending,
                pcm,
                ..
            } = self;
            pending.clear();
            converter.process(&pcm[..frames * CHANNELS], &mut |block: &[f32]| {
                pending.extend_from_slice(block);
            });
        }
    }

    fn flush_pending(&mut self) {
        let stereo = &self.pending[self.pending_start..];
        if stereo.is_empty() {
            return;
        }
        let channels = self.ring.channels();
        let written = self.ring.push_available(stereo_to_device(stereo, channels));
        if let Some(tap) = &mut self.tap {
            for frame in stereo.as_chunks::<2>().0.iter().take(written / channels) {
                tap.push((frame[0] + frame[1]) * 0.5);
            }
        }
        self.pending_start += written / channels * CHANNELS;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoiseSuppression;
    use crate::resample::tests::{sine, tone_level};
    use crate::ring::sample_ring;

    fn denoiser(mode: NoiseSuppression) -> MicrophoneDenoiser {
        MicrophoneDenoiser::new(mode, crate::denoise::high_quality_factory())
    }

    fn format(rate: u32, channels: usize) -> StreamFormat {
        StreamFormat {
            rate,
            channels,
            period_frames: None,
        }
    }

    fn capture(rate: u32, channels: usize) -> (RingProducer, CapturePipeline) {
        let (producer, consumer) = sample_ring(channels, frames_for(rate, RING_LIMIT_MS));
        let pipeline = CapturePipeline::new(format(rate, channels), consumer, 64_000).unwrap();
        (producer, pipeline)
    }

    fn playback(rate: u32, channels: usize) -> (RingConsumer, PlaybackPipeline) {
        let (producer, consumer) = sample_ring(channels, frames_for(rate, RING_LIMIT_MS));
        let pipeline = PlaybackPipeline::new(format(rate, channels), producer).unwrap();
        (consumer, pipeline)
    }

    #[test]
    fn voice_gate_opens_at_threshold_holds_then_closes() {
        let mut gate = VoiceGate::default();
        let loud = [0.1_f32; 960]; // -20 dBFS
        let quiet = [0.001_f32; 960]; // -60 dBFS
        let edge = [0.0089_f32; 960]; // about -41 dBFS: inside the hysteresis band of -40
        assert!(!gate.admit(&quiet, 40), "below threshold stays closed");
        assert!(gate.admit(&loud, 40));
        for _ in 0..GATE_HANG_FRAMES - 1 {
            assert!(gate.admit(&quiet, 40), "hangover keeps the tail");
        }
        assert!(!gate.admit(&quiet, 40), "closes after the hangover");
        assert!(gate.admit(&loud, 40));
        for _ in 0..3 * GATE_HANG_FRAMES {
            assert!(gate.admit(&edge, 40), "hysteresis band keeps it open");
        }
        assert!(gate.admit(&quiet, 0), "0 disables the gate");
    }

    #[test]
    fn sensitivity_gate_withholds_quiet_frames_and_keeps_the_clock() {
        let (mut ring, mut pipeline) = capture(48_000, 1);
        let counters = PipelineCounters::default();
        counters.gate_below_full_scale.store(40, Ordering::Relaxed);
        let mut denoiser = denoiser(NoiseSuppression::Off);
        let mut sent = Vec::new();
        for level in [0.001_f32, 0.001, 0.2, 0.2] {
            ring.push_overwrite(std::iter::repeat_n(level, 960));
            pipeline.run(&counters, &mut denoiser, None, &mut |f| sent.push(f));
        }
        assert_eq!(counters.frames_gated.load(Ordering::Relaxed), 2);
        assert_eq!(sent.iter().map(|f| f.sequence).collect::<Vec<_>>(), [2, 3]);
    }

    /// 100 ms of a 440 Hz tone of `amplitude` through the capture path.
    fn run_tone(counters: &PipelineCounters, amplitude: f32) -> Vec<CapturedFrame> {
        let (mut ring, mut pipeline) = capture(48_000, 1);
        let tone = sine(48_000, 440.0, amplitude, 4_800);
        ring.push_overwrite(tone.iter().copied());
        let mut sent = Vec::new();
        pipeline.run(
            counters,
            &mut denoiser(NoiseSuppression::Off),
            None,
            &mut |f| sent.push(f),
        );
        sent
    }

    #[test]
    fn the_meter_measures_nothing_until_asked_and_reads_the_gated_signal() {
        let counters = PipelineCounters::default();
        run_tone(&counters, 0.1);
        let silent = counters.take_input();
        assert_eq!(silent.frames, 0, "off: nothing is measured");

        counters.set_meter(true);
        let sent = run_tone(&counters, 0.1);
        assert_eq!(sent.len(), 5);
        let level = counters.take_input();
        assert_eq!(level.frames, 5);
        // 0.1 amplitude: peak -20 dBFS, RMS -23 dBFS.
        assert_eq!(level.peak_tenths_db, -200);
        assert!((level.rms_tenths_db + 230).abs() <= 2, "{level:?}");
        assert_eq!(level.clipped_frames, 0);
        assert_eq!(level.gain_tenths_db, 0);
        // The maxima are consumed; the counts are not.
        let again = counters.take_input();
        assert_eq!(again.rms_tenths_db, FLOOR_TENTHS_DB);
        assert_eq!(again.frames, 5);

        // The meter sees the signal after gain, which is also what the gate compares.
        counters.gain_bits.store(
            InputGain {
                manual_db: 6,
                automatic: false,
            }
            .to_bits(),
            Ordering::Relaxed,
        );
        run_tone(&counters, 0.1);
        let boosted = counters.take_input();
        assert!(
            (boosted.peak_tenths_db + 140).abs() <= 10,
            "+6 dB: {boosted:?}"
        );
        assert_eq!(boosted.gain_tenths_db, 60);

        // Clipping is counted and cannot pass the limiter ceiling.
        counters.gain_bits.store(
            InputGain {
                manual_db: 30,
                automatic: false,
            }
            .to_bits(),
            Ordering::Relaxed,
        );
        run_tone(&counters, 0.5);
        let hot = counters.take_input();
        assert!(hot.clipped_frames > 0, "{hot:?}");
        assert!(
            hot.peak_tenths_db <= -2,
            "limited below full scale: {hot:?}"
        );

        counters.set_meter(false);
        run_tone(&counters, 0.1);
        assert_eq!(counters.take_input().frames, hot.frames, "off again");
    }

    #[test]
    fn the_gate_compares_the_gained_level() {
        // A -52 dBFS tone is below a -40 dB threshold; +20 dB of gain brings it above.
        let counters = PipelineCounters::default();
        counters.gate_below_full_scale.store(40, Ordering::Relaxed);
        assert!(run_tone(&counters, 0.0035).is_empty());
        counters.gain_bits.store(
            InputGain {
                manual_db: 20,
                automatic: false,
            }
            .to_bits(),
            Ordering::Relaxed,
        );
        assert!(!run_tone(&counters, 0.0035).is_empty());
    }

    #[test]
    fn muted_capture_discards_samples_without_encoding_or_inference() {
        let (mut ring, mut pipeline) = capture(48_000, 1);
        // Several 20 ms frames' worth, so removing `discard()` makes `run` emit and panic.
        ring.push_overwrite(std::iter::repeat_n(0.5, 4_800));
        pipeline.discard();
        let counters = PipelineCounters::default();
        let mut denoiser = denoiser(NoiseSuppression::Light);
        pipeline.run(&counters, &mut denoiser, None, &mut |_| {
            panic!("muted capture emitted a frame")
        });
        assert_eq!(denoiser.inference_blocks(), 0);
        assert_eq!(counters.frames_encoded.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn light_runs_once_per_block_and_off_runs_no_inference() {
        for (mode, blocks) in [(NoiseSuppression::Light, 10), (NoiseSuppression::Off, 0)] {
            let (mut ring, mut pipeline) = capture(48_000, 1);
            ring.push_overwrite(std::iter::repeat_n(0.1, 4_800));
            let counters = PipelineCounters::default();
            let mut denoiser = denoiser(mode);
            let mut frames = 0;
            pipeline.run(&counters, &mut denoiser, None, &mut |_| frames += 1);
            assert_eq!(frames, 5, "{mode:?}: 100 ms is five 20 ms frames");
            assert_eq!(denoiser.inference_blocks(), blocks, "{mode:?}");
        }
    }

    #[test]
    fn disabled_playback_clears_queued_audible_samples() {
        let (consumer, mut pipeline) = playback(48_000, 2);
        pipeline.ring.push_overwrite([0.5; 8].into_iter());
        assert_eq!(consumer.len(), 8);
        pipeline.set_enabled(false);
        assert_eq!(consumer.len(), 0);
        pipeline.set_enabled(true);
        assert_eq!(consumer.len(), 0);
    }

    /// Feeds `signal` (mono, at `rate`) through a `channels`-wide capture
    /// pipeline in 10 ms device callbacks and returns the encoded frames.
    fn capture_signal(rate: u32, channels: usize, signal: &[f32]) -> Vec<CapturedFrame> {
        let (mut ring, mut pipeline) = capture(rate, channels);
        let counters = PipelineCounters::default();
        let mut frames = Vec::new();
        let mut denoiser = denoiser(NoiseSuppression::Off);
        for block in signal.chunks(frames_for(rate, 10)) {
            let interleaved = block.iter().flat_map(|&s| std::iter::repeat_n(s, channels));
            let n = block.len() * channels;
            ring.push_overwrite(interleaved.collect::<Vec<_>>().into_iter());
            assert!(n <= ring.limit());
            pipeline.run(&counters, &mut denoiser, None, &mut |frame| {
                frames.push(frame)
            });
        }
        assert_eq!(
            counters.frames_encoded.load(Ordering::Relaxed),
            frames.len() as u64
        );
        frames
    }

    fn decode_all(frames: &[CapturedFrame]) -> Vec<f32> {
        let mut decoder = VoiceDecoder::new().unwrap();
        let mut out = vec![0.0; MAX_FRAME_SAMPLES * CHANNELS];
        let mut left = Vec::new();
        for frame in frames {
            let n = decoder.decode(frame.packet.as_bytes(), &mut out).unwrap();
            left.extend(out[..n * CHANNELS].iter().step_by(2));
        }
        left
    }

    #[test]
    fn capture_at_48k_emits_one_numbered_frame_per_20ms() {
        let signal = sine(48_000, 500.0, 0.4, 48_000);
        let frames = capture_signal(48_000, 1, &signal);
        assert_eq!(frames.len(), 50);
        assert!(
            frames
                .iter()
                .enumerate()
                .all(|(i, f)| f.sequence == i as u64)
        );
        let decoded = decode_all(&frames);
        let level = tone_level(&decoded[9_600..], 48_000, 500.0);
        assert!((level - 0.4).abs() < 0.04, "level {level}");
    }

    #[test]
    fn capture_resamples_and_downmixes_a_44k_stereo_device() {
        let signal = sine(44_100, 1_000.0, 0.5, 44_100);
        let frames = capture_signal(44_100, 2, &signal);
        // One second minus the converter's partial chunk ≈ 49–50 frames.
        assert!((49..=50).contains(&frames.len()), "{}", frames.len());
        let decoded = decode_all(&frames);
        let level = tone_level(&decoded[9_600..], 48_000, 1_000.0);
        assert!((level - 0.5).abs() < 0.05, "level {level}");
    }

    #[test]
    fn capture_falls_behind_gracefully_with_bounded_latency() {
        let (mut ring, mut pipeline) = capture(48_000, 1);
        // 300 ms arrive while the engine is stalled; only 100 ms survive.
        for _ in 0..30 {
            ring.push_overwrite(
                std::iter::repeat_n(0.1, 480)
                    .collect::<Vec<_>>()
                    .into_iter(),
            );
        }
        let counters = PipelineCounters::default();
        let mut frames = 0;
        let mut denoiser = denoiser(NoiseSuppression::Off);
        pipeline.run(&counters, &mut denoiser, None, &mut |_| frames += 1);
        assert_eq!(frames, 5, "100 ms = five 20 ms frames");
    }

    fn encode_tone(frames: usize, frequency: f32) -> Vec<EncodedPacket> {
        let signal = sine(48_000, frequency, 0.5, frames * FRAME_SAMPLES);
        capture_signal(48_000, 1, &signal)
            .into_iter()
            .map(|frame| frame.packet)
            .collect()
    }

    #[test]
    fn playback_fills_to_the_target_then_waits() {
        let packets = encode_tone(20, 440.0);
        let (ring, mut pipeline) = playback(48_000, 2);
        let counters = PipelineCounters::default();
        let mut queue = packets.into_iter();
        pipeline.run(&counters, &mut || queue.next());
        // 40 ms target = two 20 ms packets; nothing more is pulled.
        assert_eq!(counters.packets_decoded.load(Ordering::Relaxed), 2);
        assert_eq!(ring.len(), 1_920 * 2);
        pipeline.run(&counters, &mut || queue.next());
        assert_eq!(counters.packets_decoded.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn playback_resamples_to_a_44k_surround_device_and_round_trips_the_tone() {
        let packets = encode_tone(50, 440.0);
        let (mut ring, mut pipeline) = playback(44_100, 6);
        let counters = PipelineCounters::default();
        let mut queue = packets.into_iter();
        let mut device = vec![0.0f32; 441 * 6];
        let mut left = Vec::new();
        let mut third_channel_energy = 0.0f32;
        for _ in 0..100 {
            pipeline.run(&counters, &mut || queue.next());
            let n = ring.pop(&mut device);
            for frame in device[..n].as_chunks::<6>().0 {
                left.push(frame[0]);
                assert_eq!(frame[0], frame[1], "mono-coded voice on both fronts");
                third_channel_energy += frame[2].abs();
            }
        }
        assert_eq!(counters.packets_decoded.load(Ordering::Relaxed), 50);
        assert!((43_000..=44_100).contains(&left.len()), "{}", left.len());
        let level = tone_level(&left[8_820..], 44_100, 440.0);
        assert!((level - 0.5).abs() < 0.05, "level {level}");
        assert_eq!(third_channel_energy, 0.0);
    }

    #[test]
    fn playback_skips_undecodable_packets() {
        let mut packets = encode_tone(3, 440.0);
        packets.insert(1, EncodedPacket::from_slice(&[0x03, 0x00]).unwrap());
        let (_ring, mut pipeline) = playback(48_000, 2);
        let counters = PipelineCounters::default();
        let mut queue = packets.into_iter();
        pipeline.run(&counters, &mut || queue.next());
        assert_eq!(counters.decode_errors.load(Ordering::Relaxed), 1);
        assert_eq!(counters.packets_decoded.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn playback_bounds_malformed_packet_attempts_per_pass() {
        let (_ring, mut pipeline) = playback(48_000, 2);
        let counters = PipelineCounters::default();
        let packet = EncodedPacket::from_slice(&[0x03, 0x00]).unwrap();
        let mut attempts = 0;
        pipeline.run(&counters, &mut || {
            attempts += 1;
            Some(packet.clone())
        });
        assert_eq!(attempts, PLAYBACK_PACKET_BUDGET);
        assert_eq!(
            counters.decode_errors.load(Ordering::Relaxed),
            PLAYBACK_PACKET_BUDGET as u64
        );
        assert_eq!(counters.packets_decoded.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_long_packet_is_held_back_instead_of_exceeding_the_ring_cap() {
        // A 120 ms packet cannot fit a 100 ms ring at once.
        let mut encoder =
            opus2::Encoder::new(48_000, opus2::Channels::Stereo, opus2::Application::Voip).unwrap();
        let pcm = vec![0.1f32; MAX_FRAME_SAMPLES * CHANNELS];
        let mut bytes = [0u8; 4_000];
        let len = encoder.encode_float(&pcm, &mut bytes).unwrap();
        let packet = EncodedPacket::from_slice(&bytes[..len]).unwrap();
        let (mut ring, mut pipeline) = playback(48_000, 2);
        let counters = PipelineCounters::default();
        let mut once = Some(packet);
        pipeline.run(&counters, &mut || once.take());
        assert_eq!(ring.len(), 4_800 * 2, "filled exactly to the 100 ms cap");
        let mut drain = vec![0.0; 4_800 * 2];
        ring.pop(&mut drain);
        pipeline.run(&counters, &mut || None);
        assert_eq!(ring.len(), 960 * 2, "remaining 20 ms delivered later");
    }

    #[test]
    fn steady_state_processing_does_not_allocate() {
        let packets = encode_tone(30, 440.0);
        let (mut capture_ring, mut capture) = capture(44_100, 2);
        let (mut playback_ring, mut playback) = playback(44_100, 2);
        let counters = PipelineCounters::default();
        let block = vec![0.2f32; 441 * 2];
        let mut device = vec![0.0f32; 441 * 2];
        let mut queue = packets.into_iter();
        let mut emitted = 0;
        let mut denoiser = denoiser(NoiseSuppression::Off);
        // The gain stage, limiter and level meter are part of the steady state.
        counters.gain_bits.store(
            InputGain {
                manual_db: 9,
                automatic: true,
            }
            .to_bits(),
            Ordering::Relaxed,
        );
        counters.set_meter(true);
        // Warm up, then measure the same work inside a callback scope so the
        // counting allocator attributes any heap use.
        let mut step = |emitted: &mut usize| {
            capture_ring.push_overwrite(block.iter().copied());
            capture.run(&counters, &mut denoiser, None, &mut |_| *emitted += 1);
            playback.run(&counters, &mut || queue.next());
            playback_ring.pop(&mut device);
        };
        for _ in 0..5 {
            step(&mut emitted);
        }
        let before = crate::alloc_counter::thread_callback_heap_ops();
        crate::alloc_counter::expect_callback_allocations(|| {
            let _scope = crate::audit::CallbackScope::enter();
            for _ in 0..40 {
                step(&mut emitted);
            }
        });
        assert_eq!(crate::alloc_counter::thread_callback_heap_ops(), before);
        assert!(emitted > 15);
    }
}
