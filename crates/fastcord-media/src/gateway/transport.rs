//! What the voice driver needs from the outside world: a secure WebSocket to
//! the voice server and a UDP socket. Production uses rustls (the same
//! ring-based provider and webpki roots as the rest of fastcord) and a Tokio
//! UDP socket; tests substitute in-process fakes.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Sink, Stream};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{WebSocketStream, client_async_with_config};

use super::wire::VERSION;

/// The web-client origin (ADR 0005).
const ORIGIN: &str = "https://discord.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Voice signaling messages are small; DAVE/MLS binary messages (milestone
/// 18) stay far below this.
pub(crate) const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

pub(crate) type TlsSocket = WebSocketStream<TlsStream<TcpStream>>;

/// Categorical: endpoints and TLS details never reach logs or messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectError;

pub(crate) trait Signaling: Send + Sync + 'static {
    type Socket: Stream<Item = Result<Message, WsError>>
        + Sink<Message, Error = WsError>
        + Unpin
        + Send
        + 'static;

    fn connect(&self, url: &str)
    -> impl Future<Output = Result<Self::Socket, ConnectError>> + Send;
}

/// One UDP socket, used only with the voice server's address.
pub(crate) trait UdpLink: Send + Sync + 'static {
    fn send_to(
        &self,
        datagram: &[u8],
        to: SocketAddr,
    ) -> impl Future<Output = io::Result<()>> + Send;

    /// Cancel-safe: dropping the future loses no datagram.
    fn recv_from(
        &self,
        buffer: &mut [u8],
    ) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send;
}

pub(crate) trait Network: Send + Sync + 'static {
    type Udp: UdpLink;

    /// A fresh socket able to reach `server`.
    fn bind(&self, server: SocketAddr) -> impl Future<Output = io::Result<Self::Udp>> + Send;
}

/// `wss://<endpoint>/?v=8` for an endpoint exactly as VOICE_SERVER_UPDATE
/// sends it (`host[:port]`, no scheme). Anything else is refused rather than
/// guessed at.
pub(crate) fn voice_url(endpoint: &str) -> Option<String> {
    let (host, port) = match endpoint.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (endpoint, None),
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let port_ok = port.is_none_or(|port| port.parse::<u16>().is_ok_and(|port| port != 0));
    (host_ok && port_ok).then(|| format!("wss://{endpoint}/?v={VERSION}"))
}

pub(crate) fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES))
}

pub(crate) struct LiveSignaling;

impl Signaling for LiveSignaling {
    type Socket = TlsSocket;

    async fn connect(&self, url: &str) -> Result<TlsSocket, ConnectError> {
        timeout(CONNECT_TIMEOUT, connect_tls(url))
            .await
            .map_err(|_| ConnectError)?
    }
}

async fn connect_tls(url: &str) -> Result<TlsSocket, ConnectError> {
    let mut request = url.into_client_request().map_err(|_| ConnectError)?;
    let uri = request.uri().clone();
    if uri.scheme_str() != Some("wss") {
        return Err(ConnectError);
    }
    let host = uri.host().ok_or(ConnectError)?.to_owned();
    let port = uri.port_u16().unwrap_or(443);
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static(ORIGIN));
    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|_| ConnectError)?;
    tcp.set_nodelay(true).map_err(|_| ConnectError)?;
    let server_name = ServerName::try_from(host).map_err(|_| ConnectError)?;
    let tls = TlsConnector::from(tls_config()?)
        .connect(server_name, tcp)
        .await
        .map_err(|_| ConnectError)?;
    let (socket, _response) = client_async_with_config(request, tls, Some(websocket_config()))
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

pub(crate) struct LiveNetwork;

impl Network for LiveNetwork {
    type Udp = UdpSocket;

    async fn bind(&self, server: SocketAddr) -> io::Result<UdpSocket> {
        let local: SocketAddr = if server.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        };
        UdpSocket::bind(local).await
    }
}

impl UdpLink for UdpSocket {
    async fn send_to(&self, datagram: &[u8], to: SocketAddr) -> io::Result<()> {
        let sent = UdpSocket::send_to(self, datagram, to).await?;
        if sent == datagram.len() {
            Ok(())
        } else {
            Err(io::ErrorKind::WriteZero.into())
        }
    }

    async fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        UdpSocket::recv_from(self, buffer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_urls_are_built_only_from_plain_endpoints() {
        assert_eq!(
            voice_url("c-ams08-1a2b3c4d.discord.media:443").as_deref(),
            Some("wss://c-ams08-1a2b3c4d.discord.media:443/?v=8")
        );
        assert_eq!(
            voice_url("smart.loyal.discord.media").as_deref(),
            Some("wss://smart.loyal.discord.media/?v=8")
        );
        for bad in [
            "",
            ":443",
            "wss://host",
            "host/path",
            "host:0",
            "host:65536",
            "host:abc",
            "user@host",
            "host?x=1",
            "ho st",
        ] {
            assert_eq!(voice_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn tls_configuration_builds_with_the_bundled_roots() {
        assert!(tls_config().is_ok());
    }

    #[tokio::test]
    async fn live_signaling_refuses_plaintext_urls() {
        assert_eq!(
            LiveSignaling.connect("ws://127.0.0.1:9/?v=8").await.err(),
            Some(ConnectError)
        );
    }

    #[tokio::test]
    async fn live_udp_sockets_exchange_datagrams() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = server.local_addr().unwrap();
        let client = LiveNetwork.bind(address).await.unwrap();
        UdpLink::send_to(&client, b"ping", address).await.unwrap();
        let mut buffer = [0; 16];
        let (len, from) = UdpSocket::recv_from(&server, &mut buffer).await.unwrap();
        assert_eq!(&buffer[..len], b"ping");
        UdpSocket::send_to(&server, b"pong", from).await.unwrap();
        let (len, source) = UdpLink::recv_from(&client, &mut buffer).await.unwrap();
        assert_eq!(&buffer[..len], b"pong");
        assert_eq!(source, address);
    }
}
