//! The client half of the WebSocket opening handshake.
//!
//! `tungstenite`'s client handshake ignores an unsolicited
//! `Sec-WebSocket-Extensions` response (`protocol/mod.rs` leaves RFC 6455 step
//! 5 as a `TODO`), so the exchange is done here: the request is still
//! serialized by tungstenite (mandatory header order and all), the response is
//! validated including the extension negotiation, and the socket is then
//! handed to `WebSocketStream::from_raw_socket` for framing.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::client::{Request, generate_request};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use crate::extension::{self, ExtensionError, OFFER, Offer, PerMessageDeflate};
use crate::stream::{CompressingStream, Limits};

/// Anything larger than this is not a valid opening handshake response.
const MAX_RESPONSE_HEADER_BYTES: usize = 16 * 1024;
const READ_CHUNK_BYTES: usize = 4096;

/// Categorical: no header text or endpoint ever reaches a display or a log.
#[derive(Debug)]
pub enum HandshakeError {
    Io(io::Error),
    /// The peer closed before finishing the response.
    Closed,
    /// The response header exceeded [`MAX_RESPONSE_HEADER_BYTES`].
    TooLong,
    /// The request could not be serialized (only fixable by the caller).
    BadRequest,
    /// The response is not an HTTP/1.1 handshake.
    BadResponse,
    /// The server did not answer 101.
    BadStatus,
    MissingUpgrade,
    MissingConnection,
    /// `Sec-WebSocket-Accept` does not match the sent key.
    BadAccept,
    /// The server selected a subprotocol that was never offered.
    UnexpectedSubprotocol,
    Extension(ExtensionError),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Io(_) => "WebSocket handshake I/O error",
            Self::Closed => "peer closed during the WebSocket handshake",
            Self::TooLong => "WebSocket handshake response too long",
            Self::BadRequest => "invalid WebSocket handshake request",
            Self::BadResponse => "malformed WebSocket handshake response",
            Self::BadStatus => "WebSocket handshake was not accepted",
            Self::MissingUpgrade => "WebSocket handshake lacks Upgrade",
            Self::MissingConnection => "WebSocket handshake lacks Connection",
            Self::BadAccept => "WebSocket handshake accept key mismatch",
            Self::UnexpectedSubprotocol => "server chose an unoffered subprotocol",
            Self::Extension(_) => "WebSocket extension negotiation failed",
        })
    }
}

impl std::error::Error for HandshakeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Extension(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for HandshakeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ExtensionError> for HandshakeError {
    fn from(error: ExtensionError) -> Self {
        Self::Extension(error)
    }
}

/// Performs the opening handshake over `stream`, negotiates
/// `permessage-deflate` and returns a `tungstenite` socket over the adapter.
///
/// The caller owns the whole-connection timeout; this function must not add a
/// timer or a polling loop.
pub async fn connect<S>(
    stream: S,
    request: Request,
    config: WebSocketConfig,
    limits: Limits,
) -> Result<WebSocketStream<CompressingStream<S>>, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    connect_with_offer(stream, request, OFFER, config, limits).await
}

/// [`connect`] with an explicit offer, for tests of the negotiation itself.
pub async fn connect_with_offer<S>(
    mut stream: S,
    mut request: Request,
    offer: &'static str,
    config: WebSocketConfig,
    limits: Limits,
) -> Result<WebSocketStream<CompressingStream<S>>, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let offer_value = HeaderValue::from_str(offer).map_err(|_| HandshakeError::BadRequest)?;
    request
        .headers_mut()
        .insert("Sec-WebSocket-Extensions", offer_value);
    let (bytes, key) = generate_request(request).map_err(|_| HandshakeError::BadRequest)?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    let (head, tail) = read_response(&mut stream).await?;
    let response = parse_response(&head)?;
    validate(&response, &key)?;
    let values: Vec<&str> = response
        .headers
        .iter()
        .filter(|(name, _)| name == "sec-websocket-extensions")
        .map(|(_, value)| value.as_str())
        .collect();
    if response
        .headers
        .iter()
        .any(|(name, _)| name == "sec-websocket-protocol")
    {
        return Err(HandshakeError::UnexpectedSubprotocol);
    }
    let offer = Offer::parse(offer)?;
    let pmd: Option<PerMessageDeflate> = extension::negotiate(offer, &values)?;
    let adapter = CompressingStream::new(stream, pmd, limits).with_preloaded(tail);
    Ok(WebSocketStream::from_raw_socket(adapter, Role::Client, Some(config)).await)
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
}

/// Reads until the response header terminator, returning the header bytes and
/// any bytes already over-read (early WebSocket frames).
async fn read_response<S>(stream: &mut S) -> Result<(Vec<u8>, Vec<u8>), HandshakeError>
where
    S: AsyncRead + Unpin,
{
    let mut head = Vec::new();
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    loop {
        if let Some(end) = find(&head, b"\r\n\r\n") {
            let tail = head.split_off(end + 4);
            head.truncate(end);
            return Ok((head, tail));
        }
        if head.len() >= MAX_RESPONSE_HEADER_BYTES {
            return Err(HandshakeError::TooLong);
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(HandshakeError::Closed);
        }
        head.extend_from_slice(&chunk[..read]);
    }
}

fn validate(response: &Response, key: &str) -> Result<(), HandshakeError> {
    if response.status != 101 {
        return Err(HandshakeError::BadStatus);
    }
    if !header_has_token(&response.headers, "upgrade", "websocket") {
        return Err(HandshakeError::MissingUpgrade);
    }
    if !header_has_token(&response.headers, "connection", "upgrade") {
        return Err(HandshakeError::MissingConnection);
    }
    let mut accepts = response
        .headers
        .iter()
        .filter(|(name, _)| name == "sec-websocket-accept");
    let Some((_, accept)) = accepts.next() else {
        return Err(HandshakeError::BadAccept);
    };
    if accepts.next().is_some() || accept != &derive_accept_key(key.as_bytes()) {
        return Err(HandshakeError::BadAccept);
    }
    Ok(())
}

fn header_has_token(headers: &[(String, String)], name: &str, token: &str) -> bool {
    headers
        .iter()
        .filter(|(header, _)| header == name)
        .flat_map(|(_, value)| value.split(','))
        .any(|value| value.trim().eq_ignore_ascii_case(token))
}

fn parse_response(head: &[u8]) -> Result<Response, HandshakeError> {
    let text = std::str::from_utf8(head).map_err(|_| HandshakeError::BadResponse)?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().ok_or(HandshakeError::BadResponse)?;
    let mut parts = status_line.split_whitespace();
    if parts.next() != Some("HTTP/1.1") {
        return Err(HandshakeError::BadResponse);
    }
    let status = parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(HandshakeError::BadResponse)?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // Obsolete line folding is not accepted.
            return Err(HandshakeError::BadResponse);
        }
        let (name, value) = line.split_once(':').ok_or(HandshakeError::BadResponse)?;
        if name.is_empty() {
            return Err(HandshakeError::BadResponse);
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok(Response { status, headers })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_token_lists_are_case_insensitive() {
        let headers = vec![("connection".to_owned(), "keep-alive, Upgrade".to_owned())];
        assert!(header_has_token(&headers, "connection", "upgrade"));
        assert!(!header_has_token(&headers, "connection", "close"));
    }

    #[test]
    fn malformed_responses_are_typed() {
        assert!(matches!(
            parse_response(b"HTTP/2 101 Switching Protocols"),
            Err(HandshakeError::BadResponse)
        ));
        assert!(matches!(
            parse_response(b"HTTP/1.1 101 Switching Protocols\r\nNoColon"),
            Err(HandshakeError::BadResponse)
        ));
        assert!(matches!(
            parse_response(b"HTTP/1.1 101 Switching Protocols\r\n Folded: yes"),
            Err(HandshakeError::BadResponse)
        ));
        let response = parse_response(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: abc\r\n",
        )
        .unwrap();
        assert_eq!(response.status, 101);
        assert_eq!(response.headers.len(), 3);
    }

    #[test]
    fn a_missing_or_mismatched_accept_key_is_rejected() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = derive_accept_key(key.as_bytes());
        let response = Response {
            status: 101,
            headers: vec![
                ("upgrade".to_owned(), "websocket".to_owned()),
                ("connection".to_owned(), "Upgrade".to_owned()),
                ("sec-websocket-accept".to_owned(), accept),
            ],
        };
        assert!(validate(&response, key).is_ok());
        let mut wrong = response;
        wrong
            .headers
            .retain(|(name, _)| name != "sec-websocket-accept");
        assert!(matches!(
            validate(&wrong, key),
            Err(HandshakeError::BadAccept)
        ));
    }
}
