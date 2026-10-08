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
use crate::dave::DaveSession;
use crate::rtp::RtpHeader;
use crate::udp::DISCOVERY_LEN;
use openmls::prelude::{
    BasicCredential, Ciphersuite, ExternalProposal, ExternalSender, GroupEpoch, GroupId,
    KeyPackageIn, MlsMessageIn, MlsMessageOut, SenderExtensionIndex, Welcome,
};
use openmls::versions::ProtocolVersion;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use openmls_traits::OpenMlsProvider;
use tls_codec::{DeserializeBytes, Serialize, VLBytes};

const KEY: [u8; 32] = [0x5A; 32];
const SSRC: u32 = 12_871;
const ME: Snowflake = Snowflake(104_694_319_306_248_192);
const ALICE: Snowflake = Snowflake(852_892_297_661_906_993);
const CAROL: Snowflake = Snowflake(4);
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
        "dave_protocol_version": 1
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
        .wait_status(|s| matches!(s, VoiceStatus::Rekeying { .. }))
        .await;
    let Message::Binary(key_package) = ws.0.next().await.unwrap().unwrap() else {
        panic!("expected DAVE key package")
    };
    assert!(key_package.len() > 1 && key_package[0] == 26);
    let key = TransportKey::from_slice(&KEY).unwrap();
    (
        udp,
        TransportCipher::new(TransportMode::Aes256GcmRtpSize, &key),
    )
}

struct TestDaveGroup {
    peer: DaveSession,
    provider: OpenMlsRustCrypto,
    delivery_signer: SignatureKeyPair,
    external_sender_bytes: Vec<u8>,
}

/// Installs a valid initial group through an external Add proposal and Welcome.
async fn install_test_dave_group(harness: &Harness, ws: &mut Ws) -> TestDaveGroup {
    const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
    let channel_id = credentials("fixture.discord.media:443").channel_id.0;
    let Message::Binary(client_key_package) = ws.0.next().await.unwrap().unwrap() else {
        panic!("expected DAVE key package")
    };
    assert_eq!(client_key_package[0], 26);

    let provider = OpenMlsRustCrypto::default();
    let delivery_signer = SignatureKeyPair::new(CIPHERSUITE.signature_algorithm()).unwrap();
    delivery_signer.store(provider.storage()).unwrap();
    let external_sender = ExternalSender::new(
        delivery_signer.public().into(),
        BasicCredential::new(b"voice-gateway".to_vec()).into(),
    );
    let external_sender_bytes = external_sender.tls_serialize_detached().unwrap();
    let mut peer = DaveSession::new(1, ALICE.0, channel_id).unwrap();
    peer.set_external_sender(&external_sender_bytes).unwrap();
    let client_key_package = KeyPackageIn::tls_deserialize_exact_bytes(&client_key_package[1..])
        .unwrap()
        .validate(provider.crypto(), ProtocolVersion::Mls10)
        .unwrap();
    let add = ExternalProposal::new_add::<OpenMlsRustCrypto>(
        client_key_package,
        GroupId::from_slice(&channel_id.to_be_bytes()),
        GroupEpoch::from(peer.epoch().unwrap()),
        &delivery_signer,
        SenderExtensionIndex::new(0),
    )
    .unwrap();
    let proposal_payload = dave_proposal_payload(&add);
    let initial = peer
        .process_proposals(0, &proposal_payload, &[ALICE.0, ME.0])
        .unwrap()
        .unwrap();
    peer.process_commit(1, &initial.commit).unwrap();
    assert!(peer.prepare_transition(1, 1).unwrap());
    peer.execute_transition(1).unwrap();

    let mut external_sender_frame = vec![0, 0, 25];
    external_sender_frame.extend_from_slice(&external_sender_bytes);
    ws.0.send(Message::binary(external_sender_frame))
        .await
        .unwrap();
    ws.send(json!({"op": 21, "d": {"protocol_version": 1, "transition_id": 1}}))
        .await;
    let mut frame = vec![0, 1, 30, 0, 1];
    frame.extend_from_slice(initial.welcome.as_deref().unwrap());
    ws.0.send(Message::binary(frame)).await.unwrap();
    assert_eq!(ws.recv_op(23).await["d"]["transition_id"], 1);
    ws.send(json!({"op": 22, "d": {"transition_id": 1}})).await;
    harness
        .wait_status(|status| matches!(status, VoiceStatus::Connected { .. }))
        .await;
    TestDaveGroup {
        peer,
        provider,
        delivery_signer,
        external_sender_bytes,
    }
}

fn dave_proposal_payload(proposal: &MlsMessageOut) -> Vec<u8> {
    VLBytes::new(proposal.tls_serialize_detached().unwrap())
        .tls_serialize_detached()
        .unwrap()
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
#[tokio::test(start_paused = true)]
async fn driver_processes_authenticated_proposals_commits_and_encrypted_opus() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let (mut udp, _) = negotiate(&mut harness, &mut ws, &BOTH_MODES).await;
    let channel_id = credentials("fixture.discord.media:443").channel_id.0;
    ws.send(session_description("aead_aes256_gcm_rtpsize"))
        .await;
    let mut fixture = install_test_dave_group(&harness, &mut ws).await;

    let mut carol = DaveSession::new(1, CAROL.0, channel_id).unwrap();
    carol
        .set_external_sender(&fixture.external_sender_bytes)
        .unwrap();
    let key_package =
        KeyPackageIn::tls_deserialize_exact_bytes(&carol.create_key_package().unwrap())
            .unwrap()
            .validate(fixture.provider.crypto(), ProtocolVersion::Mls10)
            .unwrap();
    let proposal = ExternalProposal::new_add::<OpenMlsRustCrypto>(
        key_package,
        GroupId::from_slice(&channel_id.to_be_bytes()),
        GroupEpoch::from(fixture.peer.epoch().unwrap()),
        &fixture.delivery_signer,
        SenderExtensionIndex::new(0),
    )
    .unwrap();
    let proposal_payload = dave_proposal_payload(&proposal);
    let expected_users = [ME.0, ALICE.0, CAROL.0];
    fixture
        .peer
        .process_proposals(0, &proposal_payload, &expected_users)
        .unwrap()
        .unwrap();
    ws.send(json!({"op": 11, "d": {"user_ids": [ALICE.0.to_string(), CAROL.0.to_string()]}}))
        .await;
    let mut proposal_frame = vec![0, 2, 27, 0];
    proposal_frame.extend_from_slice(&proposal_payload);
    ws.0.send(Message::binary(proposal_frame)).await.unwrap();

    let Message::Binary(commit_welcome) = ws.0.next().await.unwrap().unwrap() else {
        panic!("expected DAVE commit/welcome response")
    };
    assert_eq!(commit_welcome[0], 28);
    let encoded_commit = &commit_welcome[1..];
    let (_, welcome_remainder) = MlsMessageIn::tls_deserialize_bytes(encoded_commit).unwrap();
    let commit_len = encoded_commit.len() - welcome_remainder.len();
    let commit = encoded_commit[..commit_len].to_vec();
    assert!(!welcome_remainder.is_empty());
    let welcome = Welcome::tls_deserialize_exact_bytes(welcome_remainder).unwrap();
    let welcome = welcome.tls_serialize_detached().unwrap();
    fixture.peer.process_commit(2, &commit).unwrap();
    carol.process_welcome(2, &welcome).unwrap();

    let mut announce = vec![0, 3, 29, 0, 2];
    announce.extend_from_slice(&commit);
    ws.0.send(Message::binary(announce)).await.unwrap();
    assert_eq!(ws.recv_op(23).await["d"]["transition_id"], 2);
    ws.send(json!({"op": 22, "d": {"transition_id": 2}})).await;
    assert!(fixture.peer.prepare_transition(2, 1).unwrap());
    fixture.peer.execute_transition(2).unwrap();
    assert!(carol.prepare_transition(2, 1).unwrap());
    carol.execute_transition(2).unwrap();
    harness
        .wait_status(|status| matches!(status, VoiceStatus::Connected { .. }))
        .await;

    harness
        .session
        .media()
        .send_opus(b"actual-runtime-dave-encrypted-frame".to_vec())
        .unwrap();
    assert_eq!(ws.recv_op(5).await["d"]["speaking"], 1);
    let datagram = udp.recv().await;
    let key = TransportKey::from_slice(&KEY).unwrap();
    let cipher = TransportCipher::new(TransportMode::Aes256GcmRtpSize, &key);
    let encrypted = open_rtp(&cipher, &datagram).1;
    assert_ne!(encrypted, b"actual-runtime-dave-encrypted-frame");
    assert_eq!(
        fixture.peer.decrypt_opus(ME.0, &encrypted).unwrap(),
        b"actual-runtime-dave-encrypted-frame"
    );
}

/// One server RTP datagram from `ssrc`.
fn server_rtp(sender: &mut TransportCipher, ssrc: u32, sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut clear = Vec::new();
    RtpHeader::new(120, sequence, u32::from(sequence) * 960, ssrc).write(&mut clear);
    let mut out = Vec::new();
    sender.seal(&clear, payload, &mut out).unwrap();
    out
}

#[tokio::test(start_paused = true)]
async fn transport_ready_keeps_media_paused_until_authenticated_dave_keys() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let media = harness.session.media();
    media.send_opus(b"OPUS-FRAME-1".to_vec()).unwrap();
    media.send_opus(b"OPUS-FRAME-2".to_vec()).unwrap();

    let (mut udp, select) = negotiate(&mut harness, &mut ws, &BOTH_MODES).await;
    assert_eq!(select["d"]["data"]["mode"], "aead_aes256_gcm_rtpsize");
    ws.send(session_description("aead_aes256_gcm_rtpsize"))
        .await;
    assert!(matches!(
        harness
            .wait_status(|status| matches!(status, VoiceStatus::Rekeying { .. }))
            .await,
        VoiceStatus::Rekeying { .. }
    ));
    let Message::Binary(key_package) = ws.0.next().await.unwrap().unwrap() else {
        panic!("expected DAVE key package")
    };
    assert!(key_package.len() > 1 && key_package[0] == 26);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(udp.drain().iter().all(|datagram| is_ping(datagram)));
    assert_eq!(harness.session.stats().rtp_sent, 0);
    assert_eq!(media.send_opus(b"still-paused".to_vec()), Ok(()));
    assert_eq!(
        harness.session.status(),
        VoiceStatus::Rekeying {
            transition_id: None
        }
    );
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
        .wait_status(|status| matches!(status, VoiceStatus::Rekeying { .. }))
        .await;
    let Message::Binary(key_package) = ws.0.next().await.unwrap().unwrap() else {
        panic!("expected DAVE key package")
    };
    assert!(key_package.len() > 1 && key_package[0] == 26);
    assert!(udp.drain().iter().all(|datagram| is_ping(datagram)));
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
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let (mut udp, _) = negotiate(&mut harness, &mut ws, &BOTH_MODES).await;
    let mut unsupported = session_description("aead_aes256_gcm_rtpsize");
    unsupported["d"]["dave_protocol_version"] = json!(0);
    ws.send(unsupported).await;
    assert_eq!(harness.closed().await, CloseReason::E2eeRequired);
    assert!(udp.drain().iter().all(|datagram| is_ping(datagram)));
}

#[tokio::test(start_paused = true)]
async fn heartbeats_ack_sequences_and_resume_keeps_transport_without_releasing_media() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let mut second = signaling.accept().await;
    let (mut udp, _cipher) = connected(&mut harness, &mut ws).await;
    // A numbered JSON message, then a numbered binary message.
    ws.send(json!({"op": 11, "d": {"user_ids": ["852892297661906993"]}, "seq": 5}))
        .await;
    assert_eq!(
        harness.event().await,
        VoiceEvent::ClientsConnected(vec![ALICE])
    );
    ws.0.send(Message::binary(vec![0, 7, 99])).await.unwrap();

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
        .wait_status(|s| matches!(s, VoiceStatus::Rekeying { .. }))
        .await;
    assert_eq!(harness.binds.lock().len(), 1);
    harness
        .session
        .media()
        .send_opus(b"after".to_vec())
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(udp.drain().iter().all(|datagram| is_ping(datagram)));
    assert_eq!(harness.session.stats().rtp_sent, 0);
    assert!(second.try_recv().is_none());
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
async fn received_media_requires_dave_decryption_after_ssrc_attribution() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let (udp, _) = connected(&mut harness, &mut ws).await;
    let key = TransportKey::from_slice(&KEY).unwrap();
    let mut sender = TransportCipher::starting_at(TransportMode::Aes256GcmRtpSize, &key, 500);

    udp.send(server_rtp(&mut sender, 555, 1, b"transport-only-plaintext"));
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
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(harness.session.next_audio().now_or_never().is_none());
    assert_eq!(harness.session.stats().rejected_authentication, 1);

    udp.send(server_rtp(&mut sender, 556, 2, b"unannounced"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(harness.session.stats().dropped_unknown_ssrc, 1);
    ws.send(json!({"op": 13, "d": {"user_id": "852892297661906993"}}))
        .await;
    assert_eq!(harness.event().await, VoiceEvent::ClientDisconnected(ALICE));
    assert!(harness.session.next_audio().now_or_never().is_none());
}

#[tokio::test(start_paused = true)]
async fn receiver_reports_are_received_while_local_media_stays_paused() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let (mut udp, _cipher) = connected(&mut harness, &mut ws).await;
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

    let media = harness.session.media();
    media.send_opus(b"one".to_vec()).unwrap();
    media.send_opus(b"two".to_vec()).unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(udp.drain().iter().all(|datagram| is_ping(datagram)));
    assert_eq!(harness.session.stats().rtp_sent, 0);
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
async fn silence_tail_and_transport_nonces_wait_for_dave_keys() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let (mut udp, _) = connected(&mut harness, &mut ws).await;
    let media = harness.session.media();
    media.send_opus(b"voice".to_vec()).unwrap();
    media.end_speech().unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(udp.drain().iter().all(|datagram| is_ping(datagram)));
    assert!(ws.try_recv().is_none());
    assert_eq!(harness.session.stats().rtp_sent, 0);

    let (mut nonce_harness, nonce_signaling) = Harness::start(Options {
        first_nonce: u32::MAX - 1,
        rtp_start: Some((u16::MAX, u32::MAX - 959)),
        ..options()
    });
    let mut nonce_ws = nonce_signaling.accept().await;
    let (mut nonce_udp, _) = connected(&mut nonce_harness, &mut nonce_ws).await;
    let media = nonce_harness.session.media();
    for frame in [&b"a"[..], b"b", b"c"] {
        media.send_opus(frame.to_vec()).unwrap();
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(nonce_udp.drain().iter().all(|datagram| is_ping(datagram)));
    assert_eq!(nonce_harness.session.stats().rtp_sent, 0);
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
