//! The real client state machine against an in-process gateway that speaks real
//! WebSocket framing and performs real RSA-OAEP with the key it is sent.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{DuplexStream, duplex};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::*;
use crate::remote_auth::crypto::test_support::{encrypt_for, fingerprint_of};
use crate::remote_auth::transport::websocket_config;

const HELLO: &str = include_str!("../../../../../fixtures/remote-auth/hello.json");

struct Gateway {
    socket: WebSocketStream<DuplexStream>,
}

impl Gateway {
    async fn send(&mut self, value: Value) {
        self.socket
            .send(Message::text(value.to_string()))
            .await
            .unwrap();
    }

    async fn send_raw(&mut self, message: Message) {
        self.socket.send(message).await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        loop {
            match self.socket.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                Message::Close(_) => panic!("client closed unexpectedly"),
                _ => {}
            }
        }
    }

    async fn hello(&mut self, heartbeat_interval: u64, timeout_ms: u64) {
        self.send(
            json!({"op":"hello","heartbeat_interval":heartbeat_interval,"timeout_ms":timeout_ms}),
        )
        .await;
    }

    /// hello → init → nonce_proof handshake; returns the client's public key.
    async fn handshake(&mut self) -> String {
        let hello: Value = serde_json::from_str(HELLO).unwrap();
        self.send(hello).await;
        self.complete_key_exchange().await
    }

    async fn complete_key_exchange(&mut self) -> String {
        let init = self.recv().await;
        assert_eq!(init["op"], "init");
        let public_key = init["encoded_public_key"].as_str().unwrap().to_owned();
        let nonce = b"server-chosen-nonce-0123456789ab";
        self.send(json!({"op":"nonce_proof","encrypted_nonce":encrypt_for(&public_key, nonce)}))
            .await;
        let proof = self.recv().await;
        assert_eq!(proof["op"], "nonce_proof");
        let expected = base64_url(nonce);
        assert_eq!(proof["nonce"], expected);
        public_key
    }
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Default)]
struct FakeExchange {
    tickets: Mutex<Vec<String>>,
    reply: Mutex<Option<Result<String, RemoteAuthError>>>,
}

impl FakeExchange {
    fn replying(reply: Result<String, RemoteAuthError>) -> Self {
        Self {
            tickets: Mutex::default(),
            reply: Mutex::new(Some(reply)),
        }
    }

    fn set_reply(&self, reply: Result<String, RemoteAuthError>) {
        *self.reply.lock().unwrap() = Some(reply);
    }

    fn tickets(&self) -> Vec<String> {
        self.tickets.lock().unwrap().clone()
    }
}

impl TicketExchange for FakeExchange {
    async fn exchange(&self, ticket: &str) -> Result<String, RemoteAuthError> {
        self.tickets.lock().unwrap().push(ticket.to_owned());
        self.reply
            .lock()
            .unwrap()
            .take()
            .expect("one exchange per attempt")
    }
}

struct Harness {
    key: AttemptKey,
    exchange: Arc<FakeExchange>,
    events: mpsc::Sender<RemoteAuthEvent>,
    receiver: mpsc::Receiver<RemoteAuthEvent>,
    client_socket: Option<WebSocketStream<DuplexStream>>,
    gateway: Option<Gateway>,
}

impl Harness {
    async fn new(exchange: FakeExchange) -> Self {
        let (client_io, server_io) = duplex(64 * 1024);
        let client =
            WebSocketStream::from_raw_socket(client_io, Role::Client, Some(websocket_config()))
                .await;
        let server =
            WebSocketStream::from_raw_socket(server_io, Role::Server, Some(websocket_config()))
                .await;
        let (events, receiver) = mpsc::channel(EVENTS);
        Self {
            key: AttemptKey::generate().unwrap(),
            exchange: Arc::new(exchange),
            events,
            receiver,
            client_socket: Some(client),
            gateway: Some(Gateway { socket: server }),
        }
    }

    /// Runs the client driver and the scripted gateway together.
    async fn run<F, Fut>(&mut self, script: F) -> Result<UserToken, RemoteAuthError>
    where
        F: FnOnce(Gateway) -> Fut,
        Fut: Future<Output = ()>,
    {
        let socket = self.client_socket.take().unwrap();
        let gateway = self.gateway.take().unwrap();
        let (result, ()) = tokio::join!(
            drive(socket, &self.key, &*self.exchange, &self.events),
            script(gateway)
        );
        result
    }

    fn drain(&mut self) -> Vec<RemoteAuthEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.receiver.try_recv() {
            events.push(event);
        }
        events
    }
}

const EVENTS: usize = 8;
const PAYLOAD: &[u8] = b"175928847299117063:0:0:alt_fixture";

#[tokio::test]
async fn full_flow_authorizes_and_exchanges_the_ticket_exactly_once() {
    let mut harness = Harness::new(FakeExchange::default()).await;
    let exchange = Arc::clone(&harness.exchange);
    let result = harness
        .run(|mut gateway| async move {
            let public_key = gateway.handshake().await;
            gateway
                .send(json!({"op":"pending_remote_init","fingerprint":fingerprint_of(&public_key)}))
                .await;
            gateway
                .send(json!({"op":"pending_ticket","encrypted_user_payload":encrypt_for(&public_key, PAYLOAD)}))
                .await;
            exchange.set_reply(Ok(encrypt_for(&public_key, b"dummy.offline.token_value-123")));
            gateway
                .send(json!({"op":"pending_login","ticket":"dummy-ticket"}))
                .await;
            // The client says goodbye once it has the token.
            while let Some(Ok(message)) = gateway.socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        })
        .await;
    let token = result.unwrap();
    assert_eq!(token.expose_secret(), "dummy.offline.token_value-123");
    assert_eq!(harness.exchange.tickets(), ["dummy-ticket"]);

    let events = harness.drain();
    assert_eq!(events.len(), 2, "{events:?}");
    let RemoteAuthEvent::Qr(link) = &events[0] else {
        panic!("first event must be the QR code: {events:?}");
    };
    assert_eq!(
        link.as_str(),
        format!("https://discord.com/ra/{}", harness.key.fingerprint())
    );
    // Remaining lifetime when the code appeared: the server value minus the
    // (small, real) time the handshake took.
    assert!(link.expires_in() <= Duration::from_millis(142_637));
    assert!(link.expires_in() > Duration::from_secs(120));
    let RemoteAuthEvent::PendingUser(user) = &events[1] else {
        panic!("second event must be the pending user: {events:?}");
    };
    assert_eq!(user.id.0, 175_928_847_299_117_063);
    assert_eq!(user.username, "alt_fixture");
    // No event, and no diagnostic, reveals the token, ticket, or QR URL.
    for event in &events {
        let debug = format!("{event:?}");
        assert!(!debug.contains("dummy"), "{debug}");
        assert!(!debug.contains("discord.com/ra"), "{debug}");
    }
    assert_eq!(
        format!("{:?}", RemoteAuthEvent::Authorized(Arc::new(token))),
        "Authorized([REDACTED])"
    );
}

#[tokio::test]
async fn fingerprint_that_is_not_ours_stops_before_any_qr_code() {
    let mut harness = Harness::new(FakeExchange::default()).await;
    let result = harness
        .run(|mut gateway| async move {
            gateway.handshake().await;
            gateway
                .send(json!({"op":"pending_remote_init","fingerprint":"UZ0-kOVzXDZTFVV5_QlpURSO2BQHrtkKWHNpIGoDI0k"}))
                .await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::FingerprintMismatch);
    assert!(harness.drain().is_empty());
}

#[tokio::test]
async fn phone_cancel_after_scan_is_reported_and_never_exchanges() {
    let mut harness = Harness::new(FakeExchange::default()).await;
    let result = harness
        .run(|mut gateway| async move {
            let public_key = gateway.handshake().await;
            gateway
                .send(json!({"op":"pending_remote_init","fingerprint":fingerprint_of(&public_key)}))
                .await;
            gateway
                .send(json!({"op":"pending_ticket","encrypted_user_payload":encrypt_for(&public_key, PAYLOAD)}))
                .await;
            gateway.send(json!({"op":"cancel"})).await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::CancelledOnPhone);
    assert!(harness.exchange.tickets().is_empty());
    let events = harness.drain();
    assert!(matches!(
        events.as_slice(),
        [RemoteAuthEvent::Qr(_), RemoteAuthEvent::PendingUser(_)]
    ));
}

#[tokio::test]
async fn captcha_from_the_exchange_stops_without_a_token() {
    let mut harness = Harness::new(FakeExchange::replying(Err(RemoteAuthError::Captcha))).await;
    let result = harness
        .run(|mut gateway| async move {
            let public_key = gateway.handshake().await;
            gateway
                .send(json!({"op":"pending_remote_init","fingerprint":fingerprint_of(&public_key)}))
                .await;
            gateway
                .send(json!({"op":"pending_login","ticket":"dummy-ticket"}))
                .await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::Captcha);
    assert_eq!(harness.exchange.tickets(), ["dummy-ticket"]);
    assert!(!RemoteAuthError::Captcha.can_regenerate());
    assert!(RemoteAuthError::Captcha.to_string().contains("CAPTCHA"));
}

#[tokio::test]
async fn token_that_is_not_a_clean_credential_is_rejected() {
    for bad in [&b"has space"[..], b"", b"line\nbreak", &[0xff, 0xfe, 0x41]] {
        let mut harness = Harness::new(FakeExchange::default()).await;
        let exchange = Arc::clone(&harness.exchange);
        let result = harness
            .run(|mut gateway| async move {
                let public_key = gateway.handshake().await;
                gateway
                    .send(json!({"op":"pending_remote_init","fingerprint":fingerprint_of(&public_key)}))
                    .await;
                exchange.set_reply(Ok(encrypt_for(&public_key, bad)));
                gateway
                    .send(json!({"op":"pending_login","ticket":"dummy-ticket"}))
                    .await;
            })
            .await;
        assert_eq!(result.unwrap_err(), RemoteAuthError::Crypto, "{bad:?}");
    }
}

#[tokio::test]
async fn out_of_sequence_binary_and_malformed_frames_are_protocol_errors() {
    // pending_login before the key exchange completed.
    let mut harness = Harness::new(FakeExchange::default()).await;
    let result = harness
        .run(|mut gateway| async move {
            let hello: Value = serde_json::from_str(HELLO).unwrap();
            gateway.send(hello).await;
            gateway.recv().await;
            gateway
                .send(json!({"op":"pending_login","ticket":"dummy-ticket"}))
                .await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::Protocol);
    assert!(harness.exchange.tickets().is_empty());

    // Binary frame.
    let mut harness = Harness::new(FakeExchange::default()).await;
    let result = harness
        .run(|mut gateway| async move {
            let hello: Value = serde_json::from_str(HELLO).unwrap();
            gateway.send(hello).await;
            gateway.recv().await;
            gateway.send_raw(Message::binary(vec![1, 2, 3])).await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::Protocol);

    // Not hello first.
    let mut harness = Harness::new(FakeExchange::default()).await;
    let result = harness
        .run(|mut gateway| async move {
            gateway.send(json!({"op":"heartbeat_ack"})).await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::Protocol);

    // Undecryptable nonce.
    let mut harness = Harness::new(FakeExchange::default()).await;
    let result = harness
        .run(|mut gateway| async move {
            let hello: Value = serde_json::from_str(HELLO).unwrap();
            gateway.send(hello).await;
            gateway.recv().await;
            gateway
                .send(json!({"op":"nonce_proof","encrypted_nonce":"AAAA"}))
                .await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::Crypto);
}

#[tokio::test]
async fn unknown_ops_are_ignored_mid_flow() {
    let mut harness = Harness::new(FakeExchange::default()).await;
    let result = harness
        .run(|mut gateway| async move {
            let hello: Value = serde_json::from_str(HELLO).unwrap();
            gateway.send(hello).await;
            gateway.send(json!({"op":"something_new","x":1})).await;
            let public_key = gateway.complete_key_exchange().await;
            gateway
                .send(json!({"op":"pending_remote_init","fingerprint":fingerprint_of(&public_key)}))
                .await;
            gateway.send(json!({"op":"cancel"})).await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::CancelledOnPhone);
}

#[tokio::test(start_paused = true)]
async fn session_expires_at_the_server_timeout_even_for_a_hostile_zero() {
    let mut harness = Harness::new(FakeExchange::default()).await;
    let started = Instant::now();
    let result = harness
        .run(|mut gateway| async move {
            gateway.hello(0, 0).await;
            gateway.complete_key_exchange().await;
            // Go silent; the paused clock auto-advances to the deadline. Keep
            // acking so only the session deadline can end the attempt.
            while let Some(Ok(Message::Text(_))) = gateway.socket.next().await {
                gateway.send(json!({"op":"heartbeat_ack"})).await;
            }
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::Expired);
    // Clamped to the 5 s floor rather than expiring (or spinning) immediately.
    assert_eq!(started.elapsed(), Duration::from_millis(SESSION_RANGE_MS.0));
}

#[tokio::test(start_paused = true)]
async fn gateway_timeout_close_code_means_expired() {
    let mut harness = Harness::new(FakeExchange::default()).await;
    let result = harness
        .run(|mut gateway| async move {
            gateway.handshake().await;
            gateway
                .send_raw(Message::Close(Some(CloseFrame {
                    code: CloseCode::Library(4003),
                    reason: "".into(),
                })))
                .await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::Expired);

    let mut harness = Harness::new(FakeExchange::default()).await;
    let result = harness
        .run(|mut gateway| async move {
            gateway.handshake().await;
            gateway
                .send_raw(Message::Close(Some(CloseFrame {
                    code: CloseCode::Library(4002),
                    reason: "".into(),
                })))
                .await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::Closed(Some(4002)));
}

#[tokio::test(start_paused = true)]
async fn heartbeats_follow_the_interval_and_a_missing_ack_is_fatal() {
    let mut harness = Harness::new(FakeExchange::default()).await;
    let started = Instant::now();
    let beats = Arc::new(Mutex::new(Vec::new()));
    let script_beats = Arc::clone(&beats);
    let result = harness
        .run(move |mut gateway| async move {
            gateway.hello(3_000, 600_000).await;
            gateway.complete_key_exchange().await;
            // First beat is acked, the second is not.
            let first = gateway.recv().await;
            script_beats
                .lock()
                .unwrap()
                .push((first, started.elapsed()));
            gateway.send(json!({"op":"heartbeat_ack"})).await;
            let second = gateway.recv().await;
            script_beats
                .lock()
                .unwrap()
                .push((second, started.elapsed()));
            // Linger so the third tick (6 s -> 9 s) reveals the missing ack.
            let _ = gateway.socket.next().await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::HeartbeatLost);
    let beats = beats.lock().unwrap();
    assert_eq!(beats[0].0, json!({"op":"heartbeat"}));
    assert_eq!(beats[0].1, Duration::from_secs(3));
    assert_eq!(beats[1].0, json!({"op":"heartbeat"}));
    assert_eq!(beats[1].1, Duration::from_secs(6));
}

#[tokio::test]
async fn dropping_the_event_receiver_cancels_the_attempt() {
    let mut harness = Harness::new(FakeExchange::default()).await;
    let (events, receiver) = mpsc::channel(EVENTS);
    harness.events = events;
    drop(receiver);
    let result = harness
        .run(|mut gateway| async move {
            let hello: Value = serde_json::from_str(HELLO).unwrap();
            gateway.send(hello).await;
            // Never answer: the client must still notice the cancellation.
            let _ = gateway.socket.next().await;
        })
        .await;
    assert_eq!(result.unwrap_err(), RemoteAuthError::Cancelled);
}
