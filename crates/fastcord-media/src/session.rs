//! Correlating a voice join (SPEC §6.1 steps 1–2).
//!
//! After main Gateway opcode 4, the account's own VOICE_STATE_UPDATE carries
//! the session ID and VOICE_SERVER_UPDATE carries the voice token and
//! endpoint, in either order. [`JoinCorrelator`] pairs them for the current
//! join attempt only: every attempt and every credential change gets a fresh
//! [`Generation`], so events and connections from an earlier join can never
//! overwrite a later one. A voice token is used for exactly one generation;
//! moving channel always waits for a new one.

use std::fmt;

use fastcord_model::{Snowflake, VoiceServerUpdate, VoiceState, VoiceToken};

/// Identifies one voice connection attempt. Strictly increasing per
/// correlator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(pub u64);

/// Everything needed to open one voice connection.
#[derive(Clone)]
pub struct VoiceCredentials {
    pub generation: Generation,
    /// The guild, or the private channel for DM/group calls.
    pub server_id: Snowflake,
    pub channel_id: Snowflake,
    pub user_id: Snowflake,
    /// The main Gateway session ID; together with the token it authenticates.
    pub session_id: String,
    pub token: VoiceToken,
    /// `host[:port]` without a scheme, as VOICE_SERVER_UPDATE sends it.
    pub endpoint: String,
}

impl fmt::Debug for VoiceCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceCredentials")
            .field("generation", &self.generation)
            .field("server_id", &self.server_id)
            .field("channel_id", &self.channel_id)
            .field("user_id", &self.user_id)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// What the consumer should do after feeding an event to the correlator.
#[derive(Debug)]
pub enum Correlation {
    /// Not for the current attempt (another user, guild, or an old join).
    Ignored,
    /// Recorded; the other half of the credentials is still missing.
    Waiting,
    /// Open a new voice connection with these credentials and drop any older
    /// one; the generation is new.
    Connect(VoiceCredentials),
    /// The voice server is being reallocated: drop the connection of this
    /// attempt and wait for the next VOICE_SERVER_UPDATE.
    Reallocating,
    /// The account is no longer in voice (left elsewhere, kicked, or the
    /// channel went away): the attempt is over.
    Ended,
}

struct Attempt {
    guild_id: Option<Snowflake>,
    channel_id: Snowflake,
    session_id: Option<String>,
    server: Option<(VoiceToken, String)>,
    /// Whether credentials were handed out at least once; from then on a
    /// state in another channel of the same server is a server-side move.
    connected: bool,
}

impl fmt::Debug for Attempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attempt")
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .field(
                "session_id",
                &self.session_id.as_ref().map(|_| "[REDACTED]"),
            )
            .field("server", &self.server)
            .field("connected", &self.connected)
            .finish()
    }
}

/// Pairs VOICE_STATE_UPDATE and VOICE_SERVER_UPDATE for one account.
#[derive(Debug)]
pub struct JoinCorrelator {
    user_id: Snowflake,
    generation: Generation,
    attempt: Option<Attempt>,
}

impl JoinCorrelator {
    pub const fn new(user_id: Snowflake) -> Self {
        Self {
            user_id,
            generation: Generation(0),
            attempt: None,
        }
    }

    /// Starts a join (or move) to `channel_id` in `guild_id` (`None` for a
    /// private call) right before sending opcode 4. Credentials of any earlier
    /// attempt are discarded.
    pub fn join(&mut self, guild_id: Option<Snowflake>, channel_id: Snowflake) -> Generation {
        self.attempt = Some(Attempt {
            guild_id,
            channel_id,
            session_id: None,
            server: None,
            connected: false,
        });
        self.bump()
    }

    /// Ends the current attempt right before sending the leave opcode 4;
    /// later events for it are ignored.
    pub fn leave(&mut self) {
        self.attempt = None;
        self.bump();
    }

    /// The generation of the newest join or credential change.
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    pub fn is_joining(&self) -> bool {
        self.attempt.is_some()
    }

    /// Feeds a VOICE_STATE_UPDATE.
    pub fn voice_state(&mut self, state: &VoiceState) -> Correlation {
        if state.user_id != self.user_id {
            return Correlation::Ignored;
        }
        let Some(attempt) = self.attempt.as_mut() else {
            return Correlation::Ignored;
        };
        if state.guild_id != attempt.guild_id {
            return Correlation::Ignored;
        }
        match state.channel_id {
            None => {
                if attempt.guild_id.is_none() && !attempt.connected {
                    // A private-call state without channel says nothing about
                    // this pending call.
                    return Correlation::Ignored;
                }
                self.attempt = None;
                self.bump();
                Correlation::Ended
            }
            Some(channel) if channel != attempt.channel_id && !attempt.connected => {
                // A late state from an earlier join.
                Correlation::Ignored
            }
            Some(channel) => {
                // A different channel after connecting is a server-side move.
                // Its new token arrives in a VOICE_SERVER_UPDATE (before or
                // after this state); that update, not the move, reconnects.
                attempt.channel_id = channel;
                let session_changed =
                    attempt.session_id.as_deref() != Some(state.session_id.as_str());
                attempt.session_id = Some(state.session_id.clone());
                if session_changed {
                    self.try_connect()
                } else {
                    Correlation::Waiting
                }
            }
        }
    }

    /// Feeds a VOICE_SERVER_UPDATE.
    pub fn voice_server(&mut self, update: &VoiceServerUpdate) -> Correlation {
        let Some(attempt) = self.attempt.as_mut() else {
            return Correlation::Ignored;
        };
        let ours = match attempt.guild_id {
            Some(guild) => update.guild_id == Some(guild),
            None => update.guild_id.is_none() && update.channel_id == Some(attempt.channel_id),
        };
        if !ours {
            return Correlation::Ignored;
        }
        match update.endpoint.as_deref() {
            None | Some("") => {
                attempt.server = None;
                self.bump();
                Correlation::Reallocating
            }
            Some(endpoint) => {
                if attempt
                    .server
                    .as_ref()
                    .is_some_and(|(token, old)| *token == update.token && old == endpoint)
                {
                    return Correlation::Waiting;
                }
                attempt.server = Some((update.token.clone(), endpoint.to_owned()));
                self.try_connect()
            }
        }
    }

    fn try_connect(&mut self) -> Correlation {
        let Some(attempt) = self.attempt.as_mut() else {
            return Correlation::Ignored;
        };
        let (Some(session_id), Some((token, endpoint))) = (&attempt.session_id, &attempt.server)
        else {
            return Correlation::Waiting;
        };
        attempt.connected = true;
        let credentials = VoiceCredentials {
            generation: Generation(self.generation.0 + 1),
            server_id: attempt.guild_id.unwrap_or(attempt.channel_id),
            channel_id: attempt.channel_id,
            user_id: self.user_id,
            session_id: session_id.clone(),
            token: token.clone(),
            endpoint: endpoint.clone(),
        };
        self.bump();
        Correlation::Connect(credentials)
    }

    fn bump(&mut self) -> Generation {
        self.generation = Generation(self.generation.0 + 1);
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: Snowflake = Snowflake(10);
    const GUILD: Snowflake = Snowflake(41);
    const LOBBY: Snowflake = Snowflake(127);
    const OTHER: Snowflake = Snowflake(128);
    const DM: Snowflake = Snowflake(900);

    fn state(
        user: Snowflake,
        guild: Option<Snowflake>,
        channel: Option<Snowflake>,
        session: &str,
    ) -> VoiceState {
        VoiceState {
            guild_id: guild,
            channel_id: channel,
            user_id: user,
            session_id: session.to_owned(),
            deaf: false,
            mute: false,
            self_deaf: false,
            self_mute: false,
            self_stream: false,
            self_video: false,
            suppress: false,
        }
    }

    fn server(
        guild: Option<Snowflake>,
        channel: Option<Snowflake>,
        token: &str,
        endpoint: Option<&str>,
    ) -> VoiceServerUpdate {
        VoiceServerUpdate {
            token: VoiceToken::new(token.to_owned()),
            guild_id: guild,
            channel_id: channel,
            endpoint: endpoint.map(str::to_owned),
        }
    }

    fn connect(correlation: Correlation) -> VoiceCredentials {
        match correlation {
            Correlation::Connect(credentials) => credentials,
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    #[test]
    fn either_order_yields_one_connection_with_both_halves() {
        for state_first in [true, false] {
            let mut join = JoinCorrelator::new(ME);
            let generation = join.join(Some(GUILD), LOBBY);
            let s = state(ME, Some(GUILD), Some(LOBBY), "session-a");
            let v = server(Some(GUILD), None, "token-a", Some("voice.example:443"));
            let credentials = if state_first {
                assert!(matches!(join.voice_state(&s), Correlation::Waiting));
                connect(join.voice_server(&v))
            } else {
                assert!(matches!(join.voice_server(&v), Correlation::Waiting));
                connect(join.voice_state(&s))
            };
            assert!(credentials.generation > generation);
            assert_eq!(credentials.generation, join.generation());
            assert_eq!(credentials.server_id, GUILD);
            assert_eq!(credentials.channel_id, LOBBY);
            assert_eq!(credentials.user_id, ME);
            assert_eq!(credentials.session_id, "session-a");
            assert_eq!(credentials.token.expose_secret(), "token-a");
            assert_eq!(credentials.endpoint, "voice.example:443");
            // Repeats change nothing.
            assert!(matches!(join.voice_state(&s), Correlation::Waiting));
            assert!(matches!(join.voice_server(&v), Correlation::Waiting));
            let debug = format!("{credentials:?}");
            assert!(
                !debug.contains("token-a") && !debug.contains("session-a"),
                "{debug}"
            );
        }
    }

    #[test]
    fn correlator_debug_redacts_session_credentials() {
        let mut join = JoinCorrelator::new(ME);
        join.join(Some(GUILD), LOBBY);
        join.voice_state(&state(ME, Some(GUILD), Some(LOBBY), "fixture-session"));
        join.voice_server(&server(
            Some(GUILD),
            None,
            "fixture-token",
            Some("voice.example:443"),
        ));
        let shown = format!("{join:?}");
        assert!(!shown.contains("fixture-session"));
        assert!(!shown.contains("fixture-token"));
    }

    #[test]
    fn other_users_guilds_and_stale_channels_are_ignored() {
        let mut join = JoinCorrelator::new(ME);
        assert!(matches!(
            join.voice_state(&state(ME, Some(GUILD), Some(LOBBY), "s")),
            Correlation::Ignored
        ));
        join.join(Some(GUILD), LOBBY);
        assert!(matches!(
            join.voice_state(&state(Snowflake(11), Some(GUILD), Some(LOBBY), "s")),
            Correlation::Ignored
        ));
        assert!(matches!(
            join.voice_state(&state(ME, Some(Snowflake(42)), Some(LOBBY), "s")),
            Correlation::Ignored
        ));
        assert!(matches!(
            join.voice_server(&server(Some(Snowflake(42)), None, "t", Some("e"))),
            Correlation::Ignored
        ));
        // A late state for the previous channel before this join connected.
        assert!(matches!(
            join.voice_state(&state(ME, Some(GUILD), Some(OTHER), "s")),
            Correlation::Ignored
        ));
        assert!(matches!(
            join.voice_server(&server(Some(GUILD), None, "t", Some("e"))),
            Correlation::Waiting
        ));
        assert_eq!(
            connect(join.voice_state(&state(ME, Some(GUILD), Some(LOBBY), "s"))).channel_id,
            LOBBY
        );
    }

    #[test]
    fn a_new_join_never_reuses_the_previous_token_or_generation() {
        let mut join = JoinCorrelator::new(ME);
        join.join(Some(GUILD), LOBBY);
        join.voice_state(&state(ME, Some(GUILD), Some(LOBBY), "s"));
        let first = connect(join.voice_server(&server(Some(GUILD), None, "t1", Some("same:443"))));
        // Moving to another channel: same endpoint, but a new token is needed.
        let moving = join.join(Some(GUILD), OTHER);
        assert!(moving > first.generation);
        assert!(matches!(
            join.voice_state(&state(ME, Some(GUILD), Some(OTHER), "s")),
            Correlation::Waiting
        ));
        let second = connect(join.voice_server(&server(Some(GUILD), None, "t2", Some("same:443"))));
        assert!(second.generation > moving);
        assert_eq!(second.channel_id, OTHER);
        assert_eq!(second.token.expose_secret(), "t2");
    }

    #[test]
    fn a_server_side_move_waits_for_the_new_token() {
        let mut join = JoinCorrelator::new(ME);
        join.join(Some(GUILD), LOBBY);
        join.voice_state(&state(ME, Some(GUILD), Some(LOBBY), "s"));
        let first = connect(join.voice_server(&server(Some(GUILD), None, "t1", Some("a:443"))));
        assert!(matches!(
            join.voice_state(&state(ME, Some(GUILD), Some(OTHER), "s")),
            Correlation::Waiting
        ));
        let moved = connect(join.voice_server(&server(Some(GUILD), None, "t2", Some("a:443"))));
        assert_eq!(moved.channel_id, OTHER);
        assert!(moved.generation > first.generation);

        // The new token may also come first: it reconnects at once, and the
        // later state only relabels the channel without a second connection.
        let token_first =
            connect(join.voice_server(&server(Some(GUILD), None, "t3", Some("a:443"))));
        assert!(token_first.generation > moved.generation);
        assert!(matches!(
            join.voice_state(&state(ME, Some(GUILD), Some(LOBBY), "s")),
            Correlation::Waiting
        ));
        let generation = join.generation();
        let relabelled =
            connect(join.voice_server(&server(Some(GUILD), None, "t4", Some("a:443"))));
        assert_eq!(relabelled.channel_id, LOBBY);
        assert!(relabelled.generation > generation);
    }

    #[test]
    fn reallocation_and_a_new_server_reconnect_with_a_new_generation() {
        let mut join = JoinCorrelator::new(ME);
        join.join(Some(GUILD), LOBBY);
        join.voice_state(&state(ME, Some(GUILD), Some(LOBBY), "s"));
        let first = connect(join.voice_server(&server(Some(GUILD), None, "t1", Some("a:443"))));
        assert!(matches!(
            join.voice_server(&server(Some(GUILD), None, "t1", None)),
            Correlation::Reallocating
        ));
        let second = connect(join.voice_server(&server(Some(GUILD), None, "t2", Some("b:443"))));
        assert!(second.generation > first.generation);
        assert_eq!(second.endpoint, "b:443");
        // A new main Gateway session (fresh Identify) also needs a new connection.
        let third = connect(join.voice_state(&state(ME, Some(GUILD), Some(LOBBY), "s2")));
        assert_eq!(third.session_id, "s2");
        assert_eq!(third.token.expose_secret(), "t2");
    }

    #[test]
    fn leaving_or_being_disconnected_ends_the_attempt() {
        let mut join = JoinCorrelator::new(ME);
        join.join(Some(GUILD), LOBBY);
        join.voice_state(&state(ME, Some(GUILD), Some(LOBBY), "s"));
        connect(join.voice_server(&server(Some(GUILD), None, "t", Some("a"))));
        assert!(matches!(
            join.voice_state(&state(ME, Some(GUILD), None, "s")),
            Correlation::Ended
        ));
        assert!(!join.is_joining());
        assert!(matches!(
            join.voice_server(&server(Some(GUILD), None, "t", Some("a"))),
            Correlation::Ignored
        ));

        join.join(Some(GUILD), LOBBY);
        join.leave();
        assert!(matches!(
            join.voice_state(&state(ME, Some(GUILD), Some(LOBBY), "s")),
            Correlation::Ignored
        ));
    }

    #[test]
    fn private_calls_match_on_the_channel() {
        let mut join = JoinCorrelator::new(ME);
        join.join(None, DM);
        assert!(matches!(
            join.voice_server(&server(None, Some(Snowflake(901)), "t", Some("a"))),
            Correlation::Ignored
        ));
        // A channel-less private state before connecting is not about this call.
        assert!(matches!(
            join.voice_state(&state(ME, None, None, "s")),
            Correlation::Ignored
        ));
        assert!(matches!(
            join.voice_server(&server(None, Some(DM), "t", Some("a"))),
            Correlation::Waiting
        ));
        let credentials = connect(join.voice_state(&state(ME, None, Some(DM), "s")));
        assert_eq!(credentials.server_id, DM);
        assert_eq!(credentials.channel_id, DM);
        assert!(matches!(
            join.voice_state(&state(ME, None, None, "s")),
            Correlation::Ended
        ));
    }
}
