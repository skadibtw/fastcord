//! The real voice driver against an in-process voice Gateway (real WebSocket
//! framing over an in-memory duplex) and an in-memory UDP voice server that
//! encrypts and decrypts with the session key. Time is paused, so timeouts,
//! heartbeats, retries, and pacing run instantly and deterministically.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use fastcord_model::{Snowflake, VoiceToken};
use futures_util::{FutureExt, SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{DuplexStream, duplex};
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role};

use super::driver::Options;
use super::transport::{ConnectError, Network, Signaling, UdpLink, websocket_config};
use super::*;
use crate::crypto::{RTCP_CLEAR_LEN, TransportCipher, TransportKey};
use crate::rtcp::{self as rtcp_wire, Compound, RtcpPacket};
use crate::rtp::{ExtensionPreamble, ONE_BYTE_PROFILE, RtpHeader};
use crate::udp::DISCOVERY_LEN;

const KEY: [u8; 32] = [0x5A; 32];
const SSRC: u32 = 12_871;
const ME: Snowflake = Snowflake(104_694_319_306_248_192);
const ALICE: Snowflake = Snowflake(852_892_297_661_906_993);
const GUILD: Snowflake = Snowflake(41_771_983_423_143_937);
/// Long enough that heartbeats never interfere unless a test wants them.
const QUIET_HEARTBEAT_MS: f64 = 60_000.0;
const BOTH_MODES: [&str; 3] = [
    "aead_aes256_gcm_rtpsize",
    "aead_xchacha20_poly1305_rtpsize",
    "xsalsa20_poly1305_lite",
];

fn server_addr() -> SocketAddr {
    "198.51.100.10:50001".parse().unwrap()
}

fn external_addr() -> SocketAddr {
    "203.0.113.7:50123".parse().unwrap()
}

fn credentials(endpoint: &str) -> VoiceCredentials {
    VoiceCredentials {
        generation: Generation(3),
        server_id: GUILD,
        channel_id: Snowflake(127_121_515_262_115_840),
        user_id: ME,
        session_id: "fixture-session".to_owned(),
        token: VoiceToken::new("fixture-voice-token".to_owned()),
        endpoint: endpoint.to_owned(),
    }
}

fn options() -> Options {
    Options {
        aes_accelerated: true,
        first_nonce: 0,
        rtp_start: Some((100, 1_000)),
        nonce_seed: 7_000,
    }
}

type ServerSocket = WebSocketStream<DuplexStream>;

#[derive(Default)]
struct SignalingState {
    sockets: VecDeque<ServerSocket>,
    urls: Vec<String>,
}

#[derive(Clone, Default)]
struct FakeSignaling(Arc<Mutex<SignalingState>>);

impl FakeSignaling {
    /// Queues the next connection the client will get; returns its server end.
    async fn accept(&self) -> Ws {
        let (client_io, server_io) = duplex(1024 * 1024);
        let client =
            WebSocketStream::from_raw_socket(client_io, Role::Client, Some(websocket_config()))
                .await;
        let server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        self.0.lock().sockets.push_back(client);
        Ws(server)
    }

    fn urls(&self) -> Vec<String> {
        self.0.lock().urls.clone()
    }
}

impl Signaling for FakeSignaling {
    type Socket = ServerSocket;

    async fn connect(&self, url: &str) -> Result<ServerSocket, ConnectError> {
        let mut state = self.0.lock();
        state.urls.push(url.to_owned());
        state.sockets.pop_front().ok_or(ConnectError)
    }
}

struct FakeLink {
    to_server: mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>,
    from_server: tokio::sync::Mutex<mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>>,
}

impl UdpLink for FakeLink {
    async fn send_to(&self, datagram: &[u8], to: SocketAddr) -> io::Result<()> {
        self.to_server
            .send((datagram.to_vec(), to))
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    async fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let Some((datagram, from)) = self.from_server.lock().await.recv().await else {
            return std::future::pending().await;
        };
        let len = datagram.len().min(buffer.len());
        buffer[..len].copy_from_slice(&datagram[..len]);
        Ok((len, from))
    }
}

#[derive(Clone)]
struct FakeNetwork {
    peers: mpsc::UnboundedSender<Udp>,
    binds: Arc<Mutex<Vec<SocketAddr>>>,
}

impl Network for FakeNetwork {
    type Udp = FakeLink;

    async fn bind(&self, server: SocketAddr) -> io::Result<FakeLink> {
        self.binds.lock().push(server);
        let (to_server, from_client) = mpsc::unbounded_channel();
        let (to_client, from_server) = mpsc::unbounded_channel();
        let _ = self.peers.send(Udp {
            from_client,
            to_client,
            log: Vec::new(),
        });
        Ok(FakeLink {
            to_server,
            from_server: tokio::sync::Mutex::new(from_server),
        })
    }
}

/// The voice server's UDP side.
struct Udp {
    from_client: mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>,
    to_client: mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>,
    /// Every datagram the client sent.
    log: Vec<Vec<u8>>,
}

fn is_ping(datagram: &[u8]) -> bool {
    datagram.len() == 8 && datagram[..4] == [0x13, 0x37, 0xCA, 0xFE]
}

impl Udp {
    /// The next datagram, whatever it is.
    async fn recv_any(&mut self) -> Vec<u8> {
        let (datagram, to) = timeout(Duration::from_secs(600), self.from_client.recv())
            .await
            .expect("the client sent no datagram")
            .expect("the client dropped its socket");
        assert_eq!(to, server_addr());
        self.log.push(datagram.clone());
        datagram
    }

    /// The next datagram that is not a UDP ping.
    async fn recv(&mut self) -> Vec<u8> {
        loop {
            let datagram = self.recv_any().await;
            if !is_ping(&datagram) {
                return datagram;
            }
        }
    }

    /// Everything already sent, without waiting.
    fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Ok((datagram, _)) = self.from_client.try_recv() {
            self.log.push(datagram.clone());
            out.push(datagram);
        }
        out
    }

    fn send(&self, datagram: Vec<u8>) {
        self.send_from(datagram, server_addr());
    }

    fn send_from(&self, datagram: Vec<u8>, from: SocketAddr) {
        self.to_client.send((datagram, from)).unwrap();
    }

    fn answer_discovery(&self, request: &[u8]) {
        assert_eq!(request.len(), DISCOVERY_LEN);
        let mut response = request.to_vec();
        response[1] = 2;
        let address = external_addr().ip().to_string();
        response[8..8 + address.len()].copy_from_slice(address.as_bytes());
        response[72..].copy_from_slice(&external_addr().port().to_be_bytes());
        self.send(response);
    }
}

struct Ws(ServerSocket);

impl Ws {
    async fn send(&mut self, value: Value) {
        self.0.send(Message::text(value.to_string())).await.unwrap();
    }

    async fn hello(&mut self, interval_ms: f64) {
        self.send(json!({"op": 8, "d": {"v": 8, "heartbeat_interval": interval_ms}}))
            .await;
    }

    /// The next text message, unanswered.
    async fn recv_raw(&mut self) -> Value {
        timeout(Duration::from_secs(600), async {
            loop {
                match self.0.next().await.expect("client hung up").unwrap() {
                    Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                    Message::Close(frame) => panic!("client closed: {frame:?}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("the client sent nothing")
    }

    /// The next message with opcode `op`; heartbeats on the way are ACKed.
    async fn recv_op(&mut self, op: u64) -> Value {
        loop {
            let message = self.recv_raw().await;
            if message["op"] == op {
                return message;
            }
            assert_eq!(message["op"], 3, "unexpected {message}");
            self.send(json!({"op": 6, "d": {"t": message["d"]["t"]}}))
                .await;
        }
    }

    /// A message the client has already written, without waiting.
    fn try_recv(&mut self) -> Option<Value> {
        match self.0.next().now_or_never()??.ok()? {
            Message::Text(text) => serde_json::from_str(text.as_str()).ok(),
            _ => None,
        }
    }

    async fn close(&mut self, code: u16) {
        let _ = self
            .0
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::from(code),
                reason: "".into(),
            })))
            .await;
    }

    async fn recv_close_code(&mut self) -> Option<u16> {
        timeout(Duration::from_secs(60), async {
            loop {
                match self.0.next().await {
                    Some(Ok(Message::Close(frame))) => return frame.map(|f| u16::from(f.code)),
                    Some(Ok(_)) => {}
                    _ => return None,
                }
            }
        })
        .await
        .expect("the client neither closed nor hung up")
    }
}

struct Harness {
    session: VoiceSession,
    peers: mpsc::UnboundedReceiver<Udp>,
    binds: Arc<Mutex<Vec<SocketAddr>>>,
}

impl Harness {
    fn start(options: Options) -> (Self, FakeSignaling) {
        Self::start_with(credentials("fixture.discord.media:443"), options)
    }

    fn start_with(credentials: VoiceCredentials, options: Options) -> (Self, FakeSignaling) {
        let signaling = FakeSignaling::default();
        let (peers_tx, peers) = mpsc::unbounded_channel();
        let binds = Arc::new(Mutex::new(Vec::new()));
        let network = FakeNetwork {
            peers: peers_tx,
            binds: Arc::clone(&binds),
        };
        let session = start(signaling.clone(), network, credentials, options);
        (
            Self {
                session,
                peers,
                binds,
            },
            signaling,
        )
    }

    async fn udp(&mut self) -> Udp {
        timeout(Duration::from_secs(600), self.peers.recv())
            .await
            .expect("the client never opened UDP")
            .unwrap()
    }

    async fn wait_status(&self, wanted: impl Fn(&VoiceStatus) -> bool) -> VoiceStatus {
        let mut status = self.session.watch_status();
        let found = timeout(Duration::from_secs(600), status.wait_for(|s| wanted(s)))
            .await
            .expect("status never reached")
            .map(|s| *s);
        found.unwrap_or_else(|_| self.session.status())
    }

    async fn closed(&self) -> CloseReason {
        match self
            .wait_status(|s| matches!(s, VoiceStatus::Closed(_)))
            .await
        {
            VoiceStatus::Closed(reason) => reason,
            other => panic!("not closed: {other:?}"),
        }
    }

    async fn event(&mut self) -> VoiceEvent {
        timeout(Duration::from_secs(600), self.session.next_event())
            .await
            .expect("no event")
            .expect("events ended")
    }
}

fn ready(modes: &[&str]) -> Value {
    json!({"op": 2, "d": {
        "ssrc": SSRC,
        "ip": server_addr().ip().to_string(),
        "port": server_addr().port(),
        "modes": modes,
        "experiments": ["fixed_keyframe_interval"],
        "streams": []
    }})
}

fn session_description(mode: &str) -> Value {
    json!({"op": 4, "d": {
        "audio_codec": "opus",
        "media_session_id": "fixture-media-session",
        "mode": mode,
        "secret_key": KEY,
        "video_codec": "H264",
        "dave_protocol_version": 0
    }})
}

/// HELLO, Identify, READY, IP discovery, and Select Protocol; returns the
/// Select Protocol payload with the UDP side ready for Session Description.
async fn negotiate(harness: &mut Harness, ws: &mut Ws, modes: &[&str]) -> (Udp, Value) {
    ws.hello(QUIET_HEARTBEAT_MS).await;
    let identify = ws.recv_op(0).await;
    assert_eq!(identify["d"]["token"], "fixture-voice-token");
    ws.send(ready(modes)).await;
    let mut udp = harness.udp().await;
    let discovery = udp.recv().await;
    assert_eq!(discovery.len(), DISCOVERY_LEN);
    assert_eq!(discovery[..8], [0, 1, 0, 70, 0, 0, 0x32, 0x47]);
    udp.answer_discovery(&discovery);
    let select = ws.recv_op(1).await;
    (udp, select)
}

/// A full handshake in AES mode.
async fn connected(harness: &mut Harness, ws: &mut Ws) -> (Udp, TransportCipher) {
    let (udp, _) = negotiate(harness, ws, &BOTH_MODES).await;
    ws.send(session_description("aead_aes256_gcm_rtpsize"))
        .await;
    harness
        .wait_status(|s| matches!(s, VoiceStatus::Connected { .. }))
        .await;
    let key = TransportKey::from_slice(&KEY).unwrap();
    (
        udp,
        TransportCipher::new(TransportMode::Aes256GcmRtpSize, &key),
    )
}

/// Opens one client RTP datagram with the session key.
fn open_rtp(cipher: &TransportCipher, datagram: &[u8]) -> (RtpHeader, Vec<u8>, u32) {
    let header = RtpHeader::parse(datagram).unwrap();
    let mut body = Vec::new();
    let nonce = cipher
        .open(datagram, header.clear_len(), &mut body)
        .expect("client RTP must authenticate with the session key");
    let payload = header.split_body(&body).unwrap().payload.to_vec();
    (header, payload, nonce)
}

/// One server RTP datagram from `ssrc`.
fn server_rtp(sender: &mut TransportCipher, ssrc: u32, sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut clear = Vec::new();
    RtpHeader::new(120, sequence, u32::from(sequence) * 960, ssrc).write(&mut clear);
    let mut out = Vec::new();
    sender.seal(&clear, payload, &mut out).unwrap();
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test(start_paused = true)]
async fn the_handshake_runs_in_order_and_media_leaves_only_encrypted_after_speaking() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let media = harness.session.media();
    // Queued before the key exists; released only once it does.
    media.send_opus(b"OPUS-FRAME-1".to_vec()).unwrap();
    media.send_opus(b"OPUS-FRAME-2".to_vec()).unwrap();

    ws.hello(QUIET_HEARTBEAT_MS).await;
    assert_eq!(
        ws.recv_op(0).await,
        json!({"op": 0, "d": {
            "server_id": "41771983423143937",
            "user_id": "104694319306248192",
            "session_id": "fixture-session",
            "token": "fixture-voice-token",
            "max_dave_protocol_version": 0
        }})
    );
    assert_eq!(harness.session.status(), VoiceStatus::Identifying);
    assert_eq!(signaling.urls(), ["wss://fixture.discord.media:443/?v=8"]);
    ws.send(ready(&BOTH_MODES)).await;
    let mut udp = harness.udp().await;
    assert_eq!(*harness.binds.lock(), [server_addr()]);
    let discovery = udp.recv().await;
    assert_eq!(harness.session.status(), VoiceStatus::Discovering);
    udp.answer_discovery(&discovery);
    assert_eq!(
        ws.recv_op(1).await,
        json!({"op": 1, "d": {
            "protocol": "udp",
            "data": {"address": "203.0.113.7", "port": 50123, "mode": "aead_aes256_gcm_rtpsize"},
            "codecs": [{"name": "opus", "type": "audio", "priority": 1000, "payload_type": 120}]
        }})
    );
    assert_eq!(harness.session.status(), VoiceStatus::Negotiating);
    // Nothing but discovery has gone out over UDP while negotiating.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(udp.drain().iter().all(|d| is_ping(d)));

    ws.send(session_description("aead_aes256_gcm_rtpsize"))
        .await;
    let first = udp.recv().await;
    // The Speaking announcement was written before the first RTP packet.
    assert_eq!(
        ws.try_recv(),
        Some(json!({"op": 5, "d": {"speaking": 1, "delay": 0, "ssrc": SSRC}}))
    );
    let second = udp.recv().await;
    assert_eq!(
        harness.session.status(),
        VoiceStatus::Connected {
            mode: TransportMode::Aes256GcmRtpSize,
            ssrc: SSRC
        }
    );
    let key = TransportKey::from_slice(&KEY).unwrap();
    let cipher = TransportCipher::new(TransportMode::Aes256GcmRtpSize, &key);
    let (header, payload, nonce) = open_rtp(&cipher, &first);
    assert_eq!(
        (
            header.ssrc,
            header.payload_type,
            header.sequence,
            header.timestamp
        ),
        (SSRC, 120, 100, 1_000)
    );
    assert_eq!((payload.as_slice(), nonce), (&b"OPUS-FRAME-1"[..], 0));
    let (header, payload, nonce) = open_rtp(&cipher, &second);
    assert_eq!((header.sequence, header.timestamp), (101, 1_960));
    assert_eq!((payload.as_slice(), nonce), (&b"OPUS-FRAME-2"[..], 1));

    // Later frames go straight out; the speaking state is not repeated.
    media.send_opus(b"OPUS-FRAME-3".to_vec()).unwrap();
    let third = udp.recv().await;
    assert_eq!(open_rtp(&cipher, &third).1, b"OPUS-FRAME-3");
    assert_eq!(ws.try_recv(), None);
    for datagram in &udp.log {
        assert!(
            !contains(datagram, b"OPUS-FRAME"),
            "plaintext media on the wire"
        );
    }
    assert_eq!(harness.session.stats().rtp_sent, 3);
}

#[tokio::test(start_paused = true)]
async fn the_media_queue_is_bounded_and_rejects_invalid_frames() {
    let (harness, _signaling) = Harness::start(options());
    let media = harness.session.media();
    for _ in 0..MEDIA_QUEUE_FRAMES {
        media.send_opus(vec![1; 10]).unwrap();
    }
    assert_eq!(media.send_opus(vec![1; 10]), Err(MediaSendError::Full));
    assert_eq!(
        media.send_opus(Vec::new()),
        Err(MediaSendError::InvalidFrame)
    );
    assert_eq!(
        media.send_opus(vec![0; MAX_OPUS_FRAME + 1]),
        Err(MediaSendError::InvalidFrame)
    );
    harness.session.leave().await;
    assert_eq!(media.send_opus(vec![1]), Err(MediaSendError::Closed));
}

#[tokio::test(start_paused = true)]
async fn xchacha_is_chosen_without_aes_acceleration() {
    let (mut harness, signaling) = Harness::start(Options {
        aes_accelerated: false,
        ..options()
    });
    let mut ws = signaling.accept().await;
    let (mut udp, select) = negotiate(&mut harness, &mut ws, &BOTH_MODES).await;
    assert_eq!(
        select["d"]["data"]["mode"],
        "aead_xchacha20_poly1305_rtpsize"
    );
    ws.send(session_description("aead_xchacha20_poly1305_rtpsize"))
        .await;
    harness
        .session
        .media()
        .send_opus(b"frame".to_vec())
        .unwrap();
    let datagram = udp.recv().await;
    let key = TransportKey::from_slice(&KEY).unwrap();
    let cipher = TransportCipher::new(TransportMode::XChaCha20Poly1305RtpSize, &key);
    assert_eq!(open_rtp(&cipher, &datagram).1, b"frame");
}

#[tokio::test(start_paused = true)]
async fn ready_without_a_supported_mode_stops_before_udp() {
    let (harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    ws.hello(QUIET_HEARTBEAT_MS).await;
    ws.recv_op(0).await;
    ws.send(ready(&["xsalsa20_poly1305", "aead_aes256_gcm"]))
        .await;
    assert_eq!(harness.closed().await, CloseReason::NoSupportedMode);
    assert!(harness.binds.lock().is_empty());
    assert_eq!(signaling.urls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_bad_session_description_never_releases_media() {
    for description in [
        // Not the mode that was selected.
        session_description("aead_xchacha20_poly1305_rtpsize"),
        // DAVE was not offered.
        {
            let mut d = session_description("aead_aes256_gcm_rtpsize");
            d["d"]["dave_protocol_version"] = json!(1);
            d
        },
        // A 31-byte key.
        {
            let mut d = session_description("aead_aes256_gcm_rtpsize");
            d["d"]["secret_key"] = json!(vec![1; 31]);
            d
        },
        // Missing codec and DAVE fields are not a valid Session Description.
        {
            let mut d = session_description("aead_aes256_gcm_rtpsize");
            d["d"].as_object_mut().unwrap().remove("audio_codec");
            d
        },
        {
            let mut d = session_description("aead_aes256_gcm_rtpsize");
            d["d"]
                .as_object_mut()
                .unwrap()
                .remove("dave_protocol_version");
            d
        },
        {
            let mut d = session_description("aead_aes256_gcm_rtpsize");
            d["d"]["audio_codec"] = json!("h264");
            d
        },
    ] {
        let (mut harness, signaling) = Harness::start(options());
        let mut ws = signaling.accept().await;
        harness
            .session
            .media()
            .send_opus(b"secret".to_vec())
            .unwrap();
        let (mut udp, _) = negotiate(&mut harness, &mut ws, &BOTH_MODES).await;
        ws.send(description).await;
        assert_eq!(harness.closed().await, CloseReason::Protocol(None));
        let sent = udp.drain();
        assert!(sent.iter().all(|d| is_ping(d)), "{sent:?}");
        assert_eq!(harness.session.stats().rtp_sent, 0);
    }
}

#[tokio::test(start_paused = true)]
async fn heartbeats_ack_sequences_and_a_missed_ack_resumes_with_the_same_key() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let mut second = signaling.accept().await;
    let (mut udp, cipher) = connected(&mut harness, &mut ws).await;
    // A numbered JSON message, then a numbered binary message.
    ws.send(json!({"op": 11, "d": {"user_ids": ["852892297661906993"]}, "seq": 5}))
        .await;
    assert_eq!(
        harness.event().await,
        VoiceEvent::ClientsConnected(vec![ALICE])
    );
    ws.0.send(Message::binary(vec![0, 7, 25, 0xAA]))
        .await
        .unwrap();

    let start = Instant::now();
    let heartbeat = ws.recv_raw().await;
    assert_eq!(heartbeat, json!({"op": 3, "d": {"t": 7_000, "seq_ack": 7}}));
    assert!(Instant::now() - start <= Duration::from_secs(60));
    ws.send(json!({"op": 6, "d": {"t": 7_000}})).await;
    let unanswered = ws.recv_raw().await;
    assert_eq!(unanswered["d"]["t"], 7_001);
    // No ACK: at the next interval the connection is a zombie and resumes.
    harness
        .wait_status(|s| matches!(s, VoiceStatus::Resuming { .. }))
        .await;
    second.hello(QUIET_HEARTBEAT_MS).await;
    assert_eq!(
        second.recv_op(7).await,
        json!({"op": 7, "d": {
            "server_id": "41771983423143937",
            "session_id": "fixture-session",
            "token": "fixture-voice-token",
            "seq_ack": 7
        }})
    );
    second.send(json!({"op": 9, "d": null})).await;
    harness
        .wait_status(|s| matches!(s, VoiceStatus::Connected { .. }))
        .await;
    // Same UDP socket and key; the nonce keeps counting.
    assert_eq!(harness.binds.lock().len(), 1);
    harness
        .session
        .media()
        .send_opus(b"after".to_vec())
        .unwrap();
    let datagram = udp.recv().await;
    let (_, payload, nonce) = open_rtp(&cipher, &datagram);
    assert_eq!((payload.as_slice(), nonce), (&b"after"[..], 0));
    assert_eq!(second.recv_op(5).await["d"]["speaking"], 1);
}

#[tokio::test(start_paused = true)]
async fn the_server_may_request_a_heartbeat_at_once() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    connected(&mut harness, &mut ws).await;
    let start = Instant::now();
    ws.send(json!({"op": 3, "d": null})).await;
    let heartbeat = ws.recv_raw().await;
    assert_eq!(heartbeat["op"], 3);
    assert_eq!(heartbeat["d"]["seq_ack"], -1);
    assert_eq!(Instant::now(), start);
}

#[tokio::test(start_paused = true)]
async fn close_codes_decide_between_stopping_and_resuming() {
    let cases = [
        (4014, CloseReason::Disconnected),
        (4022, CloseReason::Disconnected),
        (4006, CloseReason::SessionInvalid),
        (4009, CloseReason::SessionInvalid),
        (4004, CloseReason::AuthenticationFailed),
        (4017, CloseReason::E2eeRequired),
        (4021, CloseReason::RateLimited),
        (4011, CloseReason::ServerNotFound),
        (4016, CloseReason::Protocol(Some(4016))),
    ];
    for (code, reason) in cases {
        let (harness, signaling) = Harness::start(options());
        let mut ws = signaling.accept().await;
        ws.hello(QUIET_HEARTBEAT_MS).await;
        ws.recv_op(0).await;
        ws.close(code).await;
        assert_eq!(harness.closed().await, reason, "{code}");
        assert_eq!(signaling.urls().len(), 1, "{code} must not reconnect");
    }

    // A crashed voice server after media started: resume.
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let mut second = signaling.accept().await;
    connected(&mut harness, &mut ws).await;
    ws.close(4015).await;
    second.hello(QUIET_HEARTBEAT_MS).await;
    assert_eq!(second.recv_op(7).await["op"], 7);

    // Before media, a dropped socket identifies again with fresh UDP.
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let mut second = signaling.accept().await;
    let (_old_udp, _) = negotiate(&mut harness, &mut ws, &BOTH_MODES).await;
    drop(ws);
    second.hello(QUIET_HEARTBEAT_MS).await;
    assert_eq!(second.recv_op(0).await["op"], 0);
    second.send(ready(&BOTH_MODES)).await;
    harness.udp().await;
    assert_eq!(harness.binds.lock().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn repeated_connection_failures_give_up() {
    let (harness, signaling) = Harness::start(options());
    assert_eq!(harness.closed().await, CloseReason::Network);
    assert_eq!(signaling.urls().len(), 6);
}

#[tokio::test(start_paused = true)]
async fn repeated_pre_key_handshake_failures_are_bounded() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    for attempt in 0..6 {
        ws.hello(QUIET_HEARTBEAT_MS).await;
        ws.recv_op(0).await;
        ws.send(ready(&BOTH_MODES)).await;
        let mut udp = harness.udp().await;
        let discovery = udp.recv().await;
        udp.answer_discovery(&discovery);
        ws.recv_op(1).await;
        tokio::time::advance(Duration::from_secs(15)).await;
        if attempt < 5 {
            harness
                .wait_status(|status| matches!(status, VoiceStatus::Connecting))
                .await;
            let next_ws = signaling.accept().await;
            tokio::time::advance(Duration::from_secs(1_u64 << attempt)).await;
            ws = next_ws;
        }
    }
    assert_eq!(harness.closed().await, CloseReason::Network);
    assert_eq!(signaling.urls().len(), 6);
}

#[tokio::test(start_paused = true)]
async fn an_extreme_hello_interval_is_clamped_without_panicking() {
    let (harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    ws.hello(f64::MAX).await;
    assert_eq!(ws.recv_op(0).await["op"], 0);
    assert_eq!(harness.session.status(), VoiceStatus::Identifying);
}

#[tokio::test(start_paused = true)]
async fn ip_discovery_retries_then_reports_blocked_udp() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    ws.hello(QUIET_HEARTBEAT_MS).await;
    ws.recv_op(0).await;
    ws.send(ready(&BOTH_MODES)).await;
    let mut udp = harness.udp().await;
    let first = udp.recv().await;
    let start = Instant::now();
    // Answers from another address or for another SSRC do not count.
    let mut wrong_ssrc = first.clone();
    wrong_ssrc[1] = 2;
    wrong_ssrc[7] ^= 1;
    udp.send(wrong_ssrc);
    let mut foreign = first.clone();
    foreign[1] = 2;
    udp.send_from(foreign, "198.51.100.99:50001".parse().unwrap());
    for _ in 1..5 {
        let retry = udp.recv().await;
        assert_eq!(retry, first);
    }
    assert_eq!(harness.closed().await, CloseReason::UdpUnreachable);
    assert_eq!(Instant::now() - start, Duration::from_secs(5));
    let stats = harness.session.stats();
    assert_eq!(stats.rejected_malformed, 1);
    assert_eq!(stats.rejected_source, 1);
}

#[tokio::test(start_paused = true)]
async fn received_audio_is_attributed_only_through_ssrc_mappings() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let (udp, _) = connected(&mut harness, &mut ws).await;
    let key = TransportKey::from_slice(&KEY).unwrap();
    let mut sender = TransportCipher::starting_at(TransportMode::Aes256GcmRtpSize, &key, 500);

    // Media races ahead of Speaking: held, then attributed.
    udp.send(server_rtp(&mut sender, 555, 65_535, b"alice-1"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    ws.send(json!({"op": 5, "d": {"speaking": 1, "ssrc": 555, "user_id": "852892297661906993"}}))
        .await;
    assert_eq!(
        harness.event().await,
        VoiceEvent::Speaking {
            user_id: ALICE,
            ssrc: 555,
            flags: 1
        }
    );
    let first = harness.session.next_audio().await.unwrap();
    assert_eq!(
        (first.user_id, first.ssrc, first.payload.as_slice()),
        (ALICE, 555, &b"alice-1"[..])
    );
    // The sequence wraps; the extended sequence keeps increasing.
    udp.send(server_rtp(&mut sender, 555, 0, b"alice-2"));
    let second = harness.session.next_audio().await.unwrap();
    assert_eq!(second.sequence, first.sequence + 1);
    assert_eq!(second.payload, b"alice-2");

    // Extension elements are encrypted with the payload and stripped from it.
    let header = RtpHeader {
        extension: Some(ExtensionPreamble {
            profile: ONE_BYTE_PROFILE,
            words: 1,
        }),
        ..RtpHeader::new(120, 1, 960, 555)
    };
    let mut clear = Vec::new();
    header.write(&mut clear);
    let mut body = vec![0x90, 0x03, 0, 0];
    body.extend_from_slice(b"alice-3");
    let mut datagram = Vec::new();
    sender.seal(&clear, &body, &mut datagram).unwrap();
    udp.send(datagram);
    assert_eq!(
        harness.session.next_audio().await.unwrap().payload,
        b"alice-3"
    );

    // Never announced: dropped after the hold window, not given to Alice.
    udp.send(server_rtp(&mut sender, 556, 1, b"stranger"));
    // Tampered, foreign, and malformed datagrams.
    let mut tampered = server_rtp(&mut sender, 555, 2, b"evil");
    tampered[14] ^= 0xFF;
    udp.send(tampered);
    udp.send_from(
        server_rtp(&mut sender, 555, 3, b"spoof"),
        "198.51.100.99:50001".parse().unwrap(),
    );
    udp.send(vec![0x80, 0x78, 0, 1]);
    udp.send(server_rtp(&mut sender, 555, 4, b""));
    udp.send(server_rtp(&mut sender, 555, 5, &[0; MAX_OPUS_FRAME + 1]));
    let malformed_extension = RtpHeader {
        extension: Some(ExtensionPreamble {
            profile: ONE_BYTE_PROFILE,
            words: 1,
        }),
        ..RtpHeader::new(120, 6, 5_760, 555)
    };
    let mut clear = Vec::new();
    malformed_extension.write(&mut clear);
    let body = vec![0x9F, 0, 0, 0, b'x'];
    let mut datagram = Vec::new();
    sender.seal(&clear, &body, &mut datagram).unwrap();
    udp.send(datagram);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let stats = harness.session.stats();
    assert_eq!(stats.dropped_unknown_ssrc, 1);
    assert_eq!(stats.rejected_authentication, 1);
    assert_eq!(stats.rejected_source, 1);
    assert_eq!(stats.rejected_malformed, 4);
    assert_eq!(stats.rtp_received, 4);

    // After Client Disconnect, Alice's SSRC is no longer hers.
    ws.send(json!({"op": 13, "d": {"user_id": "852892297661906993"}}))
        .await;
    assert_eq!(harness.event().await, VoiceEvent::ClientDisconnected(ALICE));
    udp.send(server_rtp(&mut sender, 555, 4, b"after-leave"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(harness.session.stats().dropped_unknown_ssrc, 2);
    assert!(harness.session.next_audio().now_or_never().is_none());

    // Video events map the audio SSRC too.
    ws.send(json!({"op": 12, "d": {"user_id": "852892297661906993", "audio_ssrc": 777, "video_ssrc": 778,
        "streams": [{"ssrc": 778, "rtx_ssrc": 779, "rid": "100", "quality": 100, "active": true}]}}))
        .await;
    udp.send(server_rtp(&mut sender, 777, 9, b"alice-video-audio"));
    let audio = harness.session.next_audio().await.unwrap();
    assert_eq!(
        (audio.user_id, audio.payload.as_slice()),
        (ALICE, &b"alice-video-audio"[..])
    );
}

#[tokio::test(start_paused = true)]
async fn rtcp_reports_about_our_stream_surface_and_we_send_sender_reports() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let (mut udp, cipher) = connected(&mut harness, &mut ws).await;
    let key = TransportKey::from_slice(&KEY).unwrap();
    let mut sender = TransportCipher::starting_at(TransportMode::Aes256GcmRtpSize, &key, 900);

    // A receiver report with one block about us and one about someone else.
    let mut report = vec![0x82, 201, 0, 13];
    report.extend_from_slice(&1u32.to_be_bytes());
    for (ssrc, lost) in [(SSRC, 0x0Au8), (4242, 0x80)] {
        report.extend_from_slice(&ssrc.to_be_bytes());
        report.extend_from_slice(&[lost, 0, 0, 3]);
        report.extend_from_slice(&[0, 0, 0, 50, 0, 0, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0]);
    }
    let mut datagram = Vec::new();
    sender
        .seal(
            &report[..RTCP_CLEAR_LEN],
            &report[RTCP_CLEAR_LEN..],
            &mut datagram,
        )
        .unwrap();
    udp.send(datagram);
    assert_eq!(
        harness.event().await,
        VoiceEvent::ReceptionReport {
            fraction_lost: 10,
            cumulative_lost: 3,
            jitter: 20
        }
    );

    // After sending media, a sender report follows within 5 s.
    let media = harness.session.media();
    media.send_opus(b"one".to_vec()).unwrap();
    media.send_opus(b"two".to_vec()).unwrap();
    udp.recv().await;
    udp.recv().await;
    let report = loop {
        let datagram = udp.recv().await;
        if rtcp_wire::is_rtcp(&datagram) {
            break datagram;
        }
    };
    let mut body = Vec::new();
    let nonce = cipher.open(&report, RTCP_CLEAR_LEN, &mut body).unwrap();
    assert_eq!(nonce, 2);
    let mut plain = report[..RTCP_CLEAR_LEN].to_vec();
    plain.extend_from_slice(&body);
    let Some(Ok(RtcpPacket::SenderReport(sr))) = Compound::new(&plain).next() else {
        panic!("not a sender report");
    };
    assert_eq!(sr.ssrc, SSRC);
    assert_eq!(sr.info.packet_count, 2);
    assert_eq!(sr.info.octet_count, 6);
    assert_eq!(sr.info.rtp_timestamp, 1_000 + 2 * 960);
}

#[tokio::test(start_paused = true)]
async fn udp_pings_measure_latency() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let (mut udp, _) = connected(&mut harness, &mut ws).await;
    let ping = loop {
        let datagram = udp.recv_any().await;
        if is_ping(&datagram) {
            break datagram;
        }
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut pong = ping.clone();
    pong[2..4].copy_from_slice(&[0xF0, 0x0D]);
    udp.send(pong);
    assert_eq!(
        harness.event().await,
        VoiceEvent::UdpLatency(Duration::from_millis(30))
    );
}

#[tokio::test(start_paused = true)]
async fn ending_speech_sends_five_paced_silence_frames_then_speaking_off() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let (mut udp, cipher) = connected(&mut harness, &mut ws).await;
    let media = harness.session.media();
    media.send_opus(b"voice".to_vec()).unwrap();
    udp.recv().await;
    assert_eq!(ws.recv_op(5).await["d"]["speaking"], 1);
    media.end_speech().unwrap();
    let start = Instant::now();
    let mut times = Vec::new();
    for _ in 0..5 {
        let datagram = udp.recv().await;
        assert_eq!(open_rtp(&cipher, &datagram).1, [0xF8, 0xFF, 0xFE]);
        times.push(Instant::now() - start);
    }
    assert_eq!(
        times,
        (0..5)
            .map(|i| Duration::from_millis(20 * i))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        ws.recv_op(5).await,
        json!({"op": 5, "d": {"speaking": 0, "delay": 0, "ssrc": SSRC}})
    );
    // Speaking again is announced again.
    media.send_opus(b"again".to_vec()).unwrap();
    udp.recv().await;
    assert_eq!(ws.recv_op(5).await["d"]["speaking"], 1);
}

#[tokio::test(start_paused = true)]
async fn nonce_exhaustion_stops_media_while_rtp_sequence_wraps_freely() {
    let (mut harness, signaling) = Harness::start(Options {
        first_nonce: u32::MAX - 1,
        rtp_start: Some((u16::MAX, u32::MAX - 959)),
        ..options()
    });
    let mut ws = signaling.accept().await;
    let (mut udp, cipher) = connected(&mut harness, &mut ws).await;
    let media = harness.session.media();
    for frame in [&b"a"[..], b"b", b"c"] {
        media.send_opus(frame.to_vec()).unwrap();
    }
    let (first, _, first_nonce) = open_rtp(&cipher, &udp.recv().await);
    let (second, _, second_nonce) = open_rtp(&cipher, &udp.recv().await);
    // The RTP sequence and timestamp wrap; the transport nonce does not.
    assert_eq!((first.sequence, second.sequence), (u16::MAX, 0));
    assert_eq!((first.timestamp, second.timestamp), (u32::MAX - 959, 0));
    assert_eq!((first_nonce, second_nonce), (u32::MAX - 1, u32::MAX));
    assert_eq!(harness.closed().await, CloseReason::KeyExhausted);
    // The third frame was never sent under a reused nonce.
    let rest = udp.drain();
    assert!(rest.iter().all(|d| is_ping(d)), "{rest:?}");
    assert_eq!(harness.session.stats().rtp_sent, 2);
}

#[tokio::test(start_paused = true)]
async fn leaving_closes_normally_and_drops_udp() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let (mut udp, _) = connected(&mut harness, &mut ws).await;
    let status = harness.session.watch_status();
    let session = harness.session;
    let leave = tokio::spawn(session.leave());
    assert_eq!(ws.recv_close_code().await, Some(1000));
    leave.await.unwrap();
    assert_eq!(*status.borrow(), VoiceStatus::Closed(CloseReason::Left));
    // The client's UDP socket is gone.
    assert!(udp.from_client.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn an_endpoint_with_a_scheme_or_path_is_never_contacted() {
    let (harness, signaling) = Harness::start_with(credentials("wss://evil.example/x"), options());
    assert_eq!(harness.closed().await, CloseReason::InvalidEndpoint);
    assert!(signaling.urls().is_empty());
}

#[test]
fn credentials_and_sessions_never_print_secrets() {
    let credentials = credentials("fixture.discord.media:443");
    let text = format!("{credentials:?}");
    assert!(!text.contains("fixture-voice-token") && !text.contains("fixture-session"));
}
