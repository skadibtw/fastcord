//! Optional per-connect timings. Fixed fields only: no endpoints or payloads.
use std::sync::Mutex;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug)]
pub enum ConnectPhase {
    Dns,
    Tcp,
    Tls,
    Proxy,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct PhaseTiming {
    pub operations: u32,
    pub elapsed_us: u64,
    pub last_finished_us: u64,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct ConnectTimingSnapshot {
    pub dns: PhaseTiming,
    pub tcp: PhaseTiming,
    pub tls: PhaseTiming,
    pub proxy: PhaseTiming,
}
pub struct ConnectTiming {
    started: Instant,
    counts: Mutex<ConnectTimingSnapshot>,
}
impl Default for ConnectTiming {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            counts: Mutex::default(),
        }
    }
}
impl ConnectTiming {
    pub fn snapshot(&self) -> ConnectTimingSnapshot {
        *self.counts.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub(crate) fn begin(&self, phase: ConnectPhase) -> PhaseGuard<'_> {
        PhaseGuard {
            timing: self,
            phase,
            started: Instant::now(),
        }
    }
}
pub(crate) struct PhaseGuard<'a> {
    timing: &'a ConnectTiming,
    phase: ConnectPhase,
    started: Instant,
}
impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        let mut counts = self.timing.counts.lock().unwrap_or_else(|p| p.into_inner());
        let count = match self.phase {
            ConnectPhase::Dns => &mut counts.dns,
            ConnectPhase::Tcp => &mut counts.tcp,
            ConnectPhase::Tls => &mut counts.tls,
            ConnectPhase::Proxy => &mut counts.proxy,
        };
        count.operations = count.operations.saturating_add(1);
        count.elapsed_us = count
            .elapsed_us
            .saturating_add(self.started.elapsed().as_micros().min(u64::MAX as u128) as u64);
        count.last_finished_us = self
            .timing
            .started
            .elapsed()
            .as_micros()
            .min(u64::MAX as u128) as u64;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn cancellation_records_only_the_current_connect_phase() {
        let timing = ConnectTiming::default();
        let result = tokio::time::timeout(std::time::Duration::from_millis(7), async {
            let _guard = timing.begin(ConnectPhase::Dns);
            std::future::pending::<()>().await;
        })
        .await;
        assert!(result.is_err());
        let counts = timing.snapshot();
        assert_eq!(counts.dns.operations, 1);
        assert_eq!(counts.dns.elapsed_us, 7000);
        assert_eq!(counts.tcp.operations, 0);
        assert_eq!(counts.tls.operations, 0);
    }
}
