//! Scheduling of the engine's own threads: real-time elevation and the owner's tick.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use fastcord_platform::realtime::{RealtimeGuard, promote_audio_thread};

/// Engine threads (owner, denoise worker) currently running with real-time scheduling.
static ELEVATED: AtomicU8 = AtomicU8::new(0);

pub(crate) fn elevated_threads() -> u8 {
    ELEVATED.load(Ordering::Relaxed)
}

/// Real-time scheduling for the calling thread, for work done every 10 ms; failure is
/// not an error and is visible as a missing count in [`elevated_threads`].
pub(crate) struct Elevated(RealtimeGuard);

impl Elevated {
    pub(crate) fn request() -> Self {
        let guard = promote_audio_thread(480, 48_000);
        if guard.is_realtime() {
            ELEVATED.fetch_add(1, Ordering::Relaxed);
        }
        Self(guard)
    }
}

impl Drop for Elevated {
    fn drop(&mut self) {
        if self.0.is_realtime() {
            ELEVATED.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// A wake-up that ran later than this behind its deadline counts as a late tick.
const LATE: Duration = Duration::from_millis(10);

/// Fixed-rate tick for the owner thread. It sleeps with `thread::sleep`, which on Windows
/// uses a high-resolution waitable timer, so no global `timeBeginPeriod` is needed, where
/// `recv_timeout` waits at the (15.6 ms, or throttled) system tick. Deadlines advance
/// from the previous deadline, so wake-up jitter does not accumulate; a thread that fell
/// behind skips the missed ticks instead of running them back to back.
pub(crate) struct Ticker {
    period: Duration,
    next: Instant,
}

impl Ticker {
    pub(crate) fn new(period: Duration) -> Self {
        Self {
            period,
            next: Instant::now() + period,
        }
    }

    /// Sleeps to the next deadline. False once `stop` fired or its sender is gone.
    pub(crate) fn wait(&mut self, stop: &Receiver<()>, late_ticks: &AtomicU64) -> bool {
        if let Some(remaining) = self.next.checked_duration_since(Instant::now()) {
            std::thread::sleep(remaining);
        }
        let now = Instant::now();
        if now.saturating_duration_since(self.next) > LATE {
            late_ticks.fetch_add(1, Ordering::Relaxed);
        }
        self.next += self.period;
        if self.next <= now {
            self.next = now + self.period;
        }
        matches!(stop.try_recv(), Err(TryRecvError::Empty))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_keep_a_fixed_rate_stop_promptly_and_count_a_stalled_thread() {
        let (stop_tx, stop) = std::sync::mpsc::channel();
        let late = AtomicU64::new(0);
        let mut ticker = Ticker::new(Duration::from_millis(5));
        let start = Instant::now();
        for _ in 0..20 {
            assert!(ticker.wait(&stop, &late));
        }
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(98), "{elapsed:?}");
        assert_eq!(late.load(Ordering::Relaxed), 0);
        // A stall of 50 ms is one late tick, and the missed ticks are skipped.
        std::thread::sleep(Duration::from_millis(50));
        assert!(ticker.wait(&stop, &late));
        assert_eq!(late.load(Ordering::Relaxed), 1);
        let resumed = Instant::now();
        assert!(ticker.wait(&stop, &late));
        assert!(resumed.elapsed() >= Duration::from_millis(3));
        drop(stop_tx);
        assert!(!ticker.wait(&stop, &late));
    }

    /// Manual measurement: tick lateness with 24 busy threads, normal against elevated.
    /// `cargo test -p fastcord-audio --release --locked measure_tick -- --ignored --nocapture`
    #[test]
    #[ignore = "timing measurement"]
    fn measure_tick_lateness_under_load() {
        let stop_load = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let busy: Vec<_> = (0..24)
            .map(|_| {
                let stop = stop_load.clone();
                std::thread::spawn(move || {
                    let mut x = 1u64;
                    while !stop.load(Ordering::Relaxed) {
                        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    }
                    std::hint::black_box(x);
                })
            })
            .collect();
        for elevated in [false, true] {
            let (periods, late) = std::thread::spawn(move || {
                let _guard = elevated.then(Elevated::request);
                let (_keep, stop) = std::sync::mpsc::channel();
                let late = AtomicU64::new(0);
                let mut ticker = Ticker::new(Duration::from_millis(5));
                let mut last = Instant::now();
                let mut periods = Vec::new();
                for _ in 0..600 {
                    ticker.wait(&stop, &late);
                    periods.push(last.elapsed().as_secs_f64() * 1e3);
                    last = Instant::now();
                }
                periods.sort_by(f64::total_cmp);
                (periods, late.load(Ordering::Relaxed))
            })
            .join()
            .unwrap();
            println!(
                "elevated={elevated}: period p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms, late ticks {late}",
                periods[300], periods[570], periods[594], periods[599]
            );
        }
        stop_load.store(true, Ordering::Relaxed);
        for handle in busy {
            handle.join().unwrap();
        }
    }
}
