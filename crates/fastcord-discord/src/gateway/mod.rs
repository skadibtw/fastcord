//! The main user Gateway: API v10, JSON, `compress=zlib-stream` (SPEC §4).
//!
//! One [`Gateway`] is one account's connection lifecycle. It owns a task that
//! discovers the URL, connects, heartbeats, Identifies or Resumes, decodes
//! compressed events, normalizes READY, and reconnects with bounded jittered
//! backoff. Everything it learns reaches the single consumer (the reducer) as
//! ordered [`GatewayEvent`]s through a byte-bounded queue.
//!
//! Dropping the handle ends the task; it closes the socket with code 1000,
//! which ends the Gateway session so the account does not linger as online.
//!
//! Authentication failure (close code 4004, or a 401 on discovery or any other
//! request of the account) stops all automatic attempts and ends in
//! [`ConnectionState::AuthenticationRequired`]. Challenges and other things
//! only the account holder can resolve end in [`ConnectionState::Stopped`];
//! fastcord never tries to satisfy or bypass them.

mod capabilities;
mod compression;
mod connection;
mod decode;
mod event;
mod outbox;
mod pacing;
mod profile;
mod transport;
mod url;
mod wire;

#[cfg(test)]
mod live;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;

use tokio::sync::oneshot;

pub use capabilities::{Capability, SELECTED as SELECTED_CAPABILITIES, selected_value};
pub use event::{
    ChannelUnread, ConnectionState, Dispatch, GatewayEvent, GuildCreate, GuildDelete,
    MessageDelete, PassiveUpdate, Ready, ReadySupplemental, ReconnectReason, SessionId, StopReason,
    SupplementalGuild,
};
pub use profile::{
    BUNDLED_BUILD_NUMBER, BuildNumber, BuildSource, ClientProperties, HostOs, PROFILE_VERSION,
};

use crate::{RestClient, UserToken};
use outbox::EventReceiver;
use pacing::{JitterSource, OsJitter};
use transport::{LiveTransport, Transport};

/// A running Gateway connection. Dropping it shuts the connection down.
pub struct Gateway {
    events: EventReceiver,
    /// Dropping the sender is the shutdown signal.
    _shutdown: oneshot::Sender<()>,
}

impl fmt::Debug for Gateway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Gateway")
    }
}

impl Gateway {
    /// Starts the connection task. Must be called inside a Tokio runtime, and
    /// only because of an explicit user action (login or restore). `rest` is the
    /// account's client, used for discovery and shared for authentication stop;
    /// `locale` is the system locale reported in the client profile.
    pub fn start(rest: RestClient, token: Arc<UserToken>, locale: impl Into<String>) -> Self {
        start_with(
            Arc::new(LiveTransport::new(rest)),
            token,
            locale.into(),
            OsJitter,
        )
    }

    /// The next event in order, or `None` after a terminal state has been
    /// delivered and the queue is drained.
    pub async fn next_event(&mut self) -> Option<GatewayEvent> {
        self.events.recv().await
    }
}

fn start_with<T: Transport, J: JitterSource>(
    transport: Arc<T>,
    token: Arc<UserToken>,
    locale: String,
    jitter: J,
) -> Gateway {
    let (outbox, events) = outbox::channel();
    let (shutdown, shutdown_signal) = oneshot::channel();
    tokio::spawn(connection::run(
        transport,
        token,
        locale,
        jitter,
        outbox,
        shutdown_signal,
    ));
    Gateway {
        events,
        _shutdown: shutdown,
    }
}
