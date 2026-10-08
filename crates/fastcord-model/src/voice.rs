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
}
