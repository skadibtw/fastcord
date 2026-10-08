//! Voice Gateway v8 JSON payloads: what fastcord sends and the subset of
//! server payloads it reads. Unknown fields are ignored; unknown opcodes are
//! skipped by the driver.

use std::net::SocketAddr;

use fastcord_model::Snowflake;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use zeroize::Zeroizing;

use crate::crypto::{KEY_LEN, TransportMode};
use crate::rtp::OPUS_PAYLOAD_TYPE;
use crate::session::VoiceCredentials;

/// The voice Gateway version, always explicit in the URL (SPEC §6.1).
pub(crate) const VERSION: u8 = 8;

/// The highest DAVE protocol version this build implements. DAVE arrives
/// with milestone 18; until then fastcord truthfully advertises none, and a
/// server that requires E2EE closes with 4017.
pub(crate) const MAX_DAVE_PROTOCOL_VERSION: u16 = 0;

pub(crate) mod op {
    pub(crate) const IDENTIFY: u8 = 0;
    pub(crate) const SELECT_PROTOCOL: u8 = 1;
    pub(crate) const READY: u8 = 2;
    pub(crate) const HEARTBEAT: u8 = 3;
    pub(crate) const SESSION_DESCRIPTION: u8 = 4;
    pub(crate) const SPEAKING: u8 = 5;
    pub(crate) const HEARTBEAT_ACK: u8 = 6;
    pub(crate) const RESUME: u8 = 7;
    pub(crate) const HELLO: u8 = 8;
    pub(crate) const RESUMED: u8 = 9;
    pub(crate) const CLIENTS_CONNECT: u8 = 11;
    pub(crate) const VIDEO: u8 = 12;
    pub(crate) const CLIENT_DISCONNECT: u8 = 13;
}

/// Speaking flags (opcode 5).
pub mod speaking {
    pub const VOICE: u32 = 1 << 0;
    pub const SOUNDSHARE: u32 = 1 << 1;
    pub const PRIORITY: u32 = 1 << 2;
}

#[derive(Deserialize)]
pub(crate) struct Envelope<'a> {
    pub(crate) op: u8,
    #[serde(borrow, default)]
    pub(crate) d: Option<&'a RawValue>,
    /// Present on messages the server may replay after a buffered resume.
    #[serde(default)]
    pub(crate) seq: Option<u64>,
}

#[derive(Deserialize)]
pub(crate) struct Hello {
    /// Milliseconds; Discord sends it as a float.
    pub(crate) heartbeat_interval: f64,
}

#[derive(Deserialize)]
pub(crate) struct HeartbeatAck {
    pub(crate) t: u64,
}

#[derive(Deserialize)]
pub(crate) struct Ready {
    pub(crate) ssrc: u32,
    pub(crate) ip: String,
    pub(crate) port: u16,
    pub(crate) modes: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct SessionDescription {
    pub(crate) mode: String,
    /// Deserialized straight into a fixed array: a wrong length is a decode
    /// error and no growable copy of the key is left behind.
    pub(crate) secret_key: Zeroizing<[u8; KEY_LEN]>,
    pub(crate) dave_protocol_version: u16,
    pub(crate) audio_codec: String,
}

#[derive(Deserialize)]
pub(crate) struct Speaking {
    pub(crate) user_id: Snowflake,
    pub(crate) ssrc: u32,
    #[serde(default)]
    pub(crate) speaking: u32,
}

#[derive(Deserialize)]
pub(crate) struct VideoStream {
    pub(crate) ssrc: u32,
    #[serde(default)]
    pub(crate) rtx_ssrc: Option<u32>,
}

#[derive(Deserialize)]
pub(crate) struct Video {
    pub(crate) user_id: Snowflake,
    #[serde(default)]
    pub(crate) audio_ssrc: u32,
    #[serde(default)]
    pub(crate) streams: Vec<VideoStream>,
}

#[derive(Deserialize)]
pub(crate) struct ClientsConnect {
    pub(crate) user_ids: Vec<Snowflake>,
}

#[derive(Deserialize)]
pub(crate) struct ClientDisconnect {
    pub(crate) user_id: Snowflake,
}

#[derive(Serialize)]
struct Frame<'a, T: Serialize> {
    op: u8,
    d: &'a T,
}

fn frame<T: Serialize>(op: u8, d: &T) -> String {
    // Plain structs of strings and integers: serialization cannot fail.
    serde_json::to_string(&Frame { op, d }).unwrap_or_default()
}

#[derive(Serialize)]
struct Identify<'a> {
    server_id: Snowflake,
    user_id: Snowflake,
    session_id: &'a str,
    token: &'a str,
    max_dave_protocol_version: u16,
}

/// Opcode 0. Contains the voice token: zeroed when dropped.
pub(crate) fn identify(credentials: &VoiceCredentials) -> Zeroizing<String> {
    Zeroizing::new(frame(
        op::IDENTIFY,
        &Identify {
            server_id: credentials.server_id,
            user_id: credentials.user_id,
            session_id: &credentials.session_id,
            token: credentials.token.expose_secret(),
            max_dave_protocol_version: MAX_DAVE_PROTOCOL_VERSION,
        },
    ))
}

#[derive(Serialize)]
struct Resume<'a> {
    server_id: Snowflake,
    session_id: &'a str,
    token: &'a str,
    seq_ack: i64,
}

/// Opcode 7. `seq_ack` is -1 when no numbered message arrived yet.
pub(crate) fn resume(credentials: &VoiceCredentials, seq_ack: Option<u64>) -> Zeroizing<String> {
    Zeroizing::new(frame(
        op::RESUME,
        &Resume {
            server_id: credentials.server_id,
            session_id: &credentials.session_id,
            token: credentials.token.expose_secret(),
            seq_ack: seq_ack_value(seq_ack),
        },
    ))
}

fn seq_ack_value(seq_ack: Option<u64>) -> i64 {
    seq_ack
        .and_then(|seq| i64::try_from(seq).ok())
        .unwrap_or(-1)
}

#[derive(Serialize)]
struct Heartbeat {
    t: u64,
    seq_ack: i64,
}

/// Opcode 3 in the v8 shape: a nonce and the last sequence received.
pub(crate) fn heartbeat(nonce: u64, seq_ack: Option<u64>) -> String {
    frame(
        op::HEARTBEAT,
        &Heartbeat {
            t: nonce,
            seq_ack: seq_ack_value(seq_ack),
        },
    )
}

#[derive(Serialize)]
struct SelectProtocol {
    protocol: &'static str,
    data: ProtocolData,
    codecs: [Codec; 1],
}

#[derive(Serialize)]
struct ProtocolData {
    address: String,
    port: u16,
    mode: &'static str,
}

#[derive(Serialize)]
struct Codec {
    name: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    priority: u32,
    payload_type: u8,
}

/// Opcode 1: native UDP with the discovered external address. The codec list
/// is what fastcord actually handles: Opus audio only (no video yet).
pub(crate) fn select_protocol(external: SocketAddr, mode: TransportMode) -> String {
    frame(
        op::SELECT_PROTOCOL,
        &SelectProtocol {
            protocol: "udp",
            data: ProtocolData {
                address: external.ip().to_string(),
                port: external.port(),
                mode: mode.wire_name(),
            },
            codecs: [Codec {
                name: "opus",
                kind: "audio",
                priority: 1000,
                payload_type: OPUS_PAYLOAD_TYPE,
            }],
        },
    )
}

#[derive(Serialize)]
struct SpeakingCommand {
    speaking: u32,
    delay: u32,
    ssrc: u32,
}

/// Opcode 5 for our own SSRC.
pub(crate) fn speaking(flags: u32, ssrc: u32) -> String {
    frame(
        op::SPEAKING,
        &SpeakingCommand {
            speaking: flags,
            delay: 0,
            ssrc,
        },
    )
}

#[cfg(test)]
mod tests {
    use fastcord_model::VoiceToken;
    use serde_json::{Value, json};

    use super::*;
    use crate::session::Generation;

    fn credentials() -> VoiceCredentials {
        VoiceCredentials {
            generation: Generation(1),
            server_id: Snowflake(41_771_983_423_143_937),
            channel_id: Snowflake(127_121_515_262_115_840),
            user_id: Snowflake(104_694_319_306_248_192),
            session_id: "fixture-session".to_owned(),
            token: VoiceToken::new("fixture-voice-token".to_owned()),
            endpoint: "fixture.discord.media:443".to_owned(),
        }
    }

    fn value(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn identify_and_resume_carry_the_v8_fields() {
        assert_eq!(
            value(&identify(&credentials())),
            json!({"op": 0, "d": {
                "server_id": "41771983423143937",
                "user_id": "104694319306248192",
                "session_id": "fixture-session",
                "token": "fixture-voice-token",
                "max_dave_protocol_version": 0
            }})
        );
        assert_eq!(
            value(&resume(&credentials(), Some(10))),
            json!({"op": 7, "d": {
                "server_id": "41771983423143937",
                "session_id": "fixture-session",
                "token": "fixture-voice-token",
                "seq_ack": 10
            }})
        );
        assert_eq!(value(&resume(&credentials(), None))["d"]["seq_ack"], -1);
    }

    #[test]
    fn heartbeat_select_protocol_and_speaking_shapes() {
        assert_eq!(
            value(&heartbeat(1_501_184_119_561, Some(10))),
            json!({"op": 3, "d": {"t": 1_501_184_119_561_u64, "seq_ack": 10}})
        );
        assert_eq!(value(&heartbeat(5, None))["d"]["seq_ack"], -1);
        assert_eq!(
            value(&select_protocol(
                "203.0.113.7:50123".parse().unwrap(),
                TransportMode::XChaCha20Poly1305RtpSize
            )),
            json!({"op": 1, "d": {
                "protocol": "udp",
                "data": {"address": "203.0.113.7", "port": 50123, "mode": "aead_xchacha20_poly1305_rtpsize"},
                "codecs": [{"name": "opus", "type": "audio", "priority": 1000, "payload_type": 120}]
            }})
        );
        assert_eq!(
            value(&select_protocol(
                "[2001:db8::1]:9".parse().unwrap(),
                TransportMode::Aes256GcmRtpSize
            ))["d"]["data"]["address"],
            "2001:db8::1"
        );
        assert_eq!(
            value(&speaking(speaking::VOICE, 12871)),
            json!({"op": 5, "d": {"speaking": 1, "delay": 0, "ssrc": 12871}})
        );
    }

    #[test]
    fn session_description_key_must_be_exactly_32_bytes() {
        let key: Vec<u8> = (0..32).collect();
        let text = json!({"mode": "aead_aes256_gcm_rtpsize", "secret_key": key, "audio_codec": "opus", "dave_protocol_version": 0, "media_session_id": "m"}).to_string();
        let description: SessionDescription = serde_json::from_str(&text).unwrap();
        assert_eq!(description.secret_key[31], 31);
        assert_eq!(description.dave_protocol_version, 0);
        for bad in [vec![0u8; 31], vec![0u8; 33]] {
            let text = json!({"mode": "m", "secret_key": bad, "audio_codec": "opus", "dave_protocol_version": 0}).to_string();
            assert!(serde_json::from_str::<SessionDescription>(&text).is_err());
        }
        let text = json!({"mode": "m", "secret_key": vec![256u16; 32], "audio_codec": "opus", "dave_protocol_version": 0}).to_string();
        assert!(serde_json::from_str::<SessionDescription>(&text).is_err());
    }
}
