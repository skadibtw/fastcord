use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UdpSocket, lookup_host};
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto, pki_types::ServerName};

use crate::bridge::Bridge;
use crate::selector::{
    AddressFamily, Controller, DiagnosticTarget, ProbeOutcome, ProbeRequest, TlsProfile,
};
use crate::settings::Mode;
use crate::tls::{ShapedStream, Strategy};
use crate::{HttpBuilder, NetworkError, Role};

pub trait NetworkIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> NetworkIo for T {}
pub type BoxedIo = Box<dyn NetworkIo>;
pub type TlsStream = tokio_rustls::client::TlsStream<ShapedStream<BoxedIo>>;

struct Shared {
    mode: Mode,
    controller: Controller,
    bridge: Mutex<Option<Bridge>>,
    transforms: Arc<Semaphore>,
    tunnels: Arc<Semaphore>,
    pending: Arc<Mutex<Option<Demand>>>,
    connected: Arc<Mutex<VecDeque<Connected>>>,
    connection_epoch: Arc<AtomicU64>,
    recovery_gate: Arc<tokio::sync::Mutex<()>>,
}

struct Demand {
    host: String,
    port: u16,
    role: Role,
    profile: TlsProfile,
    failure: NetworkError,
    address: Option<SocketAddr>,
    generation: u64,
}

struct Connected {
    host: String,
    port: u16,
    role: Role,
    address: SocketAddr,
    generation: u64,
    epoch: u64,
}

/// A transport task reports failure without retaining its owning bridge.
#[derive(Clone)]
pub(crate) struct FailureReporter {
    controller: Controller,
    pending: Arc<Mutex<Option<Demand>>>,
    connected: Arc<Mutex<VecDeque<Connected>>>,
    connection_epoch: Arc<AtomicU64>,
    enabled: bool,
}

impl FailureReporter {
    pub(crate) fn connected(&self, host: &str, port: u16, role: Role, address: SocketAddr) {
        if host.len() > 253 {
            return;
        }
        let mut connected = self.connected.lock().unwrap_or_else(|p| p.into_inner());
        connected.retain(|entry| {
            !(entry.host.eq_ignore_ascii_case(host) && entry.port == port && entry.role == role)
        });
        if connected.len() == 16 {
            connected.pop_front();
        }
        let epoch = self
            .connection_epoch
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        connected.push_back(Connected {
            host: host.to_owned(),
            port,
            role,
            address,
            generation: self.controller.generation(),
            epoch,
        });
    }
    pub(crate) fn record(
        &self,
        host: &str,
        port: u16,
        role: Role,
        profile: TlsProfile,
        error: NetworkError,
        address: Option<SocketAddr>,
    ) {
        self.controller.note_failure(role, error);
        if !self.enabled
            || !error.eligible_for_selection()
            || !probe_authorized(role, host)
            || host.len() > 253
            || address.is_none()
        {
            return;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.is_none() {
            *pending = Some(Demand {
                host: host.to_owned(),
                port,
                role,
                profile,
                failure: error,
                address,
                generation: self.controller.generation(),
            });
        }
    }
}

/// Owned once by the application and cloned into production clients. Changing
/// configuration constructs a new transport view but retains diagnostic budgets.
#[derive(Clone)]
pub struct NetworkContext(Arc<Shared>);

impl std::fmt::Debug for NetworkContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkContext")
            .field("mode", &self.0.mode)
            .finish_non_exhaustive()
    }
}

impl Default for NetworkContext {
    fn default() -> Self {
        Self::off()
    }
}

impl NetworkContext {
    pub fn off() -> Self {
        Self::new(Mode::Off)
    }

    pub fn new(mode: Mode) -> Self {
        Self(Arc::new(Shared {
            mode,
            controller: Controller::default(),
            bridge: Mutex::new(None),
            transforms: Arc::new(Semaphore::new(4)),
            tunnels: Arc::new(Semaphore::new(crate::bridge::TUNNEL_LIMIT)),
            pending: Arc::new(Mutex::new(None)),
            connected: Arc::new(Mutex::new(VecDeque::new())),
            connection_epoch: Arc::new(AtomicU64::new(0)),
            recovery_gate: Arc::new(tokio::sync::Mutex::new(())),
        }))
    }

    pub fn with_mode(&self, mode: Mode) -> Self {
        Self(Arc::new(Shared {
            mode,
            controller: self.0.controller.clone(),
            bridge: Mutex::new(None),
            transforms: self.0.transforms.clone(),
            tunnels: self.0.tunnels.clone(),
            pending: self.0.pending.clone(),
            connected: self.0.connected.clone(),
            connection_epoch: self.0.connection_epoch.clone(),
            recovery_gate: self.0.recovery_gate.clone(),
        }))
    }

    pub fn mode(&self) -> Mode {
        self.0.mode
    }
    pub fn controller(&self) -> &Controller {
        &self.0.controller
    }
    pub fn broker(&self) -> SocketBroker {
        SocketBroker(self.clone())
    }
    pub(crate) fn transforms(&self) -> Arc<Semaphore> {
        self.0.transforms.clone()
    }
    pub(crate) fn tunnel_slots(&self) -> Arc<Semaphore> {
        self.0.tunnels.clone()
    }
    pub(crate) fn failure_reporter(&self) -> FailureReporter {
        FailureReporter {
            controller: self.0.controller.clone(),
            pending: self.0.pending.clone(),
            connected: self.0.connected.clone(),
            connection_epoch: self.0.connection_epoch.clone(),
            enabled: self.mode() == Mode::Auto,
        }
    }

    pub fn http_builder(&self, role: Role) -> Result<HttpBuilder, NetworkError> {
        HttpBuilder::new(self.clone(), role, false)
    }

    /// Isolated test origins only. Release callers cannot request plaintext or
    /// private origins through this API.
    #[cfg(debug_assertions)]
    pub fn debug_loopback_http_builder(&self, role: Role) -> Result<HttpBuilder, NetworkError> {
        HttpBuilder::new(self.clone(), role, true)
    }

    pub async fn connect_tls(
        &self,
        host: &str,
        port: u16,
        role: Role,
    ) -> Result<TlsStream, NetworkError> {
        self.broker().connect_tls(host, port, role).await
    }

    #[cfg(debug_assertions)]
    pub async fn connect_loopback(&self, port: u16) -> Result<TcpStream, NetworkError> {
        timeout(
            Duration::from_secs(3),
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
        )
        .await
        .map_err(|_| NetworkError::TcpTimeout)?
        .map_err(|error| tcp_error(&error))
    }

    pub async fn bind_udp(&self, server: SocketAddr) -> io::Result<UdpSocket> {
        self.broker().bind_udp(server).await
    }

    #[cfg(debug_assertions)]
    pub async fn debug_bind_udp(&self, server: SocketAddr) -> io::Result<UdpSocket> {
        if !server.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                NetworkError::InvalidTarget,
            ));
        }
        let local: SocketAddr = if server.is_ipv4() {
            (Ipv4Addr::LOCALHOST, 0).into()
        } else {
            (Ipv6Addr::LOCALHOST, 0).into()
        };
        UdpSocket::bind(local).await
    }

    pub(crate) fn bridge_proxy(&self, role: Role) -> Result<reqwest::Proxy, NetworkError> {
        let mut bridge = self
            .0
            .bridge
            .lock()
            .map_err(|_| NetworkError::BridgeUnavailable)?;
        if bridge.is_none() {
            *bridge = Some(Bridge::start(self.clone())?);
        }
        bridge
            .as_ref()
            .ok_or(NetworkError::BridgeUnavailable)?
            .proxy(role)
    }

    pub(crate) fn record_failure(
        &self,
        host: &str,
        port: u16,
        role: Role,
        profile: TlsProfile,
        error: NetworkError,
        address: Option<SocketAddr>,
    ) {
        self.failure_reporter()
            .record(host, port, role, profile, error, address);
    }

    pub(crate) fn pending_failure(
        &self,
        host: &str,
        port: u16,
        role: Role,
    ) -> Option<NetworkError> {
        self.0
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .filter(|pending| {
                pending.host.eq_ignore_ascii_case(host)
                    && pending.port == port
                    && pending.role == role
            })
            .map(|pending| pending.failure)
    }

    pub(crate) fn connection_epoch(&self) -> u64 {
        self.0.connection_epoch.load(Ordering::Relaxed)
    }
    pub(crate) fn connected_address(
        &self,
        host: &str,
        port: u16,
        role: Role,
        after_epoch: u64,
    ) -> Option<SocketAddr> {
        self.0
            .connected
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .rev()
            .find(|entry| {
                entry.host.eq_ignore_ascii_case(host)
                    && entry.port == port
                    && entry.role == role
                    && entry.generation == self.controller().generation()
                    && entry.epoch > after_epoch
            })
            .map(|entry| entry.address)
    }

    /// Drives a pending failure before the next production dial. The first
    /// failed attempt returns immediately so its owner records the failure and
    /// applies its normal backoff before entering this gate. No request replay.
    pub async fn wait_for_recovery(&self) {
        if self.mode() != Mode::Auto {
            return;
        }
        let _gate = self.0.recovery_gate.lock().await;
        self.0.controller.wait_for_idle().await;
        let pending = self
            .0
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let Some(pending) = pending else {
            return;
        };
        if pending.generation != self.controller().generation() {
            return;
        }
        // A concrete IP from the failed production flow is mandatory. No DNS
        // lookup is performed outside the campaign's clock/dial budget.
        let Some(address) = pending.address else {
            return;
        };
        let family = if address.is_ipv4() {
            AddressFamily::V4
        } else {
            AddressFamily::V6
        };
        let Ok(target) = DiagnosticTarget::new(
            pending.host,
            pending.port,
            pending.role,
            family,
            pending.profile,
        ) else {
            return;
        };
        let transforms = self.0.transforms.clone();
        let _ = self
            .0
            .controller
            .diagnose(target, pending.failure, move |request| {
                let transforms = transforms.clone();
                async move { run_probe(address, request, transforms).await }
            })
            .await;
    }

    pub(crate) fn strategy(
        &self,
        host: &str,
        port: u16,
        role: Role,
        address: SocketAddr,
        profile: TlsProfile,
    ) -> Strategy {
        if self.mode() != Mode::Auto || !probe_authorized(role, host) {
            return Strategy::Direct;
        }
        let family = if address.is_ipv4() {
            AddressFamily::V4
        } else {
            AddressFamily::V6
        };
        DiagnosticTarget::new(host, port, role, family, profile)
            .ok()
            .and_then(|target| self.0.controller.strategy_for(&target))
            .unwrap_or(Strategy::Direct)
    }
}

#[derive(Clone)]
pub struct SocketBroker(NetworkContext);

impl SocketBroker {
    pub async fn connect_tls(
        &self,
        host: &str,
        port: u16,
        role: Role,
    ) -> Result<TlsStream, NetworkError> {
        authorize(role, host, port)?;
        if self.0.mode() == Mode::Proxy {
            return Err(NetworkError::ProxyRefused);
        }
        self.0.wait_for_recovery().await;
        let mut failed_address = None;
        let result = self
            .connect_tls_once(host, port, role, &mut failed_address)
            .await;
        if let Err(error) = result.as_ref() {
            self.0.record_failure(
                host,
                port,
                role,
                TlsProfile::WebSocket,
                *error,
                failed_address,
            );
        }
        // A transport change never replays the user's request or the Gateway
        // attempt. Its owner retains failure accounting and nonzero backoff.
        result
    }

    async fn connect_tls_once(
        &self,
        host: &str,
        port: u16,
        role: Role,
        failed_address: &mut Option<SocketAddr>,
    ) -> Result<TlsStream, NetworkError> {
        let (tcp, address) = dial_public(host, port, failed_address).await?;
        *failed_address = Some(address);
        let strategy = self
            .0
            .strategy(host, port, role, address, TlsProfile::WebSocket);
        let stream =
            ShapedStream::with_budget(Box::new(tcp) as BoxedIo, strategy, self.0.transforms());
        let config = tls_config(TlsProfile::WebSocket, false)?;
        let name =
            ServerName::try_from(host.to_owned()).map_err(|_| NetworkError::InvalidTarget)?;
        timeout(
            Duration::from_secs(4),
            TlsConnector::from(config).connect(name, stream),
        )
        .await
        .map_err(|_| NetworkError::TlsTimeout)?
        .map_err(|error| tls_error(&error))
    }

    pub async fn bind_udp(&self, server: SocketAddr) -> io::Result<UdpSocket> {
        if self.0.mode() == Mode::Proxy {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "The configured proxy does not provide UDP media.",
            ));
        }
        if !public_ip(server.ip()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                NetworkError::InvalidTarget,
            ));
        }
        let local: SocketAddr = if server.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        };
        UdpSocket::bind(local).await
    }
}

pub(crate) fn tls_config(
    profile: TlsProfile,
    probe: bool,
) -> Result<Arc<ClientConfig>, NetworkError> {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| NetworkError::TlsProtocolRejected)?
            .with_root_certificates(roots)
            .with_no_client_auth();
    if profile == TlsProfile::Http {
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    }
    if probe {
        config.resumption = tokio_rustls::rustls::client::Resumption::disabled();
        config.enable_early_data = false;
    }
    Ok(Arc::new(config))
}

pub(crate) fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.ends_with('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn domain(host: &str, suffix: &str) -> bool {
    host == suffix
        || host
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

pub(crate) fn authorize(role: Role, host: &str, port: u16) -> Result<(), NetworkError> {
    if port == 0 || !(valid_host(host) || host.parse::<IpAddr>().is_ok()) {
        return Err(NetworkError::InvalidTarget);
    }
    let host = host.to_ascii_lowercase();
    let allowed = match role {
        Role::Rest | Role::Login | Role::Profile => domain(&host, "discord.com"),
        Role::Gateway => domain(&host, "discord.gg"),
        Role::RemoteAuth => host == "remote-auth-gateway.discord.gg" || host == "discord.com",
        Role::Voice | Role::Stream => domain(&host, "discord.media"),
        Role::Cdn | Role::Upload => true, // Feature still validates every URL/hop.
    };
    if !allowed {
        return Err(NetworkError::InvalidTarget);
    }
    if let Ok(ip) = host.parse()
        && !public_ip(ip)
    {
        return Err(NetworkError::InvalidTarget);
    }
    Ok(())
}

pub(crate) fn probe_authorized(role: Role, host: &str) -> bool {
    // An allowed upload or external embed does not authorize probing arbitrary
    // storage hosts. Only the exact failed Discord service is probed.
    matches!(
        role,
        Role::Rest
            | Role::Login
            | Role::Profile
            | Role::Gateway
            | Role::RemoteAuth
            | Role::Voice
            | Role::Stream
    ) || ["discord.com", "discordapp.com", "discordapp.net"]
        .iter()
        .any(|suffix| domain(host, suffix))
}

pub(crate) fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_broadcast()
                && !ip.is_multicast()
                && !ip.is_unspecified()
                && a != 0
                && a < 224
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 192 && b == 0 && (c == 0 || c == 2))
                && !(a == 192 && b == 88 && c == 99)
                && !(a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                && !(a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            let segments = ip.segments();
            (segments[0] & 0xe000) == 0x2000
                && !(segments[0] == 0x2001 && (segments[1] <= 0x1ff || segments[1] == 0xdb8))
                && segments[0] != 0x2002
        }
    }
}

pub(crate) async fn resolve_public(host: &str, port: u16) -> Result<Vec<SocketAddr>, NetworkError> {
    let addresses = timeout(Duration::from_secs(3), lookup_host((host, port)))
        .await
        .map_err(|_| NetworkError::Dns)?
        .map_err(|_| NetworkError::Dns)?;
    let mut bounded = Vec::with_capacity(16);
    for address in addresses {
        // Reject mixed private/public answers as well as a private-only name.
        if !public_ip(address.ip()) {
            return Err(NetworkError::InvalidTarget);
        }
        if bounded.len() < 16 && !bounded.contains(&address) {
            bounded.push(address);
        }
    }
    if bounded.is_empty() {
        return Err(NetworkError::Dns);
    }
    Ok(bounded)
}

pub(crate) async fn dial_public(
    host: &str,
    port: u16,
    failed_address: &mut Option<SocketAddr>,
) -> Result<(TcpStream, SocketAddr), NetworkError> {
    let addresses = resolve_public(host, port).await?;
    let mut last = NetworkError::NoRoute;
    for address in addresses.iter().take(2) {
        *failed_address = Some(*address);
        match timeout(Duration::from_secs(3), TcpStream::connect(address)).await {
            Ok(Ok(stream)) => {
                stream
                    .set_nodelay(true)
                    .map_err(|_| NetworkError::NoRoute)?;
                return Ok((stream, *address));
            }
            Ok(Err(error)) => last = tcp_error(&error),
            Err(_) => last = NetworkError::TcpTimeout,
        }
    }
    Err(last)
}

pub(crate) fn tcp_error(error: &io::Error) -> NetworkError {
    match error.kind() {
        io::ErrorKind::TimedOut => NetworkError::TcpTimeout,
        io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::UnexpectedEof => NetworkError::TcpReset,
        _ => NetworkError::NoRoute,
    }
}

pub(crate) fn tls_error(error: &io::Error) -> NetworkError {
    if let Some(category) = error
        .get_ref()
        .and_then(|error| error.downcast_ref::<NetworkError>())
    {
        return *category;
    }
    if let Some(tls) = error
        .get_ref()
        .and_then(|error| error.downcast_ref::<tokio_rustls::rustls::Error>())
    {
        return match tls {
            tokio_rustls::rustls::Error::InvalidCertificate(_)
            | tokio_rustls::rustls::Error::NoCertificatesPresented => {
                NetworkError::CertificateRejected
            }
            _ => NetworkError::TlsProtocolRejected,
        };
    }
    match tcp_error(error) {
        NetworkError::TcpTimeout => NetworkError::TlsTimeout,
        NetworkError::TcpReset => NetworkError::TlsReset,
        _ => NetworkError::TlsProtocolRejected,
    }
}

async fn run_probe(
    address: SocketAddr,
    request: ProbeRequest,
    transforms: Arc<Semaphore>,
) -> ProbeOutcome {
    // Count all TLS bytes, including retransmitted records, but never keep
    // their contents. The wrapper fails closed on either byte limit.
    let counters = Arc::new(Mutex::new((0usize, 0usize)));
    let inner = async {
        let stream = timeout(request.limits.tcp_timeout, TcpStream::connect(address))
            .await
            .map_err(|_| NetworkError::TcpTimeout)?
            .map_err(|error| tcp_error(&error))?;
        stream
            .set_nodelay(true)
            .map_err(|_| NetworkError::NoRoute)?;
        let bounded = CountedIo {
            stream,
            counters: counters.clone(),
            inbound: request.limits.max_inbound_bytes,
            total: request.limits.max_network_bytes,
        };
        let stream = ShapedStream::with_budget(bounded, request.strategy, transforms);
        let name = ServerName::try_from(request.target.host.clone())
            .map_err(|_| NetworkError::InvalidTarget)?;
        let config = tls_config(request.target.profile, true)?;
        let _tls = timeout(
            request.limits.tls_timeout,
            TlsConnector::from(config).connect(name, stream),
        )
        .await
        .map_err(|_| NetworkError::TlsTimeout)?
        .map_err(|error| tls_error(&error))?;
        Ok(())
    };
    let result = timeout(request.limits.total_timeout, inner)
        .await
        .unwrap_or(Err(NetworkError::TlsTimeout));
    let (sent_bytes, received_bytes) = *counters
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ProbeOutcome {
        result,
        sent_bytes,
        received_bytes,
    }
}

struct CountedIo {
    stream: TcpStream,
    counters: Arc<Mutex<(usize, usize)>>,
    inbound: usize,
    total: usize,
}

impl AsyncRead for CountedIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let (sent, received) = *self.counters.lock().unwrap_or_else(|p| p.into_inner());
        let remaining = self
            .inbound
            .saturating_sub(received)
            .min(self.total.saturating_sub(sent + received));
        if remaining == 0 {
            return std::task::Poll::Ready(Err(io::Error::other(NetworkError::BudgetExhausted)));
        }
        let length = remaining.min(buf.remaining());
        let mut limited = tokio::io::ReadBuf::new(buf.initialize_unfilled_to(length));
        let result = std::pin::Pin::new(&mut self.stream).poll_read(cx, &mut limited);
        let len = limited.filled().len();
        if let std::task::Poll::Ready(Ok(())) = result {
            buf.advance(len);
            self.counters.lock().unwrap_or_else(|p| p.into_inner()).1 += len;
        }
        result
    }
}

impl AsyncWrite for CountedIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let (sent, received) = *self.counters.lock().unwrap_or_else(|p| p.into_inner());
        let remaining = self.total.saturating_sub(sent + received);
        if remaining == 0 {
            return std::task::Poll::Ready(Err(io::Error::other(NetworkError::BudgetExhausted)));
        }
        let result =
            std::pin::Pin::new(&mut self.stream).poll_write(cx, &buf[..buf.len().min(remaining)]);
        if let std::task::Poll::Ready(Ok(len)) = result {
            self.counters.lock().unwrap_or_else(|p| p.into_inner()).0 += len;
        }
        result
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_address_policy_covers_mapped_and_special_ranges() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
        ] {
            assert!(!public_ip(address.parse().unwrap()), "{address}");
        }
        for address in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_ip(address.parse().unwrap()));
        }
    }

    #[tokio::test]
    async fn transport_views_share_limits_and_stale_demands_do_not_probe() {
        let context = NetworkContext::new(Mode::Auto);
        let other = context.with_mode(Mode::Off).with_mode(Mode::Auto);
        assert!(Arc::ptr_eq(&context.0.transforms, &other.0.transforms));
        assert!(Arc::ptr_eq(&context.0.tunnels, &other.0.tunnels));
        for _ in 0..128 {
            context.record_failure(
                "gateway.discord.gg",
                443,
                Role::Gateway,
                TlsProfile::WebSocket,
                NetworkError::TlsReset,
                Some("1.1.1.1:443".parse().unwrap()),
            );
        }
        assert!(context.0.pending.lock().unwrap().is_some());
        context.controller().network_changed();
        other.wait_for_recovery().await;
        assert!(context.0.pending.lock().unwrap().is_none());
        assert_eq!(context.controller().budget_snapshot().rolling_dials, 0);
    }
    #[test]
    fn authorization_does_not_grant_probing_to_upload_hosts() {
        assert!(authorize(Role::Upload, "some.storage.googleapis.com", 443).is_ok());
        assert!(!probe_authorized(
            Role::Upload,
            "some.storage.googleapis.com"
        ));
        assert!(authorize(Role::Rest, "discord.com.attacker.example", 443).is_err());
        assert!(authorize(Role::Voice, "host.discord.media", 443).is_ok());
        assert!(authorize(Role::Cdn, "127.0.0.1", 443).is_err());
    }

    #[tokio::test]
    async fn probe_transport_counts_and_caps_actual_wire_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let mut remote = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let counters = Arc::new(Mutex::new((0, 0)));
        let mut counted = CountedIo {
            stream,
            counters: counters.clone(),
            inbound: 128,
            total: 192,
        };
        remote.write_all(&[7; 256]).await.unwrap();
        let mut inbound = Vec::new();
        assert!(counted.read_to_end(&mut inbound).await.is_err());
        assert_eq!(inbound.len(), 128);
        assert_eq!(*counters.lock().unwrap(), (0, 128));
        assert!(counted.write_all(&[8; 128]).await.is_err());
        let mut received = [0; 64];
        remote.read_exact(&mut received).await.unwrap();
        assert_eq!(received, [8; 64]);
        assert_eq!(*counters.lock().unwrap(), (64, 128));
    }
}
