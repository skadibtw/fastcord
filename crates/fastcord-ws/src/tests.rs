//! End-to-end tests for the handshake, the adapter and `tungstenite` framing.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncWriteExt, DuplexStream, duplex};
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use crate::extension::{ExtensionError, Offer, PerMessageDeflate};
use crate::handshake::{HandshakeError, connect};
use crate::stream::{AdapterError, CompressingStream, Limits, classify};
use crate::test_support::{Peer, request_key};

type Ws = WebSocketStream<CompressingStream<DuplexStream>>;

const DEFAULT_LIMITS: Limits = Limits::new(1024 * 1024, 1024 * 1024);
const NO_LIMITS_WAIT: Duration = Duration::from_secs(10);

fn config(limits: Limits) -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(limits.max_message_bytes))
        .max_frame_size(Some(limits.max_frame_bytes))
}

fn pmd(header: &str) -> PerMessageDeflate {
    crate::extension::negotiate(Offer::CHROMIUM, &[header])
        .unwrap()
        .unwrap()
}

fn request() -> tokio_tungstenite::tungstenite::handshake::client::Request {
    "wss://gateway.discord.gg/?v=9"
        .into_client_request()
        .unwrap()
}

async fn raw_socket(pmd: Option<PerMessageDeflate>, limits: Limits) -> (Ws, Peer<DuplexStream>) {
    let (client, peer) = crate::test_support::pair(pmd).await;
    let stream = CompressingStream::new(client, pmd, limits);
    let socket: Ws =
        WebSocketStream::from_raw_socket(stream, Role::Client, Some(config(limits))).await;
    (socket, peer)
}

async fn next(socket: &mut Ws) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
    timeout(NO_LIMITS_WAIT, socket.next())
        .await
        .expect("the client produced no message")
}

/// Runs a real handshake against a scripted peer.
async fn handshake(
    extension: Option<&'static str>,
) -> Result<(Ws, Peer<DuplexStream>, String), HandshakeError> {
    let (client_io, server_io) = duplex(1024 * 1024);
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let accepted = crate::test_support::accept(server_io, extension)
            .await
            .expect("peer handshake");
        let _ = tx.send(accepted);
    });
    let socket = connect(client_io, request(), config(DEFAULT_LIMITS), DEFAULT_LIMITS).await?;
    let (peer, request) = rx.await.expect("peer was not created");
    Ok((socket, peer, request))
}

/// Runs a handshake against a peer that answers with `make(request)`.
async fn handshake_with<F>(make: F) -> Result<Ws, HandshakeError>
where
    F: FnOnce(&str) -> String + Send + 'static,
{
    let (client_io, server_io) = duplex(1024 * 1024);
    let (done_tx, done_rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut io = server_io;
        let request = crate::test_support::read_request(&mut io)
            .await
            .expect("request");
        let response = make(&request);
        io.write_all(response.as_bytes()).await.expect("response");
        io.flush().await.expect("flush");
        let _ = done_rx.await;
    });
    let result = connect(client_io, request(), config(DEFAULT_LIMITS), DEFAULT_LIMITS).await;
    let _ = done_tx.send(());
    result
}

fn valid_response(request: &str, extension: Option<&str>) -> String {
    let key = request_key(request).expect("Sec-WebSocket-Key");
    let accept = derive_accept_key(key.as_bytes());
    let mut response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n"
    );
    if let Some(extension) = extension {
        response.push_str(&format!("Sec-WebSocket-Extensions: {extension}\r\n"));
    }
    response.push_str("\r\n");
    response
}

#[tokio::test]
async fn the_handshake_offers_the_chromium_parameters_and_compresses_both_ways() {
    let (mut socket, mut peer, request) = handshake(Some("permessage-deflate")).await.unwrap();
    let lower = request.to_ascii_lowercase();
    assert!(
        lower.contains("sec-websocket-extensions: permessage-deflate; client_max_window_bits"),
        "{request}"
    );
    assert_eq!(
        socket.get_ref().negotiated(),
        Some(PerMessageDeflate::default())
    );
    peer.send_compressed(0x1, b"hello over deflate", 3)
        .await
        .unwrap();
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::text("hello over deflate")
    );
    socket.send(Message::text("from the client")).await.unwrap();
    let (opcode, payload) = peer.recv_message().await.unwrap().unwrap();
    assert_eq!(opcode, 0x1);
    assert_eq!(payload, b"from the client");
}

#[tokio::test]
async fn a_declined_extension_is_a_pure_passthrough() {
    let (mut socket, mut peer, request) = handshake(None).await.unwrap();
    assert!(
        request
            .to_ascii_lowercase()
            .contains("sec-websocket-extensions: permessage-deflate; client_max_window_bits")
    );
    assert_eq!(socket.get_ref().negotiated(), None);
    peer.send_text("plain text").await.unwrap();
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::text("plain text")
    );
    socket.send(Message::text("plain back")).await.unwrap();
    let (opcode, payload) = peer.recv_message().await.unwrap().unwrap();
    assert_eq!(opcode, 0x1);
    assert_eq!(payload, b"plain back");
}

#[tokio::test]
async fn compressed_text_binary_and_control_frames_survive_fragmentation() {
    let (mut socket, mut peer) =
        raw_socket(Some(PerMessageDeflate::default()), DEFAULT_LIMITS).await;
    peer.send_compressed(0x1, b"fragmented, compressed text", 4)
        .await
        .unwrap();
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::text("fragmented, compressed text")
    );
    peer.send_compressed(0x2, &[0, 1, 2, 3, 254, 255], 2)
        .await
        .unwrap();
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::Binary(vec![0, 1, 2, 3, 254, 255].into())
    );
}

#[tokio::test]
async fn control_frames_are_delivered_between_compressed_fragments_and_ponged() {
    let (mut socket, mut peer) =
        raw_socket(Some(PerMessageDeflate::default()), DEFAULT_LIMITS).await;
    let compressed = peer.compress_message(b"ping between fragments").unwrap();
    assert!(compressed.len() > 6);
    peer.send_frame(false, true, 0x1, &compressed[..3])
        .await
        .unwrap();
    peer.send_ping(b"one").await.unwrap();
    peer.send_frame(false, false, 0x0, &compressed[3..6])
        .await
        .unwrap();
    peer.send_ping(b"two").await.unwrap();
    peer.send_frame(true, false, 0x0, &compressed[6..])
        .await
        .unwrap();
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::Ping(b"one".to_vec().into())
    );
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::Ping(b"two".to_vec().into())
    );
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::text("ping between fragments")
    );
    for expected in [b"one".as_slice(), b"two".as_slice()] {
        let frame = timeout(NO_LIMITS_WAIT, peer.next_frame())
            .await
            .expect("no pong")
            .unwrap()
            .expect("peer closed");
        assert_eq!(frame.opcode, 0xA);
        assert_eq!(frame.payload, expected);
    }
}

#[tokio::test]
async fn context_takeover_is_retained_across_messages() {
    let (mut socket, mut peer) =
        raw_socket(Some(PerMessageDeflate::default()), DEFAULT_LIMITS).await;
    let payload = "a repeated payload that compresses better the second time".repeat(20);
    let first = peer.compress_message(payload.as_bytes()).unwrap();
    let second = peer.compress_message(payload.as_bytes()).unwrap();
    assert!(
        second.len() < first.len() / 2,
        "context was not retained: {} then {}",
        first.len(),
        second.len()
    );
    peer.send_frame(true, true, 0x1, &first).await.unwrap();
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::text(payload.clone())
    );
    peer.send_frame(true, true, 0x1, &second).await.unwrap();
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::text(payload)
    );
}

#[tokio::test]
async fn server_no_context_takeover_resets_the_client_inflater() {
    let pmd = pmd("permessage-deflate; server_no_context_takeover");
    let (mut socket, mut peer) = raw_socket(Some(pmd), DEFAULT_LIMITS).await;
    let payload = "the same message twice".repeat(40);
    let first = peer.compress_message(payload.as_bytes()).unwrap();
    let second = peer.compress_message(payload.as_bytes()).unwrap();
    assert_eq!(first.len(), second.len(), "the peer did not reset");
    peer.send_frame(true, true, 0x1, &first).await.unwrap();
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::text(payload.clone())
    );
    peer.send_frame(true, true, 0x1, &second).await.unwrap();
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::text(payload)
    );
}

#[tokio::test]
async fn client_context_takeover_and_no_context_takeover_are_honoured() {
    // Retained: the second compressed message references the first, so it is
    // much smaller and only the peer's retained inflater can read it.
    let (mut socket, mut peer) =
        raw_socket(Some(PerMessageDeflate::default()), DEFAULT_LIMITS).await;
    let payload = "client message repeated for the compressor".repeat(30);
    socket.send(Message::text(payload.clone())).await.unwrap();
    let first = peer.next_frame().await.unwrap().unwrap();
    assert!(first.rsv1);
    socket.send(Message::text(payload.clone())).await.unwrap();
    let second = peer.next_frame().await.unwrap().unwrap();
    assert!(second.rsv1);
    assert!(second.payload.len() < first.payload.len());
    // The peer's retained inflater reads both.
    let mut inflater = flate2::Decompress::new(false);
    for frame in [&first, &second] {
        let mut input = frame.payload.clone();
        input.extend_from_slice(&[0x00, 0x00, 0xff, 0xff]);
        let mut output = Vec::new();
        crate::stream::inflate_raw(
            &mut inflater,
            &input,
            &mut output,
            &mut [0u8; 32 * 1024],
            usize::MAX,
        )
        .unwrap();
        assert_eq!(output, payload.as_bytes());
    }

    // Empty takeover: each message is its own deflate stream, so the two
    // payloads are identical and each decodes from a fresh inflater.
    let pmd = pmd("permessage-deflate; client_no_context_takeover");
    let (mut socket, mut peer) = raw_socket(Some(pmd), DEFAULT_LIMITS).await;
    socket.send(Message::text(payload.clone())).await.unwrap();
    let first = peer.next_frame().await.unwrap().unwrap();
    socket.send(Message::text(payload.clone())).await.unwrap();
    let second = peer.next_frame().await.unwrap().unwrap();
    assert_eq!(first.payload.len(), second.payload.len());
    for frame in [&first, &second] {
        let mut inflater = flate2::Decompress::new(false);
        let mut input = frame.payload.clone();
        input.extend_from_slice(&[0x00, 0x00, 0xff, 0xff]);
        let mut output = Vec::new();
        crate::stream::inflate_raw(
            &mut inflater,
            &input,
            &mut output,
            &mut [0u8; 32 * 1024],
            usize::MAX,
        )
        .unwrap();
        assert_eq!(output, payload.as_bytes());
    }
}

#[tokio::test]
async fn a_lowered_client_window_keeps_messages_uncompressed() {
    let pmd = pmd("permessage-deflate; client_max_window_bits=12");
    let (mut socket, mut peer) = raw_socket(Some(pmd), DEFAULT_LIMITS).await;
    socket.send(Message::text("small window")).await.unwrap();
    let frame = peer.next_frame().await.unwrap().unwrap();
    assert!(!frame.rsv1);
    assert_eq!(frame.payload, b"small window");
}

#[tokio::test]
async fn invalid_reserved_bits_masked_frames_and_bad_continuations_are_typed() {
    let cases: [(&[u8], AdapterError); 4] = [
        (&[0xA1, 0x00], AdapterError::InvalidReservedBits),
        (&[0xC9, 0x00], AdapterError::InvalidControlFrame),
        (
            &[0x81, 0x81, 1, 2, 3, 4, b'a' ^ 1],
            AdapterError::MaskedServerFrame,
        ),
        (&[0x80, 0x00], AdapterError::UnexpectedContinuation),
    ];
    for (bytes, expected) in cases {
        let (mut socket, mut peer) =
            raw_socket(Some(PerMessageDeflate::default()), DEFAULT_LIMITS).await;
        peer.send_raw(bytes).await.unwrap();
        let error = next(&mut socket).await.unwrap().unwrap_err();
        assert_eq!(classify(&error), Some(expected), "{bytes:?}");
    }
}

#[tokio::test]
async fn an_inflated_bomb_fails_typed_before_allocating_it() {
    let limits = Limits::new(64 * 1024, 1024 * 1024);
    let (mut socket, mut peer) = raw_socket(Some(PerMessageDeflate::default()), limits).await;
    let bomb = vec![b'a'; 200 * 1024];
    let compressed = peer.compress_message(&bomb).unwrap();
    assert!(compressed.len() < 1024);
    peer.send_frame(true, true, 0x2, &compressed).await.unwrap();
    let error = next(&mut socket).await.unwrap().unwrap_err();
    assert_eq!(classify(&error), Some(AdapterError::MessageTooLarge));
}

#[tokio::test]
async fn a_compressed_accumulation_bomb_fails_typed() {
    let limits = Limits::new(64 * 1024, 1024 * 1024);
    let (mut socket, mut peer) = raw_socket(Some(PerMessageDeflate::default()), limits).await;
    let noise = noise(200 * 1024);
    let compressed = peer.compress_message(&noise).unwrap();
    assert!(compressed.len() > 64 * 1024);
    peer.send_frame(false, true, 0x2, &compressed)
        .await
        .unwrap();
    let error = next(&mut socket).await.unwrap().unwrap_err();
    assert_eq!(classify(&error), Some(AdapterError::CompressedTooLarge));
}

#[tokio::test]
async fn an_oversized_outgoing_message_fails_typed() {
    let limits = Limits::new(64 * 1024, 64 * 1024);
    let (mut socket, _peer) = raw_socket(Some(PerMessageDeflate::default()), limits).await;
    let error = socket
        .send(Message::Binary(vec![0u8; 64 * 1024 + 1].into()))
        .await
        .unwrap_err();
    assert_eq!(classify(&error), Some(AdapterError::FrameTooLarge));
}

#[tokio::test]
async fn a_new_connection_starts_with_a_fresh_deflate_context() {
    // The peer's second message is copied from the previous message through
    // the retained window (distance 30 KiB), so only the context the first
    // message established can decode it.
    let (mut socket, mut peer) =
        raw_socket(Some(PerMessageDeflate::default()), DEFAULT_LIMITS).await;
    let seed = noise(50_000);
    let _first = peer.compress_message(&seed).unwrap();
    let stale = peer.compress_message(&seed[30_000..]).unwrap();
    assert!(stale.len() < 1024, "fixture did not use the context");
    peer.send_frame(true, true, 0x2, &stale).await.unwrap();
    match next(&mut socket).await.unwrap() {
        // A strict decoder rejects the reference to a window it never had.
        Err(_) => {}
        // A lenient one may emit bytes for the missing history; what it must
        // not do is silently reproduce the peer's previous-message payload.
        Ok(Message::Binary(bytes)) => {
            assert_ne!(
                bytes.as_ref(),
                &seed[30_000..],
                "stale context was accepted"
            );
        }
        Ok(other) => panic!("unexpected stale message: {other:?}"),
    }

    // A fresh peer and connection have independent contexts.
    let (mut socket, mut peer) =
        raw_socket(Some(PerMessageDeflate::default()), DEFAULT_LIMITS).await;
    peer.send_compressed(0x1, b"fresh", 16).await.unwrap();
    assert_eq!(
        next(&mut socket).await.unwrap().unwrap(),
        Message::text("fresh")
    );
}

#[tokio::test]
async fn handshake_rejects_bad_accept_status_extension_and_subprotocol() {
    let result = handshake_with(|request| {
        let mut response = valid_response(request, None);
        response = response.replace("Sec-WebSocket-Accept:", "Sec-WebSocket-Accept: wrong");
        response
    })
    .await;
    assert!(matches!(result, Err(HandshakeError::BadAccept)));

    let result = handshake_with(|request| {
        valid_response(request, None).replace("101 Switching Protocols", "200 OK")
    })
    .await;
    assert!(matches!(result, Err(HandshakeError::BadStatus)));

    let result = handshake_with(|request| {
        valid_response(
            request,
            Some("permessage-deflate; server_max_window_bits=10"),
        )
    })
    .await;
    assert!(matches!(
        result,
        Err(HandshakeError::Extension(
            ExtensionError::UnofferedParameter("server_max_window_bits")
        ))
    ));

    let result = handshake_with(|request| {
        let mut response = valid_response(request, None);
        response = response.replace("\r\n\r\n", "\r\nSec-WebSocket-Protocol: chat\r\n\r\n");
        response
    })
    .await;
    assert!(matches!(result, Err(HandshakeError::UnexpectedSubprotocol)));

    let result =
        handshake_with(|request| valid_response(request, Some("x-webkit-deflate-frame"))).await;
    assert!(matches!(
        result,
        Err(HandshakeError::Extension(ExtensionError::UnknownExtension))
    ));
}

#[tokio::test]
async fn handshake_rejects_an_endless_header() {
    let result = handshake_with(|_| {
        let mut response = String::from("HTTP/1.1 101 Switching Protocols\r\nX: ");
        response.push_str(&"a".repeat(20 * 1024));
        response.push_str("\r\n\r\n");
        response
    })
    .await;
    assert!(matches!(result, Err(HandshakeError::TooLong)));
}

#[tokio::test]
async fn handshake_reports_a_closed_peer() {
    let (client_io, server_io) = duplex(1024);
    tokio::spawn(async move {
        let mut io = server_io;
        let _ = crate::test_support::read_request(&mut io).await;
    });
    let result = connect(client_io, request(), config(DEFAULT_LIMITS), DEFAULT_LIMITS).await;
    assert!(matches!(result, Err(HandshakeError::Closed)));
}

/// Deterministic incompressible bytes.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x1234_5678u32;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}
