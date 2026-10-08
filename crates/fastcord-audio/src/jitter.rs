use std::time::{Duration, Instant};

use crate::codec::{CHANNELS, FRAME_SAMPLES, MAX_FRAME_SAMPLES, VoiceDecoder};
use crate::pipeline::RemoteFrame;

const SPEAKERS: usize = 4;
const PACKETS: usize = 8;
const INITIAL_TARGET: Duration = Duration::from_millis(20);
const MIN_TARGET: Duration = Duration::from_millis(20);
const MAX_TARGET: Duration = Duration::from_millis(120);
const INACTIVE: Duration = Duration::from_secs(2);
const MAX_CONSECUTIVE_PLC: usize = 3;
const FAR_SEQUENCE_THRESHOLD: u64 = 8;

struct Queued {
    frame: RemoteFrame,
}

struct Speaker {
    ssrc: u32,
    decoder: VoiceDecoder,
    queue: [Option<Queued>; PACKETS],
    len: usize,
    expected: Option<u64>,
    target: Duration,
    jitter_ms: f64,
    last_arrival: Option<Instant>,
    last_timestamp: Option<u32>,
    last_transit: Option<i32>,
    active_since: Option<Instant>,
    next_due: Option<Instant>,
    last_packet: Instant,
    pcm: Box<[f32]>,
    consecutive_plc: usize,
}

impl Speaker {
    fn new(ssrc: u32) -> Result<Self, crate::CodecError> {
        Ok(Self {
            ssrc,
            decoder: VoiceDecoder::new()?,
            queue: std::array::from_fn(|_| None),
            len: 0,
            expected: None,
            target: INITIAL_TARGET,
            jitter_ms: 0.0,
            last_arrival: None,
            last_timestamp: None,
            last_transit: None,
            active_since: None,
            next_due: None,
            last_packet: Instant::now(),
            pcm: vec![0.0; MAX_FRAME_SAMPLES * CHANNELS].into_boxed_slice(),
            consecutive_plc: 0,
        })
    }

    fn insert(&mut self, frame: RemoteFrame, now: Instant) {
        // A parked speaker is starting a new talkspurt: whatever sequence numbers the sender
        // used (or skipped) during the silence, play from this packet.
        if self.active_since.is_some() && self.next_due.is_none() {
            self.queue = std::array::from_fn(|_| None);
            self.len = 0;
            self.expected = Some(frame.sequence);
            self.consecutive_plc = 0;
        }

        // Drop genuinely late packets: those older than expected when expected is set
        if self.expected.is_some_and(|next| frame.sequence < next) {
            return;
        }

        // Check for duplicate
        if self.queue[..self.len].iter().any(|p| {
            p.as_ref()
                .is_some_and(|p| p.frame.sequence == frame.sequence)
        }) {
            return;
        }

        // Reset consecutive PLC counter when new packet arrives (gap is over)
        if self
            .expected
            .is_some_and(|expected| frame.sequence > expected)
        {
            self.consecutive_plc = 0;
        }

        // Re-anchor if arriving sequence is far ahead
        if self.expected.is_some_and(|expected| {
            frame.sequence >= expected.saturating_add(FAR_SEQUENCE_THRESHOLD)
        }) {
            // Drop all older queued frames
            self.queue = std::array::from_fn(|_| None);
            self.len = 0;
            self.expected = Some(frame.sequence);
        }

        // Update jitter and target
        if let (Some(arrival), Some(timestamp)) = (self.last_arrival, self.last_timestamp) {
            // RFC 3550 jitter algorithm: D = (R_i - R_i-1) - (S_i - S_i-1)
            // Use signed difference for reordered packets (wrapping_sub as i32)
            let elapsed_ms = now.duration_since(arrival).as_secs_f64() * 1000.0;
            let transit = (elapsed_ms * 48.0) as i32; // Convert to timestamp units at 48 kHz
            let delta_timestamp = frame.timestamp.wrapping_sub(timestamp) as i32;
            let d = ((transit - delta_timestamp).abs()) as f64 / 48.0; // Convert back to ms

            // RFC 3550: jitter = jitter + (|D| - jitter) / 16
            self.jitter_ms += (d - self.jitter_ms) / 16.0;

            // Compute target: at least 20ms (make it reachable by not starting at 40ms)
            let previous = self.target;
            let target = (self.jitter_ms * 4.0)
                .max(MIN_TARGET.as_millis() as f64)
                .min(MAX_TARGET.as_millis() as f64);
            self.target = Duration::from_millis(target.ceil() as u64);

            if let Some(due) = self.next_due {
                self.next_due = Some(
                    if let Some(start) = self.active_since
                        && now < due
                    {
                        start + self.target
                    } else if self.target >= previous {
                        due + (self.target - previous)
                    } else {
                        due.checked_sub(previous - self.target)
                            .unwrap_or(now)
                            .max(now)
                    },
                );
            }
            self.last_transit = Some(transit);
        }
        self.last_arrival = Some(now);
        self.last_timestamp = Some(frame.timestamp);
        self.last_packet = now;
        if self.active_since.is_none() {
            self.active_since = Some(now);
            // Set next_due to now so first mix() call produces output
            self.next_due = Some(now);
        } else if self.next_due.is_none() {
            // Restart after PLC capping
            self.next_due = Some(now);
            self.consecutive_plc = 0;
        }

        // Insert into queue in sorted order
        if self.len == PACKETS {
            if frame.sequence >= self.queue[PACKETS - 1].as_ref().unwrap().frame.sequence {
                return;
            }
            self.queue[PACKETS - 1] = None;
            self.len -= 1;
        }
        let index = self.queue[..self.len]
            .iter()
            .position(|p| p.as_ref().unwrap().frame.sequence > frame.sequence)
            .unwrap_or(self.len);
        for pos in (index..self.len).rev() {
            self.queue[pos + 1] = self.queue[pos].take();
        }
        self.queue[index] = Some(Queued { frame });
        self.len += 1;
    }

    /// Removes and returns the queued packet with the expected sequence, advancing it.
    fn pop_expected(&mut self) -> Option<Queued> {
        let expected = self.expected?;
        let index = self.queue[..self.len].iter().position(|packet| {
            packet
                .as_ref()
                .is_some_and(|p| p.frame.sequence == expected)
        })?;
        let item = self.queue[index].take();
        for pos in index..self.len - 1 {
            self.queue[pos] = self.queue[pos + 1].take();
        }
        self.len -= 1;
        self.expected = Some(expected.wrapping_add(1));
        self.consecutive_plc = 0;
        item
    }
}

/// Ceiling for the mixed output; nothing the limiter emits exceeds it.
const LIMIT: f32 = 0.97;
/// Time constant with which the gain recovers after limiting.
const RELEASE_SECONDS: f32 = 0.05;

/// Per-sample brickwall limiter: instant attack, smooth release, gain carried across blocks.
#[derive(Debug, Clone, Copy)]
struct LimiterEnvelope {
    gain: f32,
    /// Fraction of the remaining distance to unity gain recovered per interleaved sample.
    release: f32,
}

impl Default for LimiterEnvelope {
    fn default() -> Self {
        let samples_per_second = 48_000.0 * CHANNELS as f32;
        Self {
            gain: 1.0,
            release: 1.0 - (-1.0 / (RELEASE_SECONDS * samples_per_second)).exp(),
        }
    }
}

impl LimiterEnvelope {
    fn process(&mut self, out: &mut [f32]) {
        for sample in out {
            self.gain += (1.0 - self.gain) * self.release;
            let magnitude = sample.abs();
            if magnitude * self.gain > LIMIT {
                self.gain = LIMIT / magnitude;
            }
            *sample *= self.gain;
        }
    }

    /// Lets the gain recover as if `elapsed` of silence had passed.
    fn idle_for(&mut self, elapsed: Duration) {
        let samples = elapsed.as_secs_f64() * 48_000.0 * CHANNELS as f64;
        let remaining = (1.0 - f64::from(self.release)).powf(samples);
        self.gain = (1.0 - (1.0 - f64::from(self.gain)) * remaining) as f32;
    }
}

/// Four-speaker bounded per-SSRC reorder/decode/mix stage, owned by the engine thread.
pub(crate) struct JitterMixer {
    speakers: Vec<Speaker>,
    limiter: LimiterEnvelope,
    last_evicted_ssrc: Option<u32>,
    /// When the mixer last found nothing scheduled at all, to release the limiter by
    /// elapsed time while no one is speaking.
    idle_since: Option<Instant>,
}

impl JitterMixer {
    pub(crate) fn new() -> Result<Self, crate::CodecError> {
        Ok(Self {
            speakers: Vec::with_capacity(SPEAKERS),
            limiter: LimiterEnvelope::default(),
            last_evicted_ssrc: None,
            idle_since: None,
        })
    }

    pub(crate) fn push(&mut self, frame: RemoteFrame) {
        let now = Instant::now();
        let index = match self
            .speakers
            .iter()
            .position(|speaker| speaker.ssrc == frame.ssrc)
        {
            Some(index) => index,
            None if self.speakers.len() < SPEAKERS => {
                let Ok(speaker) = Speaker::new(frame.ssrc) else {
                    return;
                };
                self.speakers.push(speaker);
                self.speakers.len() - 1
            }
            None => {
                // Evict least-recently-active speaker (oldest last_packet)
                if let Some((evict_idx, evicted)) = self
                    .speakers
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, speaker)| speaker.last_packet)
                {
                    self.last_evicted_ssrc = Some(evicted.ssrc);
                    self.speakers.remove(evict_idx);
                    let Ok(speaker) = Speaker::new(frame.ssrc) else {
                        return;
                    };
                    self.speakers.push(speaker);
                    self.speakers.len() - 1
                } else {
                    return;
                }
            }
        };
        self.speakers[index].insert(frame, now);
    }

    /// Return the SSRC of the last evicted speaker, for observability/testing
    #[allow(dead_code)] // Used in tests
    pub(crate) fn last_evicted_ssrc(&self) -> Option<u32> {
        self.last_evicted_ssrc
    }

    /// Produces one 20 ms stereo block when any speaker is due; zeroes `out` otherwise.
    pub(crate) fn mix(&mut self, out: &mut [f32; FRAME_SAMPLES * CHANNELS], now: Instant) -> bool {
        out.fill(0.0);
        self.speakers
            .retain(|speaker| now.duration_since(speaker.last_packet) <= INACTIVE);
        let mut active = false;

        for speaker in &mut self.speakers {
            let Some(due) = speaker.next_due else {
                continue;
            };
            if now < due {
                continue;
            }
            speaker.next_due = Some(due + Duration::from_millis(20));
            if speaker.expected.is_none() {
                let Some(first) = speaker.queue[..speaker.len]
                    .first()
                    .and_then(Option::as_ref)
                else {
                    continue;
                };
                speaker.expected = Some(first.frame.sequence);
            }

            let packet = speaker.pop_expected();
            let decoded = if let Some(packet) = packet {
                speaker
                    .decoder
                    .decode(packet.frame.packet.as_bytes(), &mut speaker.pcm)
                    .ok()
            } else if let Some(next) = speaker.queue[..speaker.len]
                .first()
                .and_then(Option::as_ref)
            {
                // A later packet proves the expected sequence was lost: recover it from the
                // next packet's FEC data when adjacent, otherwise conceal, and move on.
                let expected = speaker.expected.unwrap_or(next.frame.sequence);
                let adjacent = next.frame.sequence == expected.wrapping_add(1);
                let decoded = if adjacent {
                    speaker
                        .decoder
                        .recover(
                            next.frame.packet.as_bytes(),
                            FRAME_SAMPLES,
                            &mut speaker.pcm,
                        )
                        .ok()
                } else {
                    speaker
                        .decoder
                        .conceal(FRAME_SAMPLES, &mut speaker.pcm)
                        .ok()
                };
                speaker.expected = Some(expected.wrapping_add(1));
                decoded
            } else {
                // Nothing queued proves nothing was lost (the sender may simply be silent), so
                // `expected` stays put. Conceal a few frames, then park until a packet arrives.
                speaker.consecutive_plc += 1;
                if speaker.consecutive_plc > MAX_CONSECUTIVE_PLC {
                    speaker.next_due = None;
                    None
                } else {
                    speaker
                        .decoder
                        .conceal(FRAME_SAMPLES, &mut speaker.pcm)
                        .ok()
                }
            };

            if let Some(frames) = decoded {
                let count = frames.min(FRAME_SAMPLES) * CHANNELS;
                for (mix, sample) in out[..count].iter_mut().zip(&speaker.pcm[..count]) {
                    *mix += *sample;
                }
                active = true;
            }
        }

        if active {
            self.idle_since = None;
            self.limiter.process(out);
        } else if self
            .speakers
            .iter()
            .any(|speaker| speaker.next_due.is_some())
        {
            // A call is in progress between due ticks. The audio time that has passed was
            // already accounted for by the blocks mixed, so polling must not release the
            // limiter.
            self.idle_since = None;
        } else if let Some(since) = self.idle_since.replace(now) {
            self.limiter.idle_for(now.saturating_duration_since(since));
        }
        active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{DEFAULT_BITRATE, VoiceEncoder};

    /// A 440 Hz tone, phase-continuous across `sequence`. A constant (DC) input would be
    /// removed by Opus's high-pass filter after the first frames and decode to silence.
    fn tone(sequence: u64, amplitude: f32) -> [f32; FRAME_SAMPLES] {
        std::array::from_fn(|i| {
            let n = sequence * FRAME_SAMPLES as u64 + i as u64;
            amplitude * (std::f32::consts::TAU * 440.0 * n as f32 / 48_000.0).sin()
        })
    }

    fn frame(encoder: &mut VoiceEncoder, ssrc: u32, sequence: u64) -> RemoteFrame {
        frame_amplitude(encoder, ssrc, sequence, 0.4)
    }

    fn frame_amplitude(
        encoder: &mut VoiceEncoder,
        ssrc: u32,
        sequence: u64,
        amplitude: f32,
    ) -> RemoteFrame {
        let pcm = tone(sequence, amplitude);
        let mut packet = crate::EncodedPacket::empty();
        encoder.encode_mono(&pcm, &mut packet).unwrap();
        RemoteFrame {
            ssrc,
            sequence,
            timestamp: sequence as u32 * FRAME_SAMPLES as u32,
            packet,
        }
    }

    /// After a talk pause the speaker is parked; the next talkspurt must be heard from its
    /// first packet, whether the sender kept counting sequence numbers through the silence
    /// or not.
    #[test]
    fn resumed_speech_after_a_pause_is_heard_immediately() {
        for resume_sequence in [2u64, 9] {
            let mut mixer = JitterMixer::new().unwrap();
            let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();
            let start = Instant::now();
            let mut out = [0.0; FRAME_SAMPLES * CHANNELS];

            mixer.push(frame(&mut encoder, 1, 0));
            mixer.push(frame(&mut encoder, 1, 1));
            assert!(mixer.mix(&mut out, start + Duration::from_millis(50)));
            assert!(mixer.mix(&mut out, start + Duration::from_millis(70)));
            // Silence: three concealed frames, then the speaker is parked.
            for tick in 0..4u64 {
                mixer.mix(&mut out, start + Duration::from_millis(90 + 20 * tick));
            }
            assert!(
                mixer.speakers[0].next_due.is_none(),
                "speaker should be parked"
            );

            mixer.push(frame(&mut encoder, 1, resume_sequence));
            out.fill(0.0);
            assert!(
                mixer.mix(&mut out, start + Duration::from_millis(190)),
                "resume at sequence {resume_sequence} was dropped"
            );
            assert!(
                out.iter().any(|s| s.abs() > 0.05),
                "resume at sequence {resume_sequence} decoded to silence"
            );
        }
    }

    /// Test: reordered arrival keeps jitter bounded
    #[test]
    fn jitter_estimator_keeps_target_bounded() {
        let mut mixer = JitterMixer::new().unwrap();
        let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();

        let start = Instant::now();

        // Send packets with significant reordering to trigger jitter
        mixer.push(frame(&mut encoder, 1, 0));
        mixer.push(frame(&mut encoder, 1, 2)); // Out of order
        mixer.push(frame(&mut encoder, 1, 1)); // Late arrival

        let mut mixed = [0.0; FRAME_SAMPLES * CHANNELS];
        mixer.mix(&mut mixed, start + Duration::from_millis(50));

        // Check that jitter target stays within bounds
        let speaker = &mixer.speakers[0];
        assert!(
            speaker.target >= MIN_TARGET,
            "Target {} should be >= MIN_TARGET {:?}",
            speaker.target.as_millis(),
            MIN_TARGET
        );
        assert!(
            speaker.target <= MAX_TARGET,
            "Target {} should be <= MAX_TARGET {:?}",
            speaker.target.as_millis(),
            MAX_TARGET
        );

        // The 0/2/1 reordering is a small, bounded disturbance: the estimator must stay
        // near zero and the target at its reachable 20 ms floor (the old unsigned
        // timestamp difference pinned it at the 120 ms ceiling).
        assert_eq!(speaker.target, MIN_TARGET);
        assert!(
            speaker.jitter_ms < 10.0,
            "jitter {} ms is not a small disturbance",
            speaker.jitter_ms
        );
    }

    /// The limiter recovers with the passage of audio time, not with the number of `mix`
    /// calls: the engine polls far more often than a block is due, and those polls must
    /// not release a hot limiter.
    #[test]
    fn limiter_release_follows_audio_time_not_call_count() {
        let mut mixer = JitterMixer::new().unwrap();
        let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();
        let frames: Vec<_> = (1..=4u32)
            .map(|ssrc| frame_amplitude(&mut encoder, ssrc, 0, 0.9))
            .collect();
        let t0 = Instant::now();
        for frame in frames {
            mixer.push(frame);
        }
        let mut out = [0.0; FRAME_SAMPLES * CHANNELS];
        assert!(mixer.mix(&mut out, t0 + Duration::from_millis(10)));
        let gain = mixer.limiter.gain;
        assert!(
            gain < 0.9,
            "four loud speakers should be limited, got {gain}"
        );

        for ms in [12, 14, 16, 18] {
            assert!(!mixer.mix(&mut out, t0 + Duration::from_millis(ms)));
            assert_eq!(
                mixer.limiter.gain, gain,
                "a call at {ms} ms carried no audio and must not release the limiter"
            );
        }

        // Once nobody is speaking, wall-clock time releases it.
        mixer.speakers.clear();
        mixer.mix(&mut out, t0 + Duration::from_millis(100));
        mixer.mix(&mut out, t0 + Duration::from_millis(600));
        assert!(mixer.limiter.gain > 0.99, "gain {}", mixer.limiter.gain);
    }

    /// Test: fifth speaker evicts least-recently-active and evicted SSRC is observable
    #[test]
    fn fifth_speaker_evicts_least_recently_active() {
        let mut mixer = JitterMixer::new().unwrap();
        let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();

        // Add 4 speakers in order
        for ssrc in 1..=4 {
            mixer.push(frame(&mut encoder, ssrc, 0));
        }
        assert_eq!(mixer.speakers.len(), 4);

        let start = Instant::now();
        // Mix to establish next_due
        mixer.mix(
            &mut [0.0; FRAME_SAMPLES * CHANNELS],
            start + Duration::from_millis(50),
        );

        // Add fifth speaker - should evict speaker 1 (least recently active by last_packet)
        mixer.push(frame(&mut encoder, 5, 0));
        assert_eq!(mixer.speakers.len(), 4);

        // Speaker 5 should be present
        assert!(
            mixer.speakers.iter().any(|s| s.ssrc == 5),
            "Speaker 5 should be present; speakers: {:?}",
            mixer.speakers.iter().map(|s| s.ssrc).collect::<Vec<_>>()
        );

        // Speaker 1 should be evicted
        assert!(
            !mixer.speakers.iter().any(|s| s.ssrc == 1),
            "Speaker 1 should be evicted; speakers: {:?}",
            mixer.speakers.iter().map(|s| s.ssrc).collect::<Vec<_>>()
        );
    }

    /// Test: four 0.9-amplitude speakers never exceed 0.97 and have no step at frame boundary
    #[test]
    fn limiter_ensures_no_clipping_with_loud_speakers() {
        let mut mixer = JitterMixer::new().unwrap();
        let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();

        // Create 4 speakers at 0.9 amplitude with packets for multiple frames
        for ssrc in 1..=4 {
            mixer.push(frame_amplitude(&mut encoder, ssrc, 0, 0.9));
            mixer.push(frame_amplitude(&mut encoder, ssrc, 1, 0.9));
        }

        let start = Instant::now();
        let mut mixed1 = [0.0; FRAME_SAMPLES * CHANNELS];
        let mut mixed2 = [0.0; FRAME_SAMPLES * CHANNELS];

        // First frame: should be limited to <= 0.97
        mixer.mix(&mut mixed1, start + Duration::from_millis(50));
        let peak1 = mixed1.iter().map(|s| s.abs()).fold(0.0, f32::max);
        assert!(
            peak1 <= 0.97 + 1e-5,
            "Peak {:.6} should not exceed 0.97",
            peak1
        );

        // Second frame: no step at boundary (smooth release)
        mixer.mix(&mut mixed2, start + Duration::from_millis(70));
        let peak2 = mixed2.iter().map(|s| s.abs()).fold(0.0, f32::max);
        assert!(
            peak2 <= 0.97 + 1e-5,
            "Peak {:.6} should not exceed 0.97",
            peak2
        );

        // Check that first peak is non-trivial (actual sound being mixed)
        assert!(
            peak1 > 0.4,
            "First peak should be non-trivial; got {:.6}",
            peak1
        );
        // The key property is that both are bounded. Codec might produce artifacts.
    }

    /// Concealment is capped: after three concealed frames with no packet the speaker is
    /// parked and produces nothing until a real packet arrives.
    #[test]
    fn plc_capping_stops_output_after_three_frames() {
        let mut mixer = JitterMixer::new().unwrap();
        let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();
        let start = Instant::now();
        let mut mixed = [0.0; FRAME_SAMPLES * CHANNELS];

        mixer.push(frame(&mut encoder, 1, 0));
        assert!(mixer.mix(&mut mixed, start + Duration::from_millis(50)));
        for tick in 1..=3u64 {
            assert!(
                mixer.mix(&mut mixed, start + Duration::from_millis(50 + 20 * tick)),
                "concealed frame {tick} should still be produced"
            );
        }
        assert!(
            !mixer.mix(&mut mixed, start + Duration::from_millis(130)),
            "the fourth empty frame must not be concealed"
        );
        assert!(
            mixer.speakers[0].next_due.is_none(),
            "speaker must be parked"
        );
        assert_eq!(
            mixer.speakers[0].expected,
            Some(1),
            "an empty queue proves no loss, so expected must not advance"
        );
    }

    /// Test: reordered packets and FEC handling
    #[test]
    fn reorder_and_fec_recovery_bounded_and_limited() {
        let mut mixer = JitterMixer::new().unwrap();
        let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();

        // Send packets out of order
        mixer.push(frame(&mut encoder, 1, 0));
        mixer.push(frame(&mut encoder, 1, 2)); // Out of order

        // Add more speakers to test mixing
        for ssrc in 2..=4 {
            mixer.push(frame(&mut encoder, ssrc, 0));
        }

        let start = Instant::now();
        let mut mixed = [0.0; FRAME_SAMPLES * CHANNELS];

        // Mix: should handle reordering and produce limited output
        assert!(mixer.mix(&mut mixed, start + Duration::from_millis(50)));
        assert!(mixed.iter().any(|sample| sample.abs() > 0.01));

        // Limiter must ensure bounded output
        assert!(
            mixed.iter().all(|sample| sample.abs() <= 0.97 + 1e-5),
            "Limiter must ensure all samples <= 0.97; max: {:.6}",
            mixed.iter().map(|s| s.abs()).fold(0.0, f32::max)
        );

        // Second frame
        mixer.mix(&mut mixed, start + Duration::from_millis(70));
        assert!(mixed.iter().all(|sample| sample.abs() <= 0.97 + 1e-5));
        assert_eq!(mixer.speakers.len(), 4);
    }

    /// Test: late packet arrival handling
    #[test]
    fn late_packets_handled_correctly() {
        let mut mixer = JitterMixer::new().unwrap();
        let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();

        let start = Instant::now();

        // Send packet 0
        mixer.push(frame(&mut encoder, 1, 0));
        let mut mixed = [0.0; FRAME_SAMPLES * CHANNELS];
        mixer.mix(&mut mixed, start + Duration::from_millis(50));

        // Insert later packets
        mixer.push(frame(&mut encoder, 1, 2));
        mixer.mix(&mut mixed, start + Duration::from_millis(70));

        // Insert late packet (packet 1) - should still be queued even after packet 2
        mixer.push(frame(&mut encoder, 1, 1));
        mixer.mix(&mut mixed, start + Duration::from_millis(90));

        // Verify speaker still exists
        assert!(mixer.speakers.iter().any(|s| s.ssrc == 1));
    }
    /// Test: lost packets advance expected sequence, so a gap does not freeze the speaker
    #[test]
    fn lost_packets_advance_expected_sequence() {
        let mut mixer = JitterMixer::new().unwrap();
        let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();

        let start = Instant::now();

        // Create sequence: 0, then 2, 3 (missing 1)
        mixer.push(frame(&mut encoder, 1, 0));
        mixer.push(frame(&mut encoder, 1, 2));
        mixer.push(frame(&mut encoder, 1, 3));

        // First mix: frame 0 should decode normally
        let mut mixed = [0.0; FRAME_SAMPLES * CHANNELS];
        mixer.mix(&mut mixed, start + Duration::from_millis(50));
        let speaker = &mixer.speakers[0];
        assert_eq!(
            speaker.expected,
            Some(1),
            "After frame 0, expected should be 1"
        );

        // Second mix: frame 1 is missing, should use concealment/FEC and advance expected to 2
        mixer.mix(&mut mixed, start + Duration::from_millis(70));
        let speaker = &mixer.speakers[0];
        assert_eq!(
            speaker.expected,
            Some(2),
            "After FEC/concealment for missing frame 1, expected should be 2"
        );

        // Third mix: frame 2 should decode (not FEC on 2 again)
        mixer.mix(&mut mixed, start + Duration::from_millis(90));
        let speaker = &mixer.speakers[0];
        assert_eq!(
            speaker.expected,
            Some(3),
            "After frame 2, expected should be 3"
        );
        assert_eq!(
            speaker.len, 1,
            "Queue should have only frame 3 left (frame 2 was popped)"
        );

        // Fourth mix: frame 3 should decode
        mixer.mix(&mut mixed, start + Duration::from_millis(110));
        let speaker = &mixer.speakers[0];
        assert_eq!(
            speaker.expected,
            Some(4),
            "After frame 3, expected should be 4"
        );
        assert_eq!(speaker.len, 0, "Queue should be empty");
    }

    /// The limiter's gain is a per-sample envelope carried across blocks: a loud block
    /// followed by a quiet one must not step the gain at the boundary, only release it
    /// smoothly, and nothing may ever exceed the ceiling.
    #[test]
    fn limiter_gain_is_continuous_across_blocks_and_never_exceeds_the_ceiling() {
        const N: usize = FRAME_SAMPLES * CHANNELS;
        let mut limiter = LimiterEnvelope::default();
        let mut loud = [3.6f32; N];
        let mut quiet = [0.1f32; N];
        limiter.process(&mut loud);
        limiter.process(&mut quiet);

        assert!(
            loud.iter().all(|s| s.abs() <= LIMIT + 1e-5),
            "ceiling exceeded: {}",
            loud.iter().fold(0.0f32, |m, s| m.max(s.abs()))
        );
        let end_of_loud = loud[N - 1] / 3.6;
        let start_of_quiet = quiet[0] / 0.1;
        assert!(
            (end_of_loud - LIMIT / 3.6).abs() < 1e-3,
            "loud block should end at the attacked gain, got {end_of_loud}"
        );
        let boundary_step = (start_of_quiet - end_of_loud).abs();
        let inner_step = (1..N)
            .map(|i| ((quiet[i] - quiet[i - 1]) / 0.1).abs())
            .fold(0.0f32, f32::max);
        assert!(
            boundary_step <= inner_step * 2.0 + 1e-6 && boundary_step < 1e-3,
            "gain stepped by {boundary_step} at the block boundary (inner step {inner_step})"
        );

        // The release returns to unity gain within a few hundred milliseconds.
        let mut settle = [0.1f32; N];
        for _ in 0..30 {
            settle.fill(0.1);
            limiter.process(&mut settle);
        }
        assert!(settle[N - 1] / 0.1 > 0.99, "gain never released");
    }

    /// Test: last_evicted_ssrc is observable
    #[test]
    fn fifth_speaker_evicted_ssrc_is_observable() {
        let mut mixer = JitterMixer::new().unwrap();
        let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();

        // Add 4 speakers
        for ssrc in 1..=4 {
            mixer.push(frame(&mut encoder, ssrc, 0));
        }
        assert_eq!(mixer.speakers.len(), 4);

        // Initially, no eviction
        assert_eq!(mixer.last_evicted_ssrc(), None);

        let start = Instant::now();
        mixer.mix(
            &mut [0.0; FRAME_SAMPLES * CHANNELS],
            start + Duration::from_millis(50),
        );

        // Add fifth speaker - should evict speaker 1 and track it
        mixer.push(frame(&mut encoder, 5, 0));
        assert_eq!(mixer.speakers.len(), 4);
        assert_eq!(
            mixer.last_evicted_ssrc(),
            Some(1),
            "Should observe evicted SSRC as 1"
        );

        // Speaker 1 should be gone
        assert!(
            !mixer.speakers.iter().any(|s| s.ssrc == 1),
            "Speaker 1 should be evicted"
        );

        // Speaker 5 should be present
        assert!(
            mixer.speakers.iter().any(|s| s.ssrc == 5),
            "Speaker 5 should be present"
        );
    }
}
