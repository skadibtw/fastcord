//! Demand-driven TLS diagnosis. This module never sends HTTP, authenticates, or
//! changes the account's Gateway retry/Identify accounting.
//!
//! The transport callback receives only an already-required endpoint and strict
//! per-probe limits. A candidate says that TLS was reachable, never that DPI was
//! detected or that an authenticated service/session is healthy.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::tls::Strategy;
use crate::{NetworkError, Role};

pub const CATALOG_VERSION: u32 = 1;
pub const TLS_PROFILE_VERSION: u32 = 1;
pub const MAX_TLS_PROBES: u8 = 6;
pub const MAX_CAMPAIGN_DIALS: u8 = 12;
pub const MAX_ROLLING_DIALS: usize = 24;
pub const MAX_PROBE_INBOUND: usize = 128 * 1024;
pub const MAX_CAMPAIGN_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_PENDING_BYTES: usize = 16 * 1024;
pub const MAX_CONFIRMATIONS: usize = 32;
pub const CAMPAIGN_TIMEOUT: Duration = Duration::from_secs(60);
pub const PROBE_SPACING: Duration = Duration::from_millis(250);
pub const CAMPAIGN_SPACING: Duration = Duration::from_secs(60);
pub const ROLLING_WINDOW: Duration = Duration::from_secs(10 * 60);
pub const HINT_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AddressFamily {
    V4,
    V6,
}

/// Keep probe SNI, TLS versions, roots and ALPN identical to the production
/// role. Probes always disable TLS resumption and early data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TlsProfile {
    Http,
    WebSocket,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct DiagnosticTarget {
    pub host: String,
    pub port: u16,
    pub role: Role,
    pub family: AddressFamily,
    pub profile: TlsProfile,
}

impl DiagnosticTarget {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        role: Role,
        family: AddressFamily,
        profile: TlsProfile,
    ) -> Result<Self, NetworkError> {
        let host = host.into();
        if port == 0
            || host.is_empty()
            || host.len() > 253
            || host.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
            || host.parse::<std::net::IpAddr>().is_ok()
        {
            return Err(NetworkError::InvalidTarget);
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
            role,
            family,
            profile,
        })
    }
}

impl fmt::Debug for DiagnosticTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiagnosticTarget")
            .field("host", &"[redacted]")
            .field("role", &self.role)
            .field("family", &self.family)
            .field("profile", &self.profile)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ProbeLimits {
    /// Reserved before the probe, including any family/address fallback. The
    /// conservative reservation is retained even if only one dial is needed.
    pub max_tcp_dials: u8,
    pub max_inbound_bytes: usize,
    pub max_network_bytes: usize,
    pub dns_timeout: Duration,
    pub tcp_timeout: Duration,
    pub tls_timeout: Duration,
    pub total_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct ProbeRequest {
    pub target: DiagnosticTarget,
    pub strategy: Strategy,
    pub generation: u64,
    pub limits: ProbeLimits,
}

#[derive(Clone, Copy, Debug)]
pub struct ProbeOutcome {
    pub result: Result<(), NetworkError>,
    /// Includes TLS/transport framing, in both directions, not just plaintext.
    pub sent_bytes: usize,
    pub received_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub target: DiagnosticTarget,
    pub strategy: Strategy,
    pub generation: u64,
    pub verified_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceStatus {
    Unknown,
    Selecting,
    TlsReachable,
    ServiceReachable,
    SessionHealthy,
    Failed(NetworkError),
    UdpDiscovering { variant: u8 },
    UdpUnreachable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetSnapshot {
    pub campaign_active: bool,
    pub generation: u64,
    pub rolling_dials: usize,
    pub campaign_probes: u8,
    pub campaign_dials: u8,
    pub campaign_bytes: usize,
}

/// Implemented by the OS integration crate. The callback is a bounded hint:
/// the app debounces it for 500 ms and invalidates the controller generation.
/// A hint never disconnects a healthy session or starts a probe by itself.
pub trait NetworkChangeSource: Sized {
    fn subscribe(callback: Arc<dyn Fn() + Send + Sync>) -> Result<Self, NetworkError>;
}

struct Campaign {
    id: u64,
    generation: u64,
    started_ms: u64,
    last_probe_ms: Option<u64>,
    probes: u8,
    dials: u8,
    bytes: usize,
}

#[derive(Default)]
struct Policy {
    generation: u64,
    next_campaign: u64,
    last_started_ms: Option<u64>,
    rolling_dials: VecDeque<u64>,
    active: Option<Campaign>,
    confirmations: VecDeque<Candidate>,
    status: Vec<(Role, ServiceStatus)>,
}

impl Policy {
    fn prune_dials(&mut self, now_ms: u64) {
        while self
            .rolling_dials
            .front()
            .is_some_and(|time| now_ms.saturating_sub(*time) >= ROLLING_WINDOW.as_millis() as u64)
        {
            self.rolling_dials.pop_front();
        }
    }

    fn begin(&mut self, error: NetworkError, now_ms: u64) -> Result<u64, NetworkError> {
        if !error.eligible_for_selection() {
            return Err(error);
        }
        self.prune_dials(now_ms);
        if self.active.is_some()
            || self.last_started_ms.is_some_and(|last| {
                now_ms.saturating_sub(last) < CAMPAIGN_SPACING.as_millis() as u64
            })
            || self.rolling_dials.len() + 2 > MAX_ROLLING_DIALS
        {
            return Err(NetworkError::BudgetExhausted);
        }
        self.next_campaign = self.next_campaign.wrapping_add(1);
        let id = self.next_campaign;
        self.last_started_ms = Some(now_ms);
        self.active = Some(Campaign {
            id,
            generation: self.generation,
            started_ms: now_ms,
            last_probe_ms: None,
            probes: 0,
            dials: 0,
            bytes: 0,
        });
        Ok(id)
    }

    fn reserve(&mut self, id: u64, now_ms: u64) -> Result<ProbeLimits, NetworkError> {
        self.prune_dials(now_ms);
        let campaign = self.active.as_mut().ok_or(NetworkError::Cancelled)?;
        if campaign.id != id || campaign.generation != self.generation {
            return Err(NetworkError::Cancelled);
        }
        let elapsed_ms = now_ms.saturating_sub(campaign.started_ms);
        if elapsed_ms >= CAMPAIGN_TIMEOUT.as_millis() as u64
            || campaign.probes >= MAX_TLS_PROBES
            || campaign.dials + 2 > MAX_CAMPAIGN_DIALS
            || campaign.bytes >= MAX_CAMPAIGN_BYTES
            || self.rolling_dials.len() + 2 > MAX_ROLLING_DIALS
            || campaign
                .last_probe_ms
                .is_some_and(|last| now_ms.saturating_sub(last) < PROBE_SPACING.as_millis() as u64)
        {
            return Err(NetworkError::BudgetExhausted);
        }
        campaign.last_probe_ms = Some(now_ms);
        campaign.probes += 1;
        campaign.dials += 2;
        self.rolling_dials.extend([now_ms, now_ms]);
        Ok(ProbeLimits {
            max_tcp_dials: 2,
            max_inbound_bytes: MAX_PROBE_INBOUND,
            max_network_bytes: MAX_CAMPAIGN_BYTES - campaign.bytes,
            dns_timeout: Duration::from_secs(3),
            tcp_timeout: Duration::from_secs(3),
            tls_timeout: Duration::from_secs(4),
            total_timeout: Duration::from_millis((60_000 - elapsed_ms).min(10_000)),
        })
    }

    fn account(&mut self, id: u64, outcome: &ProbeOutcome) -> Result<(), NetworkError> {
        let campaign = self.active.as_mut().ok_or(NetworkError::Cancelled)?;
        if campaign.id != id || campaign.generation != self.generation {
            return Err(NetworkError::Cancelled);
        }
        let bytes = outcome.sent_bytes.saturating_add(outcome.received_bytes);
        campaign.bytes = campaign.bytes.saturating_add(bytes);
        if outcome.received_bytes > MAX_PROBE_INBOUND || campaign.bytes > MAX_CAMPAIGN_BYTES {
            return Err(NetworkError::BudgetExhausted);
        }
        Ok(())
    }

    fn set_status(&mut self, role: Role, status: ServiceStatus) -> bool {
        if let Some((_, saved)) = self.status.iter_mut().find(|(saved, _)| *saved == role) {
            if *saved == status {
                return false;
            }
            *saved = status;
            true
        } else if self.status.len() < 9 {
            self.status.push((role, status));
            true
        } else {
            false
        }
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.confirmations.clear();
        // Neither budget is reset. Healthy sessions stay healthy: an OS hint
        // does not establish that a working route has become unusable.
        for (_, status) in &mut self.status {
            if !matches!(
                status,
                ServiceStatus::SessionHealthy | ServiceStatus::ServiceReachable
            ) {
                *status = ServiceStatus::Unknown;
            }
        }
    }
}

struct Shared {
    epoch: Instant,
    policy: Mutex<Policy>,
    changes: watch::Sender<u64>,
    active: watch::Sender<bool>,
    updates: watch::Sender<u64>,
}

/// One shared instance lives across every account, reconnect, media connection,
/// manual retry and settings change in the process. Dropping a diagnosis future
/// cancels its probe and releases the single campaign, preserving all budgets.
#[derive(Clone)]
pub struct Controller(Arc<Shared>);

impl Default for Controller {
    fn default() -> Self {
        let (changes, _) = watch::channel(0);
        let (active, _) = watch::channel(false);
        let (updates, _) = watch::channel(0);
        Self(Arc::new(Shared {
            epoch: Instant::now(),
            policy: Mutex::new(Policy::default()),
            changes,
            active,
            updates,
        }))
    }
}

impl fmt::Debug for Controller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Controller")
            .field("budget", &self.budget_snapshot())
            .finish()
    }
}

impl Controller {
    pub fn new() -> Self {
        Self::default()
    }

    fn policy(&self) -> MutexGuard<'_, Policy> {
        self.0
            .policy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now_ms(&self) -> u64 {
        self.0.epoch.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    fn notify(&self) {
        self.0
            .updates
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    /// Event-driven UI status changes. This channel is deliberately separate
    /// from network-generation cancellation: showing progress must not cancel
    /// the campaign that produced it. No idle timer or byte-level updates.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.0.updates.subscribe()
    }

    fn set_status(&self, role: Role, status: ServiceStatus) {
        let changed = self.policy().set_status(role, status);
        if changed {
            self.notify();
        }
    }

    pub fn network_changed(&self) {
        let generation = {
            let mut policy = self.policy();
            policy.invalidate();
            policy.generation
        };
        self.0.changes.send_replace(generation);
        self.notify();
    }

    pub fn generation(&self) -> u64 {
        self.policy().generation
    }

    /// Production retries wait here without polling while the one shared
    /// unauthenticated campaign owns the route-selection gate.
    pub async fn wait_for_idle(&self) {
        let mut active = self.0.active.subscribe();
        loop {
            if self.policy().active.is_none() {
                return;
            }
            if active.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn budget_snapshot(&self) -> BudgetSnapshot {
        let mut policy = self.policy();
        policy.prune_dials(self.now_ms());
        BudgetSnapshot {
            campaign_active: policy.active.is_some(),
            generation: policy.generation,
            rolling_dials: policy.rolling_dials.len(),
            campaign_probes: policy.active.as_ref().map_or(0, |campaign| campaign.probes),
            campaign_dials: policy.active.as_ref().map_or(0, |campaign| campaign.dials),
            campaign_bytes: policy.active.as_ref().map_or(0, |campaign| campaign.bytes),
        }
    }

    pub fn strategy_for(&self, target: &DiagnosticTarget) -> Option<Strategy> {
        let now = self.now_ms();
        let policy = self.policy();
        policy.confirmations.iter().find_map(|candidate| {
            (candidate.target == *target
                && candidate.generation == policy.generation
                && now.saturating_sub(candidate.verified_at_ms) < HINT_LIFETIME.as_millis() as u64)
                .then_some(candidate.strategy)
        })
    }

    pub fn service_status(&self, role: Role) -> ServiceStatus {
        self.policy()
            .status
            .iter()
            .find_map(|(saved_role, status)| (*saved_role == role).then_some(*status))
            .unwrap_or(ServiceStatus::Unknown)
    }

    pub fn note_service_reachable(&self, role: Role) {
        self.set_status(role, ServiceStatus::ServiceReachable);
    }

    pub fn note_session_healthy(&self, role: Role) {
        self.set_status(role, ServiceStatus::SessionHealthy);
    }

    pub fn note_udp_unreachable(&self, role: Role) {
        self.set_status(role, ServiceStatus::UdpUnreachable);
    }

    /// Progress in a separately opted-in discovery campaign. This is neither
    /// evidence of TLS reachability nor a new TLS diagnostic campaign.
    pub fn note_udp_discovery(&self, role: Role, variant: u8) {
        if (1..=3).contains(&variant) {
            self.set_status(role, ServiceStatus::UdpDiscovering { variant });
        }
    }

    pub fn note_failure(&self, role: Role, failure: NetworkError) {
        self.set_status(role, ServiceStatus::Failed(failure));
    }

    /// The caller must first record the real production failure in its normal
    /// retry accounting. This method never returns a new production transport
    /// or resends any request; the existing driver retains all account budgets
    /// and waits for both diagnosis and its normal nonzero backoff.
    pub async fn diagnose<F, Fut>(
        &self,
        target: DiagnosticTarget,
        error: NetworkError,
        mut probe: F,
    ) -> Result<Option<Candidate>, NetworkError>
    where
        F: FnMut(ProbeRequest) -> Fut,
        Fut: Future<Output = ProbeOutcome>,
    {
        // Fields are visible to the transport executor, so revalidate at this
        // boundary as well: a caller cannot bypass target bounds by building
        // or modifying a struct directly.
        let target = DiagnosticTarget::new(
            target.host,
            target.port,
            target.role,
            target.family,
            target.profile,
        )?;
        // Subscribe before reading generation so a simultaneous OS hint cannot
        // be lost between the initial check and waiting on the first probe.
        let mut changes = self.0.changes.subscribe();
        let (id, generation) = {
            let mut policy = self.policy();
            let id = policy.begin(error, self.now_ms())?;
            self.0.active.send_replace(true);
            policy.set_status(target.role, ServiceStatus::Selecting);
            (id, policy.generation)
        };
        self.notify();
        let mut guard = CampaignGuard {
            controller: self,
            id,
            role: target.role,
            completed: false,
        };
        let strategies = [
            Strategy::Direct,
            Strategy::TlsRecordSplit,
            Strategy::WriteSplit,
            Strategy::Combined,
        ];
        let mut strategy_index = 0;
        let mut successes = 0;
        loop {
            if self.generation() != generation {
                return Err(NetworkError::Cancelled);
            }
            let reservation = self.policy().reserve(id, self.now_ms());
            let limits = match reservation {
                Ok(limits) => limits,
                Err(failure) => {
                    self.note_failure(target.role, failure);
                    guard.completed = true;
                    return Err(failure);
                }
            };
            let request = ProbeRequest {
                target: target.clone(),
                strategy: strategies[strategy_index],
                generation,
                limits,
            };
            let outcome = tokio::select! {
                biased;
                _ = changes.changed() => return Err(NetworkError::Cancelled),
                outcome = tokio::time::timeout(limits.total_timeout, probe(request)) => {
                    match outcome {
                        Ok(outcome) => outcome,
                        // The callback enforces byte limits at I/O. A timed-out
                        // probe reserves its whole byte allowance conservatively.
                        Err(_) => ProbeOutcome {
                            result: Err(NetworkError::TlsTimeout),
                            sent_bytes: limits.max_network_bytes,
                            received_bytes: 0,
                        },
                    }
                }
            };
            let accounting = self.policy().account(id, &outcome);
            if let Err(failure) = accounting {
                self.note_failure(target.role, failure);
                guard.completed = true;
                return Err(failure);
            }
            if outcome.result.is_ok() {
                successes += 1;
                if successes == 2 {
                    let candidate = Candidate {
                        target,
                        strategy: strategies[strategy_index],
                        generation,
                        verified_at_ms: self.now_ms(),
                    };
                    let mut policy = self.policy();
                    if generation != policy.generation {
                        return Err(NetworkError::Cancelled);
                    }
                    // Each stored target is <=253 bytes; 32 entries, their
                    // fixed-size metadata and the active target stay <16 KiB.
                    policy
                        .confirmations
                        .retain(|saved| saved.target != candidate.target);
                    if policy.confirmations.len() == MAX_CONFIRMATIONS {
                        policy.confirmations.pop_front();
                    }
                    policy.confirmations.push_back(candidate.clone());
                    policy.set_status(candidate.target.role, ServiceStatus::TlsReachable);
                    drop(policy);
                    self.notify();
                    guard.completed = true;
                    return Ok(Some(candidate));
                }
            } else {
                // Certificates/protocol rejection are terminal evidence; never
                // try to alter TLS in order to bypass validation or challenges.
                let failure = outcome
                    .result
                    .err()
                    .unwrap_or(NetworkError::TlsProtocolRejected);
                if !failure.eligible_for_selection() {
                    self.note_failure(target.role, failure);
                    guard.completed = true;
                    return Err(failure);
                }
                successes = 0;
                strategy_index += 1;
                if strategy_index == strategies.len() {
                    self.note_failure(target.role, failure);
                    guard.completed = true;
                    return Ok(None);
                }
            }
            tokio::select! {
                biased;
                _ = changes.changed() => return Err(NetworkError::Cancelled),
                _ = tokio::time::sleep(PROBE_SPACING) => {}
            }
        }
    }
}

struct CampaignGuard<'a> {
    controller: &'a Controller,
    id: u64,
    role: Role,
    completed: bool,
}

impl Drop for CampaignGuard<'_> {
    fn drop(&mut self) {
        let mut policy = self.controller.policy();
        if policy
            .active
            .as_ref()
            .is_some_and(|campaign| campaign.id == self.id)
        {
            policy.active = None;
            self.controller.0.active.send_replace(false);
            if !self.completed
                && policy
                    .status
                    .iter()
                    .any(|(role, status)| *role == self.role && *status == ServiceStatus::Selecting)
            {
                policy.set_status(self.role, ServiceStatus::Failed(NetworkError::Cancelled));
            }
            drop(policy);
            self.controller.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> DiagnosticTarget {
        DiagnosticTarget::new(
            "gateway.discord.gg",
            443,
            Role::Gateway,
            AddressFamily::V4,
            TlsProfile::WebSocket,
        )
        .unwrap()
    }

    #[test]
    fn only_concrete_bounded_hosts_and_ports_enter_the_controller() {
        for host in [
            "*.discord.media",
            "https://discord.com",
            "discord.com/token?x=y",
            "127.0.0.1",
            "::1",
            "",
            "a..b",
            "a\n.b",
        ] {
            assert!(
                DiagnosticTarget::new(
                    host,
                    443,
                    Role::Voice,
                    AddressFamily::V4,
                    TlsProfile::WebSocket
                )
                .is_err()
            );
        }
        assert!(
            DiagnosticTarget::new(
                "a.discord.media",
                0,
                Role::Voice,
                AddressFamily::V4,
                TlsProfile::WebSocket
            )
            .is_err()
        );
        let mut canary = target();
        canary.host = "canary-secret-never-logged.example".to_owned();
        assert!(!format!("{canary:?}").contains("canary-secret"));
    }

    #[test]
    fn noneligible_failure_never_consumes_a_probe_budget() {
        let mut policy = Policy::default();
        for failure in [
            NetworkError::CertificateRejected,
            NetworkError::TlsProtocolRejected,
            NetworkError::WebSocketUpgrade,
            NetworkError::ProxyRefused,
            NetworkError::Dns,
        ] {
            assert_eq!(policy.begin(failure, 0), Err(failure));
        }
        assert!(policy.last_started_ms.is_none());
        assert!(policy.rolling_dials.is_empty());
    }

    #[test]
    fn campaign_is_singleton_and_reserves_at_most_six_probes_twelve_dials() {
        let mut policy = Policy::default();
        let id = policy.begin(NetworkError::TlsReset, 0).unwrap();
        assert_eq!(
            policy.begin(NetworkError::TlsReset, 0),
            Err(NetworkError::BudgetExhausted)
        );
        for i in 0..6 {
            let limits = policy.reserve(id, i * 250).unwrap();
            assert_eq!(limits.max_tcp_dials, 2);
            assert_eq!(limits.max_inbound_bytes, 128 * 1024);
        }
        assert_eq!(policy.active.as_ref().unwrap().probes, 6);
        assert_eq!(policy.active.as_ref().unwrap().dials, 12);
        assert!(matches!(
            policy.reserve(id, 1500),
            Err(NetworkError::BudgetExhausted)
        ));
    }

    #[test]
    fn injected_clock_spacing_deadline_and_rolling_budget_survive_changes() {
        let mut policy = Policy::default();
        let id = policy.begin(NetworkError::TlsTimeout, 0).unwrap();
        policy.reserve(id, 0).unwrap();
        assert!(policy.reserve(id, 249).is_err());
        policy.reserve(id, 250).unwrap();
        policy.invalidate();
        assert!(matches!(
            policy.reserve(id, 500),
            Err(NetworkError::Cancelled)
        ));
        policy.active = None;
        assert!(policy.begin(NetworkError::TlsTimeout, 59_999).is_err());
        let id = policy.begin(NetworkError::TlsTimeout, 60_000).unwrap();
        assert!(policy.reserve(id, 120_000).is_err());
        policy.active = None;
        policy.rolling_dials = std::iter::repeat_n(60_000, 24).collect();
        assert!(policy.begin(NetworkError::TlsTimeout, 120_000).is_err());
        policy.invalidate();
        assert!(policy.begin(NetworkError::TlsTimeout, 600_000).is_err());
        assert!(policy.begin(NetworkError::TlsTimeout, 660_000).is_ok());
    }

    #[test]
    fn inbound_and_total_byte_caps_are_enforced_without_wrapping() {
        let mut policy = Policy::default();
        let id = policy.begin(NetworkError::TlsReset, 0).unwrap();
        policy.reserve(id, 0).unwrap();
        assert_eq!(
            policy.account(
                id,
                &ProbeOutcome {
                    result: Ok(()),
                    sent_bytes: 0,
                    received_bytes: MAX_PROBE_INBOUND + 1
                }
            ),
            Err(NetworkError::BudgetExhausted)
        );
        assert_eq!(
            policy.account(
                id,
                &ProbeOutcome {
                    result: Ok(()),
                    sent_bytes: usize::MAX,
                    received_bytes: usize::MAX
                }
            ),
            Err(NetworkError::BudgetExhausted)
        );
    }

    #[tokio::test]
    async fn two_tls_successes_confirm_only_tls_and_exact_family_endpoint() {
        let controller = Controller::new();
        let candidate = controller
            .diagnose(target(), NetworkError::TlsReset, |_| async {
                ProbeOutcome {
                    result: Ok(()),
                    sent_bytes: 300,
                    received_bytes: 1000,
                }
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(candidate.strategy, Strategy::Direct);
        assert_eq!(controller.strategy_for(&target()), Some(Strategy::Direct));
        assert_eq!(
            controller.service_status(Role::Gateway),
            ServiceStatus::TlsReachable
        );
        let mut ipv6 = target();
        ipv6.family = AddressFamily::V6;
        assert_eq!(controller.strategy_for(&ipv6), None);
        assert_eq!(controller.budget_snapshot().rolling_dials, 4);
        assert!(!controller.budget_snapshot().campaign_active);
        controller.note_service_reachable(Role::Gateway);
        controller.network_changed();
        assert_eq!(controller.strategy_for(&target()), None);
        assert_eq!(
            controller.service_status(Role::Gateway),
            ServiceStatus::ServiceReachable
        );
        assert_eq!(controller.budget_snapshot().rolling_dials, 4);
    }

    #[tokio::test]
    async fn certificate_error_stops_after_one_probe_and_keeps_the_budget() {
        let controller = Controller::new();
        let result = controller
            .diagnose(target(), NetworkError::TlsReset, |_| async {
                ProbeOutcome {
                    result: Err(NetworkError::CertificateRejected),
                    sent_bytes: 300,
                    received_bytes: 1000,
                }
            })
            .await;
        assert!(matches!(result, Err(NetworkError::CertificateRejected)));
        assert_eq!(controller.budget_snapshot().rolling_dials, 2);
        assert_eq!(controller.strategy_for(&target()), None);
    }

    #[tokio::test]
    async fn network_change_promptly_cancels_a_waiting_probe_without_refunding() {
        let controller = Controller::new();
        let another = controller.clone();
        let task = tokio::spawn(async move {
            another
                .diagnose(target(), NetworkError::TlsReset, |_| async {
                    std::future::pending::<ProbeOutcome>().await
                })
                .await
        });
        tokio::task::yield_now().await;
        controller.network_changed();
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(100), task)
                .await
                .unwrap()
                .unwrap(),
            Err(NetworkError::Cancelled)
        ));
        assert_eq!(controller.budget_snapshot().rolling_dials, 2);
        assert!(!controller.budget_snapshot().campaign_active);
        assert!(matches!(
            controller
                .diagnose(target(), NetworkError::TlsReset, |_| async {
                    unreachable!("cooldown must reject before the callback")
                })
                .await,
            Err(NetworkError::BudgetExhausted)
        ));
    }

    #[tokio::test]
    async fn dropping_diagnosis_releases_single_campaign_without_resetting_accounting() {
        let controller = Controller::new();
        let mut diagnosis = Box::pin(controller.diagnose(
            target(),
            NetworkError::TlsReset,
            |_| async { std::future::pending::<ProbeOutcome>().await },
        ));
        assert!(futures_poll_once(diagnosis.as_mut()).await.is_none());
        assert!(controller.budget_snapshot().campaign_active);
        drop(diagnosis);
        assert!(!controller.budget_snapshot().campaign_active);
        assert_eq!(controller.budget_snapshot().rolling_dials, 2);
        assert!(matches!(
            controller
                .diagnose(target(), NetworkError::TlsReset, |_| async {
                    unreachable!("cancellation must not reset cooldown")
                })
                .await,
            Err(NetworkError::BudgetExhausted)
        ));
    }

    #[tokio::test]
    async fn ui_watch_coalesces_equal_status_and_never_cancels_diagnosis() {
        let controller = Controller::new();
        let mut updates = controller.changes();
        assert!(!updates.has_changed().unwrap());
        controller.note_service_reachable(Role::Rest);
        updates.changed().await.unwrap();
        let first = *updates.borrow_and_update();
        controller.note_service_reachable(Role::Rest);
        assert!(!updates.has_changed().unwrap());
        let mut diagnosis = Box::pin(controller.diagnose(
            target(),
            NetworkError::TlsReset,
            |_| async { std::future::pending::<ProbeOutcome>().await },
        ));
        assert!(futures_poll_once(diagnosis.as_mut()).await.is_none());
        updates.changed().await.unwrap();
        assert_ne!(*updates.borrow_and_update(), first);
        // Another service can become reachable during a campaign without
        // producing a generation change or cancelling that campaign.
        controller.note_session_healthy(Role::Voice);
        updates.changed().await.unwrap();
        assert!(futures_poll_once(diagnosis.as_mut()).await.is_none());
        assert!(controller.budget_snapshot().campaign_active);
        drop(diagnosis);
        updates.changed().await.unwrap();
        assert!(!controller.budget_snapshot().campaign_active);
        assert!(!updates.has_changed().unwrap());
        controller.network_changed();
        updates.changed().await.unwrap();
        assert_eq!(controller.generation(), 1);
        assert_eq!(
            controller.service_status(Role::Voice),
            ServiceStatus::SessionHealthy
        );
    }

    #[tokio::test]
    async fn udp_progress_is_bounded_coalesced_and_does_not_change_tls_budgets() {
        let controller = Controller::new();
        let mut updates = controller.changes();
        for invalid in [0, 4, u8::MAX] {
            controller.note_udp_discovery(Role::Voice, invalid);
        }
        assert_eq!(
            controller.service_status(Role::Voice),
            ServiceStatus::Unknown
        );
        assert!(!updates.has_changed().unwrap());
        let mut diagnosis = Box::pin(controller.diagnose(
            target(),
            NetworkError::TlsReset,
            |_| async { std::future::pending::<ProbeOutcome>().await },
        ));
        assert!(futures_poll_once(diagnosis.as_mut()).await.is_none());
        let before = controller.budget_snapshot();
        updates.borrow_and_update();
        for variant in 1..=3 {
            controller.note_udp_discovery(Role::Voice, variant);
            assert!(updates.has_changed().unwrap());
            updates.borrow_and_update();
            controller.note_udp_discovery(Role::Voice, variant);
            assert!(!updates.has_changed().unwrap());
            assert_eq!(
                controller.service_status(Role::Voice),
                ServiceStatus::UdpDiscovering { variant }
            );
            assert!(futures_poll_once(diagnosis.as_mut()).await.is_none());
        }
        let after = controller.budget_snapshot();
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.rolling_dials, before.rolling_dials);
        assert_eq!(after.campaign_dials, before.campaign_dials);
        assert_eq!(after.campaign_probes, before.campaign_probes);
        assert_eq!(after.campaign_bytes, before.campaign_bytes);
        assert!(after.campaign_active);
    }

    #[tokio::test]
    async fn budget_failure_remains_visible_as_budget_failure() {
        let controller = Controller::new();
        let result = controller
            .diagnose(target(), NetworkError::TlsReset, |_| async {
                ProbeOutcome {
                    result: Ok(()),
                    sent_bytes: MAX_CAMPAIGN_BYTES + 1,
                    received_bytes: 0,
                }
            })
            .await;
        assert!(matches!(result, Err(NetworkError::BudgetExhausted)));
        assert_eq!(
            controller.service_status(Role::Gateway),
            ServiceStatus::Failed(NetworkError::BudgetExhausted)
        );
        assert!(!controller.budget_snapshot().campaign_active);
    }

    async fn futures_poll_once<F: Future>(future: std::pin::Pin<&mut F>) -> Option<F::Output> {
        let mut future = future;
        std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
            std::task::Poll::Ready(value) => std::task::Poll::Ready(Some(value)),
            std::task::Poll::Pending => std::task::Poll::Ready(None),
        })
        .await
    }
}
