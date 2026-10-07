//! Gateway frame envelope and the payloads this client sends.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::capabilities;
use super::profile::ClientProperties;

pub(crate) mod op {
    pub(crate) const DISPATCH: u8 = 0;
    pub(crate) const HEARTBEAT: u8 = 1;
    pub(crate) const IDENTIFY: u8 = 2;
    pub(crate) const RESUME: u8 = 6;
    pub(crate) const RECONNECT: u8 = 7;
    pub(crate) const INVALID_SESSION: u8 = 9;
    pub(crate) const HELLO: u8 = 10;
    pub(crate) const HEARTBEAT_ACK: u8 = 11;
}

/// Outbound payloads must stay under 15 KiB or the server closes with 4002.
pub(crate) const MAX_OUTBOUND_BYTES: usize = 15 * 1024;

/// Every inbound frame. `d` stays a raw slice of the decompressed event so
/// that large payloads are decoded straight into typed structs, never into a
/// duplicate untyped JSON tree.
#[derive(Deserialize)]
pub(crate) struct Envelope<'a> {
    pub(crate) op: u8,
    #[serde(borrow, default)]
    pub(crate) d: Option<&'a RawValue>,
    #[serde(default)]
    pub(crate) s: Option<u64>,
    #[serde(borrow, default)]
    pub(crate) t: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
pub(crate) struct Hello {
    pub(crate) heartbeat_interval: u64,
}

#[derive(Serialize)]
struct Frame<'a, T: Serialize + ?Sized> {
    op: u8,
    d: &'a T,
}

/// Why an outbound payload could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutboundError;

fn frame<T: Serialize + ?Sized>(op: u8, d: &T) -> Result<String, OutboundError> {
    let text = serde_json::to_string(&Frame { op, d }).map_err(|_| OutboundError)?;
    if text.len() > MAX_OUTBOUND_BYTES {
        return Err(OutboundError);
    }
    Ok(text)
}

pub(crate) fn heartbeat(sequence: Option<u64>) -> String {
    match sequence {
        Some(sequence) => format!(r#"{{"op":1,"d":{sequence}}}"#),
        None => r#"{"op":1,"d":null}"#.to_owned(),
    }
}

#[derive(Serialize)]
struct Presence {
    status: &'static str,
    since: u64,
    activities: [(); 0],
    afk: bool,
}

#[derive(Serialize)]
struct ClientState {
    guild_versions: BTreeMap<String, u64>,
}

#[derive(Serialize)]
struct Identify<'a> {
    token: &'a str,
    capabilities: u64,
    properties: &'a ClientProperties,
    presence: Presence,
    /// Legacy payload compression stays off: transport compression
    /// (zlib-stream) is in use and the two are mutually exclusive.
    compress: bool,
    client_state: ClientState,
}

pub(crate) fn identify(
    token: &str,
    properties: &ClientProperties,
) -> Result<String, OutboundError> {
    frame(
        op::IDENTIFY,
        &Identify {
            token,
            capabilities: capabilities::selected_value(),
            properties,
            // User accounts: the server ignores the initial presence, so state
            // "unknown" and no activities, as a first connection would.
            presence: Presence {
                status: "unknown",
                since: 0,
                activities: [],
                afk: false,
            },
            compress: false,
            // Cold login: no guild versions are persisted, so none are claimed.
            client_state: ClientState {
                guild_versions: BTreeMap::new(),
            },
        },
    )
}

#[derive(Serialize)]
struct Resume<'a> {
    token: &'a str,
    session_id: &'a str,
    seq: u64,
}

pub(crate) fn resume(token: &str, session_id: &str, seq: u64) -> Result<String, OutboundError> {
    frame(
        op::RESUME,
        &Resume {
            token,
            session_id,
            seq,
        },
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::gateway::profile::HostOs;

    const IDENTIFY_FIXTURE: &str =
        include_str!("../../../../fixtures/gateway/identify.sanitized.json");

    #[test]
    fn identify_matches_the_recorded_sanitized_payload() {
        let properties = ClientProperties::web(HostOs::Windows, "en-US", 631_730);
        let text = identify("REDACTED-TOKEN", &properties).unwrap();
        let sent: Value = serde_json::from_str(&text).unwrap();
        let recorded: Value = serde_json::from_str(IDENTIFY_FIXTURE).unwrap();
        assert_eq!(sent, recorded);
        // Opcode 2, no bot intents, no payload compression, empty version cache.
        assert_eq!(sent["op"], 2);
        assert!(sent["d"].get("intents").is_none());
        assert_eq!(sent["d"]["compress"], false);
        assert_eq!(sent["d"]["client_state"]["guild_versions"], json!({}));
        assert_eq!(sent["d"]["capabilities"], capabilities::selected_value());
        assert!(text.len() < MAX_OUTBOUND_BYTES);
    }

    #[test]
    fn token_is_raw_and_json_escaped() {
        let properties = ClientProperties::web(HostOs::Linux, "en-US", 1);
        let text = identify("a\"b\\c", &properties).unwrap();
        let sent: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(sent["d"]["token"], "a\"b\\c");
    }

    #[test]
    fn resume_and_heartbeat_shapes() {
        let resume: Value = serde_json::from_str(&resume("t", "sess", 42).unwrap()).unwrap();
        assert_eq!(
            resume,
            json!({"op": 6, "d": {"token": "t", "session_id": "sess", "seq": 42}})
        );
        assert_eq!(heartbeat(Some(7)), r#"{"op":1,"d":7}"#);
        assert_eq!(heartbeat(None), r#"{"op":1,"d":null}"#);
    }

    #[test]
    fn oversized_outbound_payloads_are_refused_locally() {
        let properties = ClientProperties::web(HostOs::Linux, "en-US", 1);
        let huge = "x".repeat(MAX_OUTBOUND_BYTES);
        assert_eq!(identify(&huge, &properties), Err(OutboundError));
        assert_eq!(resume(&huge, "s", 1), Err(OutboundError));
    }

    #[test]
    fn envelope_borrows_the_payload_without_building_a_tree() {
        let text =
            br#"{"t":"MESSAGE_CREATE","s":5,"op":0,"d":{"id":"1","nested":[1,2,{"x":null}]}}"#;
        let envelope: Envelope<'_> = serde_json::from_slice(text).unwrap();
        assert_eq!(envelope.op, 0);
        assert_eq!(envelope.s, Some(5));
        assert_eq!(envelope.t.as_deref(), Some("MESSAGE_CREATE"));
        assert_eq!(
            envelope.d.unwrap().get(),
            r#"{"id":"1","nested":[1,2,{"x":null}]}"#
        );
        for bare in [
            &br#"{"op":11}"#[..],
            br#"{"op":11,"d":null,"s":null,"t":null}"#,
        ] {
            let envelope: Envelope<'_> = serde_json::from_slice(bare).unwrap();
            assert!(envelope.d.is_none() && envelope.s.is_none() && envelope.t.is_none());
        }
    }
}
