//! What the connection driver needs from the outside world: discovery, a
//! WebSocket, and the account's authentication-stop signal.
//! The production implementation uses the account's REST client and a rustls
//! WebSocket; tests substitute an in-process server.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{Sink, Stream};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use super::compression::MAX_EVENT_BYTES;

use super::profile::ClientProperties;
use super::url::GatewayUrl;
use crate::ws::{self, ConnectError, TlsSocket};
use crate::{Priority, RestClient, RestError};

/// The documented client origin; Discord's gateways reject other origins.
const ORIGIN: &str = "https://discord.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiscoverError {
    /// `GET /gateway` answered 401: the token is dead.
    AuthenticationRequired,
    /// `GET /gateway` answered 403 (Cloudflare or a ban).
    Forbidden,
    Failed,
}

pub(crate) trait Transport: Send + Sync + 'static {
    type Socket: Stream<Item = Result<Message, WsError>>
        + Sink<Message, Error = WsError>
        + Unpin
        + Send
        + 'static;

    /// Production returns the REST client's ledger, which survives replacement
    /// Gateway tasks. Isolated fake transports may use a fresh test ledger.
    fn retry_ledger(&self) -> Arc<super::RetryLedger> {
        Arc::default()
    }

    fn note_service_reachable(&self) {}

    fn note_session_healthy(&self) {}

    /// `GET /gateway`: the base WebSocket URL.
    fn discover(&self) -> impl Future<Output = Result<String, DiscoverError>> + Send;

    fn connect(
        &self,
        url: &GatewayUrl,
    ) -> impl Future<Output = Result<Self::Socket, ConnectError>> + Send;

    /// Completes when the account's authenticated work has been stopped
    /// elsewhere (REST observed a 401, or explicit logout).
    fn authentication_required(&self) -> impl Future<Output = ()> + Send;

    /// The Gateway rejected the token: stop every authenticated worker.
    fn stop_authenticated_work(&self);
}

pub(crate) fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_EVENT_BYTES))
        .max_frame_size(Some(MAX_EVENT_BYTES))
}

pub(crate) async fn connect_tls(
    network: &fastcord_net::NetworkContext,
    url: &GatewayUrl,
    browser: &ClientProperties,
) -> Result<TlsSocket, ConnectError> {
    ws::connect(
        network,
        fastcord_net::Role::Gateway,
        url.as_str(),
        ORIGIN,
        browser,
        websocket_config(),
        CONNECT_TIMEOUT,
    )
    .await
}

#[derive(Deserialize)]
pub(crate) struct GatewayInfo {
    pub(crate) url: String,
}

pub(crate) struct LiveTransport {
    rest: RestClient,
}

impl LiveTransport {
    pub(crate) fn new(rest: RestClient) -> Self {
        Self { rest }
    }
}

impl Transport for LiveTransport {
    type Socket = TlsSocket;

    fn retry_ledger(&self) -> Arc<super::RetryLedger> {
        self.rest.gateway_retries()
    }

    fn note_service_reachable(&self) {
        self.rest
            .network()
            .controller()
            .note_service_reachable(fastcord_net::Role::Gateway);
    }

    fn note_session_healthy(&self) {
        self.rest
            .network()
            .controller()
            .note_session_healthy(fastcord_net::Role::Gateway);
    }

    async fn discover(&self) -> Result<String, DiscoverError> {
        match self
            .rest
            .get::<GatewayInfo>("/gateway", Priority::UserRead)
            .await
        {
            Ok(info) => Ok(info.url),
            Err(RestError::AuthenticationRequired | RestError::InvalidToken) => {
                Err(DiscoverError::AuthenticationRequired)
            }
            Err(RestError::PermissionDenied) => Err(DiscoverError::Forbidden),
            Err(_) => Err(DiscoverError::Failed),
        }
    }

    async fn connect(&self, url: &GatewayUrl) -> Result<TlsSocket, ConnectError> {
        connect_tls(self.rest.network(), url, &self.rest.identity().properties()).await
    }

    async fn authentication_required(&self) {
        self.rest.authentication_required().await;
    }

    fn stop_authenticated_work(&self) {
        self.rest.stop_authenticated_work();
    }
}
