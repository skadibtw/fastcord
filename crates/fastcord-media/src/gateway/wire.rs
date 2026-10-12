//! Voice Gateway v8 JSON payloads and DAVE binary opcode framing. Unknown
//! non-DAVE opcodes are skipped by the driver.

use std::net::SocketAddr;

use fastcord_model::Snowflake;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use zeroize::Zeroizing;

use crate::crypto::{KEY_LEN, TransportMode};
use crate::rtp::OPUS_PAYLOAD_TYPE;
use crate::session::VoiceCredentials;
use crate::video::{CodecOffer, SendOptions, VideoOptions};

/// The RTP stream ID of the single screen layer fastcord sends. Discord's
/// clients name layers by quality; a screen share has one at 100.
pub(crate) const SCREEN_RID: &str = "100";

/// The voice Gateway version, always explicit in the URL (SPEC §6.1).
pub(crate) const VERSION: u8 = 8;

/// DAVE protocol v1, the only version implemented by fastcord-media.
pub(crate) const MAX_DAVE_PROTOCOL_VERSION: u16 = 1;

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
    pub(crate) const SESSION_UPDATE: u8 = 14;
    pub(crate) const MEDIA_SINK_WANTS: u8 = 15;
    pub(crate) const DAVE_PREPARE_TRANSITION: u8 = 21;
    pub(crate) const DAVE_EXECUTE_TRANSITION: u8 = 22;
    pub(crate) const DAVE_TRANSITION_READY: u8 = 23;
    pub(crate) const DAVE_PREPARE_EPOCH: u8 = 24;
    pub(crate) const DAVE_EXTERNAL_SENDER: u8 = 25;
    pub(crate) const DAVE_KEY_PACKAGE: u8 = 26;
    pub(crate) const DAVE_PROPOSALS: u8 = 27;
    pub(crate) const DAVE_COMMIT_WELCOME: u8 = 28;
    pub(crate) const DAVE_ANNOUNCE_COMMIT: u8 = 29;
    pub(crate) const DAVE_WELCOME: u8 = 30;
    pub(crate) const DAVE_INVALID_COMMIT_WELCOME: u8 = 31;
}

pub(crate) enum ServerBinary<'a> {
    ExternalSender(&'a [u8]),
    Proposals {
        operation: u8,
        payload: &'a [u8],
    },
    Commit {
        transition_id: u16,
        payload: &'a [u8],
    },
    Welcome {
        transition_id: u16,
        payload: &'a [u8],
    },
    Unknown,
}

pub(crate) struct SequencedBinary<'a> {
    pub(crate) sequence: u16,
    pub(crate) message: ServerBinary<'a>,
}

/// Server binary frames are `sequence:u16be + opcode:u8 + payload`.
pub(crate) fn server_binary(frame: &[u8]) -> Result<SequencedBinary<'_>, ()> {
    if frame.len() < 3 {
        return Err(());
    }
    let sequence = u16::from_be_bytes([frame[0], frame[1]]);
    let message = match frame[2] {
        op::DAVE_EXTERNAL_SENDER if frame.len() > 3 => ServerBinary::ExternalSender(&frame[3..]),
        op::DAVE_PROPOSALS if frame.len() > 4 && frame[3] <= 1 => ServerBinary::Proposals {
            operation: frame[3],
            payload: &frame[4..],
        },
        op::DAVE_ANNOUNCE_COMMIT if frame.len() > 5 => ServerBinary::Commit {
            transition_id: u16::from_be_bytes([frame[3], frame[4]]),
            payload: &frame[5..],
        },
        op::DAVE_WELCOME if frame.len() > 5 => ServerBinary::Welcome {
            transition_id: u16::from_be_bytes([frame[3], frame[4]]),
            payload: &frame[5..],
        },
        op::DAVE_EXTERNAL_SENDER
        | op::DAVE_PROPOSALS
        | op::DAVE_ANNOUNCE_COMMIT
        | op::DAVE_WELCOME => return Err(()),
        _ => ServerBinary::Unknown,
    };
    Ok(SequencedBinary { sequence, message })
}

#[derive(Deserialize)]
pub(crate) struct DavePrepareTransition {
    pub(crate) protocol_version: u16,
    pub(crate) transition_id: u16,
}

#[derive(Deserialize)]
pub(crate) struct DaveTransition {
    pub(crate) transition_id: u16,
}

#[derive(Deserialize)]
pub(crate) struct DavePrepareEpoch {
    pub(crate) protocol_version: u16,
    pub(crate) epoch: u64,
}

fn transition_payload(transition_id: u16) -> impl Serialize {
    #[derive(Serialize)]
    struct Payload {
        transition_id: u16,
    }
    Payload { transition_id }
}

pub(crate) fn dave_transition_ready(transition_id: u16) -> String {
    frame(
        op::DAVE_TRANSITION_READY,
        &transition_payload(transition_id),
    )
}

pub(crate) fn dave_invalid_commit_welcome(transition_id: u16) -> String {
    frame(
        op::DAVE_INVALID_COMMIT_WELCOME,
        &transition_payload(transition_id),
    )
}

/// Client binary frames contain an opcode and payload, never the server's
/// sequence-number prefix.
pub(crate) fn dave_key_package(key_package: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(key_package.len() + 1);
    frame.push(op::DAVE_KEY_PACKAGE);
    frame.extend_from_slice(key_package);
    frame
}

pub(crate) fn dave_commit_welcome(commit: &[u8], welcome: Option<&[u8]>) -> Vec<u8> {
    let welcome_len = welcome.map_or(0, <[u8]>::len);
    let mut frame = Vec::with_capacity(commit.len() + welcome_len + 1);
    frame.push(op::DAVE_COMMIT_WELCOME);
    frame.extend_from_slice(commit);
    if let Some(welcome) = welcome {
        frame.extend_from_slice(welcome);
    }
    frame
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
    /// The send layers the server assigned to the streams offered in
    /// Identify. Kept untyped here so an unexpected shape can never break an
    /// audio-only connection; [`Ready::layer`] reads it only when video was
    /// offered.
    #[serde(default)]
    pub(crate) streams: Vec<serde_json::Value>,
}

/// One send layer from READY.
#[derive(Deserialize)]
struct ReadyStream {
    ssrc: u32,
    #[serde(default)]
    rtx_ssrc: Option<u32>,
    #[serde(default)]
    rid: Option<String>,
}

/// The SSRCs and stream ID the server assigned to our one screen layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AssignedLayer {
    pub(crate) ssrc: u32,
    pub(crate) rtx_ssrc: u32,
    pub(crate) rid: String,
}

impl Ready {
    /// The first assigned layer. The RTX SSRC defaults to the primary plus one
    /// when omitted (docs.discord.food, Stream Structure); a missing or
    /// non-ASCII stream ID defaults to [`SCREEN_RID`]. IDs the one-byte RTP
    /// extension cannot carry are refused.
    pub(crate) fn layer(&self) -> Option<AssignedLayer> {
        self.streams.iter().find_map(|value| {
            let stream = ReadyStream::deserialize(value).ok()?;
            if stream.ssrc == 0 {
                return None;
            }
            let rtx_ssrc = stream
                .rtx_ssrc
                .filter(|ssrc| *ssrc != 0)
                .unwrap_or(stream.ssrc.wrapping_add(1));
            let rid = stream.rid.unwrap_or_else(|| SCREEN_RID.to_owned());
            let valid_rid = !rid.is_empty()
                && rid.len() <= 16
                && rid.bytes().all(|b| b.is_ascii_alphanumeric());
            valid_rid.then_some(AssignedLayer {
                ssrc: stream.ssrc,
                rtx_ssrc,
                rid,
            })
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct SessionDescription {
    pub(crate) mode: String,
    /// Deserialized straight into a fixed array: a wrong length is a decode
    /// error and no growable copy of the key is left behind.
    pub(crate) secret_key: Zeroizing<[u8; KEY_LEN]>,
    pub(crate) dave_protocol_version: u16,
    pub(crate) audio_codec: String,
    /// The codec this client should *send*; absent on audio-only servers.
    #[serde(default)]
    pub(crate) video_codec: Option<String>,
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
    /// 0–100; the highest active layer is the one a viewer wants.
    #[serde(default)]
    pub(crate) quality: Option<u32>,
    #[serde(default)]
    pub(crate) active: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct Video {
    pub(crate) user_id: Snowflake,
    #[serde(default)]
    pub(crate) audio_ssrc: u32,
    #[serde(default)]
    pub(crate) streams: Vec<VideoStream>,
}

/// Opcode 14 from the server: the codec we should send changed.
#[derive(Deserialize)]
pub(crate) struct SessionUpdate {
    #[serde(default)]
    pub(crate) video_codec: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    video: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    streams: Vec<IdentifyStream>,
    max_dave_protocol_version: u16,
}

#[derive(Serialize)]
struct Resolution {
    #[serde(rename = "type")]
    kind: &'static str,
    width: u32,
    height: u32,
}

/// A layer offered in Identify (docs.discord.food, Stream Structure). A stream
/// connection offers `screen`; the server answers `video` in READY.
#[derive(Serialize)]
struct IdentifyStream {
    #[serde(rename = "type")]
    kind: &'static str,
    rid: &'static str,
    quality: u32,
    max_bitrate: u32,
    max_framerate: u32,
    max_resolution: Resolution,
}

fn fixed(options: &SendOptions) -> Resolution {
    Resolution {
        kind: "fixed",
        width: u32::from(options.width),
        height: u32::from(options.height),
    }
}

/// Opcode 0. Contains the voice token: zeroed when dropped. Without `video` it
/// is exactly the audio-only payload; with it the connection declares video
/// support and, for an owner, offers the one screen layer.
pub(crate) fn identify(
    credentials: &VoiceCredentials,
    video: Option<&VideoOptions>,
) -> Zeroizing<String> {
    let streams = video
        .and_then(|video| video.send)
        .map(|send| {
            vec![IdentifyStream {
                kind: "screen",
                rid: SCREEN_RID,
                quality: 100,
                max_bitrate: send.bitrate,
                max_framerate: u32::from(send.framerate),
                max_resolution: fixed(&send),
            }]
        })
        .unwrap_or_default();
    Zeroizing::new(frame(
        op::IDENTIFY,
        &Identify {
            server_id: credentials.server_id,
            user_id: credentials.user_id,
            session_id: &credentials.session_id,
            token: credentials.token.expose_secret(),
            video: video.map(|_| true),
            streams,
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
    codecs: Vec<Codec>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    rtx_payload_type: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decode: Option<bool>,
}

fn video_codec(index: usize, offer: &CodecOffer) -> Codec {
    Codec {
        name: offer.codec.wire_name(),
        kind: "video",
        // Unique per type, as a multiple of 1000.
        priority: (index as u32 + 1) * 1000,
        payload_type: offer.payload_type,
        rtx_payload_type: Some(offer.rtx_payload_type),
        encode: Some(offer.encode),
        decode: Some(offer.decode),
    }
}

/// Opcode 1: native UDP with the discovered external address. The codec list
/// is what fastcord actually handles: Opus, plus the video codecs of its
/// [`VideoOptions`] when the connection offers any, each with its own payload
/// types and direction.
pub(crate) fn select_protocol(
    external: SocketAddr,
    mode: TransportMode,
    video: Option<&VideoOptions>,
) -> String {
    let mut codecs = vec![Codec {
        name: "opus",
        kind: "audio",
        priority: 1000,
        payload_type: OPUS_PAYLOAD_TYPE,
        rtx_payload_type: None,
        encode: None,
        decode: None,
    }];
    if let Some(video) = video {
        codecs.extend(
            video
                .codecs
                .iter()
                .enumerate()
                .map(|(index, offer)| video_codec(index, offer)),
        );
    }
    frame(
        op::SELECT_PROTOCOL,
        &SelectProtocol {
            protocol: "udp",
            data: ProtocolData {
                address: external.ip().to_string(),
                port: external.port(),
                mode: mode.wire_name(),
            },
            codecs,
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

/// A send layer as announced in opcode 12.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Layer {
    pub(crate) assigned: AssignedLayer,
    pub(crate) options: SendOptions,
    /// The bitrate cap announced to the server.
    pub(crate) max_bitrate: u32,
}

#[derive(Serialize)]
struct VideoStreamCommand<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    rid: &'a str,
    ssrc: u32,
    active: bool,
    quality: u32,
    rtx_ssrc: u32,
    max_bitrate: u32,
    max_framerate: u32,
    max_resolution: Resolution,
}

#[derive(Serialize)]
struct VideoCommand<'a> {
    audio_ssrc: u32,
    video_ssrc: u32,
    rtx_ssrc: u32,
    streams: Vec<VideoStreamCommand<'a>>,
}

/// Opcode 12 for our own connection: the layer's state, or (with no layer)
/// that this connection sends no video, which is also what a viewer declares
/// before receiving.
pub(crate) fn video_state(audio_ssrc: u32, layer: Option<(&Layer, bool)>) -> String {
    let Some((layer, active)) = layer else {
        return frame(
            op::VIDEO,
            &VideoCommand {
                audio_ssrc,
                video_ssrc: 0,
                rtx_ssrc: 0,
                streams: Vec::new(),
            },
        );
    };
    frame(
        op::VIDEO,
        &VideoCommand {
            audio_ssrc,
            video_ssrc: layer.assigned.ssrc,
            rtx_ssrc: layer.assigned.rtx_ssrc,
            streams: vec![VideoStreamCommand {
                kind: "video",
                rid: &layer.assigned.rid,
                ssrc: layer.assigned.ssrc,
                active,
                quality: 100,
                rtx_ssrc: layer.assigned.rtx_ssrc,
                max_bitrate: layer.max_bitrate,
                max_framerate: u32::from(layer.options.framerate),
                max_resolution: fixed(&layer.options),
            }],
        },
    )
}

/// Opcode 15 from a viewer: the quality (0 turns a stream off, 100 is the
/// best layer) wanted for each listed primary SSRC; every other stream is
/// off.
pub(crate) fn media_sink_wants(wants: &[(u32, u8)]) -> String {
    let mut map = serde_json::Map::new();
    for &(ssrc, quality) in wants {
        map.insert(ssrc.to_string(), serde_json::Value::from(quality));
    }
    map.insert("any".to_owned(), serde_json::Value::from(0));
    frame(op::MEDIA_SINK_WANTS, &map)
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
            dave_group_id: None,
        }
    }

    fn value(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn identify_and_resume_carry_the_v8_fields() {
        assert_eq!(
            value(&identify(&credentials(), None)),
            json!({"op": 0, "d": {
                "server_id": "41771983423143937",
                "user_id": "104694319306248192",
                "session_id": "fixture-session",
                "token": "fixture-voice-token",
                "max_dave_protocol_version": 1
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
                TransportMode::XChaCha20Poly1305RtpSize,
                None
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
                TransportMode::Aes256GcmRtpSize,
                None
            ))["d"]["data"]["address"],
            "2001:db8::1"
        );
        assert_eq!(
            value(&speaking(speaking::VOICE, 12871)),
            json!({"op": 5, "d": {"speaking": 1, "delay": 0, "ssrc": 12871}})
        );
    }
    #[test]
    fn dave_json_and_binary_opcodes_match_reference_framing() {
        assert_eq!(
            value(&dave_transition_ready(42)),
            json!({"op": 23, "d": {"transition_id": 42}})
        );
        assert_eq!(
            value(&dave_invalid_commit_welcome(42)),
            json!({"op": 31, "d": {"transition_id": 42}})
        );

        assert_eq!(dave_key_package(&[0xAA, 0xBB]), [26, 0xAA, 0xBB]);
        assert_eq!(
            dave_commit_welcome(&[0x11, 0x22], Some(&[0x33, 0x44])),
            [28, 0x11, 0x22, 0x33, 0x44]
        );
        assert_eq!(dave_commit_welcome(&[0x11], None), [28, 0x11]);

        let proposal = server_binary(&[0x12, 0x34, 27, 0, 3, 0xAA, 0xBB]).unwrap();
        assert_eq!(proposal.sequence, 0x1234);
        assert!(matches!(
            proposal.message,
            ServerBinary::Proposals {
                operation: 0,
                payload: [3, 0xAA, 0xBB]
            }
        ));

        let commit = server_binary(&[0xAB, 0xCD, 29, 0x01, 0x23, 0x99]).unwrap();
        assert_eq!(commit.sequence, 0xABCD);
        assert!(matches!(
            commit.message,
            ServerBinary::Commit {
                transition_id: 0x0123,
                payload: [0x99]
            }
        ));
        assert!(server_binary(&[0, 1, 27, 2, 0]).is_err());
        assert!(server_binary(&[0, 1, 29, 0, 1]).is_err());
    }

    #[test]
    fn session_description_key_must_be_exactly_32_bytes() {
        let key: Vec<u8> = (0..32).collect();
        let text = json!({"mode": "aead_aes256_gcm_rtpsize", "secret_key": key, "audio_codec": "opus", "dave_protocol_version": 1, "media_session_id": "m"}).to_string();
        let description: SessionDescription = serde_json::from_str(&text).unwrap();
        assert_eq!(description.secret_key[31], 31);
        assert_eq!(description.dave_protocol_version, 1);
        for bad in [vec![0u8; 31], vec![0u8; 33]] {
            let text = json!({"mode": "m", "secret_key": bad, "audio_codec": "opus", "dave_protocol_version": 1}).to_string();
            assert!(serde_json::from_str::<SessionDescription>(&text).is_err());
        }
        let text = json!({"mode": "m", "secret_key": vec![256u16; 32], "audio_codec": "opus", "dave_protocol_version": 1}).to_string();
        assert!(serde_json::from_str::<SessionDescription>(&text).is_err());
    }

    #[test]
    fn video_identify_and_select_protocol_declare_exactly_what_is_offered() {
        // A viewer: video support, no layer, decode only.
        let watch = VideoOptions::watch();
        assert_eq!(
            value(&identify(&credentials(), Some(&watch))),
            json!({"op": 0, "d": {
                "server_id": "41771983423143937",
                "user_id": "104694319306248192",
                "session_id": "fixture-session",
                "token": "fixture-voice-token",
                "video": true,
                "max_dave_protocol_version": 1
            }})
        );
        let select = value(&select_protocol(
            "203.0.113.7:50123".parse().unwrap(),
            TransportMode::Aes256GcmRtpSize,
            Some(&watch),
        ));
        assert_eq!(
            select["d"]["codecs"],
            json!([
                {"name": "opus", "type": "audio", "priority": 1000, "payload_type": 120},
                {"name": "H264", "type": "video", "priority": 1000, "payload_type": 103,
                 "rtx_payload_type": 104, "encode": false, "decode": true},
                {"name": "AV1", "type": "video", "priority": 2000, "payload_type": 101,
                 "rtx_payload_type": 102, "encode": false, "decode": true}
            ])
        );

        // An owner also offers the one screen layer with its limits.
        let share = VideoOptions::share(SendOptions::SCREEN_720P30);
        let identify = value(&identify(&credentials(), Some(&share)));
        assert_eq!(identify["d"]["video"], true);
        assert_eq!(
            identify["d"]["streams"],
            json!([{
                "type": "screen", "rid": "100", "quality": 100,
                "max_bitrate": 2_500_000, "max_framerate": 30,
                "max_resolution": {"type": "fixed", "width": 1280, "height": 720}
            }])
        );
        let select = value(&select_protocol(
            "203.0.113.7:50123".parse().unwrap(),
            TransportMode::Aes256GcmRtpSize,
            Some(&share),
        ));
        assert_eq!(select["d"]["codecs"][1]["encode"], true);
        assert_eq!(select["d"]["codecs"][1]["decode"], true);
        assert_eq!(select["d"]["codecs"][2]["encode"], false);
        assert_eq!(select["d"]["codecs"][2]["decode"], true);
    }

    #[test]
    fn ready_streams_assign_the_screen_layer_or_nothing() {
        let ready = |streams: Value| -> Ready {
            serde_json::from_value(json!({
                "ssrc": 12871, "ip": "127.0.0.1", "port": 1234,
                "modes": ["aead_aes256_gcm_rtpsize"], "streams": streams
            }))
            .unwrap()
        };
        let layer = ready(json!([{
            "type": "video", "ssrc": 12872, "rtx_ssrc": 12873, "rid": "100",
            "quality": 100, "active": false
        }]))
        .layer()
        .unwrap();
        assert_eq!(
            layer,
            AssignedLayer {
                ssrc: 12872,
                rtx_ssrc: 12873,
                rid: "100".to_owned()
            }
        );
        // The RTX SSRC defaults to the primary plus one; the RID to "100".
        let defaulted = ready(json!([{"ssrc": 40}])).layer().unwrap();
        assert_eq!((defaulted.rtx_ssrc, defaulted.rid.as_str()), (41, "100"));
        // Nothing usable: no streams, a zero SSRC, an unrepresentable RID, a
        // stream of the wrong shape, or a READY with no streams field at all.
        for streams in [
            json!([]),
            json!([{"ssrc": 0, "rtx_ssrc": 1}]),
            json!([{"ssrc": 5, "rid": "has space"}]),
            json!([{"ssrc": 5, "rid": "x".repeat(17)}]),
            json!(["junk", {"ssrc": "not a number"}]),
        ] {
            assert!(ready(streams).layer().is_none());
        }
        let bare: Ready =
            serde_json::from_value(json!({"ssrc": 1, "ip": "127.0.0.1", "port": 1, "modes": []}))
                .unwrap();
        assert!(bare.layer().is_none());
    }

    #[test]
    fn video_state_and_media_sink_wants_follow_the_documented_shapes() {
        let layer = Layer {
            assigned: AssignedLayer {
                ssrc: 13_960,
                rtx_ssrc: 13_961,
                rid: "100".to_owned(),
            },
            options: SendOptions::SCREEN_720P30,
            max_bitrate: 2_500_000,
        };
        let expected = |active: bool| {
            json!({"op": 12, "d": {
                "audio_ssrc": 13_959, "video_ssrc": 13_960, "rtx_ssrc": 13_961,
                "streams": [{
                    "type": "video", "rid": "100", "ssrc": 13_960, "active": active,
                    "quality": 100, "rtx_ssrc": 13_961, "max_bitrate": 2_500_000,
                    "max_framerate": 30,
                    "max_resolution": {"type": "fixed", "width": 1280, "height": 720}
                }]
            }})
        };
        assert_eq!(
            value(&video_state(13_959, Some((&layer, true)))),
            expected(true)
        );
        assert_eq!(
            value(&video_state(13_959, Some((&layer, false)))),
            expected(false)
        );
        assert_eq!(
            value(&video_state(13_959, None)),
            json!({"op": 12, "d": {
                "audio_ssrc": 13_959, "video_ssrc": 0, "rtx_ssrc": 0, "streams": []
            }})
        );
        assert_eq!(
            value(&media_sink_wants(&[(8964, 100)])),
            json!({"op": 15, "d": {"8964": 100, "any": 0}})
        );
        assert_eq!(
            value(&media_sink_wants(&[])),
            json!({"op": 15, "d": {"any": 0}})
        );
    }

    #[test]
    fn received_video_streams_tolerate_missing_and_null_optional_fields() {
        let video: Video = serde_json::from_value(json!({
            "user_id": "852892297661906993", "audio_ssrc": 13_959, "video_ssrc": 13_960,
            "streams": [
                {"ssrc": 13_960, "rtx_ssrc": 13_961, "rid": "100", "quality": 100,
                 "max_resolution": {"width": 0, "type": "source", "height": 0},
                 "max_framerate": 60, "active": true},
                {"ssrc": 13_962, "quality": null, "active": null}
            ]
        }))
        .unwrap();
        assert_eq!(video.streams.len(), 2);
        assert_eq!(video.streams[0].active, Some(true));
        assert_eq!(video.streams[1].rtx_ssrc, None);
        assert_eq!(video.streams[1].quality, None);
        let update: SessionUpdate =
            serde_json::from_value(json!({"video_codec": "AV1", "keyframe_interval": 5000}))
                .unwrap();
        assert_eq!(update.video_codec.as_deref(), Some("AV1"));
        let description: SessionDescription = serde_json::from_value(json!({
            "mode": "m", "secret_key": vec![0u8; 32], "audio_codec": "opus",
            "dave_protocol_version": 1, "video_codec": "H264"
        }))
        .unwrap();
        assert_eq!(description.video_codec.as_deref(), Some("H264"));
    }
}
