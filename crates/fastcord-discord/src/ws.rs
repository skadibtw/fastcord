//! WebSocket framing over the application-scoped socket broker.
use fastcord_net::{BoxedIo, NetworkContext, Role};
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{WebSocketStream, client_async_with_config};

pub(crate) type TlsSocket = WebSocketStream<BoxedIo>;
/// Categorical; no URLs, credentials or raw TLS errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnectError;

pub(crate) async fn connect(
    network: &NetworkContext,
    role: Role,
    url: &str,
    origin: &'static str,
    config: WebSocketConfig,
    limit: Duration,
) -> Result<TlsSocket, ConnectError> {
    connect_inner(network, role, url, origin, config, limit).await
}

async fn connect_inner(
    network: &NetworkContext,
    role: Role,
    url: &str,
    origin: &'static str,
    config: WebSocketConfig,
    limit: Duration,
) -> Result<TlsSocket, ConnectError> {
    let mut request = url.into_client_request().map_err(|_| ConnectError)?;
    let uri = request.uri().clone();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static(origin));
    #[cfg(feature = "fake-discord")]
    if crate::fake_origin::active() {
        let port = crate::fake_origin::port().ok_or(ConnectError)?;
        if uri.scheme_str() != Some("ws")
            || uri.host() != Some("127.0.0.1")
            || uri.port_u16() != Some(port)
        {
            return Err(ConnectError);
        }
        let tcp = network
            .connect_loopback(port)
            .await
            .map_err(|_| ConnectError)?;
        let (socket, _) = timeout(
            limit,
            client_async_with_config(request, Box::new(tcp) as BoxedIo, Some(config)),
        )
        .await
        .map_err(|_| ConnectError)?
        .map_err(|_| ConnectError)?;
        return Ok(socket);
    }
    if uri.scheme_str() != Some("wss") {
        return Err(ConnectError);
    }
    let host = uri.host().ok_or(ConnectError)?;
    let tls = network
        .connect_tls(host, uri.port_u16().unwrap_or(443), role)
        .await
        .map_err(|_| ConnectError)?;
    let (socket, _) = timeout(
        limit,
        client_async_with_config(request, Box::new(tls) as BoxedIo, Some(config)),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn malformed_or_plaintext_urls_are_refused() {
        let network = NetworkContext::off();
        for url in ["ws://gateway.discord.gg", "https://discord.com", "invalid"] {
            assert_eq!(
                connect(
                    &network,
                    Role::Gateway,
                    url,
                    "https://discord.com",
                    WebSocketConfig::default(),
                    Duration::from_secs(1)
                )
                .await
                .err(),
                Some(ConnectError)
            );
        }
    }
}
