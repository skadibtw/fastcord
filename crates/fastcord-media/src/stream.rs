//! Independent Go Live stream connection correlation (SPEC §8.1 / M24).
//!
//! A stream is correlated by the server-advertised owner/location key. RTC IDs
//! arrive with STREAM_CREATE; the token and endpoint arrive with
//! STREAM_SERVER_UPDATE.

use std::collections::HashMap;
use std::fmt;

use fastcord_model::{Snowflake, StreamCreate, StreamKey, StreamServerUpdate, VoiceToken};

use crate::session::Generation;

/// Credentials for one independent stream voice-v8 connection.
#[derive(Clone)]
pub struct StreamCredentials {
    pub generation: Generation,
    pub key: StreamKey,
    pub user_id: Snowflake,
    pub session_id: String,
    pub rtc_server_id: Snowflake,
    pub rtc_channel_id: Snowflake,
    /// Discord's stream MLS group identity (`rtc_server_id - 1`).
    pub mls_group_id: u64,
    pub token: VoiceToken,
    pub endpoint: String,
}

impl fmt::Debug for StreamCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamCredentials")
            .field("generation", &self.generation)
            .field("key", &self.key)
            .field("user_id", &self.user_id)
            .field("session_id", &"[REDACTED]")
            .field("rtc_server_id", &self.rtc_server_id)
            .field("rtc_channel_id", &self.rtc_channel_id)
            .field("mls_group_id", &self.mls_group_id)
            .field("token", &self.token)
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl StreamCredentials {
    /// Converts the independent RTC identity into the common v8 transport
    /// credentials while keeping the stream MLS group override explicit.
    pub fn voice_credentials(self) -> crate::session::VoiceCredentials {
        crate::session::VoiceCredentials {
            generation: self.generation,
            server_id: self.rtc_server_id,
            channel_id: self.rtc_channel_id,
            user_id: self.user_id,
            session_id: self.session_id,
            token: self.token,
            endpoint: self.endpoint,
            dave_group_id: Some(self.mls_group_id),
        }
    }
}

#[derive(Debug)]
pub enum StreamCorrelation {
    Ignored,
    Waiting,
    Connect(StreamCredentials),
    Reallocating {
        key: StreamKey,
        generation: Generation,
    },
    Ended {
        key: StreamKey,
        generation: Generation,
    },
    /// Invalid stream identity or unusable server credentials; fail closed.
    Invalid {
        key: Option<StreamKey>,
        generation: Generation,
    },
}

#[derive(Clone, Debug)]
struct Pending {
    create: Option<StreamCreate>,
    server: Option<StreamServerUpdate>,
    generation: Generation,
    connected: bool,
}

/// Pairs STREAM_CREATE and STREAM_SERVER_UPDATE in either order.
#[derive(Debug)]
pub struct StreamCorrelator {
    user_id: Snowflake,
    session_id: String,
    generation: u64,
    streams: HashMap<StreamKey, Pending>,
}

impl StreamCorrelator {
    pub fn new(user_id: Snowflake, session_id: impl Into<String>) -> Self {
        Self {
            user_id,
            session_id: session_id.into(),
            generation: 0,
            streams: HashMap::new(),
        }
    }

    /// Feeds STREAM_CREATE and retains its RTC IDs for the separate session.
    pub fn create(&mut self, event: &StreamCreate) -> StreamCorrelation {
        let Some(key) = StreamKey::from_wire(&event.stream_key) else {
            return StreamCorrelation::Invalid {
                key: None,
                generation: self.bump(),
            };
        };
        let generation = self.bump();
        let previous = self.streams.remove(&key);
        let server = previous
            .filter(|pending| pending.create.is_none())
            .and_then(|pending| pending.server);
        self.streams.insert(
            key.clone(),
            Pending {
                create: Some(event.clone()),
                server,
                generation,
                connected: false,
            },
        );
        self.try_connect(key)
    }

    /// Feeds STREAM_SERVER_UPDATE; changed credentials advance the stream's
    /// generation. A null endpoint retires the current RTC connection.
    pub fn server_update(&mut self, event: &StreamServerUpdate) -> StreamCorrelation {
        let Some(key) = StreamKey::from_wire(&event.stream_key) else {
            return StreamCorrelation::Invalid {
                key: None,
                generation: self.bump(),
            };
        };
        let unchanged = self
            .streams
            .get(&key)
            .and_then(|pending| pending.server.as_ref())
            .is_some_and(|server| server == event);
        if unchanged {
            return StreamCorrelation::Waiting;
        }

        let generation = self.bump();
        let pending = self.streams.entry(key.clone()).or_insert(Pending {
            create: None,
            server: None,
            generation,
            connected: false,
        });
        pending.server = Some(event.clone());
        pending.generation = generation;
        pending.connected = false;
        if event.endpoint.as_deref().is_none_or(str::is_empty) {
            return StreamCorrelation::Reallocating { key, generation };
        }
        self.try_connect(key)
    }

    /// Retires one stream after a local unwatch, before a delayed server event
    /// can restart it.
    pub fn retire(&mut self, stream_key: &str) -> StreamCorrelation {
        let Some(key) = StreamKey::from_wire(stream_key) else {
            return StreamCorrelation::Ignored;
        };
        let Some(pending) = self.streams.remove(&key) else {
            return StreamCorrelation::Ignored;
        };
        StreamCorrelation::Ended {
            key,
            generation: self.bump_after(pending.generation),
        }
    }

    /// Ends only the named stream. Late deletes cannot disturb another stream
    /// or the parent voice session.
    pub fn delete(&mut self, stream_key: &str) -> StreamCorrelation {
        self.retire(stream_key)
    }

    pub fn is_active(&self, key: &StreamKey) -> bool {
        self.streams.contains_key(key)
    }

    fn try_connect(&mut self, key: StreamKey) -> StreamCorrelation {
        let Some(pending) = self.streams.get_mut(&key) else {
            return StreamCorrelation::Ignored;
        };
        let (Some(create), Some(server)) = (pending.create.as_ref(), pending.server.as_ref())
        else {
            return StreamCorrelation::Waiting;
        };
        if pending.connected {
            return StreamCorrelation::Waiting;
        }
        let Some(endpoint) = server
            .endpoint
            .as_deref()
            .filter(|endpoint| !endpoint.is_empty())
        else {
            return StreamCorrelation::Waiting;
        };
        let Some(mls_group_id) = create.rtc_server_id.0.checked_sub(1) else {
            return StreamCorrelation::Invalid {
                key: Some(key),
                generation: pending.generation,
            };
        };
        if create.rtc_channel_id.0 == 0 {
            return StreamCorrelation::Invalid {
                key: Some(key),
                generation: pending.generation,
            };
        }
        pending.connected = true;
        StreamCorrelation::Connect(StreamCredentials {
            generation: pending.generation,
            key,
            user_id: self.user_id,
            session_id: self.session_id.clone(),
            rtc_server_id: create.rtc_server_id,
            rtc_channel_id: create.rtc_channel_id,
            mls_group_id,
            token: server.token.clone(),
            endpoint: endpoint.to_owned(),
        })
    }

    fn bump(&mut self) -> Generation {
        self.generation = self.generation.saturating_add(1);
        Generation(self.generation)
    }

    fn bump_after(&mut self, generation: Generation) -> Generation {
        let next = self.bump();
        Generation(next.0.max(generation.0.saturating_add(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> &'static str {
        "guild:123:456:1"
    }

    fn create(rtc_server_id: u64) -> StreamCreate {
        StreamCreate {
            stream_key: key().into(),
            rtc_server_id: Snowflake(rtc_server_id),
            rtc_channel_id: Snowflake(789),
        }
    }

    fn server(endpoint: Option<&str>, token: &str) -> StreamServerUpdate {
        StreamServerUpdate {
            stream_key: key().into(),
            token: VoiceToken::new(token.into()),
            endpoint: endpoint.map(str::to_owned),
        }
    }

    fn connected(result: StreamCorrelation) -> StreamCredentials {
        match result {
            StreamCorrelation::Connect(credentials) => credentials,
            other => panic!("expected connect, got {other:?}"),
        }
    }

    #[test]
    fn pairs_documented_payloads_in_both_orders_and_preserves_rtc_identities() {
        let mut first_order = StreamCorrelator::new(Snowflake(1), "main-session");
        assert!(matches!(
            first_order.server_update(&server(Some("rtc.example.test:443"), "secret-a")),
            StreamCorrelation::Waiting
        ));
        let first = connected(first_order.create(&create(500)));
        assert_eq!(first.mls_group_id, 499);
        assert_eq!(first.rtc_server_id, Snowflake(500));
        assert_eq!(first.rtc_channel_id, Snowflake(789));
        assert_eq!(first.session_id, "main-session");
        assert_eq!(first.generation, Generation(2));
        let rtc = first.voice_credentials();
        assert_eq!(rtc.server_id, Snowflake(500));
        assert_eq!(rtc.channel_id, Snowflake(789));
        assert_eq!(rtc.dave_group_id, Some(499));

        let mut reverse_order = StreamCorrelator::new(Snowflake(1), "main-session");
        assert!(matches!(
            reverse_order.create(&create(501)),
            StreamCorrelation::Waiting
        ));
        let second = connected(
            reverse_order.server_update(&server(Some("rtc.example.test:443"), "secret-b")),
        );
        assert_eq!(second.generation, Generation(2));
        assert_eq!(second.mls_group_id, 500);
    }

    #[test]
    fn invalid_rtc_identity_fails_closed_and_reallocation_ends_only_that_stream() {
        let mut correlator = StreamCorrelator::new(Snowflake(1), "main-session");
        correlator.create(&create(0));
        assert!(matches!(
            correlator.server_update(&server(Some("rtc.example.test:443"), "secret")),
            StreamCorrelation::Invalid { .. }
        ));
        correlator.create(&create(500));
        assert!(matches!(
            correlator.server_update(&server(Some("rtc.example.test:443"), "secret")),
            StreamCorrelation::Connect(_)
        ));
        assert!(matches!(
            correlator.server_update(&server(None, "new-secret")),
            StreamCorrelation::Reallocating { .. }
        ));
    }

    #[test]
    fn retire_prevents_a_late_server_update_from_restarting_the_stream() {
        let mut correlator = StreamCorrelator::new(Snowflake(1), "main-session");
        correlator.create(&create(500));
        let _ = correlator.server_update(&server(Some("rtc.example.test:443"), "secret"));
        assert!(matches!(
            correlator.retire(key()),
            StreamCorrelation::Ended { .. }
        ));
        assert!(matches!(
            correlator.server_update(&server(Some("rtc.example.test:443"), "late-secret")),
            StreamCorrelation::Waiting
        ));
    }

    #[test]
    fn stream_lifetime_does_not_mutate_parent_voice_generation() {
        let mut parent = crate::session::JoinCorrelator::new(Snowflake(1));
        let parent_generation = parent.join(Some(Snowflake(123)), Snowflake(456));
        let mut stream = StreamCorrelator::new(Snowflake(1), "main-session");
        let _ = stream.server_update(&server(Some("rtc.example.test:443"), "secret"));
        let stream_credentials = connected(stream.create(&create(500)));
        assert_ne!(stream_credentials.generation, parent_generation);
        assert_eq!(parent.generation(), parent_generation);
        let _ = stream.delete(key());
        assert_eq!(parent.generation(), parent_generation);
    }
}
