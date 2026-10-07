//! The real connection driver against an in-process Gateway that speaks real
//! WebSocket framing and a real persistent zlib stream. Time is paused, so
//! heartbeats, backoff, and timeouts run instantly and deterministically.
//!
//! Event order matters in these scripts: the client emits `AwaitHello` before
//! HELLO but `Identifying`/`Resuming` only after it, so a test must let the
//! server speak first (`handshake`/`handshake_resume`) before waiting for those
//! states.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{DuplexStream, duplex};
use tokio::sync::watch;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role};

use super::compression::{MAX_EVENT_BYTES, test_support::ServerCompressor};
use super::pacing::{JitterSource, SEND_LIMIT, backoff_delay};
use super::transport::{DiscoverError, Transport, websocket_config};
use super::url::GatewayUrl;
use super::*;
use crate::ws::ConnectError;

const READY: &str = include_str!("../../../../fixtures/gateway/ready.json");
const SUPPLEMENTAL: &str = include_str!("../../../../fixtures/gateway/ready_supplemental.json");
const MESSAGE: &str = include_str!("../../../../fixtures/model/message_create.json");
const DISCOVERED: &str = "wss://gateway.discord.gg/?v=10&encoding=json&compress=zlib-stream";
const RESUME_URL: &str =
    "wss://gateway-us-east1-b.discord.gg/?v=10&encoding=json&compress=zlib-stream";
const FETCHED_BUILD: u32 = 700_123;
const HEARTBEAT_MS: u64 = 40_000;

struct Fixed(f64);

impl JitterSource for Fixed {
    fn unit(&mut self) -> f64 {
        self.0
    }
}

type ServerSocket = WebSocketStream<DuplexStream>;

struct FakeTransport {
    sockets: Mutex<VecDeque<Option<ServerSocket>>>,
    discoveries: Mutex<VecDeque<Result<String, DiscoverError>>>,
    connected: Mutex<Vec<String>>,
    discovered: AtomicUsize,
    stopped: watch::Sender<bool>,
}

impl FakeTransport {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sockets: Mutex::default(),
            discoveries: Mutex::default(),
            connected: Mutex::default(),
            discovered: AtomicUsize::new(0),
            stopped: watch::channel(false).0,
        })
    }

    /// Queues the next connection the client will get and returns the server
    /// end of it.
    async fn accept(&self) -> Server {
        let (client_io, server_io) = duplex(16 * 1024 * 1024);
        let client =
            WebSocketStream::from_raw_socket(client_io, Role::Client, Some(websocket_config()))
                .await;
        let socket = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        self.sockets.lock().push_back(Some(client));
        Server {
            socket,
            compressor: ServerCompressor::new(),
        }
    }

    /// The next connect attempt fails.
    fn refuse(&self) {
        self.sockets.lock().push_back(None);
    }

    fn connected(&self) -> Vec<String> {
        self.connected.lock().clone()
    }

    fn token_rejected(&self) -> bool {
        *self.stopped.borrow()
    }
}

impl Transport for FakeTransport {
    type Socket = ServerSocket;

    async fn build_number(&self) -> BuildNumber {
        BuildNumber::fetched(FETCHED_BUILD)
    }

    async fn discover(&self) -> Result<String, DiscoverError> {
        self.discovered.fetch_add(1, Ordering::SeqCst);
        self.discoveries
            .lock()
            .pop_front()
            .unwrap_or_else(|| Ok("wss://gateway.discord.gg".to_owned()))
    }

    async fn connect(&self, url: &GatewayUrl) -> Result<ServerSocket, ConnectError> {
        self.connected.lock().push(url.as_str().to_owned());
        self.sockets
            .lock()
            .pop_front()
            .flatten()
            .ok_or(ConnectError)
    }

    async fn authentication_required(&self) {
        let mut stopped = self.stopped.subscribe();
        let _ = stopped.wait_for(|stopped| *stopped).await;
    }

    fn stop_authenticated_work(&self) {
        self.stopped.send_replace(true);
    }
}

struct Server {
    socket: ServerSocket,
    compressor: ServerCompressor,
}

fn envelope(name: &str, sequence: u64, data: &str) -> String {
    format!(r#"{{"t":"{name}","s":{sequence},"op":0,"d":{data}}}"#)
}

impl Server {
    async fn send_text_compressed(&mut self, text: &str) {
        let bytes = self.compressor.event(text.as_bytes());
        self.socket.send(Message::binary(bytes)).await.unwrap();
    }

    async fn send_json(&mut self, value: Value) {
        self.send_text_compressed(&value.to_string()).await;
    }

    async fn dispatch(&mut self, name: &str, sequence: u64, data: &str) {
        self.send_text_compressed(&envelope(name, sequence, data))
            .await;
    }

    /// One event delivered as many WebSocket messages of `chunk` bytes.
    async fn dispatch_fragmented(&mut self, name: &str, sequence: u64, data: &str, chunk: usize) {
        let bytes = self
            .compressor
            .event(envelope(name, sequence, data).as_bytes());
        for piece in bytes.chunks(chunk) {
            self.socket
                .send(Message::binary(piece.to_vec()))
                .await
                .unwrap();
        }
    }

    async fn hello(&mut self, interval_ms: u64) {
        self.send_json(json!({"op": 10, "d": {"heartbeat_interval": interval_ms}}))
            .await;
    }

    async fn ack(&mut self) {
        self.send_json(json!({"op": 11, "d": null})).await;
    }

    /// The next frame the client sent, as JSON.
    async fn recv(&mut self) -> Value {
        timeout(Duration::from_secs(3600), async {
            loop {
                match self.socket.next().await.expect("client hung up").unwrap() {
                    Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                    Message::Close(frame) => panic!("client closed: {frame:?}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("the client sent nothing")
    }

    /// The close code the client sent before dropping the connection.
    async fn recv_close_code(&mut self) -> Option<u16> {
        timeout(Duration::from_secs(60), async {
            loop {
                match self.socket.next().await {
                    Some(Ok(Message::Close(frame))) => return frame.map(|f| u16::from(f.code)),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return None,
                }
            }
        })
        .await
        .expect("no close from the client")
    }

    async fn close_with(&mut self, code: u16) {
        let frame = CloseFrame {
            code: code.into(),
            reason: "".into(),
        };
        self.socket.send(Message::Close(Some(frame))).await.unwrap();
    }

    /// hello, then the client's Identify.
    async fn handshake(&mut self) -> Value {
        self.hello(HEARTBEAT_MS).await;
        let identify = self.recv().await;
        assert_eq!(identify["op"], 2, "expected Identify, got {identify}");
        identify
    }

    /// hello, then the client's Resume.
    async fn handshake_resume(&mut self) -> Value {
        self.hello(HEARTBEAT_MS).await;
        let resume = self.recv().await;
        assert_eq!(resume["op"], 6, "expected Resume, got {resume}");
        resume
    }
}

fn start(transport: &Arc<FakeTransport>, jitter: f64) -> Gateway {
    start_with(
        Arc::clone(transport),
        Arc::new(UserToken::new("test-token".to_owned())),
        "en-US".to_owned(),
        Fixed(jitter),
    )
}

async fn next(gateway: &mut Gateway) -> GatewayEvent {
    timeout(Duration::from_secs(3600), gateway.next_event())
        .await
        .expect("no event arrived")
        .expect("the gateway ended")
}

/// Skips `Connecting`, `AwaitHello`, and `Identifying`/`Resuming`.
async fn skip_connection_states(gateway: &mut Gateway) {
    for _ in 0..3 {
        assert!(matches!(next(gateway).await, GatewayEvent::State(_)));
    }
}

async fn expect_state(gateway: &mut Gateway, state: ConnectionState) {
    assert_eq!(next(gateway).await, GatewayEvent::State(state));
}

async fn next_dispatch(gateway: &mut Gateway) -> (u64, Dispatch) {
    match next(gateway).await {
        GatewayEvent::Dispatch { sequence, event } => (sequence, event),
        other => panic!("expected a dispatch, got {other:?}"),
    }
}

async fn expect_reconnecting(gateway: &mut Gateway) -> (u32, Duration, ReconnectReason) {
    match next(gateway).await {
        GatewayEvent::State(ConnectionState::Reconnecting {
            attempt,
            delay,
            reason,
        }) => (attempt, delay, reason),
        other => panic!("expected Reconnecting, got {other:?}"),
    }
}

async fn expect_end(gateway: &mut Gateway) {
    let ended = timeout(Duration::from_secs(3600), gateway.next_event())
        .await
        .expect("the gateway kept running");
    assert_eq!(ended, None);
}

async fn expect_quiet(gateway: &mut Gateway) {
    assert!(
        timeout(Duration::from_secs(1), gateway.next_event())
            .await
            .is_err(),
        "no further event was expected"
    );
}

/// Everything until the gateway ends; returns the last event.
async fn drain(gateway: &mut Gateway) -> Option<GatewayEvent> {
    let mut last = None;
    while let Some(event) = timeout(Duration::from_secs(3600), gateway.next_event())
        .await
        .expect("the gateway kept running")
    {
        last = Some(event);
    }
    last
}

/// Connects, identifies, and consumes everything through `State(Ready)`.
async fn ready(jitter: f64) -> (Gateway, Arc<FakeTransport>, Server) {
    let transport = FakeTransport::new();
    let mut server = transport.accept().await;
    let mut gateway = start(&transport, jitter);
    server.handshake().await;
    server.dispatch("READY", 1, READY).await;
    expect_state(&mut gateway, ConnectionState::Connecting { attempt: 0 }).await;
    expect_state(&mut gateway, ConnectionState::AwaitHello).await;
    expect_state(&mut gateway, ConnectionState::Identifying).await;
    let (sequence, event) = next_dispatch(&mut gateway).await;
    assert_eq!(sequence, 1);
    assert!(matches!(event, Dispatch::Ready(_)));
    expect_state(&mut gateway, ConnectionState::Ready).await;
    (gateway, transport, server)
}

fn message_json(id: u64, content: &str) -> String {
    let mut message: Value = serde_json::from_str(MESSAGE).unwrap();
    message["id"] = json!(id.to_string());
    message["content"] = json!(content);
    message.to_string()
}

#[tokio::test(start_paused = true)]
async fn identify_flow_sends_the_web_profile_and_delivers_normalized_ready() {
    let transport = FakeTransport::new();
    let mut server = transport.accept().await;
    let mut gateway = start(&transport, 0.5);
    let identify = server.handshake().await;

    assert_eq!(transport.connected(), [DISCOVERED]);
    let data = &identify["d"];
    assert_eq!(data["token"], "test-token");
    assert_eq!(data["compress"], false);
    assert_eq!(data["capabilities"], selected_value());
    assert!(data.get("intents").is_none());
    assert_eq!(data["client_state"]["guild_versions"], json!({}));
    let properties = &data["properties"];
    assert_eq!(properties["browser"], "Chrome");
    assert_eq!(properties["release_channel"], "stable");
    assert_eq!(properties["client_build_number"], FETCHED_BUILD);
    assert_eq!(properties["system_locale"], "en-US");

    server.dispatch("READY", 1, READY).await;
    server.dispatch("READY_SUPPLEMENTAL", 2, SUPPLEMENTAL).await;
    expect_state(&mut gateway, ConnectionState::Connecting { attempt: 0 }).await;
    expect_state(&mut gateway, ConnectionState::AwaitHello).await;
    expect_state(&mut gateway, ConnectionState::Identifying).await;
    let (sequence, event) = next_dispatch(&mut gateway).await;
    assert_eq!(sequence, 1);
    let Dispatch::Ready(ready) = event else {
        panic!("expected READY");
    };
    assert_eq!(ready.guilds.len(), 2);
    assert_eq!(ready.unavailable_guilds.len(), 1);
    assert_eq!(ready.users.len(), 4);
    expect_state(&mut gateway, ConnectionState::Ready).await;
    let (sequence, event) = next_dispatch(&mut gateway).await;
    assert_eq!(sequence, 2);
    let Dispatch::ReadySupplemental(supplemental) = event else {
        panic!("expected READY_SUPPLEMENTAL");
    };
    assert_eq!(supplemental.guilds[0].voice_states.len(), 1);
    assert_eq!(supplemental.guilds[0].members.len(), 3);
}

#[tokio::test(start_paused = true)]
async fn fragmented_compressed_events_reassemble_and_share_one_dictionary() {
    let transport = FakeTransport::new();
    let mut server = transport.accept().await;
    let mut gateway = start(&transport, 0.5);
    server.handshake().await;
    // READY split into 7-byte WebSocket messages; the next event into 1-byte
    // ones, so even the flush suffix straddles messages.
    server.dispatch_fragmented("READY", 1, READY, 7).await;
    server
        .dispatch_fragmented("MESSAGE_CREATE", 2, &message_json(10, "fragmented"), 1)
        .await;
    server
        .dispatch("MESSAGE_CREATE", 3, &message_json(11, "whole"))
        .await;
    skip_connection_states(&mut gateway).await;
    assert!(matches!(
        next_dispatch(&mut gateway).await.1,
        Dispatch::Ready(_)
    ));
    expect_state(&mut gateway, ConnectionState::Ready).await;
    for (expected_sequence, content) in [(2, "fragmented"), (3, "whole")] {
        let (sequence, event) = next_dispatch(&mut gateway).await;
        assert_eq!(sequence, expected_sequence);
        let Dispatch::MessageCreate(message) = event else {
            panic!("expected MESSAGE_CREATE");
        };
        assert_eq!(message.content, content);
    }
}

#[tokio::test(start_paused = true)]
async fn heartbeats_are_jittered_acknowledged_and_carry_the_latest_sequence() {
    let (mut gateway, _transport, mut server) = ready(0.5).await;
    let started = Instant::now();
    // The first heartbeat is offset by jitter * interval.
    let first = server.recv().await;
    let first_at = Instant::now();
    assert_eq!(first, json!({"op": 1, "d": 1}));
    assert_eq!(first_at - started, Duration::from_millis(HEARTBEAT_MS / 2));
    server.ack().await;
    // Unknown events are consumed but their sequence still counts.
    server.dispatch("TYPING_START", 7, "{}").await;
    server.dispatch("SESSIONS_REPLACE", 8, "[]").await;
    let second = server.recv().await;
    assert_eq!(second, json!({"op": 1, "d": 8}));
    assert_eq!(
        Instant::now() - first_at,
        Duration::from_millis(HEARTBEAT_MS)
    );
    server.ack().await;
    // No event was surfaced for the unknown dispatches, and nothing reconnected.
    expect_quiet(&mut gateway).await;
}

#[tokio::test(start_paused = true)]
async fn server_heartbeat_requests_are_answered_immediately_but_bounded() {
    let (_gateway, _transport, mut server) = ready(0.5).await;
    let started = Instant::now();
    server.send_json(json!({"op": 1, "d": null})).await;
    assert_eq!(server.recv().await, json!({"op": 1, "d": 1}));
    assert!(Instant::now() - started < Duration::from_secs(1));
    // A flood of requests cannot push the connection over 120 sends per 60 s:
    // Identify and the answer above already used two slots.
    for _ in 0..300 {
        server.send_json(json!({"op": 1, "d": null})).await;
    }
    let mut answers = 0;
    while let Ok(frame) = timeout(Duration::from_secs(2), server.recv()).await {
        assert_eq!(frame["op"], 1);
        answers += 1;
    }
    assert_eq!(answers, SEND_LIMIT - 2);
}

#[tokio::test(start_paused = true)]
async fn missing_ack_closes_and_resumes_the_session_without_a_new_identify() {
    let transport = FakeTransport::new();
    let mut first = transport.accept().await;
    let mut second = transport.accept().await;
    let mut gateway = start(&transport, 0.5);
    first.hello(2_000).await;
    assert_eq!(first.recv().await["op"], 2);
    first.dispatch("READY", 1, READY).await;
    first
        .dispatch("MESSAGE_CREATE", 2, &message_json(10, "before"))
        .await;
    // The first heartbeat (1 s) is acknowledged; the one at 3 s is not.
    assert_eq!(first.recv().await, json!({"op": 1, "d": 2}));
    first.ack().await;
    assert_eq!(first.recv().await, json!({"op": 1, "d": 2}));
    skip_connection_states(&mut gateway).await;
    assert!(matches!(
        next_dispatch(&mut gateway).await.1,
        Dispatch::Ready(_)
    ));
    expect_state(&mut gateway, ConnectionState::Ready).await;
    assert!(matches!(
        next_dispatch(&mut gateway).await.1,
        Dispatch::MessageCreate(_)
    ));
    // The connection died young, so the backoff starts (half to all of 1 s;
    // 750 ms at 0.5 jitter). It is never left displaying "connected".
    let (attempt, delay, reason) = expect_reconnecting(&mut gateway).await;
    assert_eq!(reason, ReconnectReason::HeartbeatTimeout);
    assert_eq!(attempt, 1);
    assert_eq!(delay, backoff_delay(1, 0.5));

    let resume = second.handshake_resume().await;
    assert_eq!(
        resume,
        json!({"op": 6, "d": {"token": "test-token", "session_id": "fixture-session-id-0001", "seq": 2}})
    );
    expect_state(&mut gateway, ConnectionState::Connecting { attempt: 1 }).await;
    expect_state(&mut gateway, ConnectionState::AwaitHello).await;
    expect_state(&mut gateway, ConnectionState::Resuming).await;
    // Resume goes to the READY-provided address, not the discovered one, and
    // no new discovery or Identify happened.
    assert_eq!(transport.connected(), [DISCOVERED, RESUME_URL]);
    assert_eq!(transport.discovered.load(Ordering::SeqCst), 1);
    second.dispatch("RESUMED", 3, r#"{"_trace":[]}"#).await;
    let (sequence, event) = next_dispatch(&mut gateway).await;
    assert_eq!((sequence, event), (3, Dispatch::Resumed));
    expect_state(&mut gateway, ConnectionState::Ready).await;
}

#[tokio::test(start_paused = true)]
async fn resume_replay_is_deduplicated_and_applied_in_order() {
    let (mut gateway, transport, mut first) = ready(0.5).await;
    let mut second = transport.accept().await;
    first
        .dispatch("MESSAGE_CREATE", 2, &message_json(20, "one"))
        .await;
    first
        .dispatch("MESSAGE_CREATE", 3, &message_json(21, "two"))
        .await;
    // A duplicate on the same connection is dropped too.
    first
        .dispatch("MESSAGE_CREATE", 3, &message_json(21, "two"))
        .await;
    for expected in [2, 3] {
        assert_eq!(next_dispatch(&mut gateway).await.0, expected);
    }
    // The connection drops without a close frame.
    drop(first);
    let (_, _, reason) = expect_reconnecting(&mut gateway).await;
    assert!(matches!(
        reason,
        ReconnectReason::Closed(None) | ReconnectReason::Network
    ));
    let resume = second.handshake_resume().await;
    assert_eq!(resume["d"]["seq"], 3);
    skip_connection_states(&mut gateway).await;
    // The server replays from the sequence it has: 3 again, then new events.
    second
        .dispatch("MESSAGE_CREATE", 3, &message_json(21, "two"))
        .await;
    second
        .dispatch("MESSAGE_CREATE", 4, &message_json(22, "three"))
        .await;
    second
        .dispatch("MESSAGE_CREATE", 4, &message_json(22, "three"))
        .await;
    second
        .dispatch("MESSAGE_DELETE", 5, r#"{"id":"20","channel_id":"500"}"#)
        .await;
    second.dispatch("RESUMED", 6, "{}").await;
    let mut delivered = Vec::new();
    loop {
        let (sequence, event) = next_dispatch(&mut gateway).await;
        delivered.push((sequence, event.name()));
        if matches!(event, Dispatch::Resumed) {
            break;
        }
    }
    assert_eq!(
        delivered,
        [(4, "MESSAGE_CREATE"), (5, "MESSAGE_DELETE"), (6, "RESUMED")]
    );
    expect_state(&mut gateway, ConnectionState::Ready).await;
    // Replay did not produce a second READY, and nothing else is pending.
    expect_quiet(&mut gateway).await;
}

#[tokio::test(start_paused = true)]
async fn a_healthy_connection_resumes_immediately_on_server_reconnect() {
    let (mut gateway, transport, mut first) = ready(0.5).await;
    let mut second = transport.accept().await;
    // Ready for longer than the stability window.
    tokio::time::sleep(Duration::from_secs(31)).await;
    first.send_json(json!({"op": 7, "d": null})).await;
    let (attempt, delay, reason) = expect_reconnecting(&mut gateway).await;
    assert_eq!(reason, ReconnectReason::ServerRequested);
    assert_eq!((attempt, delay), (0, Duration::ZERO));
    assert_eq!(second.handshake_resume().await["d"]["seq"], 1);
}

#[tokio::test(start_paused = true)]
async fn non_resumable_invalid_session_reidentifies_with_the_same_profile() {
    let (mut gateway, transport, mut first) = ready(0.5).await;
    let mut second = transport.accept().await;
    first.send_json(json!({"op": 9, "d": false})).await;
    let (_, delay, reason) = expect_reconnecting(&mut gateway).await;
    assert_eq!(reason, ReconnectReason::InvalidSession { resumable: false });
    assert_eq!(delay, Duration::from_millis(3_000), "1-5 s, at 0.5 jitter");
    let identify = second.handshake().await;
    // New session: the discovered address, not the resume address.
    assert_eq!(transport.connected(), [DISCOVERED, DISCOVERED]);
    assert_eq!(
        identify["d"]["properties"]["client_build_number"],
        FETCHED_BUILD
    );
    second.dispatch("READY", 1, READY).await;
    skip_connection_states(&mut gateway).await;
    // A fresh READY replaces session-owned state.
    assert!(matches!(
        next_dispatch(&mut gateway).await.1,
        Dispatch::Ready(_)
    ));
    expect_state(&mut gateway, ConnectionState::Ready).await;
}

#[tokio::test(start_paused = true)]
async fn identify_payload_is_identical_across_reconnects() {
    let transport = FakeTransport::new();
    let mut first = transport.accept().await;
    let mut second = transport.accept().await;
    let mut third = transport.accept().await;
    let _gateway = start(&transport, 0.5);
    let one = first.handshake().await;
    first.send_json(json!({"op": 9, "d": false})).await;
    let two = second.handshake().await;
    second.send_json(json!({"op": 9, "d": false})).await;
    let three = third.handshake().await;
    assert_eq!(one, two);
    assert_eq!(two, three);
}

#[tokio::test(start_paused = true)]
async fn resumable_invalid_session_resumes() {
    let (mut gateway, transport, mut first) = ready(0.5).await;
    let mut second = transport.accept().await;
    first.send_json(json!({"op": 9, "d": true})).await;
    let (_, _, reason) = expect_reconnecting(&mut gateway).await;
    assert_eq!(reason, ReconnectReason::InvalidSession { resumable: true });
    assert_eq!(second.handshake_resume().await["d"]["seq"], 1);
}

#[tokio::test(start_paused = true)]
async fn repeated_identify_rejections_stop_instead_of_looping() {
    let transport = FakeTransport::new();
    let mut servers = Vec::new();
    for _ in 0..5 {
        servers.push(transport.accept().await);
    }
    let mut gateway = start(&transport, 0.5);
    for server in &mut servers {
        server.handshake().await;
        server.send_json(json!({"op": 9, "d": false})).await;
    }
    assert_eq!(
        drain(&mut gateway).await,
        Some(GatewayEvent::State(ConnectionState::Stopped(
            StopReason::IdentifyRejected
        )))
    );
    assert_eq!(transport.connected().len(), 5);
}

#[tokio::test(start_paused = true)]
async fn authentication_failure_close_stops_all_attempts_and_the_account() {
    let (mut gateway, transport, mut server) = ready(0.5).await;
    // A second connection is available, but must never be used.
    let _unused = transport.accept().await;
    server.close_with(4004).await;
    expect_state(&mut gateway, ConnectionState::AuthenticationRequired).await;
    expect_end(&mut gateway).await;
    assert!(transport.token_rejected(), "REST work must stop too");
    assert_eq!(transport.connected().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn rest_observing_a_401_stops_the_gateway() {
    let (mut gateway, transport, mut server) = ready(0.5).await;
    transport.stop_authenticated_work();
    expect_state(&mut gateway, ConnectionState::AuthenticationRequired).await;
    // Logout stops the account's REST work just before dropping the handle;
    // the session must end either way.
    assert_eq!(server.recv_close_code().await, Some(1000));
    expect_end(&mut gateway).await;
    assert_eq!(transport.connected().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn discovery_401_returns_to_login_without_connecting() {
    let transport = FakeTransport::new();
    transport
        .discoveries
        .lock()
        .push_back(Err(DiscoverError::AuthenticationRequired));
    let mut gateway = start(&transport, 0.5);
    expect_state(&mut gateway, ConnectionState::Connecting { attempt: 0 }).await;
    expect_state(&mut gateway, ConnectionState::AuthenticationRequired).await;
    expect_end(&mut gateway).await;
    assert!(transport.connected().is_empty());
}

#[tokio::test(start_paused = true)]
async fn failed_discovery_and_connects_back_off_exponentially_then_recover() {
    let transport = FakeTransport::new();
    transport
        .discoveries
        .lock()
        .push_back(Err(DiscoverError::Failed));
    for _ in 0..3 {
        transport.refuse();
    }
    let mut server = transport.accept().await;
    let mut gateway = start(&transport, 0.5);
    expect_state(&mut gateway, ConnectionState::Connecting { attempt: 0 }).await;
    let (attempt, delay, reason) = expect_reconnecting(&mut gateway).await;
    assert_eq!((attempt, reason), (1, ReconnectReason::DiscoveryFailed));
    assert_eq!(delay, backoff_delay(1, 0.5));
    for tried in 1..=3u32 {
        expect_state(&mut gateway, ConnectionState::Connecting { attempt: tried }).await;
        let (attempt, delay, reason) = expect_reconnecting(&mut gateway).await;
        assert_eq!(
            (attempt, reason),
            (tried + 1, ReconnectReason::ConnectFailed)
        );
        assert_eq!(delay, backoff_delay(tried + 1, 0.5));
    }
    // 750 ms, 1.5 s, 3 s, 6 s at 0.5 jitter.
    assert_eq!(backoff_delay(4, 0.5), Duration::from_secs(6));
    expect_state(&mut gateway, ConnectionState::Connecting { attempt: 4 }).await;
    server.handshake().await;
    // A failed connect asks Discord for a fresh address each time.
    assert_eq!(transport.discovered.load(Ordering::SeqCst), 5);
}

#[tokio::test(start_paused = true)]
async fn missing_hello_replaces_the_connection() {
    let transport = FakeTransport::new();
    let _silent = transport.accept().await;
    let mut gateway = start(&transport, 0.5);
    expect_state(&mut gateway, ConnectionState::Connecting { attempt: 0 }).await;
    expect_state(&mut gateway, ConnectionState::AwaitHello).await;
    let (_, _, reason) = expect_reconnecting(&mut gateway).await;
    assert_eq!(reason, ReconnectReason::HelloTimeout);
}

#[tokio::test(start_paused = true)]
async fn hostile_heartbeat_interval_is_clamped() {
    let transport = FakeTransport::new();
    let mut server = transport.accept().await;
    let _gateway = start(&transport, 0.0);
    server.hello(0).await;
    assert_eq!(server.recv().await["op"], 2);
    // Jitter 0: the first beat is immediate; the next must be a second later,
    // not a busy loop.
    assert_eq!(server.recv().await["op"], 1);
    let first = Instant::now();
    server.ack().await;
    assert_eq!(server.recv().await["op"], 1);
    assert_eq!(Instant::now() - first, Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn rejected_closes_and_other_terminal_conditions_stop_with_a_reason() {
    for (code, stop) in [
        (4014, StopReason::Rejected(4014)),
        (4002, StopReason::Rejected(4002)),
        (4015, StopReason::TooManySessions),
    ] {
        let (mut gateway, transport, mut server) = ready(0.5).await;
        server.close_with(code).await;
        expect_state(&mut gateway, ConnectionState::Stopped(stop)).await;
        expect_end(&mut gateway).await;
        assert_eq!(transport.connected().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn session_ending_closes_reidentify_and_other_closes_resume() {
    for (code, expect_resume) in [(4000u16, true), (4008, true), (4007, false), (4009, false)] {
        let (mut gateway, transport, mut first) = ready(0.5).await;
        let mut second = transport.accept().await;
        first.close_with(code).await;
        let (_, _, reason) = expect_reconnecting(&mut gateway).await;
        assert_eq!(reason, ReconnectReason::Closed(Some(code)));
        second.hello(HEARTBEAT_MS).await;
        let frame = second.recv().await;
        assert_eq!(frame["op"], if expect_resume { 6 } else { 2 }, "{code}");
    }
}

#[tokio::test(start_paused = true)]
async fn an_event_over_the_ceiling_stops_with_a_startup_error() {
    let transport = FakeTransport::new();
    let mut server = transport.accept().await;
    let mut gateway = start(&transport, 0.5);
    server.handshake().await;
    // 20 MiB decompressed, a few tens of KiB on the wire.
    let filler = "a".repeat(MAX_EVENT_BYTES + 4 * 1024 * 1024);
    let data = format!(r#"{{"session_id":"s","padding":"{filler}"}}"#);
    server.dispatch("READY", 1, &data).await;
    assert_eq!(
        drain(&mut gateway).await,
        Some(GatewayEvent::State(ConnectionState::Stopped(
            StopReason::EventTooLarge
        )))
    );
    assert_eq!(transport.connected().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn malformed_ready_stops_instead_of_continuing_with_partial_state() {
    let transport = FakeTransport::new();
    let mut server = transport.accept().await;
    let mut gateway = start(&transport, 0.5);
    server.handshake().await;
    server.dispatch("READY", 1, r#"{"session_id":"s"}"#).await;
    skip_connection_states(&mut gateway).await;
    expect_state(
        &mut gateway,
        ConnectionState::Stopped(StopReason::MalformedReady),
    )
    .await;
    expect_end(&mut gateway).await;
}

#[tokio::test(start_paused = true)]
async fn a_required_account_action_is_surfaced_and_stops() {
    let transport = FakeTransport::new();
    let mut server = transport.accept().await;
    let mut gateway = start(&transport, 0.5);
    server.handshake().await;
    let mut ready: Value = serde_json::from_str(READY).unwrap();
    ready["required_action"] = json!("REQUIRE_VERIFIED_EMAIL");
    server.dispatch("READY", 1, &ready.to_string()).await;
    skip_connection_states(&mut gateway).await;
    assert!(matches!(
        next_dispatch(&mut gateway).await.1,
        Dispatch::Ready(_)
    ));
    expect_state(&mut gateway, ConnectionState::Ready).await;
    expect_state(
        &mut gateway,
        ConnectionState::Stopped(StopReason::ActionRequired(
            "REQUIRE_VERIFIED_EMAIL".to_owned(),
        )),
    )
    .await;
    expect_end(&mut gateway).await;
    assert_eq!(transport.connected().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn discovery_that_points_off_discord_is_never_sent_the_token() {
    let transport = FakeTransport::new();
    transport
        .discoveries
        .lock()
        .push_back(Ok("wss://gateway.discord.gg.evil.example".to_owned()));
    let mut gateway = start(&transport, 0.5);
    expect_state(&mut gateway, ConnectionState::Connecting { attempt: 0 }).await;
    expect_state(
        &mut gateway,
        ConnectionState::Stopped(StopReason::InvalidGatewayUrl),
    )
    .await;
    assert!(transport.connected().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_unusable_resume_address_falls_back_to_discovery() {
    let transport = FakeTransport::new();
    let mut first = transport.accept().await;
    let mut second = transport.accept().await;
    let _gateway = start(&transport, 0.5);
    first.handshake().await;
    let mut ready: Value = serde_json::from_str(READY).unwrap();
    ready["resume_gateway_url"] = json!("wss://evil.example");
    first.dispatch("READY", 1, &ready.to_string()).await;
    drop(first);
    second.handshake_resume().await;
    assert_eq!(transport.connected(), [DISCOVERED, DISCOVERED]);
}

#[tokio::test(start_paused = true)]
async fn a_stalled_consumer_gets_backpressure_but_heartbeats_continue() {
    let transport = FakeTransport::new();
    let mut server = transport.accept().await;
    let mut gateway = start(&transport, 0.5);
    server.hello(10_000).await;
    assert_eq!(server.recv().await["op"], 2);
    server.dispatch("READY", 1, READY).await;
    // 30 events of ~150 KB: far more than the 2 MiB queue holds.
    let big = "x".repeat(150_000);
    for n in 0..30u64 {
        server
            .dispatch("MESSAGE_CREATE", 2 + n, &message_json(100 + n, &big))
            .await;
    }
    // The consumer reads nothing for several heartbeat intervals. Heartbeats
    // must keep flowing, and nothing may be dropped or reconnected.
    for _ in 0..5 {
        assert_eq!(server.recv().await["op"], 1);
        server.ack().await;
    }
    skip_connection_states(&mut gateway).await;
    assert!(matches!(
        next_dispatch(&mut gateway).await.1,
        Dispatch::Ready(_)
    ));
    expect_state(&mut gateway, ConnectionState::Ready).await;
    for n in 0..30u64 {
        let (sequence, event) = next_dispatch(&mut gateway).await;
        assert_eq!(sequence, 2 + n, "events stay ordered and complete");
        let Dispatch::MessageCreate(message) = event else {
            panic!("expected MESSAGE_CREATE");
        };
        assert_eq!(message.content.len(), 150_000);
    }
    expect_quiet(&mut gateway).await;
}

#[tokio::test(start_paused = true)]
async fn dropping_the_handle_closes_with_1000_so_the_session_ends() {
    let (gateway, _transport, mut server) = ready(0.5).await;
    drop(gateway);
    assert_eq!(server.recv_close_code().await, Some(1000));
}

#[tokio::test(start_paused = true)]
async fn corrupt_compression_replaces_the_connection_with_a_new_dictionary() {
    let (mut gateway, transport, mut first) = ready(0.5).await;
    let mut second = transport.accept().await;
    let mut garbage = vec![0x12u8; 32];
    garbage.extend_from_slice(&[0x00, 0x00, 0xff, 0xff]);
    first.socket.send(Message::binary(garbage)).await.unwrap();
    let (_, _, reason) = expect_reconnecting(&mut gateway).await;
    assert_eq!(reason, ReconnectReason::Protocol);
    second.handshake_resume().await;
}

#[test]
fn gateway_debug_names_no_account_data() {
    let event = GatewayEvent::State(ConnectionState::Stopped(StopReason::ActionRequired(
        "X".to_owned(),
    )));
    assert!(format!("{event:?}").contains("ActionRequired"));
    assert_eq!(
        format!("{:?}", SessionId("secret-session".to_owned())),
        "SessionId([REDACTED])"
    );
}
