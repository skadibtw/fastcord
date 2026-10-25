//! TLS WebSocket client shared by the remote-auth and main Gateway connections.
//!
//! TLS uses the same rustls/ring stack and webpki roots as the REST client.
//! The opening handshake and the RFC 7692 permessage-deflate adapter live in
//! `fastcord-ws`, which also serves the voice Gateway in `fastcord-media`;
//! `tokio-tungstenite` is used only for WebSocket framing so it cannot pull a
//! second crypto provider into the binary.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(not(feature = "fake-discord"))]
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
#[cfg(not(feature = "fake-discord"))]
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::gateway::ClientProperties;

#[cfg(not(feature = "fake-discord"))]
type Stream = TlsStream<TcpStream>;
#[cfg(feature = "fake-discord")]
type Stream = fake::Stream;

pub(crate) type TlsSocket = fastcord_ws::Socket<Stream>;

/// A TLS stream, or, only in the test-only fake mode, a plain loopback one.
#[cfg(feature = "fake-discord")]
pub(crate) mod fake {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::TcpStream;
    use tokio_rustls::client::TlsStream;

    pub(crate) enum Stream {
        Tls(Box<TlsStream<TcpStream>>),
        Plain(TcpStream),
    }

    impl From<TlsStream<TcpStream>> for Stream {
        fn from(stream: TlsStream<TcpStream>) -> Self {
            Self::Tls(Box::new(stream))
        }
    }

    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
                Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.get_mut() {
                Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
                Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
                Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
                Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            }
        }
    }

    /// The only plaintext connection this client ever makes: `ws://127.0.0.1:<port>`
    /// of the configured fake origin. Anything else is refused while it is set.
    pub(super) async fn connect(
        mut request: tokio_tungstenite::tungstenite::handshake::client::Request,
        origin: &'static str,
        browser: &super::ClientProperties,
        config: super::WebSocketConfig,
    ) -> Result<super::TlsSocket, super::ConnectError> {
        use super::{ConnectError, HeaderValue};
        let uri = request.uri().clone();
        let port = crate::fake_origin::port().ok_or(ConnectError)?;
        if uri.scheme_str() != Some("ws")
            || uri.host() != Some("127.0.0.1")
            || uri.port_u16() != Some(port)
        {
            return Err(ConnectError);
        }
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_static(origin));
        super::browser_headers(&mut request, browser);
        let tcp = TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|_| ConnectError)?;
        tcp.set_nodelay(true).map_err(|_| ConnectError)?;
        super::handshake(Stream::Plain(tcp), request, config).await
    }
}

/// Categorical: URLs and TLS errors never reach logs or messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectError;

/// Connects to a `wss://` URL. The `Origin` header is the documented client
/// origin; Discord's gateways reject other origins. The handshake carries the
/// rest of what the desktop client's Chromium sends (`browser_headers`) and
/// offers `permessage-deflate`.
pub(crate) async fn connect(
    url: &str,
    origin: &'static str,
    browser: &ClientProperties,
    config: WebSocketConfig,
    limit: Duration,
) -> Result<TlsSocket, ConnectError> {
    timeout(limit, connect_inner(url, origin, browser, config))
        .await
        .map_err(|_| ConnectError)?
}

/// `User-Agent`, `Accept-Language`, `Accept-Encoding` and the no-cache pair of
/// a Chromium WebSocket handshake. No cookie: these hosts are on `discord.gg`,
/// which never sets any. The permessage-deflate offer is added by
/// [`handshake`].
fn browser_headers(request: &mut Request, browser: &ClientProperties) {
    let headers = request.headers_mut();
    if let Ok(agent) = HeaderValue::from_str(&browser.browser_user_agent) {
        headers.insert("User-Agent", agent);
    }
    if let Ok(language) = HeaderValue::from_str(&browser.accept_language()) {
        headers.insert("Accept-Language", language);
    }
    headers.insert(
        "Accept-Encoding",
        HeaderValue::from_static("gzip, deflate, br, zstd"),
    );
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers.insert("Pragma", HeaderValue::from_static("no-cache"));
}

/// The opening handshake, the extension negotiation and the framing adapter
/// from `fastcord-ws`.
async fn handshake<S>(
    stream: S,
    request: Request,
    config: WebSocketConfig,
) -> Result<fastcord_ws::Socket<S>, ConnectError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let limits = fastcord_ws::Limits::new(
        config.max_message_size.unwrap_or(64 << 20),
        config.max_frame_size.unwrap_or(16 << 20),
    );
    fastcord_ws::connect(stream, request, config, limits)
        .await
        .map_err(|_| ConnectError)
}

async fn connect_inner(
    url: &str,
    origin: &'static str,
    browser: &ClientProperties,
    config: WebSocketConfig,
) -> Result<TlsSocket, ConnectError> {
    let request = url.into_client_request().map_err(|_| ConnectError)?;
    #[cfg(feature = "fake-discord")]
    if crate::fake_origin::active() {
        return fake::connect(request, origin, browser, config).await;
    }
    let uri = request.uri().clone();
    if uri.scheme_str() != Some("wss") {
        return Err(ConnectError);
    }
    let host = uri.host().ok_or(ConnectError)?.to_owned();
    let port = uri.port_u16().unwrap_or(443);
    let mut request = request;
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static(origin));
    browser_headers(&mut request, browser);
    let tcp = fastcord_bypass::connect(&host, port)
        .await
        .map_err(|_| ConnectError)?;
    tcp.set_nodelay(true).map_err(|_| ConnectError)?;
    let server_name = ServerName::try_from(host).map_err(|_| ConnectError)?;
    let tls = TlsConnector::from(tls_config()?)
        .connect(server_name, tcp)
        .await
        .map_err(|_| ConnectError)?;
    // With `fake-discord` the stream is a TLS/plain wrapper; without it, TLS itself.
    #[allow(clippy::useless_conversion)]
    let stream = tls.into();
    handshake(stream, request, config).await
}

fn tls_config() -> Result<Arc<ClientConfig>, ConnectError> {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| ConnectError)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    #[test]
    fn tls_configuration_builds_with_the_bundled_roots() {
        assert!(tls_config().is_ok());
    }

    #[tokio::test]
    async fn plaintext_and_malformed_urls_are_rejected_before_any_connection() {
        let config = WebSocketConfig::default();
        let limit = Duration::from_secs(5);
        for url in [
            "ws://gateway.discord.gg",
            "https://discord.com",
            "not a url",
        ] {
            assert_eq!(
                connect(url, "https://discord.com", &browser(), config, limit)
                    .await
                    .err(),
                Some(ConnectError),
                "{url}"
            );
        }
    }

    fn browser() -> ClientProperties {
        ClientProperties::bundled(crate::gateway::HostOs::Windows, "en-US")
    }

    #[test]
    fn the_handshake_carries_the_desktop_browser_headers() {
        let mut request = "wss://gateway.discord.gg/?v=9"
            .into_client_request()
            .unwrap();
        browser_headers(&mut request, &browser());
        let headers = request.headers();
        assert_eq!(headers["User-Agent"], browser().browser_user_agent);
        assert_eq!(headers["Accept-Language"], "en-US,en;q=0.9");
        assert_eq!(headers["Cache-Control"], "no-cache");
        assert_eq!(headers["Pragma"], "no-cache");
        assert_eq!(headers["Accept-Encoding"], "gzip, deflate, br, zstd");
        assert!(!headers.contains_key("Cookie"));
    }

    #[tokio::test]
    async fn the_real_handshake_offers_the_extension_and_a_decline_still_works() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let peer = tokio::spawn(async move {
            fastcord_ws::test_support::accept(server_io, None)
                .await
                .expect("peer handshake")
        });
        let mut request = "wss://gateway.discord.gg/?v=9"
            .into_client_request()
            .unwrap();
        browser_headers(&mut request, &browser());
        let mut socket = handshake(client_io, request, WebSocketConfig::default())
            .await
            .unwrap();
        let (mut peer, request) = peer.await.unwrap();
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("user-agent: mozilla/5.0"));
        assert!(lower.contains("cache-control: no-cache"));
        assert!(lower.contains("permessage-deflate; client_max_window_bits"));
        assert!(socket.get_ref().negotiated().is_none());
        peer.send_text("plain hello").await.unwrap();
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(message, Message::text("plain hello"));
    }
}
