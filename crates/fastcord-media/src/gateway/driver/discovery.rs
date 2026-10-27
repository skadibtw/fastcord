//! A user join owns this budget, including signaling retries. No packets,
//! credentials or network identifiers are retained here.
use std::time::Duration;
use tokio::time::Instant;

pub(super) const PRELUDE: &[u8; 1200] =
    include_bytes!("../../../../../fixtures/voice/udp-quic-v2.bin");
pub(super) const DEADLINE: Duration = Duration::from_secs(16);
/// Let the classification prelude leave before Discord discovery. This is
/// asynchronous, and remains inside the join deadline and cancellation loop.
pub(super) const PRELUDE_GAP: Duration = Duration::from_millis(50);

pub(super) struct Campaign {
    pub experimental: bool,
    pub rotate_ports: bool,
    pub variant: u8,
    pub selected: bool,
    started: Option<Instant>,
    sockets: u8,
    requests: u8,
    preludes: u8,
}
impl Campaign {
    pub fn new(experimental: bool) -> Self {
        Self {
            experimental,
            rotate_ports: experimental,
            variant: 1,
            selected: false,
            started: None,
            sockets: 0,
            requests: 0,
            preludes: 0,
        }
    }
    pub fn with_port_recovery(mut self, enabled: bool) -> Self {
        self.rotate_ports |= enabled;
        self
    }
    pub fn deadline(&self) -> Option<Instant> {
        (!self.selected)
            .then_some(self.started)
            .flatten()
            .map(|at| at + DEADLINE)
    }
    pub fn expired(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|at| now >= at)
    }
    pub fn reserve_flow(&mut self, now: Instant) -> Option<u8> {
        if self.selected || self.expired(now) || self.sockets >= 3 {
            return None;
        }
        self.started.get_or_insert(now);
        let preludes = if self.experimental {
            match self.variant {
                2 => 3,
                3 => 6,
                _ => 0,
            }
        } else {
            0
        };
        if self.preludes + preludes > 9 {
            return None;
        }
        self.sockets += 1;
        self.preludes += preludes;
        Some(preludes)
    }
    pub fn reserve_request(&mut self, now: Instant) -> bool {
        if self.selected || self.expired(now) || self.requests >= 15 {
            return false;
        }
        self.requests += 1;
        true
    }
    pub fn advance(&mut self) -> bool {
        if !self.rotate_ports || self.selected || self.variant >= 3 {
            return false;
        }
        self.variant += 1;
        true
    }
}
