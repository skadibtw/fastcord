use std::time::{Duration, Instant};

/// A monotonic clock, including its timer source, shared by an account scheduler.
/// Implementations must not move backwards. Neither server wall-clock timestamps
/// nor the local wall clock participate in rate-limit scheduling.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Duration;
    fn sleep_until(&self, deadline: Duration) -> impl Future<Output = ()> + Send;
}

#[derive(Clone, Debug)]
pub struct MonotonicClock {
    epoch: Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    async fn sleep_until(&self, deadline: Duration) {
        // Chunk exceptionally distant server deadlines rather than overflowing
        // an OS-specific Instant representation. This is not a rate-limit cap.
        while self.now() < deadline {
            let delay = deadline.saturating_sub(self.now());
            tokio::time::sleep(delay.min(Duration::from_secs(86_400))).await;
        }
    }
}
