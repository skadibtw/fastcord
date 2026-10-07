//! What the connection driver needs from the outside world: discovery, a
//! WebSocket, the build number, and the account's authentication-stop signal.
//! The production implementation uses the account's REST client and a rustls
//! WebSocket; tests substitute an in-process server.

use std::time::Duration;

use futures_util::{Sink, Stream};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use super::compression::MAX_EVENT_BYTES;
use super::profile::{BuildNumber, resolve_build_number};
use super::url::GatewayUrl;
use crate::ws::{self, ConnectError, TlsSocket};
use crate::{Priority, RestClient, RestError};

/// The documented web-client origin; Discord's gateways reject other origins.
const ORIGIN: &str = "https://discord.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiscoverError {
    /// `GET /gateway` answered 401: the token is dead.
    AuthenticationRequired,
    Failed,
}

pub(crate) trait Transport: Send + Sync + 'static {
    type Socket: Stream<Item = Result<Message, WsError>>
        + Sink<Message, Error = WsError>
        + Unpin
        + Send
        + 'static;

    /// The `client_build_number` for this run's profile.
    fn build_number(&self) -> impl Future<Output = BuildNumber> + Send;

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

pub(crate) async fn connect_tls(url: &GatewayUrl) -> Result<TlsSocket, ConnectError> {
    ws::connect(url.as_str(), ORIGIN, websocket_config(), CONNECT_TIMEOUT).await
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

    async fn build_number(&self) -> BuildNumber {
        resolve_build_number().await
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
            Err(_) => Err(DiscoverError::Failed),
        }
    }

    async fn connect(&self, url: &GatewayUrl) -> Result<TlsSocket, ConnectError> {
        connect_tls(url).await
    }

    async fn authentication_required(&self) {
        self.rest.authentication_required().await;
    }

    fn stop_authenticated_work(&self) {
        self.rest.stop_authenticated_work();
    }
}
