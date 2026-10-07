//! TLS WebSocket connection to the fixed remote-auth gateway host.

use std::time::Duration;

use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::RemoteAuthError;
use crate::ws::{self, TlsSocket};

const GATEWAY_URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
/// The gateway rejects other origins; this is the documented web-client origin.
const ORIGIN: &str = "https://discord.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Every legitimate frame is a few hundred bytes.
pub(crate) const MAX_MESSAGE_BYTES: usize = 32 * 1024;

pub(crate) type GatewaySocket = TlsSocket;

pub(crate) fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES))
}

pub(crate) async fn connect() -> Result<GatewaySocket, RemoteAuthError> {
    ws::connect(GATEWAY_URL, ORIGIN, websocket_config(), CONNECT_TIMEOUT)
        .await
        .map_err(|_| RemoteAuthError::Connect)
}
