//! The main user Gateway: API v9 (the official client's, ADR 0005),
//! JSON, `compress=zlib-stream` (SPEC §4).
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
pub(crate) mod profile;
mod stream;
mod subscription;
mod transport;
mod url;
mod voice;
mod wire;

#[cfg(test)]
mod live;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, watch};

pub use capabilities::{Capability, SELECTED as SELECTED_CAPABILITIES, selected_value};
pub use event::{
    ChannelUnread, ConnectionState, Dispatch, GatewayEvent, GroupId, GuildCreate, GuildDelete,
    GuildEmojisUpdate, GuildMemberEvent, GuildMemberRemove, GuildMuteUpdate, GuildRoleDelete,
    GuildRoleEvent, GuildUpdate, ListGroup, ListRow, MemberListId, MemberListOp, MemberListUpdate,
    MessageAck, MessageDelete, MessageDeleteBulk, MessagePollVote, MessageReaction,
    MessageReactionRemoveAll, MessageReactionRemoveEmoji, MuteSettings, NotifyLevel, PassiveUpdate,
    ReadStateEntry, Ready, ReadyRead, ReadySupplemental, ReconnectReason, RelationshipUpdate,
    SessionId, SoundboardPlay, StopReason, SupplementalGuild, ThreadListSync, ThreadMemberUpdate,
    ThreadMembersUpdate, TypingStart, VoiceStateUpdate,
};
pub use event::{MAX_PRESENCES, MAX_RELATIONSHIPS, PresenceUpdate};
pub use profile::{
    BUNDLED_BUILD_NUMBER, BUNDLED_NATIVE_BUILD_NUMBER, CHROMIUM_VERSION, ClientProperties,
    ELECTRON_VERSION, Host, HostOs, Identity, PIN_HOST_VERSION, PROFILE_VERSION, Versions,
    is_installation_id, resolve_versions,
};
pub use stream::{StreamControl, StreamSignalQueueError};
pub use subscription::{
    GuildSubscription, MAX_MEMBERS_PER_GUILD, MAX_RANGES, MemberRange, RANGE_BLOCK,
    SubscriptionTarget, Subscriptions,
};
pub use voice::VoiceStateControl;

use crate::{RestClient, UserToken};
use outbox::EventReceiver;
use pacing::{JitterSource, OsJitter};
use transport::{LiveTransport, Transport};

/// A running Gateway connection. Dropping it shuts the connection down.
pub struct Gateway {
    events: EventReceiver,
    subscriptions: Subscriptions,
    voice: VoiceStateControl,
    stream: StreamControl,
    /// Dropping the sender is the shutdown signal.
    _shutdown: oneshot::Sender<()>,
    closer: GatewayCloser,
}

/// Closes the Gateway cleanly (close code 1000, so the session ends at once)
/// and waits for the connection task to finish. Clones share one connection;
/// holding one does not keep it alive.
#[derive(Clone)]
pub struct GatewayCloser {
    request: Arc<watch::Sender<bool>>,
    done: watch::Receiver<bool>,
}

impl GatewayCloser {
    /// Asks the connection to close and waits up to `wait` for its task to end.
    /// Returns whether it ended in time. Safe to call again or after the end.
    pub async fn close(&self, wait: Duration) -> bool {
        self.request.send_replace(true);
        let mut done = self.done.clone();
        // A dropped sender means the task is gone too.
        tokio::time::timeout(wait, async {
            done.wait_for(|ended| *ended).await.map(|_| ())
        })
        .await
        .is_ok()
    }
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
    /// Identify sends its [`Identity`], and the installation id READY issues is
    /// adopted there, so later requests carry it too.
    pub fn start(rest: RestClient, token: Arc<UserToken>) -> Self {
        let identity = rest.identity().clone();
        start_with(
            Arc::new(LiveTransport::new(rest)),
            token,
            identity,
            OsJitter,
        )
    }

    /// The next event in order, or `None` after a terminal state has been
    /// delivered and the queue is drained.
    pub async fn next_event(&mut self) -> Option<GatewayEvent> {
        self.events.recv().await
    }

    /// The handle through which the consumer states which guilds, member-list
    /// ranges, and members it needs (opcode 37). Clones share one target.
    pub fn subscriptions(&self) -> Subscriptions {
        self.subscriptions.clone()
    }

    /// The handle through which the consumer joins, moves between, or leaves
    /// voice channels and calls (opcode 4). Clones share one target.
    pub fn voice_state(&self) -> VoiceStateControl {
        self.voice.clone()
    }

    /// The handle for creating, watching, pausing, and ending streams.
    pub fn streams(&self) -> StreamControl {
        self.stream.clone()
    }

    /// The handle that closes this connection cleanly on Quit.
    pub fn closer(&self) -> GatewayCloser {
        self.closer.clone()
    }
}

/// Decodes a recorded payload the way the connection would, for reducer tests.
#[cfg(test)]
pub(crate) fn decode_fixture(name: &str, raw: &str) -> Dispatch {
    match decode::decode_dispatch(name, raw) {
        decode::Decoded::Event(event) => event,
        decode::Decoded::Ready(ready) => Dispatch::Ready(Box::new(ready.ready)),
        _ => panic!("{name} fixture did not decode"),
    }
}

fn start_with<T: Transport, J: JitterSource>(
    transport: Arc<T>,
    token: Arc<UserToken>,
    identity: Identity,
    jitter: J,
) -> Gateway {
    let (outbox, events) = outbox::channel();
    let (subscriptions, wanted) = Subscriptions::new();
    let (voice, voice_wanted) = VoiceStateControl::new();
    let (stream, stream_wanted) = StreamControl::new();
    let (shutdown, shutdown_signal) = oneshot::channel();
    let (request, close) = watch::channel(false);
    let (ended, done) = watch::channel(false);
    let task = connection::run(
        transport,
        token,
        identity,
        jitter,
        outbox,
        connection::Wanted {
            subscriptions: wanted,
            voice: voice_wanted,
            stream: stream_wanted,
            close,
        },
        shutdown_signal,
    );
    tokio::spawn(async move {
        task.await;
        let _ = ended.send(true);
    });
    Gateway {
        closer: GatewayCloser {
            request: Arc::new(request),
            done,
        },
        events,
        subscriptions,
        voice,
        stream,
        _shutdown: shutdown,
    }
}
