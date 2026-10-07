//! UI-side view of the account's Gateway connection. The protocol, session,
//! heartbeats, and reconnects live in `fastcord_discord::gateway::Gateway`; this
//! module only turns its ordered events into the few status changes the screen
//! shows, and is dropped, with the connection it owns, when the account screen
//! is left (which closes the Gateway session cleanly).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use fastcord_discord::gateway::{
    ConnectionState, Dispatch, Gateway, GatewayEvent, ReconnectReason, StopReason,
};
use fastcord_discord::{RestClient, UserToken};
use iced::futures::{Stream, stream};
use iced::task::Handle;

/// What READY said about the account, for display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub guilds: usize,
    pub unavailable: usize,
    pub direct_messages: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayStatus {
    Connecting {
        attempt: u32,
    },
    SigningIn,
    Resuming,
    Ready(Counts),
    Reconnecting {
        attempt: u32,
        delay: Duration,
        reason: ReconnectReason,
    },
    /// Terminal: the token is no longer accepted.
    AuthenticationRequired,
    /// Terminal: reconnecting cannot help.
    Stopped(StopReason),
}

impl GatewayStatus {
    pub fn describe(&self) -> String {
        match self {
            Self::Connecting { attempt: 0 } => "Connecting to Discord…".to_owned(),
            Self::Connecting { attempt } => {
                format!("Connecting to Discord… (attempt {})", attempt + 1)
            }
            Self::SigningIn => "Connected; signing in…".to_owned(),
            Self::Resuming => "Connected; resuming your session…".to_owned(),
            Self::Ready(counts) => {
                let mut text = format!(
                    "Connected to Discord: {} servers, {} direct messages.",
                    counts.guilds, counts.direct_messages
                );
                if counts.unavailable > 0 {
                    text.push_str(&format!(" {} servers are unavailable.", counts.unavailable));
                }
                text
            }
            Self::Reconnecting {
                attempt,
                delay,
                reason,
            } => format!(
                "Connection lost ({}). Reconnecting in {} (attempt {attempt}).",
                describe_reason(*reason),
                describe_delay(*delay)
            ),
            Self::AuthenticationRequired => {
                "Discord no longer accepts this login. Log in again.".to_owned()
            }
            Self::Stopped(reason) => format!("Disconnected: {reason}"),
        }
    }
}

fn describe_reason(reason: ReconnectReason) -> &'static str {
    match reason {
        ReconnectReason::ServerRequested => "Discord asked to reconnect",
        ReconnectReason::HeartbeatTimeout => "Discord stopped responding",
        ReconnectReason::HelloTimeout => "Discord did not greet the connection",
        ReconnectReason::Protocol => "unreadable data",
        ReconnectReason::InvalidSession { .. } => "session expired",
        ReconnectReason::Closed(_) => "closed by Discord",
        ReconnectReason::Network => "network error",
        ReconnectReason::ConnectFailed => "could not connect",
        ReconnectReason::DiscoveryFailed => "could not reach Discord",
    }
}

/// "now" / "2 seconds": static text, so no per-second redraw timer is needed.
fn describe_delay(delay: Duration) -> String {
    match delay.as_secs_f64() {
        seconds if seconds < 0.5 => "a moment".to_owned(),
        seconds if seconds < 1.5 => "1 second".to_owned(),
        seconds => format!("{} seconds", seconds.round() as u64),
    }
}

/// Folds the ordered event stream into status changes. Everything that is not
/// a status change (messages, guild updates, replay) is consumed here and is
/// for the reducer of a later milestone, not for the screen.
#[derive(Default)]
struct Tracker {
    counts: Counts,
}

impl Tracker {
    fn apply(&mut self, event: GatewayEvent) -> Option<GatewayStatus> {
        match event {
            GatewayEvent::State(state) => Some(match state {
                ConnectionState::Connecting { attempt } => GatewayStatus::Connecting { attempt },
                ConnectionState::AwaitHello | ConnectionState::Identifying => {
                    GatewayStatus::SigningIn
                }
                ConnectionState::Resuming => GatewayStatus::Resuming,
                ConnectionState::Ready => GatewayStatus::Ready(self.counts),
                ConnectionState::Reconnecting {
                    attempt,
                    delay,
                    reason,
                } => GatewayStatus::Reconnecting {
                    attempt,
                    delay,
                    reason,
                },
                ConnectionState::AuthenticationRequired => GatewayStatus::AuthenticationRequired,
                ConnectionState::Stopped(reason) => GatewayStatus::Stopped(reason),
            }),
            GatewayEvent::Dispatch { event, .. } => {
                match event {
                    // A new session replaces everything the old one counted.
                    Dispatch::Ready(ready) => {
                        self.counts = Counts {
                            guilds: ready.guilds.len(),
                            unavailable: ready.unavailable_guilds.len(),
                            direct_messages: ready.private_channels.len(),
                        };
                    }
                    Dispatch::ReadySupplemental(supplemental) => {
                        self.counts.direct_messages += supplemental.lazy_private_channels.len();
                    }
                    _ => {}
                }
                None
            }
        }
    }
}

struct Worker {
    start: Option<(RestClient, Arc<UserToken>, String)>,
    gateway: Option<Gateway>,
    tracker: Tracker,
}

/// A worker stream for one account session. The connection starts when the
/// stream is first polled (inside iced's Tokio runtime) and ends, closing the
/// socket with code 1000 so the session ends, when the stream is dropped,
/// which `Handle::abort` does.
pub fn status_stream(
    rest: RestClient,
    token: Arc<UserToken>,
    locale: String,
) -> impl Stream<Item = GatewayStatus> {
    let worker = Worker {
        start: Some((rest, token, locale)),
        gateway: None,
        tracker: Tracker::default(),
    };
    stream::unfold(worker, |mut worker| async move {
        let mut gateway = match worker.gateway.take() {
            Some(gateway) => gateway,
            None => {
                let (rest, token, locale) = worker.start.take()?;
                Gateway::start(rest, token, locale)
            }
        };
        loop {
            let event = gateway.next_event().await?;
            if let Some(status) = worker.tracker.apply(event) {
                worker.gateway = Some(gateway);
                return Some((status, worker));
            }
        }
    })
}

/// The account screen's connection state. Messages from an earlier session
/// (identified by `id`) are ignored.
pub struct GatewayPanel {
    pub id: u64,
    pub status: GatewayStatus,
    /// Aborts the worker, and with it the connection, when dropped.
    _worker: Handle,
}

impl fmt::Debug for GatewayPanel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayPanel")
            .field("id", &self.id)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl GatewayPanel {
    pub fn new(id: u64, worker: Handle) -> Self {
        Self {
            id,
            status: GatewayStatus::Connecting { attempt: 0 },
            _worker: worker.abort_on_drop(),
        }
    }
}

#[cfg(test)]
mod tests {
    use fastcord_discord::gateway::{Ready, SessionId};
    use fastcord_model::{Channel, ChannelKind, Guild, Snowflake, User};

    use super::*;

    fn user() -> User {
        User {
            id: Snowflake(1),
            username: "alt_fixture".to_owned(),
            global_name: None,
            avatar: None,
            bot: false,
        }
    }

    fn ready_event(guilds: usize, unavailable: usize, dms: usize) -> GatewayEvent {
        let guild = |id| Guild {
            id: Snowflake(id),
            name: String::new(),
            icon: None,
            owner_id: None,
            roles: Vec::new(),
            channels: Vec::new(),
            members: Vec::new(),
            member_count: 0,
        };
        let dm = |id| Channel {
            id: Snowflake(id),
            kind: ChannelKind::Dm,
            guild_id: None,
            name: None,
            position: None,
            parent_id: None,
            permission_overwrites: Vec::new(),
            recipients: Vec::new(),
            recipient_ids: Vec::new(),
            last_message_id: None,
        };
        GatewayEvent::Dispatch {
            sequence: 1,
            event: Dispatch::Ready(Box::new(Ready {
                session_id: SessionId::new("fixture-session".to_owned()),
                user: user(),
                users: vec![user()],
                guilds: (0..guilds as u64).map(guild).collect(),
                unavailable_guilds: (0..unavailable as u64).map(Snowflake).collect(),
                private_channels: (0..dms as u64).map(dm).collect(),
            })),
        }
    }

    #[test]
    fn states_map_to_statuses_and_ready_reports_the_counts_from_ready() {
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.apply(GatewayEvent::State(ConnectionState::Connecting {
                attempt: 0
            })),
            Some(GatewayStatus::Connecting { attempt: 0 })
        );
        for state in [ConnectionState::AwaitHello, ConnectionState::Identifying] {
            assert_eq!(
                tracker.apply(GatewayEvent::State(state)),
                Some(GatewayStatus::SigningIn)
            );
        }
        // READY itself is not a status change; the Ready state that follows is.
        assert_eq!(tracker.apply(ready_event(3, 1, 2)), None);
        let status = tracker
            .apply(GatewayEvent::State(ConnectionState::Ready))
            .unwrap();
        assert_eq!(
            status,
            GatewayStatus::Ready(Counts {
                guilds: 3,
                unavailable: 1,
                direct_messages: 2
            })
        );
        assert!(status.describe().starts_with("Connected to Discord"));
        assert_eq!(
            status.describe(),
            "Connected to Discord: 3 servers, 2 direct messages. 1 servers are unavailable."
        );
    }

    #[test]
    fn reconnecting_is_never_shown_as_connected_and_resume_keeps_the_counts() {
        let mut tracker = Tracker::default();
        let _ = tracker.apply(ready_event(2, 0, 1));
        let lost = tracker
            .apply(GatewayEvent::State(ConnectionState::Reconnecting {
                attempt: 2,
                delay: Duration::from_millis(1_500),
                reason: ReconnectReason::HeartbeatTimeout,
            }))
            .unwrap();
        assert!(!lost.describe().contains("Connected"));
        assert_eq!(
            lost.describe(),
            "Connection lost (Discord stopped responding). Reconnecting in 2 seconds (attempt 2)."
        );
        assert_eq!(
            tracker.apply(GatewayEvent::State(ConnectionState::Resuming)),
            Some(GatewayStatus::Resuming)
        );
        // RESUMED is replay bookkeeping, not a status; the counts survive it.
        assert_eq!(
            tracker.apply(GatewayEvent::Dispatch {
                sequence: 9,
                event: Dispatch::Resumed
            }),
            None
        );
        assert_eq!(
            tracker.apply(GatewayEvent::State(ConnectionState::Ready)),
            Some(GatewayStatus::Ready(Counts {
                guilds: 2,
                unavailable: 0,
                direct_messages: 1
            }))
        );
    }

    #[test]
    fn a_new_ready_replaces_the_old_counts() {
        let mut tracker = Tracker::default();
        let _ = tracker.apply(ready_event(5, 0, 5));
        let _ = tracker.apply(ready_event(1, 0, 0));
        assert_eq!(
            tracker.apply(GatewayEvent::State(ConnectionState::Ready)),
            Some(GatewayStatus::Ready(Counts {
                guilds: 1,
                unavailable: 0,
                direct_messages: 0
            }))
        );
    }

    #[test]
    fn terminal_states_explain_themselves() {
        let mut tracker = Tracker::default();
        let auth = tracker
            .apply(GatewayEvent::State(ConnectionState::AuthenticationRequired))
            .unwrap();
        assert!(auth.describe().contains("Log in again"));
        let stopped = tracker
            .apply(GatewayEvent::State(ConnectionState::Stopped(
                StopReason::ActionRequired("REQUIRE_VERIFIED_EMAIL".to_owned()),
            )))
            .unwrap();
        assert!(stopped.describe().contains("official Discord client"));
        assert!(!stopped.describe().contains("Connected"));
    }

    #[test]
    fn delays_are_described_without_a_countdown() {
        assert_eq!(describe_delay(Duration::ZERO), "a moment");
        assert_eq!(describe_delay(Duration::from_millis(750)), "1 second");
        assert_eq!(describe_delay(Duration::from_secs(32)), "32 seconds");
    }
}
