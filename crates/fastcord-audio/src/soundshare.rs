//! Stream-audio send path (SPEC §8.7, milestone 35): stereo PCM captured from
//! an application or the system is encoded as 20 ms Opus music frames.
//!
//! The capture thread owns a [`SoundShareInput`], whose [`push`] writes into a
//! preallocated, 200 ms bounded ring without locking or allocating (the oldest
//! samples are dropped when the encoder falls behind, so latency never grows).
//! One encoder thread drains whole 20 ms frames and hands them to the sink; it
//! never denoises, gates the microphone, or touches the call's engine. A
//! stretch of digital silence ends the "speech" burst (the sink gets
//! [`ShareFrame::End`]) so an idle share sends nothing; sound resumes it.
//! Dropping the [`SoundShare`] stops and joins the thread.
//!
//! [`push`]: SoundShareInput::push

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::codec::{
    CHANNELS, CodecError, EncodedPacket, FRAME_SAMPLES, MusicEncoder, SOUNDSHARE_BITRATE,
};
use crate::ring::{RingConsumer, RingProducer, sample_ring};

/// Most audio buffered between capture and encoder.
const RING_FRAMES: usize = 9_600;
const FRAME_VALUES: usize = FRAME_SAMPLES * CHANNELS;
/// Peak below this (about -80 dBFS) counts as silence.
const SILENT_PEAK: f32 = 1.0e-4;
/// Silence this long (500 ms) after sound ends the burst.
const HANGOVER_FRAMES: u32 = 25;
/// A capture that delivers nothing for this long ends the burst.
const STALL: Duration = Duration::from_millis(200);
const POLL: Duration = Duration::from_millis(5);

/// What the encoder thread hands to the connection.
#[derive(Debug)]
pub enum ShareFrame {
    /// The next 20 ms Opus packet (the encoder thread may allocate).
    Audio(Vec<u8>),
    /// The audio went quiet: end the burst (speaking off).
    End,
}

#[derive(Default)]
struct Counters {
    sent: AtomicU64,
    dropped_samples: AtomicU64,
}

/// Counters for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SoundShareStats {
    pub frames_sent: u64,
    /// Captured samples discarded because the encoder fell behind.
    pub dropped_samples: u64,
}

/// Decides, per 20 ms frame, whether it is sent and whether the burst ended.
#[derive(Default)]
struct Gate {
    quiet_frames: u32,
    speaking: bool,
}

impl Gate {
    /// `(send, end)` for a frame whose loudest sample is `peak`.
    fn frame(&mut self, peak: f32) -> (bool, bool) {
        if peak >= SILENT_PEAK {
            self.quiet_frames = 0;
            self.speaking = true;
            return (true, false);
        }
        if !self.speaking {
            return (false, false);
        }
        self.quiet_frames += 1;
        if self.quiet_frames >= HANGOVER_FRAMES {
            self.speaking = false;
            return (false, true);
        }
        (true, false)
    }

    /// The capture stopped delivering: end the burst if one is open.
    fn stalled(&mut self) -> bool {
        std::mem::take(&mut self.speaking)
    }
}

/// The capture thread's half: real-time safe.
pub struct SoundShareInput {
    producer: RingProducer,
    counters: Arc<Counters>,
}

impl SoundShareInput {
    /// Queues interleaved stereo 48 kHz samples (whole frames). Never blocks,
    /// locks or allocates.
    pub fn push(&mut self, samples: &[f32]) {
        let whole = samples.len() / CHANNELS * CHANNELS;
        let dropped = self
            .producer
            .push_overwrite(samples[..whole].iter().copied());
        if dropped > 0 {
            self.counters
                .dropped_samples
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }
}

/// The encoder thread. Dropping it stops and joins the thread.
pub struct SoundShare {
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    thread: Option<JoinHandle<()>>,
}

impl SoundShare {
    pub fn stats(&self) -> SoundShareStats {
        SoundShareStats {
            frames_sent: self.counters.sent.load(Ordering::Relaxed),
            dropped_samples: self.counters.dropped_samples.load(Ordering::Relaxed),
        }
    }
}

impl Drop for SoundShare {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts the encoder thread; `sink` runs on it.
pub fn start(
    sink: impl FnMut(ShareFrame) + Send + 'static,
) -> Result<(SoundShare, SoundShareInput), CodecError> {
    let encoder = MusicEncoder::new(SOUNDSHARE_BITRATE)?;
    let (producer, consumer) = sample_ring(CHANNELS, RING_FRAMES);
    let counters = Arc::new(Counters::default());
    let stop = Arc::new(AtomicBool::new(false));
    let thread = thread::Builder::new()
        .name("fastcord-soundshare".to_owned())
        .spawn({
            let stop = Arc::clone(&stop);
            let counters = Arc::clone(&counters);
            move || run(encoder, consumer, sink, &stop, &counters)
        })
        .map_err(|_| CodecError::EmptyPacket)?;
    Ok((
        SoundShare {
            stop,
            counters: Arc::clone(&counters),
            thread: Some(thread),
        },
        SoundShareInput { producer, counters },
    ))
}

fn run(
    mut encoder: MusicEncoder,
    mut ring: RingConsumer,
    mut sink: impl FnMut(ShareFrame),
    stop: &AtomicBool,
    counters: &Counters,
) {
    let mut pcm = [0f32; FRAME_VALUES];
    let mut gate = Gate::default();
    let mut last_data = Instant::now();
    while !stop.load(Ordering::Acquire) {
        if ring.len() < FRAME_VALUES {
            if last_data.elapsed() >= STALL && gate.stalled() {
                sink(ShareFrame::End);
            }
            thread::sleep(POLL);
            continue;
        }
        last_data = Instant::now();
        ring.pop(&mut pcm);
        let peak = pcm.iter().fold(0f32, |peak, sample| peak.max(sample.abs()));
        let (send, end) = gate.frame(peak);
        if send {
            let mut packet = EncodedPacket::empty();
            if encoder.encode_stereo(&pcm, &mut packet).is_ok() {
                counters.sent.fetch_add(1, Ordering::Relaxed);
                sink(ShareFrame::Audio(packet.as_bytes().to_vec()));
            }
        }
        if end {
            sink(ShareFrame::End);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn tone(frames: usize, amplitude: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let v = amplitude * (n as f32 * 0.05).sin();
                [v, -v]
            })
            .collect()
    }

    fn collect_until(
        events: &Arc<Mutex<Vec<bool>>>,
        wanted: impl Fn(&[bool]) -> bool,
    ) -> Vec<bool> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let seen = events.lock().unwrap().clone();
            if wanted(&seen) || Instant::now() > deadline {
                return seen;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_gate_sends_sound_holds_through_a_hangover_then_ends_once() {
        let mut gate = Gate::default();
        assert_eq!(
            gate.frame(0.0),
            (false, false),
            "idle silence sends nothing"
        );
        assert_eq!(gate.frame(0.5), (true, false));
        for _ in 0..HANGOVER_FRAMES - 1 {
            assert_eq!(gate.frame(0.0), (true, false));
        }
        assert_eq!(gate.frame(0.0), (false, true));
        assert_eq!(gate.frame(0.0), (false, false), "the end is announced once");
        assert!(!gate.stalled());
        gate.frame(0.5);
        assert!(gate.stalled());
        assert!(!gate.stalled());
    }

    #[test]
    fn synthetic_stereo_becomes_20_ms_opus_packets_then_ends() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&events);
        let (share, mut input) = start(move |frame| match frame {
            ShareFrame::Audio(bytes) => {
                // Real stereo music packets, one 20 ms frame each.
                let bytes = &bytes[..];
                assert!(!bytes.is_empty());
                assert_eq!(
                    opus2::packet::get_nb_channels(bytes).unwrap(),
                    opus2::Channels::Stereo
                );
                assert_eq!(opus2::packet::get_nb_samples(bytes, 48_000).unwrap(), 960);
                seen.lock().unwrap().push(true);
            }
            ShareFrame::End => seen.lock().unwrap().push(false),
        })
        .unwrap();
        // 10 frames of sound, then enough silence to end the burst, fed at
        // about real time so the bounded ring never overflows.
        let mut feed = |samples: &[f32]| {
            for frame in samples.chunks(FRAME_VALUES) {
                input.push(frame);
                thread::sleep(Duration::from_millis(2));
            }
        };
        feed(&tone(FRAME_SAMPLES * 10, 0.5));
        feed(&vec![0.0; FRAME_VALUES * (HANGOVER_FRAMES as usize + 2)]);
        let events = collect_until(&events, |seen| seen.last() == Some(&false));
        let audio = events.iter().filter(|&&audio| audio).count();
        assert_eq!(audio, 10 + HANGOVER_FRAMES as usize - 1, "{audio}");
        assert_eq!(events.iter().filter(|&&audio| !audio).count(), 1);
        assert_eq!(share.stats().frames_sent, audio as u64);
    }

    #[test]
    fn a_stalled_capture_ends_the_burst_and_overflow_is_bounded() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&events);
        let (share, mut input) = start(move |frame| {
            seen.lock()
                .unwrap()
                .push(matches!(frame, ShareFrame::Audio(_)));
        })
        .unwrap();
        input.push(&tone(FRAME_SAMPLES * 3, 0.5));
        let events = collect_until(&events, |seen| seen.last() == Some(&false));
        assert_eq!(events, [true, true, true, false]);
        // More than the ring holds: the oldest samples are dropped, not queued.
        drop(share);
        let (share, mut input) = start(|_| {}).unwrap();
        input.push(&vec![0.1; RING_FRAMES * CHANNELS * 3]);
        assert!(share.stats().dropped_samples > 0);
    }

    #[test]
    fn dropping_the_share_joins_the_thread() {
        let (share, _input) = start(|_| {}).unwrap();
        drop(share);
    }
}
