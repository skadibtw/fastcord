//! Correlation lives on a connector future, never on the most recent request
//! to a host. Only a new connection visits this layer; HTTP pools stay usable
//! while an unrelated service is being diagnosed.
use std::error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower::{Layer, Service};

use crate::selector::TlsProfile;
use crate::settings::Mode;
use crate::{NetworkContext, NetworkError, Role};

type BoxError = Box<dyn Error + Send + Sync>;
const LIMIT: usize = crate::bridge::TUNNEL_LIMIT;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub host: String,
    pub port: u16,
    pub role: Role,
    pub profile: TlsProfile,
}

#[derive(Default, Clone)]
struct State {
    target: Option<Endpoint>,
    address: Option<SocketAddr>,
    connected: bool,
    failure: Option<NetworkError>,
}

pub(crate) struct Attempt {
    pub id: u64,
    pub role: Role,
    generation: u64,
    mode: Mode,
    state: Mutex<State>,
}

impl Attempt {
    pub fn target(&self, host: &str, port: u16) {
        if host.len() <= 253 {
            self.state.lock().unwrap_or_else(|p| p.into_inner()).target = Some(Endpoint {
                host: host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_ascii_lowercase(),
                port,
                role: self.role,
                profile: crate::broker::http_profile(self.mode, self.role),
            });
        }
    }
    pub fn connected(&self, address: Option<SocketAddr>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.address = address;
        state.connected = true;
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn matches(&self, host: &str, port: u16) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .target
            .as_ref()
            .is_some_and(|target| target.host.eq_ignore_ascii_case(host) && target.port == port)
    }
    pub fn failed(&self, failure: NetworkError) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).failure = Some(failure);
    }
}

pub(crate) struct Attempts {
    next: AtomicU64,
    entries: Mutex<Vec<Weak<Attempt>>>,
    slots: Arc<Semaphore>,
}

impl Default for Attempts {
    fn default() -> Self {
        Self {
            next: AtomicU64::new(1),
            entries: Mutex::new(Vec::new()),
            slots: Arc::new(Semaphore::new(LIMIT)),
        }
    }
}

struct Registration {
    attempt: Arc<Attempt>,
    registry: Arc<Attempts>,
    _permit: OwnedSemaphorePermit,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.registry
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|entry| entry.upgrade().is_some_and(|a| a.id != self.attempt.id));
    }
}

impl Attempts {
    async fn register(
        self: &Arc<Self>,
        role: Role,
        generation: u64,
        mode: Mode,
    ) -> Result<Registration, NetworkError> {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| NetworkError::Cancelled)?;
        let id = self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| NetworkError::BudgetExhausted)?;
        let attempt = Arc::new(Attempt {
            id,
            role,
            generation,
            mode,
            state: Mutex::default(),
        });
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Arc::downgrade(&attempt));
        Ok(Registration {
            attempt,
            registry: self.clone(),
            _permit: permit,
        })
    }
    pub fn get(&self, id: u64, role: Role) -> Option<Arc<Attempt>> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .find(|entry| entry.id == id && entry.role == role)
    }
}

tokio::task_local! { static CURRENT: Arc<Attempt>; }

/// The private CONNECT auth carries this ID, never the origin headers. Missing
/// connector scope produces an invalid credential rather than a direct route.
pub(crate) fn current(host: &str, port: u16) -> Option<u64> {
    CURRENT
        .try_with(|attempt| {
            attempt.target(host, port);
            attempt.id
        })
        .ok()
}

#[derive(Clone)]
pub(crate) struct ConnectLayer {
    pub context: NetworkContext,
    pub role: Role,
    pub timeout: Option<std::time::Duration>,
}
#[derive(Clone)]
pub(crate) struct Connector<S> {
    inner: S,
    config: ConnectLayer,
}
impl<S> Layer<S> for ConnectLayer {
    type Service = Connector<S>;
    fn layer(&self, inner: S) -> Self::Service {
        Connector {
            inner,
            config: self.clone(),
        }
    }
}
impl<S, R> Service<R> for Connector<S>
where
    R: Send + 'static,
    S: Service<R, Error = BoxError> + Clone + Send + Sync + 'static,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, BoxError>> + Send>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        self.inner.poll_ready(cx)
    }
    fn call(&mut self, request: R) -> Self::Future {
        let replacement = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, replacement);
        let config = self.config.clone();
        let deadline = config.timeout.map(|duration| {
            let now = tokio::time::Instant::now();
            now.checked_add(duration).unwrap_or(now)
        });
        Box::pin(async move {
            let registration = bounded(deadline, async {
                Ok(config
                    .context
                    .attempts()
                    .register(
                        config.role,
                        config.context.controller().generation(),
                        config.context.mode(),
                    )
                    .await?)
            })
            .await?;
            let attempt = registration.attempt.clone();
            CURRENT
                .scope(attempt.clone(), async move {
                    // Reqwest invokes its custom proxy matcher during Service::call.
                    // Keep the scope across polling too; integration tests exercise
                    // the real connector so upstream changes cannot lose correlation.
                    let connecting = inner.call(request);
                    let target = attempt
                        .state
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .target
                        .clone()
                        .ok_or(NetworkError::BridgeUnavailable)?;
                    let result = bounded(deadline, connecting).await;
                    if let Err(raw) = result {
                        let state = attempt
                            .state
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .clone();
                        let mut failure = state.failure.unwrap_or_else(|| {
                            classify(raw.as_ref(), NetworkError::TlsProtocolRejected)
                        });
                        if !state.connected && state.failure.is_none() {
                            failure = NetworkError::BridgeUnavailable;
                        }
                        if state.connected {
                            failure = match failure {
                                NetworkError::TcpReset => NetworkError::TlsReset,
                                NetworkError::TcpTimeout => NetworkError::TlsTimeout,
                                other => other,
                            };
                        }
                        if state.failure.is_none()
                            && attempt.generation == config.context.controller().generation()
                        {
                            config.context.record_failure(
                                &target.host,
                                target.port,
                                target.role,
                                target.profile,
                                failure,
                                state.address,
                            );
                        }
                        return Err(Box::new(failure) as BoxError);
                    }
                    drop(registration);
                    result
                })
                .await
        })
    }
}

async fn bounded<T>(
    deadline: Option<tokio::time::Instant>,
    future: impl Future<Output = Result<T, BoxError>>,
) -> Result<T, BoxError> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, future)
            .await
            .unwrap_or_else(|_| Err(Box::new(NetworkError::TcpTimeout))),
        None => future.await,
    }
}

pub(crate) fn classify(error: &(dyn Error + 'static), mut category: NetworkError) -> NetworkError {
    let mut source = Some(error);
    while let Some(inner) = source {
        if let Some(net) = inner.downcast_ref::<NetworkError>() {
            return *net;
        }
        if let Some(tls) = inner.downcast_ref::<tokio_rustls::rustls::Error>() {
            return match tls {
                tokio_rustls::rustls::Error::InvalidCertificate(_)
                | tokio_rustls::rustls::Error::NoCertificatesPresented => {
                    NetworkError::CertificateRejected
                }
                _ => NetworkError::TlsProtocolRejected,
            };
        }
        if let Some(io) = inner.downcast_ref::<std::io::Error>() {
            if let Some(payload) = io.get_ref() {
                if let Some(net) = payload.downcast_ref::<NetworkError>() {
                    return *net;
                }
                if payload.is::<tokio_rustls::rustls::Error>() {
                    return crate::broker::tls_error(io);
                }
            }
            category = crate::broker::tcp_error(io);
        }
        source = inner.source();
    }
    category
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone)]
    struct WaitingConnector {
        scoped: bool,
    }
    impl Service<()> for WaitingConnector {
        type Response = ();
        type Error = BoxError;
        type Future = Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, _: ()) -> Self::Future {
            if self.scoped {
                current("discord.com", 443).unwrap();
                CURRENT.with(|attempt| attempt.connected(Some("1.1.1.1:443".parse().unwrap())));
            }
            Box::pin(std::future::pending())
        }
    }
    #[tokio::test(start_paused = true)]
    async fn handshake_timeout_records_only_its_own_endpoint_and_releases_registration() {
        let context = NetworkContext::new(crate::settings::Mode::Auto);
        let mut connector = ConnectLayer {
            context: context.clone(),
            role: Role::Rest,
            timeout: Some(std::time::Duration::from_secs(3)),
        }
        .layer(WaitingConnector { scoped: true });
        let error = connector.call(()).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<NetworkError>(),
            Some(&NetworkError::TlsTimeout)
        );
        assert_eq!(
            context.controller().service_status(Role::Rest),
            crate::selector::ServiceStatus::Failed(NetworkError::TlsTimeout)
        );
        assert_eq!(context.controller().budget_snapshot().rolling_dials, 0);
        assert!(context.attempts().entries.lock().unwrap().is_empty());
    }
    #[tokio::test(start_paused = true)]
    async fn cancelling_a_connector_is_not_reported_as_a_timeout_or_new_recovery_demand() {
        let context = NetworkContext::new(crate::settings::Mode::Auto);
        let mut connector = ConnectLayer {
            context: context.clone(),
            role: Role::Rest,
            timeout: Some(std::time::Duration::from_secs(3)),
        }
        .layer(WaitingConnector { scoped: true });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), connector.call(()))
                .await
                .is_err()
        );
        assert_eq!(
            context.controller().service_status(Role::Rest),
            crate::selector::ServiceStatus::Unknown
        );
        assert!(context.attempts().entries.lock().unwrap().is_empty());
        let mut unscoped = ConnectLayer {
            context,
            role: Role::Rest,
            timeout: None,
        }
        .layer(WaitingConnector { scoped: false });
        assert_eq!(
            unscoped
                .call(())
                .await
                .unwrap_err()
                .downcast_ref::<NetworkError>(),
            Some(&NetworkError::BridgeUnavailable)
        );
    }
    #[tokio::test]
    async fn ipv6_url_brackets_do_not_change_the_connect_authority_binding() {
        let registry = Arc::new(Attempts::default());
        let registered = registry.register(Role::Cdn, 0, Mode::Off).await.unwrap();
        registered.attempt.target("[2606:4700::1]", 443);
        assert!(registered.attempt.matches("2606:4700::1", 443));
        assert!(!registered.attempt.matches("2606:4700::2", 443));
        assert!(!registered.attempt.matches("2606:4700::1", 444));
    }

    #[tokio::test]
    async fn connector_receipts_keep_the_http_sni_policy_of_their_context() {
        let registry = Arc::new(Attempts::default());
        for (mode, role, profile) in [
            (Mode::Auto, Role::Rest, TlsProfile::HttpWithoutSni),
            (Mode::Auto, Role::Cdn, TlsProfile::Http),
            (Mode::Proxy, Role::Rest, TlsProfile::Http),
            (Mode::Off, Role::Rest, TlsProfile::Http),
        ] {
            let receipt = registry.register(role, 0, mode).await.unwrap();
            receipt.attempt.target("discord.com", 443);
            assert_eq!(
                receipt
                    .attempt
                    .state
                    .lock()
                    .unwrap()
                    .target
                    .as_ref()
                    .unwrap()
                    .profile,
                profile
            );
        }
    }
    #[tokio::test]
    async fn simultaneous_attempts_to_one_endpoint_keep_their_own_failure_and_address() {
        let registry = Arc::new(Attempts::default());
        let a = registry.register(Role::Rest, 0, Mode::Off).await.unwrap();
        let b = registry.register(Role::Rest, 0, Mode::Off).await.unwrap();
        for attempt in [&a.attempt, &b.attempt] {
            attempt.target("discord.com", 443);
        }
        let address_a = "1.1.1.1:443".parse().unwrap();
        let address_b = "8.8.8.8:443".parse().unwrap();
        registry
            .get(b.attempt.id, Role::Rest)
            .unwrap()
            .connected(Some(address_b));
        registry
            .get(a.attempt.id, Role::Rest)
            .unwrap()
            .connected(Some(address_a));
        registry
            .get(a.attempt.id, Role::Rest)
            .unwrap()
            .failed(NetworkError::ProxyAuthentication);
        let state = b.attempt.state.lock().unwrap().clone();
        assert_eq!(state.address, Some(address_b));
        assert_eq!(state.failure, None);
        assert_eq!(
            a.attempt.state.lock().unwrap().failure,
            Some(NetworkError::ProxyAuthentication)
        );
        assert!(registry.get(b.attempt.id, Role::Cdn).is_none());
        let old = a.attempt.id;
        let stale = a.attempt.clone();
        drop(a);
        assert!(registry.get(old, Role::Rest).is_none());
        let next = registry.register(Role::Rest, 0, Mode::Off).await.unwrap();
        assert_ne!(old, next.attempt.id);
        stale.failed(NetworkError::TcpReset);
        assert_eq!(next.attempt.state.lock().unwrap().failure, None);
    }
    #[tokio::test]
    async fn cancelled_registration_releases_its_slot_and_receipts_are_bounded() {
        let registry = Arc::new(Attempts::default());
        let mut held = Vec::new();
        for _ in 0..LIMIT {
            held.push(registry.register(Role::Cdn, 0, Mode::Off).await.unwrap());
        }
        let waiting = registry.register(Role::Cdn, 0, Mode::Off);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut waiting)
                .await
                .is_err()
        );
        let old = held.pop().unwrap();
        let id = old.attempt.id;
        drop(old);
        let next = waiting.await.unwrap();
        assert_ne!(id, next.attempt.id);
        assert_eq!(registry.entries.lock().unwrap().len(), LIMIT);
        drop(next);
        drop(held);
        assert!(registry.entries.lock().unwrap().is_empty());
        assert_eq!(registry.slots.available_permits(), LIMIT);
    }
}
