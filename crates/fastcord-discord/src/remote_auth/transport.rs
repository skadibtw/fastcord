//! TLS WebSocket connection to the fixed remote-auth gateway host.
//!
//! TLS uses the same rustls/ring stack and webpki roots as the REST client;
//! `tokio-tungstenite` is used only for WebSocket framing so it cannot pull a
//! second crypto provider into the binary.

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{WebSocketStream, client_async_with_config};

use super::RemoteAuthError;

const GATEWAY_HOST: &str = "remote-auth-gateway.discord.gg";
const GATEWAY_URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
/// The gateway rejects other origins; this is the documented web-client origin.
const ORIGIN: &str = "https://discord.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Every legitimate frame is a few hundred bytes.
pub(crate) const MAX_MESSAGE_BYTES: usize = 32 * 1024;

pub(crate) type GatewaySocket = WebSocketStream<TlsStream<TcpStream>>;

pub(crate) fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES))
}

pub(crate) async fn connect() -> Result<GatewaySocket, RemoteAuthError> {
    timeout(CONNECT_TIMEOUT, connect_inner())
        .await
        .map_err(|_| RemoteAuthError::Connect)?
}

async fn connect_inner() -> Result<GatewaySocket, RemoteAuthError> {
    let tcp = TcpStream::connect((GATEWAY_HOST, 443))
        .await
        .map_err(|_| RemoteAuthError::Connect)?;
    tcp.set_nodelay(true)
        .map_err(|_| RemoteAuthError::Connect)?;
    let server_name = ServerName::try_from(GATEWAY_HOST).map_err(|_| RemoteAuthError::Connect)?;
    let tls = TlsConnector::from(tls_config()?)
        .connect(server_name, tcp)
        .await
        .map_err(|_| RemoteAuthError::Connect)?;
    let mut request = GATEWAY_URL
        .into_client_request()
        .map_err(|_| RemoteAuthError::Connect)?;
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static(ORIGIN));
    let (socket, _response) = client_async_with_config(request, tls, Some(websocket_config()))
        .await
        .map_err(|_| RemoteAuthError::Connect)?;
    Ok(socket)
}

fn tls_config() -> Result<Arc<ClientConfig>, RemoteAuthError> {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| RemoteAuthError::Connect)?
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
}
