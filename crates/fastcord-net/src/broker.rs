use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
#[cfg(debug_assertions)]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UdpSocket, lookup_host};
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto, pki_types::ServerName};

use crate::bridge::Bridge;
use crate::proxy::{ProxyConfig, ProxyCredentials};
use crate::selector::{
    AddressFamily, Controller, DiagnosticTarget, ProbeOutcome, ProbeRequest, TlsProfile,
};
use crate::settings::Mode;
use crate::socket::NativeIo;
use crate::tls::{ShapedStream, Strategy};
use crate::{DatagramSocket, HttpBuilder, NetworkError, Role};

pub trait NetworkIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> NetworkIo for T {}
pub type BoxedIo = Box<dyn NetworkIo>;
pub type TlsStream = tokio_rustls::client::TlsStream<ShapedStream<BoxedIo>>;

struct Shared {
    prefix_factory: Option<Arc<dyn crate::tls::FakePrefixFactory>>,
    mode: Mode,
    experimental_udp: bool,
    proxy: Option<Arc<ProxyCredentials>>,
    controller: Controller,
    bridge: Mutex<Option<Bridge>>,
    bridge_secret: Arc<OnceLock<Result<Arc<zeroize::Zeroizing<String>>, NetworkError>>>,
    transforms: Arc<Semaphore>,
    tunnels: Arc<Semaphore>,
    pending: Arc<Mutex<Option<Demand>>>,
    attempts: Arc<crate::connect::Attempts>,
    recovering: Arc<Mutex<Option<crate::connect::Endpoint>>>,
    recovery_gate: Arc<tokio::sync::Mutex<()>>,
    associations: Arc<Semaphore>,
    #[cfg(debug_assertions)]
    probe_output: Arc<std::sync::atomic::AtomicBool>,
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

/// A transport task reports failure without retaining its owning bridge.
#[derive(Clone)]
pub(crate) struct FailureReporter {
    controller: Controller,
    pending: Arc<Mutex<Option<Demand>>>,
    enabled: bool,
}

impl FailureReporter {
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
        let generation = self.controller.generation();
        if pending
            .as_ref()
            .is_none_or(|pending| pending.generation != generation)
        {
            *pending = Some(Demand {
                host: host.to_owned(),
                port,
                role,
                profile,
                failure: error,
                address,
                generation,
            });
        }
    }
}

struct RecoveryScope(Arc<Mutex<Option<crate::connect::Endpoint>>>);
impl Drop for RecoveryScope {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).take();
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
            prefix_factory: None,
            mode,
            experimental_udp: false,
            proxy: None,
            controller: Controller::default(),
            bridge: Mutex::new(None),
            bridge_secret: Arc::new(OnceLock::new()),
            transforms: Arc::new(Semaphore::new(4)),
            tunnels: Arc::new(Semaphore::new(crate::bridge::TUNNEL_LIMIT)),
            pending: Arc::new(Mutex::new(None)),
            attempts: Arc::default(),
            recovering: Arc::default(),
            recovery_gate: Arc::new(tokio::sync::Mutex::new(())),
            associations: Arc::new(Semaphore::new(crate::udp::MAX_ASSOCIATIONS)),
            #[cfg(debug_assertions)]
            probe_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }))
    }

    /// The one derivation of a view (#135): only the per-view settings are
    /// arguments; every process-wide budget, gate and diagnostic is shared
    /// here, so a new one cannot be split by a forgotten clone. Each view
    /// starts its own bridge.
    fn view(
        &self,
        prefix_factory: Option<Arc<dyn crate::tls::FakePrefixFactory>>,
        mode: Mode,
        experimental_udp: bool,
        proxy: Option<Arc<ProxyCredentials>>,
    ) -> Self {
        Self(Arc::new(Shared {
            prefix_factory,
            mode,
            experimental_udp,
            proxy,
            controller: self.0.controller.clone(),
            bridge: Mutex::new(None),
            bridge_secret: self.0.bridge_secret.clone(),
            transforms: self.0.transforms.clone(),
            tunnels: self.0.tunnels.clone(),
            pending: self.0.pending.clone(),
            attempts: self.0.attempts.clone(),
            recovering: self.0.recovering.clone(),
            recovery_gate: self.0.recovery_gate.clone(),
            associations: self.0.associations.clone(),
            #[cfg(debug_assertions)]
            probe_output: self.0.probe_output.clone(),
        }))
    }

    pub fn with_mode(&self, mode: Mode) -> Self {
        self.view(
            self.0.prefix_factory.clone(),
            mode,
            self.0.experimental_udp,
            self.0.proxy.clone(),
        )
    }

    pub fn mode(&self) -> Mode {
        self.0.mode
    }
    /// Experimental discovery applies only to a direct UDP flow. A proxy
    /// requires an explicit relay and must never leak preludes directly.
    pub fn experimental_udp(&self) -> bool {
        self.0.experimental_udp && self.mode() != Mode::Proxy
    }

    pub fn with_experimental_udp(&self, enabled: bool) -> Self {
        self.view(
            self.0.prefix_factory.clone(),
            self.0.mode,
            enabled,
            self.0.proxy.clone(),
        )
    }
    pub fn with_proxy(
        &self,
        config: ProxyConfig,
        password: zeroize::Zeroizing<String>,
    ) -> Result<Self, NetworkError> {
        let proxy = Arc::new(ProxyCredentials::new(config, password)?);
        Ok(self.view(
            self.0.prefix_factory.clone(),
            Mode::Proxy,
            self.0.experimental_udp,
            Some(proxy),
        ))
    }
    pub fn proxy_config(&self) -> Option<&ProxyConfig> {
        self.0.proxy.as_ref().map(|proxy| &proxy.config)
    }
    pub fn with_prefix_factory(&self, factory: Arc<dyn crate::tls::FakePrefixFactory>) -> Self {
        self.set_prefix_factory(Some(factory))
    }
    pub fn without_prefix_factory(&self) -> Self {
        self.set_prefix_factory(None)
    }
    fn set_prefix_factory(&self, factory: Option<Arc<dyn crate::tls::FakePrefixFactory>>) -> Self {
        self.view(
            factory,
            self.0.mode,
            self.0.experimental_udp,
            self.0.proxy.clone(),
        )
    }
    pub(crate) fn prefix_factory(&self) -> Option<Arc<dyn crate::tls::FakePrefixFactory>> {
        self.0.prefix_factory.clone()
    }
    pub fn select_packet_strategy(&self, strategy: Strategy) -> io::Result<()> {
        self.0
            .prefix_factory
            .as_ref()
            .ok_or(io::ErrorKind::Unsupported)?
            .select_strategy(strategy)?;
        self.controller().network_changed();
        Ok(())
    }
    pub fn media_supported(&self) -> bool {
        self.mode() != Mode::Proxy
            || self
                .proxy_config()
                .is_some_and(|proxy| proxy.kind == crate::ProxyKind::Socks5)
    }
    pub(crate) fn proxy_credentials(&self) -> Option<Arc<ProxyCredentials>> {
        self.0.proxy.clone()
    }
    pub fn controller(&self) -> &Controller {
        &self.0.controller
    }
    pub fn broker(&self) -> SocketBroker {
        SocketBroker(self.clone())
    }
    pub(crate) fn bridge_secret(&self) -> Result<Arc<zeroize::Zeroizing<String>>, NetworkError> {
        self.0
            .bridge_secret
            .get_or_init(crate::bridge::new_secret)
            .clone()
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

    pub async fn bind_udp(&self, server: SocketAddr) -> io::Result<DatagramSocket> {
        self.broker().bind_udp(server).await
    }

    #[cfg(debug_assertions)]
    pub async fn debug_bind_udp(&self, server: SocketAddr) -> io::Result<DatagramSocket> {
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
        UdpSocket::bind(local).await.map(DatagramSocket::Direct)
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

    #[cfg(debug_assertions)]
    pub fn debug_probe_output(&self, enabled: bool) {
        self.0.probe_output.store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn attempts(&self) -> Arc<crate::connect::Attempts> {
        self.0.attempts.clone()
    }

    pub(crate) async fn wait_for_endpoint(&self, endpoint: &crate::connect::Endpoint) {
        self.recover(None, Some(endpoint)).await;
    }

    /// Drives a pending failure before the next production dial. The first
    /// failed attempt returns immediately so its owner records the failure and
    /// applies its normal backoff before entering this gate. No request replay.
    pub async fn wait_for_recovery(&self) {
        self.recover(None, None).await;
    }

    #[cfg(debug_assertions)]
    pub async fn debug_recover_with(&self, strategy: Strategy) {
        self.recover(Some(strategy), None).await;
    }

    async fn recover(&self, forced: Option<Strategy>, endpoint: Option<&crate::connect::Endpoint>) {
        if self.mode() != Mode::Auto {
            return;
        }
        {
            let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
            if pending
                .as_ref()
                .is_some_and(|demand| demand.generation != self.controller().generation())
            {
                pending.take();
            }
        }
        let matches = |demand: &Demand| {
            endpoint.is_none_or(|endpoint| {
                endpoint.host.eq_ignore_ascii_case(&demand.host)
                    && endpoint.port == demand.port
                    && endpoint.role == demand.role
                    && endpoint.profile == demand.profile
            })
        };
        let active_matches = || {
            endpoint.is_some_and(|endpoint| {
                self.0
                    .recovering
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
                    == Some(endpoint)
            })
        };
        if endpoint.is_some()
            && !self
                .0
                .pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .is_some_and(matches)
            && !active_matches()
        {
            return;
        }
        let _gate = match self.0.recovery_gate.try_lock() {
            Ok(gate) => gate,
            Err(_) if endpoint.is_some() && !active_matches() => return,
            Err(_) => self.0.recovery_gate.lock().await,
        };
        self.0.controller.wait_for_idle().await;
        let pending = {
            let mut pending = self.0.pending.lock().unwrap_or_else(|p| p.into_inner());
            if pending.as_ref().is_some_and(matches) {
                pending.take()
            } else {
                None
            }
        };
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
        *self.0.recovering.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(crate::connect::Endpoint {
                host: target.host.clone(),
                port: target.port,
                role: target.role,
                profile: target.profile,
            });
        let _scope = RecoveryScope(self.0.recovering.clone());
        let transforms = self.0.transforms.clone();
        // A helper that is gone cannot be probed either: fall back to the
        // ordinary catalog instead of planning a campaign around it.
        let factory = self
            .0
            .prefix_factory
            .clone()
            .filter(|factory| factory.available());
        #[cfg(debug_assertions)]
        let output = self.0.probe_output.clone();
        let catalog = forced
            .or_else(|| factory.as_ref().map(|f| f.strategy()))
            .map(|strategy| [strategy]);
        let plan = catalog
            .as_ref()
            .map_or(Strategy::ORDER.as_slice(), |one| one.as_slice());
        let _ = self
            .0
            .controller
            .diagnose_plan(target, pending.failure, plan, move |request| {
                let transforms = transforms.clone();
                let factory = factory.clone();
                #[cfg(debug_assertions)]
                let output = output.clone();
                async move {
                    #[cfg(debug_assertions)]
                    let strategy = request.strategy;
                    #[cfg(debug_assertions)]
                    let verbose = output.load(Ordering::Relaxed);
                    #[cfg(not(debug_assertions))]
                    let verbose = false;
                    let result = run_probe(address, request, transforms, factory, verbose).await;
                    #[cfg(debug_assertions)]
                    if output.load(Ordering::Relaxed) {
                        println!(
                            "TLS probe {strategy:?}: {:?}; sent={} received={}",
                            result.result, result.sent_bytes, result.received_bytes
                        );
                    }
                    result
                }
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
        if self.mode() != Mode::Auto {
            return Strategy::Direct;
        }
        select_strategy(
            self.controller(),
            self.0.prefix_factory.as_deref(),
            host,
            port,
            role,
            address,
            profile,
        )
    }
}

pub(crate) fn select_strategy(
    controller: &Controller,
    factory: Option<&dyn crate::tls::FakePrefixFactory>,
    host: &str,
    port: u16,
    role: Role,
    address: SocketAddr,
    profile: TlsProfile,
) -> Strategy {
    if !probe_authorized(role, host) {
        return Strategy::Direct;
    }
    if let Some(factory) = factory
        && factory.available()
    {
        return factory.strategy();
    }
    let family = if address.is_ipv4() {
        AddressFamily::V4
    } else {
        AddressFamily::V6
    };
    DiagnosticTarget::new(host, port, role, family, profile)
        .ok()
        .and_then(|target| controller.strategy_for(&target))
        .filter(|strategy| !matches!(strategy, Strategy::PacketFake | Strategy::PacketOverlap))
        .unwrap_or(Strategy::Direct)
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
        self.0
            .wait_for_endpoint(&crate::connect::Endpoint {
                host: host.to_ascii_lowercase(),
                port,
                role,
                profile: TlsProfile::WebSocket,
            })
            .await;
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
        let (io, strategy, control) = if self.0.mode() == Mode::Proxy {
            let proxy = self
                .0
                .proxy_credentials()
                .ok_or(NetworkError::ProxyConfiguration)?;
            (
                crate::proxy::dial(&proxy, host, port).await?,
                Strategy::Direct,
                None,
            )
        } else {
            let (tcp, address) = dial_public(host, port, failed_address).await?;
            *failed_address = Some(address);
            let strategy = self
                .0
                .strategy(host, port, role, address, TlsProfile::WebSocket);
            let native = NativeIo::new(tcp).with_factory(self.0.prefix_factory());
            let control = native.control();
            (Box::new(native) as BoxedIo, strategy, Some(control))
        };
        let stream = ShapedStream::with_budget(io, strategy, self.0.transforms());
        let stream = if let Some(control) = control {
            stream.with_control(control)
        } else {
            stream
        };
        let config = tls_config(TlsProfile::WebSocket, self.0.mode() == Mode::Auto)?;
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

    pub async fn bind_udp(&self, server: SocketAddr) -> io::Result<DatagramSocket> {
        if self.0.mode() == Mode::Proxy {
            let proxy = self
                .0
                .proxy_credentials()
                .ok_or_else(|| io::Error::other(NetworkError::ProxyConfiguration))?;
            return DatagramSocket::associate(proxy, server, self.0.0.associations.clone()).await;
        }
        if !public_ip(server.ip()) || server.port() == 0 {
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
        UdpSocket::bind(local).await.map(DatagramSocket::Direct)
    }
}

/// The client config for a profile, built once per process (#131): one
/// config means one session cache, so WebSocket and proxy reconnects outside
/// Auto mode resume TLS instead of verifying the whole chain again. Probes
/// share a config too; theirs never resumes.
pub(crate) fn tls_config(
    profile: TlsProfile,
    probe: bool,
) -> Result<Arc<ClientConfig>, NetworkError> {
    type Slot = std::sync::OnceLock<Result<Arc<ClientConfig>, NetworkError>>;
    static CONFIGS: [Slot; 4] = [const { Slot::new() }; 4];
    let index = usize::from(profile == TlsProfile::WebSocket) * 2 + usize::from(probe);
    CONFIGS[index]
        .get_or_init(|| build_tls_config(profile, probe))
        .clone()
}

fn build_tls_config(profile: TlsProfile, probe: bool) -> Result<Arc<ClientConfig>, NetworkError> {
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
    crate::dial::connect(&addresses, Duration::from_secs(3), failed_address).await
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
    factory: Option<Arc<dyn crate::tls::FakePrefixFactory>>,
    verbose: bool,
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
        let bounded = NativeIo::bounded(
            stream,
            counters.clone(),
            request.limits.max_inbound_bytes,
            request.limits.max_network_bytes,
        )
        .with_factory(factory);
        let control = bounded.control();
        let stream =
            ShapedStream::with_budget(bounded, request.strategy, transforms).with_control(control);
        let name = ServerName::try_from(request.target.host.clone())
            .map_err(|_| NetworkError::InvalidTarget)?;
        let config = tls_config(request.target.profile, true)?;
        let _tls = timeout(
            request.limits.tls_timeout,
            TlsConnector::from(config).connect(name, stream),
        )
        .await
        .map_err(|_| NetworkError::TlsTimeout)?
        .map_err(|error| {
            if verbose {
                println!(
                    "TLS transport error: kind={:?}; os={:?}",
                    error.kind(),
                    error.raw_os_error()
                );
            }
            tls_error(&error)
        })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_configs_are_built_once_so_sessions_can_resume() {
        let first = tls_config(TlsProfile::WebSocket, false).unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &tls_config(TlsProfile::WebSocket, false).unwrap()
        ));
        let probe = tls_config(TlsProfile::WebSocket, true).unwrap();
        assert!(!Arc::ptr_eq(&first, &probe));
        assert!(!probe.enable_early_data);
        let http = tls_config(TlsProfile::Http, false).unwrap();
        assert_eq!(http.alpn_protocols, [b"h2".to_vec(), b"http/1.1".to_vec()]);
        assert!(first.alpn_protocols.is_empty());
    }

    struct DeadFactory;
    impl crate::tls::FakePrefixFactory for DeadFactory {
        fn available(&self) -> bool {
            false
        }
        fn start(
            &self,
            _: std::net::TcpStream,
            _: Vec<u8>,
            _: Vec<u8>,
            _: Strategy,
        ) -> io::Result<crate::tls::PrefixFuture> {
            Err(io::ErrorKind::Unsupported.into())
        }
    }

    #[test]
    fn a_dead_packet_factory_falls_back_to_the_ordinary_catalog() {
        let controller = Controller::new();
        let address: SocketAddr = "1.1.1.1:443".parse().unwrap();
        // Alive factory: its own strategy wins, as before.
        struct AliveFactory(DeadFactory);
        impl crate::tls::FakePrefixFactory for AliveFactory {
            fn start(
                &self,
                socket: std::net::TcpStream,
                original: Vec<u8>,
                fake: Vec<u8>,
                strategy: Strategy,
            ) -> io::Result<crate::tls::PrefixFuture> {
                self.0.start(socket, original, fake, strategy)
            }
        }
        assert_eq!(
            select_strategy(
                &controller,
                Some(&AliveFactory(DeadFactory)),
                "discord.com",
                443,
                Role::Rest,
                address,
                TlsProfile::Http
            ),
            Strategy::PacketFake
        );
        // The same factory once its helper is gone must not be selected: the
        // ordinary catalog (here Direct, nothing confirmed yet) takes over.
        assert_eq!(
            select_strategy(
                &controller,
                Some(&DeadFactory),
                "discord.com",
                443,
                Role::Rest,
                address,
                TlsProfile::Http
            ),
            Strategy::Direct
        );
    }

    #[test]
    fn private_bridge_credential_lasts_across_all_application_context_views() {
        let context = NetworkContext::off();
        let first = context.bridge_secret().unwrap();
        let view = context
            .with_mode(Mode::Auto)
            .with_experimental_udp(true)
            .with_proxy(
                ProxyConfig {
                    kind: crate::ProxyKind::Socks5,
                    host: "127.0.0.1".into(),
                    port: 1080,
                    username: None,
                },
                zeroize::Zeroizing::new(String::new()),
            )
            .unwrap();
        assert!(Arc::ptr_eq(&first, &view.bridge_secret().unwrap()));
        assert!(!Arc::ptr_eq(
            &first,
            &NetworkContext::off().bridge_secret().unwrap()
        ));
    }
    #[test]
    fn experimental_udp_requires_separate_opt_in_preserves_budgets_and_never_leaks_past_proxy() {
        let context = NetworkContext::off();
        assert!(!context.experimental_udp());
        assert!(!context.with_mode(Mode::Auto).experimental_udp());
        let enabled = context.with_experimental_udp(true);
        assert!(enabled.experimental_udp());
        assert!(enabled.with_mode(Mode::Auto).experimental_udp());
        assert!(!enabled.with_experimental_udp(false).experimental_udp());
        assert!(Arc::ptr_eq(&context.0.pending, &enabled.0.pending));
        assert!(Arc::ptr_eq(
            &context.0.recovery_gate,
            &enabled.0.recovery_gate
        ));
        assert!(Arc::ptr_eq(&context.0.transforms, &enabled.0.transforms));
        assert!(Arc::ptr_eq(&context.0.tunnels, &enabled.0.tunnels));
        assert!(Arc::ptr_eq(
            &context.0.associations,
            &enabled.0.associations
        ));
        context.controller().network_changed();
        assert_eq!(
            context.controller().generation(),
            enabled.controller().generation()
        );
        let proxy = enabled
            .with_proxy(
                ProxyConfig {
                    kind: crate::ProxyKind::Socks5,
                    host: "127.0.0.1".into(),
                    port: 1080,
                    username: None,
                },
                zeroize::Zeroizing::new(String::new()),
            )
            .unwrap();
        assert!(!proxy.experimental_udp());
        assert!(!proxy.with_experimental_udp(true).experimental_udp());
        assert!(proxy.with_mode(Mode::Off).experimental_udp());
        assert_eq!(
            proxy.with_experimental_udp(false).proxy_config(),
            proxy.proxy_config()
        );
        assert_eq!(
            proxy.controller().generation(),
            context.controller().generation()
        );
    }
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
        let mut counted = NativeIo::bounded(stream, counters.clone(), 128, 192);
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

#[cfg(test)]
mod recovery_scope_tests {
    use super::*;
    use crate::connect::Endpoint;
    fn endpoint(host: &str, role: Role) -> Endpoint {
        Endpoint {
            host: host.into(),
            port: 443,
            role,
            profile: TlsProfile::Http,
        }
    }
    #[tokio::test(start_paused = true)]
    async fn a_cdn_campaign_does_not_block_api_or_other_cdn_endpoints() {
        let context = NetworkContext::new(Mode::Auto);
        let active = endpoint("cdn.discordapp.com", Role::Cdn);
        *context.0.recovering.lock().unwrap() = Some(active.clone());
        let scope = RecoveryScope(context.0.recovering.clone());
        let gate = context.0.recovery_gate.lock().await;
        for target in [
            endpoint("discord.com", Role::Rest),
            endpoint("media.discordapp.net", Role::Cdn),
            endpoint("cdn.discordapp.com", Role::Upload),
        ] {
            timeout(Duration::from_millis(1), context.wait_for_endpoint(&target))
                .await
                .unwrap();
        }
        assert!(
            timeout(Duration::from_millis(1), context.wait_for_endpoint(&active))
                .await
                .is_err()
        );
        drop(scope);
        drop(gate);
        context.wait_for_endpoint(&active).await;
        assert!(context.0.recovering.lock().unwrap().is_none());
    }
    #[tokio::test]
    async fn network_changes_cannot_leave_a_stale_demand_blocking_other_endpoints() {
        let context = NetworkContext::new(Mode::Auto);
        let report = |host, role| {
            context.record_failure(
                host,
                443,
                role,
                TlsProfile::Http,
                NetworkError::TlsReset,
                Some("1.1.1.1:443".parse().unwrap()),
            )
        };
        report("cdn.discordapp.com", Role::Cdn);
        context.controller().network_changed();
        report("discord.com", Role::Rest);
        assert_eq!(
            context.0.pending.lock().unwrap().as_ref().unwrap().host,
            "discord.com"
        );
        context.controller().network_changed();
        context
            .wait_for_endpoint(&endpoint("media.discordapp.net", Role::Cdn))
            .await;
        assert!(context.0.pending.lock().unwrap().is_none());
        assert_eq!(context.controller().budget_snapshot().rolling_dials, 0);
    }
    #[tokio::test]
    async fn an_unrelated_request_does_not_take_the_failed_endpoints_pending_demand() {
        let context = NetworkContext::new(Mode::Auto);
        context.record_failure(
            "cdn.discordapp.com",
            443,
            Role::Cdn,
            TlsProfile::Http,
            NetworkError::TlsReset,
            Some("1.1.1.1:443".parse().unwrap()),
        );
        context
            .wait_for_endpoint(&endpoint("discord.com", Role::Rest))
            .await;
        assert!(context.0.pending.lock().unwrap().is_some());
        assert_eq!(context.controller().budget_snapshot().rolling_dials, 0);
        context.controller().network_changed();
        context
            .wait_for_endpoint(&endpoint("cdn.discordapp.com", Role::Cdn))
            .await;
        assert!(context.0.pending.lock().unwrap().is_none());
    }
}
