//! Reconnect delays, heartbeat jitter, and the Gateway send ceiling.

use std::collections::VecDeque;
use std::time::Duration;

use rand_core::{OsRng, RngCore};
use tokio::time::Instant;

/// A source of uniform values in `[0, 1)`. Injected so schedules are testable.
pub(crate) trait JitterSource: Send + 'static {
    fn unit(&mut self) -> f64;
}

pub(crate) struct OsJitter;

impl JitterSource for OsJitter {
    fn unit(&mut self) -> f64 {
        // 53 random bits: exactly representable, never 1.0.
        (OsRng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Bounded exponential reconnect delay with jitter: the n-th consecutive
/// failure waits between half and all of `min(60 s, 1 s * 2^(n-1))`, so a
/// service outage is never hammered and clients do not reconnect in lockstep.
pub(crate) fn backoff_delay(consecutive_failures: u32, unit: f64) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(16);
    let ceiling = BACKOFF_BASE.saturating_mul(1 << exponent).min(BACKOFF_CAP);
    ceiling.mul_f64(0.5 + 0.5 * unit.clamp(0.0, 1.0))
}

/// Wait before re-identifying after a non-resumable Invalid Session: Discord
/// asks for a random 1 to 5 seconds.
pub(crate) fn invalid_session_delay(unit: f64) -> Duration {
    Duration::from_millis(1_000 + (4_000.0 * unit.clamp(0.0, 1.0)) as u64)
}

/// Documented ceiling: 120 commands per connection per 60 seconds.
pub(crate) const SEND_LIMIT: usize = 120;
const SEND_WINDOW: Duration = Duration::from_secs(60);

/// Sliding-window count of frames sent on one connection. Heartbeat, Identify,
/// and Resume are always sent (the connection cannot live without them) but
/// are counted, so optional traffic only ever gets what they leave over.
pub(crate) struct SendBudget {
    sent: VecDeque<Instant>,
}

impl SendBudget {
    pub(crate) fn new() -> Self {
        Self {
            sent: VecDeque::with_capacity(SEND_LIMIT),
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .sent
            .front()
            .is_some_and(|at| now.duration_since(*at) >= SEND_WINDOW)
        {
            self.sent.pop_front();
        }
    }

    /// Whether another optional frame fits in the current window.
    pub(crate) fn has_capacity(&mut self, now: Instant) -> bool {
        self.prune(now);
        self.sent.len() < SEND_LIMIT
    }

    /// Counts a frame that has been sent.
    pub(crate) fn record(&mut self, now: Instant) {
        self.prune(now);
        if self.sent.len() == SEND_LIMIT {
            self.sent.pop_front();
        }
        self.sent.push_back(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_with_jitter_and_is_capped() {
        // Lowest and highest jitter for the first failures: half to all of 1, 2, 4 s.
        assert_eq!(backoff_delay(1, 0.0), Duration::from_millis(500));
        assert_eq!(backoff_delay(1, 1.0), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, 0.0), Duration::from_secs(1));
        assert_eq!(backoff_delay(3, 1.0), Duration::from_secs(4));
        // Capped at 60 s no matter how long the outage lasts.
        for failures in [7, 8, 20, u32::MAX] {
            assert_eq!(backoff_delay(failures, 1.0), Duration::from_secs(60));
            assert_eq!(backoff_delay(failures, 0.0), Duration::from_secs(30));
        }
        // Monotonic in the failure count at fixed jitter.
        let mut previous = Duration::ZERO;
        for failures in 1..12 {
            let delay = backoff_delay(failures, 0.5);
            assert!(delay >= previous);
            previous = delay;
        }
    }

    #[test]
    fn invalid_session_delay_is_one_to_five_seconds() {
        assert_eq!(invalid_session_delay(0.0), Duration::from_secs(1));
        assert_eq!(
            invalid_session_delay(0.999_999),
            Duration::from_millis(4_999)
        );
        assert_eq!(invalid_session_delay(7.0), Duration::from_secs(5));
    }

    #[test]
    fn os_jitter_stays_in_the_unit_interval() {
        let mut jitter = OsJitter;
        for _ in 0..1000 {
            let value = jitter.unit();
            assert!((0.0..1.0).contains(&value));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn send_budget_is_a_sliding_window() {
        let mut budget = SendBudget::new();
        let start = Instant::now();
        for _ in 0..SEND_LIMIT {
            assert!(budget.has_capacity(Instant::now()));
            budget.record(Instant::now());
        }
        assert!(!budget.has_capacity(Instant::now()));
        // Mandatory frames beyond the ceiling are still counted without growth.
        budget.record(Instant::now());
        assert_eq!(budget.sent.len(), SEND_LIMIT);
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!budget.has_capacity(Instant::now()));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(budget.has_capacity(Instant::now()));
        assert!(Instant::now().duration_since(start) >= SEND_WINDOW);
    }
}
