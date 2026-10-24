//! Reconnect delays, heartbeat jitter, and the Gateway send ceiling.

use std::collections::VecDeque;
use std::sync::Mutex;
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

/// Identifies allowed in one rolling day. Resumes are not counted; a client
/// that needs more is looping, so reconnecting stops until the user asks.
pub(crate) const MAX_IDENTIFIES_PER_DAY: usize = 20;
const IDENTIFY_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
/// Consecutive failed connection attempts before reconnecting stops.
pub(crate) const MAX_FAILED_ATTEMPTS: u32 = 10;
/// `GET /gateway` answers 403 this many times in a row before reconnecting
/// stops (Cloudflare or a ban: asking again does not help).
pub(crate) const MAX_DISCOVERY_FORBIDDEN: u32 = 2;

/// Account safety accounting owned by the validated REST client, rather than a
/// disposable Gateway driver. Reconnect, TLS recovery, proxy/strategy changes
/// and OS network hints cannot refill it. No credential or account ID is kept.
#[derive(Default)]
pub(crate) struct RetryLedger {
    state: Mutex<RetryState>,
}

#[derive(Default)]
struct RetryState {
    identifies: IdentifyBudget,
    failures: u32,
    forbidden: u32,
    retry_not_before: Option<Instant>,
}

impl RetryLedger {
    pub(crate) fn failures(&self) -> u32 {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .failures
    }

    pub(crate) fn forbidden(&self) -> u32 {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .forbidden
    }

    pub(crate) fn failed_attempt(&self) -> u32 {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.failures = state.failures.saturating_add(1);
        state.failures
    }

    pub(crate) fn discovery_forbidden(&self) -> u32 {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.forbidden = state.forbidden.saturating_add(1);
        state.forbidden
    }

    pub(crate) fn discovery_succeeded(&self) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .forbidden = 0;
    }

    pub(crate) fn connection_ended(&self, healthy: bool) -> u32 {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        // Preserve the existing health rule: only 30 seconds of normal READY
        // session operation resets consecutive failures, never a TLS probe.
        state.failures = if healthy {
            0
        } else {
            state.failures.saturating_add(1)
        };
        state.failures
    }

    pub(crate) fn allows_identify(&self, now: Instant) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .identifies
            .allows_another(now)
    }

    /// Reserve before attempting the write, including ambiguous failed writes.
    pub(crate) fn reserve_identify(&self, now: Instant) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.identifies.allows_another(now) {
            return false;
        }
        state.identifies.record(now);
        true
    }

    pub(crate) fn retry_not_before(&self) -> Option<Instant> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retry_not_before
    }

    pub(crate) fn schedule_retry(&self, deadline: Instant) -> Instant {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let deadline = state
            .retry_not_before
            .map_or(deadline, |old| old.max(deadline));
        state.retry_not_before = Some(deadline);
        deadline
    }
}

/// Identifies sent in the last 24 hours, in memory only.
#[derive(Default)]
pub(crate) struct IdentifyBudget {
    sent: VecDeque<Instant>,
}

impl IdentifyBudget {
    fn prune(&mut self, now: Instant) {
        while self
            .sent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= IDENTIFY_WINDOW)
        {
            self.sent.pop_front();
        }
    }

    pub(crate) fn record(&mut self, now: Instant) {
        self.prune(now);
        self.sent.push_back(now);
    }

    /// Whether one more Identify stays within the daily allowance.
    pub(crate) fn allows_another(&mut self, now: Instant) -> bool {
        self.prune(now);
        self.sent.len() < MAX_IDENTIFIES_PER_DAY
    }
}

/// Documented ceiling: 120 commands per connection per 60 seconds.
pub(crate) const SEND_LIMIT: usize = 120;
pub(crate) const SEND_WINDOW: Duration = Duration::from_secs(60);

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

    /// Frames still available in the current window.
    pub(crate) fn remaining(&mut self, now: Instant) -> usize {
        self.prune(now);
        SEND_LIMIT - self.sent.len()
    }

    /// When more than `reserve` frames will be available again if nothing else
    /// is sent meanwhile; `now` if they already are.
    pub(crate) fn free_at(&mut self, now: Instant, reserve: usize) -> Instant {
        self.prune(now);
        // Frames that must leave the window before one more fits above the reserve.
        let excess = (self.sent.len() + reserve + 1).saturating_sub(SEND_LIMIT);
        match excess.checked_sub(1) {
            None => now,
            Some(index) => self
                .sent
                .get(index)
                .map_or(now + SEND_WINDOW, |at| *at + SEND_WINDOW),
        }
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

    #[tokio::test(start_paused = true)]
    async fn shared_ledger_caps_identifies_and_only_normal_health_resets_failures() {
        let ledger = std::sync::Arc::new(RetryLedger::default());
        let clone = ledger.clone();
        for _ in 0..20 {
            assert!(ledger.reserve_identify(Instant::now()));
        }
        assert!(!clone.reserve_identify(Instant::now()));
        assert!(!clone.allows_identify(Instant::now()));
        assert_eq!(ledger.failed_attempt(), 1);
        assert_eq!(clone.connection_ended(false), 2);
        assert_eq!(clone.discovery_forbidden(), 1);
        assert_eq!(ledger.forbidden(), 1);
        assert_eq!(ledger.connection_ended(true), 0);
        assert_eq!(clone.failures(), 0);
        assert_eq!(
            clone.forbidden(),
            1,
            "session health does not clear discovery counters"
        );
        clone.discovery_succeeded();
        assert_eq!(ledger.forbidden(), 0);
        assert!(
            !ledger.allows_identify(Instant::now()),
            "normal health never refills Identify quota"
        );
        tokio::time::advance(IDENTIFY_WINDOW).await;
        assert!(clone.allows_identify(Instant::now()));
    }

    #[tokio::test(start_paused = true)]
    async fn retained_backoff_cannot_be_shortened_by_driver_replacement() {
        let ledger = RetryLedger::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        assert_eq!(ledger.schedule_retry(deadline), deadline);
        assert_eq!(
            ledger.schedule_retry(Instant::now() + Duration::from_millis(1)),
            deadline
        );
        assert_eq!(ledger.retry_not_before(), Some(deadline));
    }

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

    #[tokio::test(start_paused = true)]
    async fn the_identify_allowance_is_twenty_in_a_rolling_day() {
        let mut budget = IdentifyBudget::default();
        for _ in 0..MAX_IDENTIFIES_PER_DAY {
            assert!(budget.allows_another(Instant::now()));
            budget.record(Instant::now());
            tokio::time::advance(Duration::from_secs(60)).await;
        }
        assert!(!budget.allows_another(Instant::now()));
        // The first one leaves the window a day after it was sent.
        tokio::time::advance(IDENTIFY_WINDOW - Duration::from_secs(60 * 20)).await;
        assert!(budget.allows_another(Instant::now()));
        budget.record(Instant::now());
        assert!(!budget.allows_another(Instant::now()));
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

    #[tokio::test(start_paused = true)]
    async fn free_at_names_when_frames_above_a_reserve_return() {
        let mut budget = SendBudget::new();
        let start = Instant::now();
        for _ in 0..110 {
            budget.record(Instant::now());
            tokio::time::advance(Duration::from_millis(500)).await;
        }
        let now = Instant::now();
        assert_eq!(budget.free_at(now, 5), now);
        // Eleven frames must leave; the eleventh was sent at 5 s.
        let at = budget.free_at(now, 20);
        assert_eq!(at, start + Duration::from_secs(65));
        assert_eq!(budget.remaining(at - Duration::from_millis(100)), 20);
        assert_eq!(budget.remaining(at), 21);
        // A reserve the window can never satisfy waits a whole window, never spins.
        assert_eq!(budget.free_at(at, SEND_LIMIT), at + SEND_WINDOW);
    }
}
