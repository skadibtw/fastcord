//! Shared application-scoped network policy. This is an architectural boundary,
//! not an operating-system sandbox. It never changes system networking.

mod bridge;
mod broker;
mod http;
pub mod selector;
pub mod settings;
pub mod tls;

pub use broker::{BoxedIo, NetworkContext, SocketBroker, TlsStream};
pub use http::{HttpBuilder, HttpClient, HttpError, HttpRequestBuilder};
use serde::{Deserialize, Serialize};

/// Authorization and TLS profile are attached to a feature, never to a token,
/// URL path, query, account or request body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    Rest,
    Gateway,
    RemoteAuth,
    Voice,
    Stream,
    Login,
    Profile,
    Cdn,
    Upload,
}

impl Role {
    pub(crate) const ALL: [Self; 9] = [
        Self::Rest,
        Self::Gateway,
        Self::RemoteAuth,
        Self::Voice,
        Self::Stream,
        Self::Login,
        Self::Profile,
        Self::Cdn,
        Self::Upload,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Rest => "rest",
            Self::Gateway => "gateway",
            Self::RemoteAuth => "remote-auth",
            Self::Voice => "voice",
            Self::Stream => "stream",
            Self::Login => "login",
            Self::Profile => "profile",
            Self::Cdn => "cdn",
            Self::Upload => "upload",
        }
    }
}

/// No raw library error or target can be formatted through this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkError {
    InvalidTarget,
    Dns,
    NoRoute,
    TcpTimeout,
    TcpReset,
    TlsTimeout,
    TlsReset,
    CertificateRejected,
    TlsProtocolRejected,
    WebSocketUpgrade,
    ProxyRefused,
    BudgetExhausted,
    Unsupported,
    BridgeUnavailable,
    Cancelled,
}

impl NetworkError {
    pub const fn eligible_for_selection(self) -> bool {
        matches!(
            self,
            Self::TcpTimeout | Self::TcpReset | Self::TlsTimeout | Self::TlsReset
        )
    }
}

impl std::fmt::Display for NetworkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidTarget => "The network destination was refused.",
            Self::Dns => "The service name could not be resolved.",
            Self::NoRoute => "There is no route to the service.",
            Self::TcpTimeout => "The TCP connection timed out.",
            Self::TcpReset => "The TCP connection was reset.",
            Self::TlsTimeout => "The TLS handshake timed out.",
            Self::TlsReset => "The TLS connection was reset during its handshake.",
            Self::CertificateRejected => "The service certificate was rejected.",
            Self::TlsProtocolRejected => "The TLS handshake was rejected.",
            Self::WebSocketUpgrade => "The WebSocket upgrade failed.",
            Self::ProxyRefused => "The configured proxy refused the connection.",
            Self::BudgetExhausted => "The network recovery budget is exhausted.",
            Self::Unsupported => "This network configuration is unsupported.",
            Self::BridgeUnavailable => "The private network bridge is unavailable.",
            Self::Cancelled => "Network recovery was cancelled.",
        })
    }
}

impl std::error::Error for NetworkError {}
