//! Real-time scheduling for the audio engine's own threads (the device callbacks are
//! promoted by cpal's `realtime` feature). Windows: MMCSS ("Audio" task, reverted on
//! drop); macOS: Mach time-constraint policy; Linux: `SCHED_RR` through
//! `sched_setscheduler` (no D-Bus/rtkit, so it needs an `RLIMIT_RTPRIO` allowance).
//! Never fatal: the guard says whether the thread was elevated, and an unelevated
//! thread simply keeps running at normal priority.

use audio_thread_priority::{RtPriorityHandle, demote_current_thread_from_real_time};

/// Keeps the calling thread elevated; dropping it on that same thread restores the
/// previous scheduling.
pub struct RealtimeGuard(Option<RtPriorityHandle>);

impl RealtimeGuard {
    /// Whether the thread really runs with real-time scheduling.
    pub fn is_realtime(&self) -> bool {
        self.0.is_some()
    }
}

/// Asks the OS for real-time scheduling of the calling thread, for work done once per
/// `period_frames` frames at `sample_rate` Hz. Must be dropped on the same thread.
pub fn promote_audio_thread(period_frames: u32, sample_rate: u32) -> RealtimeGuard {
    RealtimeGuard(
        audio_thread_priority::promote_current_thread_to_real_time(period_frames, sample_rate).ok(),
    )
}

impl Drop for RealtimeGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = demote_current_thread_from_real_time(handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Elevation may be refused (CI, containers); neither outcome may fail, and the
    /// guard must restore the thread when dropped.
    #[test]
    fn promotion_is_best_effort_and_reverts_on_drop() {
        std::thread::spawn(|| {
            for _ in 0..2 {
                let guard = promote_audio_thread(480, 48_000);
                println!("realtime granted: {}", guard.is_realtime());
            }
        })
        .join()
        .unwrap();
    }
}
