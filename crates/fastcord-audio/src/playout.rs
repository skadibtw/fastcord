//! Output-clock observations. The callback writes atomics only; the owner
//! publishes a bounded set of RTP anchors outside the real-time callback.
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

pub const MAX_PLAYOUT_SOURCES: usize = 16;
const FRESH: Duration = Duration::from_millis(250);
#[derive(Clone, Copy, Debug)]
pub struct PlayoutPoint {
    pub ssrc: u32,
    pub rtp_timestamp: u32,
    /// When this sample reaches the device, including reported output latency.
    pub at: Instant,
    pub observed_at: Instant,
}
pub struct PlayoutClock {
    points: Mutex<Vec<PlayoutPoint>>,
}
impl Default for PlayoutClock {
    fn default() -> Self {
        Self {
            points: Mutex::new(Vec::with_capacity(MAX_PLAYOUT_SOURCES)),
        }
    }
}
impl PlayoutClock {
    pub fn point(&self, ssrc: u32) -> Option<PlayoutPoint> {
        self.points
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|p| p.ssrc == ssrc && p.observed_at.elapsed() <= FRESH)
            .copied()
    }
    pub(crate) fn publish(&self, points: &[PlayoutPoint]) {
        let mut target = self.points.lock().unwrap_or_else(|p| p.into_inner());
        target.clear();
        target.extend_from_slice(&points[..points.len().min(MAX_PLAYOUT_SOURCES)]);
    }
    pub(crate) fn clear(&self) {
        self.points
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }
}

pub(crate) struct OutputPosition {
    epoch: Instant,
    rate: u32,
    channels: usize,
    sequence: AtomicU64,
    read: AtomicU64,
    callback_us: AtomicU64,
    delay_us: AtomicU64,
    frames: AtomicU64,
}
#[derive(Clone, Copy)]
pub(crate) struct OutputObservation {
    pub read: usize,
    pub at: Instant,
    pub observed_at: Instant,
}
impl OutputPosition {
    pub(crate) fn new(rate: u32, channels: usize) -> Self {
        Self {
            epoch: Instant::now(),
            rate,
            channels,
            sequence: AtomicU64::new(0),
            read: AtomicU64::new(0),
            callback_us: AtomicU64::new(0),
            delay_us: AtomicU64::new(0),
            frames: AtomicU64::new(0),
        }
    }
    pub(crate) fn record(
        &self,
        read: usize,
        samples: usize,
        now: Instant,
        latency: Option<Duration>,
    ) {
        let Some(latency) = latency.filter(|d| *d <= Duration::from_millis(500)) else {
            return;
        };
        if samples == 0 {
            return;
        }
        self.sequence.fetch_add(1, Ordering::AcqRel);
        self.read.store(read as u64, Ordering::Relaxed);
        self.callback_us.store(
            now.saturating_duration_since(self.epoch)
                .as_micros()
                .min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        self.delay_us
            .store(latency.as_micros() as u64, Ordering::Relaxed);
        self.frames
            .store((samples / self.channels) as u64, Ordering::Relaxed);
        self.sequence.fetch_add(1, Ordering::Release);
    }
    pub(crate) fn snapshot(&self) -> Option<OutputObservation> {
        for _ in 0..3 {
            let seq = self.sequence.load(Ordering::Acquire);
            if seq == 0 || seq & 1 != 0 {
                continue;
            }
            let read = self.read.load(Ordering::Relaxed) as usize;
            let callback = self.callback_us.load(Ordering::Relaxed);
            let delay = self.delay_us.load(Ordering::Relaxed);
            let frames = self.frames.load(Ordering::Relaxed);
            std::sync::atomic::fence(Ordering::Acquire);
            if self.sequence.load(Ordering::Acquire) != seq {
                continue;
            }
            let observed_at = self.epoch + Duration::from_micros(callback);
            if observed_at.elapsed() > FRESH {
                return None;
            }
            return Some(OutputObservation {
                read,
                observed_at,
                at: observed_at
                    + Duration::from_micros(delay)
                    + Duration::from_secs_f64(frames as f64 / f64::from(self.rate)),
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn atomic_snapshot_never_combines_two_callbacks() {
        let clock = Arc::new(OutputPosition::new(48_000, 2));
        let writer = clock.clone();
        let thread = std::thread::spawn(move || {
            for n in 1..20_000 {
                writer.record(n, 960, writer.epoch, Some(Duration::from_micros(n as u64)));
            }
        });
        while !thread.is_finished() {
            if let Some(p) = clock.snapshot() {
                assert_eq!(
                    p.at.duration_since(p.observed_at),
                    Duration::from_micros(p.read as u64) + Duration::from_millis(10)
                );
            }
        }
        thread.join().unwrap();
    }
    #[test]
    fn silence_and_unknown_latency_do_not_advance_the_audio_clock() {
        let clock = OutputPosition::new(48_000, 2);
        clock.record(960, 960, clock.epoch, None);
        assert!(clock.snapshot().is_none());
        clock.record(960, 960, clock.epoch, Some(Duration::from_millis(7)));
        let first = clock.snapshot().unwrap();
        clock.record(
            960,
            0,
            clock.epoch + Duration::from_millis(20),
            Some(Duration::ZERO),
        );
        assert_eq!(clock.snapshot().unwrap().at, first.at);
        clock.callback_us.store(0, Ordering::Relaxed);
        let mut stale = OutputPosition::new(48_000, 2);
        stale.epoch -= FRESH + Duration::from_millis(1);
        stale.record(960, 960, stale.epoch, Some(Duration::ZERO));
        assert!(stale.snapshot().is_none());
    }
    #[test]
    fn source_clock_is_bounded_and_expires() {
        let clock = PlayoutClock::default();
        let now = Instant::now();
        let points: Vec<_> = (0..32)
            .map(|ssrc| PlayoutPoint {
                ssrc,
                rtp_timestamp: 0,
                at: now,
                observed_at: now,
            })
            .collect();
        clock.publish(&points);
        assert!(clock.point(15).is_some());
        assert!(clock.point(16).is_none());
        clock.publish(&[PlayoutPoint {
            ssrc: 1,
            observed_at: now - FRESH - Duration::from_millis(1),
            ..points[0]
        }]);
        assert!(clock.point(1).is_none());
        clock.clear();
        assert!(clock.points.lock().unwrap().is_empty());
    }
}
