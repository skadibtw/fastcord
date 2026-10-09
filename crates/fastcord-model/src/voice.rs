use std::fmt;

use zeroize::Zeroizing;

use crate::Snowflake;

/// A user's voice connection state. `channel_id` is `None` when the user is
/// not connected.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoiceState {
    /// Omitted inside guild payloads, where the guild is implied.
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    #[serde(default)]
    pub channel_id: Option<Snowflake>,
    pub user_id: Snowflake,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub deaf: bool,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub self_deaf: bool,
    #[serde(default)]
    pub self_mute: bool,
    #[serde(default)]
    pub self_stream: bool,
    #[serde(default)]
    pub self_video: bool,
    #[serde(default)]
    pub suppress: bool,
}

impl fmt::Debug for VoiceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceState")
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .field("user_id", &self.user_id)
            .field("session_id", &"[REDACTED]")
            .field("deaf", &self.deaf)
            .field("mute", &self.mute)
            .field("self_deaf", &self.self_deaf)
            .field("self_mute", &self.self_mute)
            .field("self_stream", &self.self_stream)
            .field("self_video", &self.self_video)
            .field("suppress", &self.suppress)
            .finish()
    }
}

/// The body of main Gateway opcode 4 (Update Voice State): join, move, or
/// leave (`channel_id: None`) a voice channel or call. One request per explicit
/// user action; the Gateway serializes it as decimal-string IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct VoiceStateRequest {
    /// `None` for private-channel calls.
    pub guild_id: Option<Snowflake>,
    /// `None` disconnects from voice.
    pub channel_id: Option<Snowflake>,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub self_video: bool,
}

impl VoiceStateRequest {
    pub const fn join(
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        self_mute: bool,
        self_deaf: bool,
    ) -> Self {
        Self {
            guild_id,
            channel_id: Some(channel_id),
            self_mute,
            self_deaf,
            self_video: false,
        }
    }

    /// Disconnects the account from voice in `guild_id` (or from a private
    /// call when `None`).
    pub const fn leave(guild_id: Option<Snowflake>) -> Self {
        Self {
            guild_id,
            channel_id: None,
            self_mute: false,
            self_deaf: false,
            self_video: false,
        }
    }
}

/// A voice server token. Never logged: `Debug` is redacted and the bytes are
/// zeroed when the last copy is dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct VoiceToken(Zeroizing<String>);

impl VoiceToken {
    pub fn new(token: String) -> Self {
        Self(Zeroizing::new(token))
    }

    /// The raw token, for the voice Identify/Resume payload only.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for VoiceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VoiceToken(<redacted>)")
    }
}

impl<'de> serde::Deserialize<'de> for VoiceToken {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Self::new)
    }
}

/// VOICE_SERVER_UPDATE: the voice server for the account's current voice
/// connection. A `None` endpoint means the server went away and is being
/// reallocated: disconnect and wait for the next update.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct VoiceServerUpdate {
    pub token: VoiceToken,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    /// Present for private-channel calls.
    #[serde(default)]
    pub channel_id: Option<Snowflake>,
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// The type of stream (opcode 18).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamType {
    #[serde(rename = "guild")]
    Guild,
    #[serde(rename = "call")]
    Call,
}

/// Main Gateway opcode 18: create a stream in a voice channel.
#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub struct StreamCreateRequest {
    /// `"guild"` or `"call"`
    #[serde(rename = "type")]
    pub stream_type: StreamType,
    /// The stream channel (voice channel or private call channel).
    pub channel_id: Snowflake,
    /// The guild ID, if any (absent for calls).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guild_id: Option<Snowflake>,
    /// Region preference (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_region: Option<String>,
}

impl StreamCreateRequest {
    /// Create a guild stream request.
    pub fn guild(guild_id: Snowflake, channel_id: Snowflake) -> Self {
        Self {
            stream_type: StreamType::Guild,
            channel_id,
            guild_id: Some(guild_id),
            preferred_region: None,
        }
    }

    /// Create a call stream request.
    pub fn call(channel_id: Snowflake) -> Self {
        Self {
            stream_type: StreamType::Call,
            channel_id,
            guild_id: None,
            preferred_region: None,
        }
    }
}

impl fmt::Debug for StreamCreateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamCreateRequest")
            .field("stream_type", &self.stream_type)
            .field("channel_id", &self.channel_id)
            .field("guild_id", &self.guild_id)
            .field("preferred_region", &self.preferred_region)
            .finish()
    }
}

/// A stream key that identifies one stream owner in one location.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StreamKey {
    /// `"guild:{guild_id}:{channel_id}:{owner_id}"`
    Guild {
        guild_id: Snowflake,
        channel_id: Snowflake,
        owner_id: Snowflake,
    },
    /// `"call:{channel_id}:{owner_id}"`
    Call {
        channel_id: Snowflake,
        owner_id: Snowflake,
    },
}

impl StreamKey {
    /// Create from the string form in STREAM_CREATE.
    pub fn from_wire(s: &str) -> Option<Self> {
        let mut parts = s.split(':');
        match parts.next()? {
            "guild" => {
                let guild_id = parts.next()?.parse::<u64>().ok()?;
                let channel_id = parts.next()?.parse::<u64>().ok()?;
                let owner_id = parts.next()?.parse::<u64>().ok()?;
                if parts.next().is_some() || guild_id == 0 || channel_id == 0 || owner_id == 0 {
                    return None;
                }
                Some(Self::Guild {
                    guild_id: Snowflake(guild_id),
                    channel_id: Snowflake(channel_id),
                    owner_id: Snowflake(owner_id),
                })
            }
            "call" => {
                let channel_id = parts.next()?.parse::<u64>().ok()?;
                let owner_id = parts.next()?.parse::<u64>().ok()?;
                if parts.next().is_some() || channel_id == 0 || owner_id == 0 {
                    return None;
                }
                Some(Self::Call {
                    channel_id: Snowflake(channel_id),
                    owner_id: Snowflake(owner_id),
                })
            }
            _ => None,
        }
    }

    /// Serialize to the wire form.
    pub fn to_wire(&self) -> String {
        match self {
            Self::Guild {
                guild_id,
                channel_id,
                owner_id,
            } => format!("guild:{}:{}:{}", guild_id.0, channel_id.0, owner_id.0),
            Self::Call {
                channel_id,
                owner_id,
            } => format!("call:{}:{}", channel_id.0, owner_id.0),
        }
    }
}

/// STREAM_SERVER_UPDATE: a stream's voice token and allocated endpoint.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct StreamServerUpdate {
    pub stream_key: String,
    pub token: VoiceToken,
    pub endpoint: Option<String>,
}

impl fmt::Display for StreamServerUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "StreamServerUpdate({}, {:?})",
            self.stream_key, self.endpoint
        )
    }
}

/// STREAM_CREATE: the stream and its independent RTC identities.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct StreamCreate {
    pub stream_key: String,
    pub rtc_server_id: Snowflake,
    pub rtc_channel_id: Snowflake,
}

/// Reason a stream ended (STREAM_DELETE reason field).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamDeleteReason {
    UserRequested,
    StreamEnded,
    StreamFull,
    Unauthorized,
    SafetyGuildRateLimited,
    ParseFailed,
    InvalidChannel,
}

/// STREAM_DELETE: the end of a stream.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct StreamDelete {
    /// The stream key.
    pub stream_key: String,
    /// Why the stream ended.
    pub reason: StreamDeleteReason,
    #[serde(default)]
    pub unavailable: bool,
}

/// STREAM_UPDATE: optional updates during a stream's lifetime.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct StreamUpdate {
    /// The stream key.
    pub stream_key: String,
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_state_debug_redacts_session_id() {
        let state: VoiceState =
            serde_json::from_str(r#"{"user_id":"5","session_id":"fixture-session"}"#).unwrap();
        let shown = format!("{state:?}");
        assert!(!shown.contains("fixture-session"));
        assert!(shown.contains("[REDACTED]"));
    }
    #[test]
    fn voice_state_requests_serialize_as_opcode_4_bodies() {
        let join = VoiceStateRequest::join(Some(Snowflake(41)), Snowflake(127), true, false);
        assert_eq!(
            serde_json::to_value(join).unwrap(),
            serde_json::json!({
                "guild_id": "41",
                "channel_id": "127",
                "self_mute": true,
                "self_deaf": false,
                "self_video": false
            })
        );
        let leave = VoiceStateRequest::leave(None);
        assert_eq!(
            serde_json::to_value(leave).unwrap(),
            serde_json::json!({
                "guild_id": null,
                "channel_id": null,
                "self_mute": false,
                "self_deaf": false,
                "self_video": false
            })
        );
    }

    #[test]
    fn voice_server_updates_decode_and_never_print_the_token() {
        let update: VoiceServerUpdate = serde_json::from_str(
            r#"{"token":"fixture-voice-token","guild_id":"41771983423143937","endpoint":"smart.loyal.discord.media:1337"}"#,
        )
        .unwrap();
        assert_eq!(update.guild_id, Some(Snowflake(41_771_983_423_143_937)));
        assert_eq!(update.channel_id, None);
        assert_eq!(
            update.endpoint.as_deref(),
            Some("smart.loyal.discord.media:1337")
        );
        assert_eq!(update.token.expose_secret(), "fixture-voice-token");
        assert!(!format!("{update:?}").contains("fixture-voice-token"));

        let reallocating: VoiceServerUpdate = serde_json::from_str(
            r#"{"token":"t","guild_id":null,"channel_id":"5","endpoint":null}"#,
        )
        .unwrap();
        assert_eq!(reallocating.endpoint, None);
        assert_eq!(reallocating.channel_id, Some(Snowflake(5)));
    }
    #[test]
    fn stream_keys_parse_documented_owner_qualified_forms() {
        assert_eq!(
            StreamKey::from_wire("guild:839502008108580904:850360749460553769:852892297661906993"),
            Some(StreamKey::Guild {
                guild_id: Snowflake(839502008108580904),
                channel_id: Snowflake(850360749460553769),
                owner_id: Snowflake(852892297661906993),
            })
        );
        assert_eq!(
            StreamKey::from_wire("call:1110739331624210483:852892297661906993"),
            Some(StreamKey::Call {
                channel_id: Snowflake(1110739331624210483),
                owner_id: Snowflake(852892297661906993),
            })
        );
        assert_eq!(
            StreamKey::from_wire("guild:839502008108580904:850360749460553769:852892297661906993")
                .unwrap()
                .to_wire(),
            "guild:839502008108580904:850360749460553769:852892297661906993"
        );
        for invalid in [
            "",
            "guild:0:127:1",
            "guild:41:0:1",
            "guild:41:127:0",
            "guild:41:127",
            "guild:41:127:1:2",
            "call:0:1",
            "call:127:0",
            "call:127",
            "test:1",
            "not-a-key",
        ] {
            assert_eq!(StreamKey::from_wire(invalid), None);
        }
    }

    #[test]
    fn stream_events_decode_documented_field_ownership_and_reasons() {
        let create: StreamCreate = serde_json::from_str(
            r#"{"stream_key":"guild:41:127:1","rtc_server_id":"500","rtc_channel_id":"789","region":"us-east","viewer_ids":["2"],"paused":false}"#,
        ).unwrap();
        assert_eq!(create.rtc_server_id, Snowflake(500));
        assert_eq!(create.rtc_channel_id, Snowflake(789));
        let server: StreamServerUpdate = serde_json::from_str(
            r#"{"token":"fixture-token","stream_key":"guild:41:127:1","guild_id":"41","endpoint":"rtc.example:443"}"#,
        ).unwrap();
        assert_eq!(server.endpoint.as_deref(), Some("rtc.example:443"));
        assert_eq!(server.token.expose_secret(), "fixture-token");
        assert!(!format!("{server:?}").contains("fixture-token"));

        for reason in [
            "user_requested",
            "stream_ended",
            "stream_full",
            "unauthorized",
            "safety_guild_rate_limited",
            "parse_failed",
            "invalid_channel",
        ] {
            let delete: StreamDelete = serde_json::from_str(&format!(
                r#"{{"stream_key":"call:127:1","reason":"{reason}","unavailable":false}}"#
            ))
            .unwrap();
            assert_eq!(delete.stream_key, "call:127:1");
            assert!(!delete.unavailable);
        }
    }
}
