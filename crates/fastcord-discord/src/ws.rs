//! TLS WebSocket client shared by the remote-auth and main Gateway connections.
//!
//! TLS uses the same rustls/ring stack and webpki roots as the REST client;
//! `tokio-tungstenite` is used only for WebSocket framing so it cannot pull a
//! second crypto provider into the binary.

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
#[cfg(not(feature = "fake-discord"))]
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{WebSocketStream, client_async_with_config};

#[cfg(not(feature = "fake-discord"))]
type Stream = TlsStream<TcpStream>;
#[cfg(feature = "fake-discord")]
type Stream = fake::Stream;

pub(crate) type TlsSocket = WebSocketStream<Stream>;

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
        let tcp = TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|_| ConnectError)?;
        tcp.set_nodelay(true).map_err(|_| ConnectError)?;
        let (socket, _response) =
            super::client_async_with_config(request, Stream::Plain(tcp), Some(config))
                .await
                .map_err(|_| ConnectError)?;
        Ok(socket)
    }
}

/// Categorical: URLs and TLS errors never reach logs or messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectError;

/// Connects to a `wss://` URL. The `Origin` header is the documented web-client
/// origin; Discord's gateways reject other origins.
pub(crate) async fn connect(
    url: &str,
    origin: &'static str,
    config: WebSocketConfig,
    limit: Duration,
) -> Result<TlsSocket, ConnectError> {
    timeout(limit, connect_inner(url, origin, config))
        .await
        .map_err(|_| ConnectError)?
}

async fn connect_inner(
    url: &str,
    origin: &'static str,
    config: WebSocketConfig,
) -> Result<TlsSocket, ConnectError> {
    let mut request = url.into_client_request().map_err(|_| ConnectError)?;
    #[cfg(feature = "fake-discord")]
    if crate::fake_origin::active() {
        return fake::connect(request, origin, config).await;
    }
    let uri = request.uri().clone();
    if uri.scheme_str() != Some("wss") {
        return Err(ConnectError);
    }
    let host = uri.host().ok_or(ConnectError)?.to_owned();
    let port = uri.port_u16().unwrap_or(443);
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static(origin));
    let tcp = TcpStream::connect((host.as_str(), port))
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
    let (socket, _response) = client_async_with_config(request, stream, Some(config))
        .await
        .map_err(|_| ConnectError)?;
    Ok(socket)
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
                connect(url, "https://discord.com", config, limit)
                    .await
                    .err(),
                Some(ConnectError),
                "{url}"
            );
        }
    }
}
