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

use futures_util::{FutureExt, SinkExt, StreamExt};
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
use super::subscription::{COALESCE, CONTROL_RESERVE};
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
    ready_with_heartbeat(jitter, HEARTBEAT_MS).await
}

async fn ready_with_heartbeat(
    jitter: f64,
    interval_ms: u64,
) -> (Gateway, Arc<FakeTransport>, Server) {
    let transport = FakeTransport::new();
    let mut server = transport.accept().await;
    let mut gateway = start(&transport, jitter);
    server.hello(interval_ms).await;
    assert_eq!(server.recv().await["op"], 2);
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

// ----- lazy subscriptions (opcode 37) -----

const NAVIGATION: &str = include_str!("../../../../fixtures/gateway/subscriptions_navigation.json");
const FIRST_GUILD: u64 = 41_771_983_423_143_937;
const FIRST_CHANNEL: u64 = 41_771_983_423_143_938;
const SECOND_GUILD: u64 = 41_771_983_423_143_941;
const SECOND_CHANNEL: u64 = 41_771_983_423_143_942;

fn snowflake(value: u64) -> fastcord_model::Snowflake {
    fastcord_model::Snowflake(value)
}

impl Server {
    /// Everything the client has sent that is already waiting, in order.
    /// Heartbeats are acknowledged. Only opcodes this client is meant to use
    /// are accepted: in particular opcode 14 must never appear.
    async fn service(&mut self) -> Vec<Value> {
        let mut frames = Vec::new();
        while let Some(Some(Ok(message))) = self.socket.next().now_or_never() {
            let Message::Text(text) = message else {
                continue;
            };
            let frame: Value = serde_json::from_str(text.as_str()).unwrap();
            let op = frame["op"].as_u64().unwrap();
            assert!(matches!(op, 1 | 2 | 6 | 37), "unexpected opcode {op}");
            if op == 1 {
                self.ack().await;
            }
            frames.push(frame);
        }
        frames
    }

    /// The subscription frames sent so far.
    async fn subscription_frames(&mut self) -> Vec<Value> {
        self.service()
            .await
            .into_iter()
            .filter(|frame| frame["op"] == 37)
            .collect()
    }
}

/// Lets the client task run for `span` of virtual time.
async fn settle(span: Duration) {
    // Process notifications before advancing, then run newly due timers
    // before observing the socket. Sleep completion alone need not poll peers.
    tokio::task::yield_now().await;
    tokio::time::sleep(span).await;
    tokio::task::yield_now().await;
}

#[tokio::test(start_paused = true)]
async fn two_guild_navigation_emits_only_intended_subscriptions() {
    let (gateway, _transport, mut server) = ready(0.5).await;
    let subscriptions = gateway.subscriptions();
    let mut frames = Vec::new();

    // Nothing is sent for a session nobody has asked anything of.
    settle(Duration::from_secs(2)).await;
    assert!(server.subscription_frames().await.is_empty());

    // Select a channel of the first guild.
    subscriptions.update(|target| {
        target.select_channel(snowflake(FIRST_GUILD), snowflake(FIRST_CHANNEL));
    });
    settle(Duration::from_secs(1)).await;
    frames.extend(server.subscription_frames().await);

    // Navigate to the second guild: one frame releases the first and
    // subscribes the second; nothing else.
    subscriptions.update(|target| {
        target.select_channel(snowflake(SECOND_GUILD), snowflake(SECOND_CHANNEL));
    });
    settle(Duration::from_secs(1)).await;
    frames.extend(server.subscription_frames().await);

    // Joining a call in the first guild keeps it subscribed (and only it).
    subscriptions.update(|target| target.set_voice_guild(Some(snowflake(FIRST_GUILD))));
    settle(Duration::from_secs(1)).await;
    frames.extend(server.subscription_frames().await);

    let expected: Vec<Value> = serde_json::from_str(NAVIGATION).unwrap();
    assert_eq!(frames, expected);

    // Setting what is already set, in any order, sends nothing.
    subscriptions.update(|target| {
        target.set_voice_guild(Some(snowflake(FIRST_GUILD)));
        target.select_channel(snowflake(SECOND_GUILD), snowflake(SECOND_CHANNEL));
    });
    settle(Duration::from_secs(5)).await;
    assert!(server.subscription_frames().await.is_empty());
    assert_eq!(
        subscriptions.snapshot().voice_guild(),
        Some(snowflake(FIRST_GUILD))
    );
}

#[tokio::test(start_paused = true)]
async fn rapid_selection_changes_are_coalesced_and_only_the_last_state_is_sent() {
    let (gateway, _transport, mut server) = ready(0.5).await;
    let subscriptions = gateway.subscriptions();
    subscriptions.update(|t| t.select_channel(snowflake(1), snowflake(11)));
    settle(Duration::from_millis(80)).await;
    subscriptions.update(|t| t.select_channel(snowflake(2), snowflake(22)));
    settle(Duration::from_millis(80)).await;
    subscriptions.update(|t| t.select_channel(snowflake(3), snowflake(33)));
    settle(Duration::from_millis(80)).await;
    assert!(
        server.subscription_frames().await.is_empty(),
        "still inside the coalescing window"
    );
    settle(Duration::from_secs(1)).await;
    let frames = server.subscription_frames().await;
    assert_eq!(frames.len(), 1);
    let sent = frames[0]["d"]["subscriptions"].as_object().unwrap();
    assert_eq!(
        sent.keys().collect::<Vec<_>>(),
        ["3"],
        "1 and 2 were never sent"
    );
    assert_eq!(sent["3"]["channels"], json!({"33": [[0, 99]]}));
}

#[tokio::test(start_paused = true)]
async fn the_viewport_and_member_interest_reach_the_wire_without_resubscribing() {
    let (gateway, _transport, mut server) = ready(0.5).await;
    let subscriptions = gateway.subscriptions();
    subscriptions.update(|t| t.select_channel(snowflake(1), snowflake(11)));
    settle(Duration::from_secs(1)).await;
    assert_eq!(server.subscription_frames().await.len(), 1);

    subscriptions.update(|t| {
        t.set_viewport(120, 180);
        t.set_member_interest(snowflake(1), [snowflake(7), snowflake(9)]);
    });
    settle(Duration::from_secs(1)).await;
    let frames = server.subscription_frames().await;
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0]["d"]["subscriptions"],
        json!({"1": {
            "typing": true, "threads": false, "activities": false, "member_updates": false,
            "members": ["7", "9"], "channels": {"11": [[100, 199]]},
            "thread_member_lists": []
        }})
    );

    // Scrolling back replaces the old viewport block explicitly, and clearing
    // the selection releases the guild with every field spelled out.
    subscriptions.update(|t| t.set_viewport(0, 40));
    settle(Duration::from_secs(1)).await;
    let frames = server.subscription_frames().await;
    assert_eq!(
        frames[0]["d"]["subscriptions"]["1"]["channels"],
        json!({"11": [[0, 99]]})
    );
    subscriptions.update(|t| t.clear_selection());
    settle(Duration::from_secs(1)).await;
    let frames = server.subscription_frames().await;
    assert_eq!(
        frames[0]["d"]["subscriptions"],
        json!({"1": {
            "typing": false, "threads": false, "activities": false, "member_updates": false,
            "members": [], "channels": {}, "thread_member_lists": []
        }})
    );
}

#[tokio::test(start_paused = true)]
async fn a_subscription_chosen_before_ready_is_sent_when_the_session_is_ready() {
    let transport = FakeTransport::new();
    let mut server = transport.accept().await;
    let mut gateway = start(&transport, 0.5);
    let subscriptions = gateway.subscriptions();
    subscriptions.update(|t| t.select_channel(snowflake(1), snowflake(11)));
    server.handshake().await;
    settle(Duration::from_secs(2)).await;
    assert!(
        server.service().await.iter().all(|frame| frame["op"] != 37),
        "a session that is not ready accepts no subscriptions"
    );
    server.dispatch("READY", 1, READY).await;
    skip_connection_states(&mut gateway).await;
    next_dispatch(&mut gateway).await;
    expect_state(&mut gateway, ConnectionState::Ready).await;
    settle(Duration::from_secs(1)).await;
    let frames = server.subscription_frames().await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["d"]["subscriptions"]["1"]["typing"], true);
}

#[tokio::test(start_paused = true)]
async fn every_new_connection_sends_the_complete_wanted_state_again() {
    let (mut gateway, transport, mut first) = ready(0.5).await;
    let subscriptions = gateway.subscriptions();
    subscriptions.update(|t| {
        t.select_channel(snowflake(FIRST_GUILD), snowflake(FIRST_CHANNEL));
        t.set_voice_guild(Some(snowflake(SECOND_GUILD)));
    });
    settle(Duration::from_secs(1)).await;
    let original = first.subscription_frames().await;
    assert_eq!(original.len(), 1);
    assert_eq!(
        original[0]["d"]["subscriptions"].as_object().unwrap().len(),
        2
    );

    // The session ends (4009): a new Identify, a new READY, everything again.
    let mut second = transport.accept().await;
    first.close_with(4009).await;
    expect_reconnecting(&mut gateway).await;
    second.handshake().await;
    second.dispatch("READY", 1, READY).await;
    skip_connection_states(&mut gateway).await;
    next_dispatch(&mut gateway).await;
    expect_state(&mut gateway, ConnectionState::Ready).await;
    settle(Duration::from_secs(1)).await;
    assert_eq!(second.subscription_frames().await, original);

    // A resumed session repeats it as well; the server never has to guess
    // what survived. Nothing else is added.
    let mut third = transport.accept().await;
    second.close_with(4000).await;
    expect_reconnecting(&mut gateway).await;
    third.handshake_resume().await;
    third.dispatch("RESUMED", 2, r#"{"_trace":[]}"#).await;
    skip_connection_states(&mut gateway).await;
    next_dispatch(&mut gateway).await;
    expect_state(&mut gateway, ConnectionState::Ready).await;
    settle(Duration::from_secs(1)).await;
    assert_eq!(third.subscription_frames().await, original);
    settle(Duration::from_secs(5)).await;
    assert!(third.subscription_frames().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn resumed_subscriptions_release_disconnected_navigation_even_after_a_failed_connect() {
    let (mut gateway, transport, mut first) = ready(0.5).await;
    let subscriptions = gateway.subscriptions();
    subscriptions.update(|target| {
        target.select_channel(snowflake(FIRST_GUILD), snowflake(FIRST_CHANNEL));
        target.set_voice_guild(Some(snowflake(SECOND_GUILD)));
        target.set_member_interest(snowflake(SECOND_GUILD), [snowflake(7), snowflake(9)]);
    });
    settle(Duration::from_secs(1)).await;
    let original = first.subscription_frames().await;
    assert_eq!(original.len(), 1);

    transport.refuse();
    let mut second = transport.accept().await;
    first.close_with(4000).await;
    let (attempt, _, reason) = expect_reconnecting(&mut gateway).await;
    assert_eq!((attempt, reason), (1, ReconnectReason::Closed(Some(4000))));
    subscriptions.update(|target| target.select_channel(snowflake(3), snowflake(33)));
    expect_state(&mut gateway, ConnectionState::Connecting { attempt: 1 }).await;
    let (attempt, _, reason) = expect_reconnecting(&mut gateway).await;
    assert_eq!((attempt, reason), (2, ReconnectReason::ConnectFailed));
    let resume = second.handshake_resume().await;
    assert_eq!(resume["d"]["seq"], 1);
    skip_connection_states(&mut gateway).await;
    settle(Duration::from_secs(1)).await;
    assert!(
        second.subscription_frames().await.is_empty(),
        "the old ledger must remain paused until RESUMED"
    );

    second.dispatch("RESUMED", 2, r#"{"_trace":[]}"#).await;
    assert_eq!(next_dispatch(&mut gateway).await, (2, Dispatch::Resumed));
    expect_state(&mut gateway, ConnectionState::Ready).await;
    settle(Duration::from_secs(1)).await;
    let frames = second.subscription_frames().await;
    assert_eq!(frames.len(), 1);
    let sent = frames[0]["d"]["subscriptions"].as_object().unwrap();
    assert_eq!(sent.len(), 3);
    let released = json!({
        "typing": false, "threads": false, "activities": false,
        "member_updates": false, "members": [],
        "channels": {}, "thread_member_lists": []
    });
    let first_key = FIRST_GUILD.to_string();
    let second_key = SECOND_GUILD.to_string();
    assert_eq!(sent[&first_key], released);
    assert_eq!(
        sent[&second_key], original[0]["d"]["subscriptions"][&second_key],
        "the unchanged voice guild is replayed as well"
    );
    assert_eq!(sent["3"]["channels"], json!({"33": [[0, 99]]}));
    assert_eq!(sent["3"]["typing"], true);
    settle(Duration::from_secs(1)).await;
    assert!(second.subscription_frames().await.is_empty());

    // Clearing every interest while disconnected still needs releases, not an
    // empty plan. Already-released guilds must not reappear in the ledger.
    let mut third = transport.accept().await;
    second.close_with(4000).await;
    expect_reconnecting(&mut gateway).await;
    subscriptions.update(|target| {
        target.clear_selection();
        target.set_voice_guild(None);
    });
    third.handshake_resume().await;
    third.dispatch("RESUMED", 3, r#"{"_trace":[]}"#).await;
    skip_connection_states(&mut gateway).await;
    assert_eq!(next_dispatch(&mut gateway).await, (3, Dispatch::Resumed));
    expect_state(&mut gateway, ConnectionState::Ready).await;
    settle(Duration::from_secs(1)).await;
    let frames = third.subscription_frames().await;
    assert_eq!(frames.len(), 1);
    let sent = frames[0]["d"]["subscriptions"].as_object().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent["3"], released);
    assert_eq!(sent[&second_key], released);
    assert!(!sent.contains_key(&first_key));
    assert_eq!(transport.connected().len(), 4);
    settle(Duration::from_secs(1)).await;
    assert!(third.subscription_frames().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn subscription_churn_never_starves_heartbeats_or_exceeds_the_send_ceiling() {
    let (gateway, transport, mut server) = ready(0.5).await;
    let subscriptions = gateway.subscriptions();
    let started = Instant::now();
    let mut frames: Vec<(Duration, Value)> = Vec::new();
    // 150 navigations, 300 ms apart, would be 150 frames without a ceiling.
    for n in 0..150u64 {
        subscriptions.update(|t| t.select_channel(snowflake(1 + n % 2), snowflake(11 + n % 2)));
        settle(Duration::from_millis(300)).await;
        frames.extend(
            server
                .service()
                .await
                .into_iter()
                .map(|frame| (started.elapsed(), frame)),
        );
    }
    // Then keep going until the window has slid and the backlog is sent.
    for _ in 0..140 {
        settle(Duration::from_secs(1)).await;
        frames.extend(
            server
                .service()
                .await
                .into_iter()
                .map(|frame| (started.elapsed(), frame)),
        );
    }
    // At no point did 60 s hold more than the ceiling, and subscriptions
    // always left room for control traffic.
    for (at, _) in &frames {
        let window = frames
            .iter()
            .filter(|(other, _)| *other + Duration::from_secs(60) > *at && other <= at)
            .count();
        assert!(window <= SEND_LIMIT, "{window} frames within 60 s");
    }
    let first_minute = frames
        .iter()
        .filter(|(at, frame)| *at < Duration::from_secs(45) && frame["op"] == 37)
        .count();
    assert!(
        first_minute <= SEND_LIMIT - CONTROL_RESERVE,
        "{first_minute} subscription frames"
    );
    assert!(
        first_minute > 50,
        "the ceiling must not be reached trivially low"
    );
    assert!(
        frames.iter().any(|(_, frame)| frame["op"] == 1),
        "heartbeats continued"
    );
    // The connection survived and the final wanted state did arrive.
    assert_eq!(transport.connected().len(), 1);
    let last = frames
        .iter()
        .rev()
        .find(|(_, frame)| frame["op"] == 37)
        .map(|(_, frame)| frame)
        .unwrap();
    let wanted = subscriptions.snapshot();
    let final_guild = wanted.selected_guild().unwrap().to_string();
    assert_eq!(last["d"]["subscriptions"][&final_guild]["typing"], true);
}

#[tokio::test(start_paused = true)]
async fn a_due_heartbeat_precedes_a_subscription_at_the_same_deadline() {
    let (gateway, _transport, mut server) = ready_with_heartbeat(0.5, 1_000).await;
    let subscriptions = gateway.subscriptions();
    settle(COALESCE).await;
    subscriptions.update(|target| target.select_channel(snowflake(1), snowflake(11)));
    settle(COALESCE).await;
    let frames = server.service().await;
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], json!({"op": 1, "d": 1}));
    assert_eq!(frames[1]["op"], 37);
    assert_eq!(frames[1]["d"]["subscriptions"]["1"]["typing"], true);
}

#[tokio::test(start_paused = true)]
async fn subscription_churn_reserves_short_interval_heartbeats_for_the_whole_window() {
    let (gateway, transport, mut server) = ready_with_heartbeat(0.5, 1_000).await;
    let subscriptions = gateway.subscriptions();
    let started = Instant::now();
    let mut frames: Vec<(Duration, Value)> = Vec::new();
    for n in 0..260u64 {
        subscriptions.update(|target| {
            target.select_channel(snowflake(1 + n % 2), snowflake(11 + n % 2));
        });
        settle(Duration::from_millis(300)).await;
        frames.extend(
            server
                .service()
                .await
                .into_iter()
                .map(|frame| (started.elapsed(), frame)),
        );
    }
    for (at, _) in &frames {
        let window = frames
            .iter()
            .filter(|(other, _)| *other + Duration::from_secs(60) > *at && other <= at)
            .count();
        // Identify is the one frame already consumed by the ready helper.
        let identify = usize::from(*at < Duration::from_secs(60));
        assert!(
            window + identify <= SEND_LIMIT,
            "{} frames within 60 s",
            window + identify
        );
    }
    let first_minute = frames
        .iter()
        .filter(|(at, frame)| *at < Duration::from_secs(60) && frame["op"] == 37)
        .count();
    assert!(first_minute > 0, "optional traffic still makes progress");
    assert!(
        first_minute <= SEND_LIMIT - CONTROL_RESERVE - 60,
        "{first_minute} subscription frames left too little room for one-second heartbeats"
    );
    assert_eq!(
        frames.iter().filter(|(_, frame)| frame["op"] == 1).count(),
        78
    );
    assert_eq!(transport.connected().len(), 1, "no missed heartbeat ACKs");
}

#[tokio::test(start_paused = true)]
async fn only_documented_opcodes_are_ever_sent_across_a_whole_session() {
    let (gateway, _transport, mut server) = ready(0.5).await;
    let subscriptions = gateway.subscriptions();
    let mut all = Vec::new();
    for step in 0..6u64 {
        subscriptions.update(|t| match step % 3 {
            0 => t.select_channel(snowflake(1), snowflake(11)),
            1 => t.set_voice_guild(Some(snowflake(2))),
            _ => t.clear_selection(),
        });
        settle(Duration::from_secs(1)).await;
        all.extend(server.service().await);
    }
    // `service` rejects any opcode but heartbeat, identify, resume, and 37.
    assert!(all.iter().any(|frame| frame["op"] == 37));
    assert!(all.iter().all(|frame| frame["op"] != 14));
}
