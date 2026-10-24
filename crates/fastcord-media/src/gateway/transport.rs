//! What the voice driver needs from the outside world: a secure WebSocket to
//! the voice server and a UDP socket. Production uses rustls (the same
//! ring-based provider and webpki roots as the rest of fastcord) and a Tokio
//! UDP socket; tests substitute in-process fakes.

use std::io;
use std::net::SocketAddr;

use std::time::Duration;

use futures_util::{Sink, Stream};
use tokio::net::UdpSocket;
use tokio::time::timeout;

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

pub(crate) type TlsSocket = WebSocketStream<fastcord_net::BoxedIo>;

/// Categorical: endpoints and TLS details never reach logs or messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectError;

pub(crate) trait Signaling: Send + Sync + 'static {
    fn note_status(&self, _status: super::VoiceStatus) {}
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

pub(crate) struct LiveSignaling(
    pub(crate) fastcord_net::NetworkContext,
    pub(crate) fastcord_net::Role,
);

impl Signaling for LiveSignaling {
    type Socket = TlsSocket;

    fn note_status(&self, status: super::VoiceStatus) {
        match status {
            super::VoiceStatus::Identifying => self.0.controller().note_service_reachable(self.1),
            super::VoiceStatus::Connected { .. } => {
                self.0.controller().note_session_healthy(self.1)
            }
            super::VoiceStatus::Closed(super::CloseReason::UdpUnreachable) => {
                self.0.controller().note_udp_unreachable(self.1)
            }
            _ => {}
        }
    }

    async fn connect(&self, url: &str) -> Result<TlsSocket, ConnectError> {
        connect_tls(&self.0, self.1, url).await
    }
}

async fn connect_tls(
    network: &fastcord_net::NetworkContext,
    role: fastcord_net::Role,
    url: &str,
) -> Result<TlsSocket, ConnectError> {
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
    let tls = network
        .connect_tls(&host, port, role)
        .await
        .map_err(|_| ConnectError)?;
    let (socket, _response) = timeout(
        CONNECT_TIMEOUT,
        client_async_with_config(
            request,
            Box::new(tls) as fastcord_net::BoxedIo,
            Some(websocket_config()),
        ),
    )
    .await
    .map_err(|_| {
        network
            .controller()
            .note_failure(role, fastcord_net::NetworkError::WebSocketUpgrade);
        ConnectError
    })?
    .map_err(|_| {
        network
            .controller()
            .note_failure(role, fastcord_net::NetworkError::WebSocketUpgrade);
        ConnectError
    })?;
    Ok(socket)
}

pub(crate) struct LiveNetwork(pub(crate) fastcord_net::NetworkContext);
impl Network for LiveNetwork {
    type Udp = UdpSocket;
    async fn bind(&self, server: SocketAddr) -> io::Result<UdpSocket> {
        self.0.bind_udp(server).await
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

    #[tokio::test]
    async fn live_signaling_refuses_plaintext_urls() {
        assert_eq!(
            LiveSignaling(
                fastcord_net::NetworkContext::off(),
                fastcord_net::Role::Voice
            )
            .connect("ws://127.0.0.1:9/?v=8")
            .await
            .err(),
            Some(ConnectError)
        );
    }

    #[tokio::test]
    async fn live_udp_sockets_exchange_datagrams() {
        let server = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = server.local_addr().unwrap();
        let client = fastcord_net::NetworkContext::off()
            .debug_bind_udp(address)
            .await
            .unwrap();
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
