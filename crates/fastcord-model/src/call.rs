//! Private-channel call signaling payloads (SPEC §16 milestone 47).
//!
//! The wire shapes are from observed web-client traffic and are unverified
//! against a live account (`docs/TESTING.md`).

use crate::{Snowflake, VoiceState};

/// CALL_CREATE: a call exists in a DM or group DM. `ringing` are the users
/// still being rung; `voice_states` are those already connected.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct CallCreate {
    pub channel_id: Snowflake,
    #[serde(default)]
    pub ringing: Vec<Snowflake>,
    #[serde(default)]
    pub voice_states: Vec<VoiceState>,
}

/// CALL_UPDATE: the ringing set changed. Participants arrive as ordinary
/// voice state updates.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct CallUpdate {
    pub channel_id: Snowflake,
    #[serde(default)]
    pub ringing: Vec<Snowflake>,
}

/// CALL_DELETE: the call ended (or became unavailable).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct CallDelete {
    pub channel_id: Snowflake,
}
