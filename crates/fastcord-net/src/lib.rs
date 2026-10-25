//! Shared application-scoped network policy. This is an architectural boundary,
//! not an operating-system sandbox. It never changes system networking.

mod bridge;
mod broker;
mod connect;
mod dial;
mod http;
pub mod proxy;
pub mod selector;
pub mod settings;
mod socket;
pub mod tls;
pub mod udp;

pub use broker::{BoxedIo, NetworkContext, SocketBroker, TlsStream};
pub use http::{HttpBuilder, HttpClient, HttpError, HttpRequestBuilder};
pub use proxy::{ProxyConfig, ProxyKind};
use serde::{Deserialize, Serialize};
pub use udp::DatagramSocket;

/// Shared public-origin policy, also checked by the optional native helper.
pub fn is_public_destination(address: std::net::IpAddr) -> bool {
    broker::public_ip(address)
}

/// Explicit reservations for the new subsystem. Kernel socket buffers and
/// existing HTTP/TLS/feature-media allocations require separate measurements.
pub const MANAGED_BUFFER_BUDGET_BYTES: usize = 4 * 1024 * 1024;
pub const MANAGED_BUFFER_RESERVATION_BYTES: usize = bridge::TUNNEL_LIMIT * (2 * bridge::FORWARD_BUFFER + bridge::HEADER_LIMIT)
    + 4 * tls::MAX_STAGING_BYTES
    + udp::MAX_ASSOCIATIONS * 65_536
    // Bounded settings input/serialization and conservative room for control
    // tasks, the 32 connector receipts/active endpoint and remaining small scratch buffers.
    + 2 * settings::MAX_SETTINGS_BYTES
    // One helper capture, four bounded public-hello RPCs and pipe buffers.
    + 192 * 1024
    + 256 * 1024;
const _: () = assert!(MANAGED_BUFFER_RESERVATION_BYTES <= MANAGED_BUFFER_BUDGET_BYTES);

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
    ProxyConfiguration,
    ProxyDns,
    ProxyTimeout,
    ProxyCertificateRejected,
    ProxyAuthentication,
    ProxyProtocol,
    ProxyUdpClosed,
    ProxyUdpUnavailable,
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
            Self::ProxyConfiguration => "The proxy configuration is invalid or incomplete.",
            Self::ProxyDns => "The proxy name could not be resolved.",
            Self::ProxyTimeout => "The connection to the proxy timed out.",
            Self::ProxyCertificateRejected => "The HTTPS proxy certificate was rejected.",
            Self::ProxyAuthentication => "The proxy rejected its login details.",
            Self::ProxyProtocol => "The proxy returned an invalid response.",
            Self::ProxyUdpClosed => "The SOCKS5 UDP relay control connection closed.",
            Self::ProxyUdpUnavailable => {
                "The configured SOCKS5 proxy could not establish a UDP relay."
            }
            Self::BudgetExhausted => "The network recovery budget is exhausted.",
            Self::Unsupported => "This network configuration is unsupported.",
            Self::BridgeUnavailable => "The private network bridge is unavailable.",
            Self::Cancelled => "Network recovery was cancelled.",
        })
    }
}

impl std::error::Error for NetworkError {}
