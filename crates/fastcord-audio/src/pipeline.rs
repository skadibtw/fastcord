//! Engine-thread processing between the device rings and Opus.
//!
//! ```text
//! capture ring (device rate, device channels)
//!   -> downmix to mono -> resample to 48 kHz -> 960-sample frames -> Opus
//! Opus packets -> decode 48 kHz stereo -> resample to device rate
//!   -> device channel layout -> playback ring (~40 ms target, 100 ms cap)
//! ```
//!
//! All buffers are sized when a stream opens; steady-state processing does
//! not allocate.
use std::sync::atomic::{AtomicU64, Ordering};

use crate::channels::{downmix_to_mono, stereo_to_device};
use crate::codec::{
    CHANNELS, EncodedPacket, FRAME_SAMPLES, MAX_FRAME_SAMPLES, SAMPLE_RATE, VoiceDecoder,
    VoiceEncoder,
};
use crate::device::StreamFormat;
use crate::error::AudioError;
use crate::jitter::JitterMixer;
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
    /// Encoded frames dropped because the consumer was not keeping up.
    pub(crate) frames_dropped: AtomicU64,
    pub(crate) packets_decoded: AtomicU64,
    pub(crate) decode_errors: AtomicU64,
}

/// Frames of `ms` milliseconds at `rate`, at least one.
pub(crate) fn frames_for(rate: u32, ms: u32) -> usize {
    (u64::from(rate) * u64::from(ms) / 1_000).max(1) as usize
}

pub(crate) struct CapturePipeline {
    ring: RingConsumer,
    device: Box<[f32]>,
    mono: Box<[f32]>,
    converter: RateConverter,
    frame: Box<[f32; FRAME_SAMPLES]>,
    filled: usize,
    encoder: VoiceEncoder,
    sequence: u64,
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
            frame: Box::new([0.0; FRAME_SAMPLES]),
            filled: 0,
            encoder: VoiceEncoder::new(bitrate)?,
            sequence: 0,
            ring,
        })
    }

    /// Drains the capture ring, emitting every completed frame.
    pub(crate) fn run(
        &mut self,
        counters: &PipelineCounters,
        emit: &mut impl FnMut(CapturedFrame),
    ) {
        let Self {
            ring,
            device,
            mono,
            converter,
            frame,
            filled,
            encoder,
            sequence,
        } = self;
        loop {
            let samples = ring.pop(device);
            if samples == 0 {
                break;
            }
            let frames = downmix_to_mono(&device[..samples], ring.channels(), mono);
            converter.process(&mono[..frames], &mut |mut block: &[f32]| {
                while !block.is_empty() {
                    let take = (FRAME_SAMPLES - *filled).min(block.len());
                    frame[*filled..*filled + take].copy_from_slice(&block[..take]);
                    *filled += take;
                    block = &block[take..];
                    if *filled < FRAME_SAMPLES {
                        break;
                    }
                    *filled = 0;
                    let mut packet = EncodedPacket::empty();
                    match encoder.encode_mono(frame, &mut packet) {
                        Ok(()) => {
                            counters.frames_encoded.fetch_add(1, Ordering::Relaxed);
                            emit(CapturedFrame {
                                packet,
                                sequence: *sequence,
                            });
                        }
                        Err(_) => {
                            counters.encode_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    // Sequence advances even for a failed frame so timing
                    // stays aligned with the capture clock.
                    *sequence += 1;
                }
            });
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
            ring,
        })
    }

    pub(crate) fn push_remote(&mut self, frame: RemoteFrame) {
        self.mixer.push(frame);
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
                if self.mixer.mix(&mut self.mixed, std::time::Instant::now()) {
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
        self.pending_start += written / channels * CHANNELS;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resample::tests::{sine, tone_level};
    use crate::ring::sample_ring;

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

    /// Feeds `signal` (mono, at `rate`) through a `channels`-wide capture
    /// pipeline in 10 ms device callbacks and returns the encoded frames.
    fn capture_signal(rate: u32, channels: usize, signal: &[f32]) -> Vec<CapturedFrame> {
        let (mut ring, mut pipeline) = capture(rate, channels);
        let counters = PipelineCounters::default();
        let mut frames = Vec::new();
        for block in signal.chunks(frames_for(rate, 10)) {
            let interleaved = block.iter().flat_map(|&s| std::iter::repeat_n(s, channels));
            let n = block.len() * channels;
            ring.push_overwrite(interleaved.collect::<Vec<_>>().into_iter());
            assert!(n <= ring.limit());
            pipeline.run(&counters, &mut |frame| frames.push(frame));
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
        pipeline.run(&counters, &mut |_| frames += 1);
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
        // Warm up, then measure the same work inside a callback scope so the
        // counting allocator attributes any heap use.
        let mut step = |emitted: &mut usize| {
            capture_ring.push_overwrite(block.iter().copied());
            capture.run(&counters, &mut |_| *emitted += 1);
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
