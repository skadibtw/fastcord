//! Restricted HTTP construction. Authentication, CDN and upload clients remain
//! separate; the wrapper validates every request and never replays a body.

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Body, IntoUrl, Method, Request, Response};

use crate::broker::{authorize, resolve_public, tls_config};
use crate::selector::TlsProfile;
use crate::settings::Mode;
use crate::{NetworkContext, NetworkError, Role};

/// Only policy-preserving HTTP options are exposed. Proxy/DNS/TLS verification
/// cannot be replaced by a feature client.
pub struct HttpBuilder {
    inner: reqwest::ClientBuilder,
    context: NetworkContext,
    role: Role,
    local: bool,
    invalid_headers: bool,
    connector: Option<crate::connect::ConnectLayer>,
}

impl HttpBuilder {
    pub(crate) fn new(
        context: NetworkContext,
        role: Role,
        local: bool,
    ) -> Result<Self, NetworkError> {
        let mut inner = reqwest::Client::builder()
            .no_proxy()
            .https_only(!local)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            // Each idle pooled connection holds a bridge tunnel slot (#127).
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .use_preconfigured_tls(
                (*tls_config(TlsProfile::Http, context.mode() == Mode::Auto)?).clone(),
            );
        let connector = if !local && context.mode() != Mode::Off {
            if context.mode() == Mode::Proxy && context.proxy_config().is_none() {
                return Err(NetworkError::ProxyConfiguration);
            }
            inner = inner.proxy(context.bridge_proxy(role)?);
            Some(crate::connect::ConnectLayer {
                context: context.clone(),
                role,
                timeout: None,
            })
        } else {
            if !local {
                inner = inner.dns_resolver(Arc::new(PublicResolver { role }));
            }
            None
        };
        Ok(Self {
            inner,
            context,
            role,
            local,
            invalid_headers: false,
            connector,
        })
    }

    pub fn default_headers(mut self, headers: HeaderMap) -> Self {
        self.invalid_headers |= !allowed_headers(self.role, &headers);
        self.inner = self.inner.default_headers(headers);
        self
    }

    #[cfg(test)]
    pub(crate) fn fixture_tls(mut self, config: tokio_rustls::rustls::ClientConfig) -> Self {
        self.inner = self.inner.use_preconfigured_tls(config);
        self
    }

    pub fn user_agent<V>(mut self, value: V) -> Self
    where
        V: TryInto<HeaderValue>,
        V::Error: Into<http::Error>,
    {
        self.inner = self.inner.user_agent(value);
        self
    }
    pub fn connect_timeout(mut self, value: Duration) -> Self {
        if let Some(connector) = &mut self.connector {
            connector.timeout = Some(value);
        } else {
            self.inner = self.inner.connect_timeout(value);
        }
        self
    }
    pub fn timeout(mut self, value: Duration) -> Self {
        self.inner = self.inner.timeout(value);
        self
    }
    pub fn read_timeout(mut self, value: Duration) -> Self {
        self.inner = self.inner.read_timeout(value);
        self
    }
    /// TLS and redirect behavior are fixed by the factory, rather than exposed
    /// as escape hatches through reqwest's complete builder.
    pub fn use_rustls_tls(self) -> Self {
        self
    }
    pub fn https_only(self, _https: bool) -> Self {
        self
    }
    pub fn no_proxy(self) -> Self {
        self
    }
    pub fn redirect(self, _policy: reqwest::redirect::Policy) -> Self {
        self
    }
    pub fn retry(self, _policy: reqwest::retry::Builder) -> Self {
        self
    }

    pub fn build(self) -> Result<HttpClient, NetworkError> {
        if self.invalid_headers {
            return Err(NetworkError::InvalidTarget);
        }
        let inner = if let Some(connector) = self.connector {
            self.inner.connector_layer(connector)
        } else {
            self.inner
        };
        let inner = inner
            .build()
            .map_err(|_| NetworkError::TlsProtocolRejected)?;
        Ok(HttpClient {
            inner,
            context: self.context,
            role: self.role,
            local: self.local,
        })
    }
}

#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    context: NetworkContext,
    role: Role,
    local: bool,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClient")
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> HttpRequestBuilder {
        HttpRequestBuilder {
            inner: self.inner.request(method, url),
            client: self.clone(),
        }
    }
    pub fn get<U: IntoUrl>(&self, url: U) -> HttpRequestBuilder {
        self.request(Method::GET, url)
    }
    pub fn post<U: IntoUrl>(&self, url: U) -> HttpRequestBuilder {
        self.request(Method::POST, url)
    }
    pub fn put<U: IntoUrl>(&self, url: U) -> HttpRequestBuilder {
        self.request(Method::PUT, url)
    }
    pub fn delete<U: IntoUrl>(&self, url: U) -> HttpRequestBuilder {
        self.request(Method::DELETE, url)
    }

    pub async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        let url = request.url();
        let host = url
            .host_str()
            .ok_or_else(|| HttpError::from_category(NetworkError::InvalidTarget))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| HttpError::from_category(NetworkError::InvalidTarget))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || !allowed_headers(self.role, request.headers())
        {
            return Err(HttpError::from_category(NetworkError::InvalidTarget));
        }
        if self.local {
            if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1")
                || !matches!(url.scheme(), "http" | "https")
            {
                return Err(HttpError::from_category(NetworkError::InvalidTarget));
            }
        } else {
            if url.scheme() != "https" {
                return Err(HttpError::from_category(NetworkError::InvalidTarget));
            }
            let bare_host = host.trim_start_matches('[').trim_end_matches(']');
            authorize(self.role, bare_host, port).map_err(HttpError::from_category)?;
        }
        if !self.local {
            self.context
                .wait_for_endpoint(&crate::connect::Endpoint {
                    host: host
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .to_ascii_lowercase(),
                    port,
                    role: self.role,
                    profile: TlsProfile::Http,
                })
                .await;
        }
        let result = self.inner.execute(request).await.map_err(HttpError::from);
        if let Err(error) = result.as_ref() {
            if !self.local && self.context.mode() == Mode::Off && error.is_connect() {
                self.context
                    .controller()
                    .note_failure(self.role, error.category());
            }
        } else {
            self.context.controller().note_service_reachable(self.role);
        }
        result
    }
}

pub struct HttpRequestBuilder {
    inner: reqwest::RequestBuilder,
    client: HttpClient,
}

impl HttpRequestBuilder {
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<http::Error>,
    {
        self.inner = self.inner.header(key, value);
        self
    }
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.inner = self.inner.headers(headers);
        self
    }
    pub fn query<T: serde::Serialize + ?Sized>(mut self, query: &T) -> Self {
        self.inner = self.inner.query(query);
        self
    }
    pub fn body<B: Into<Body>>(mut self, body: B) -> Self {
        self.inner = self.inner.body(body);
        self
    }
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.inner = self.inner.timeout(timeout);
        self
    }
    pub fn build(self) -> Result<Request, reqwest::Error> {
        self.inner.build()
    }
    pub async fn send(self) -> Result<Response, HttpError> {
        self.client
            .execute(self.inner.build().map_err(HttpError::from)?)
            .await
    }
}

/// Reqwest errors may embed signed URLs; keep only categorical predicates.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct HttpError {
    category: NetworkError,
    connect: bool,
    timeout: bool,
    body: bool,
    request: bool,
}

impl HttpError {
    fn from_category(category: NetworkError) -> Self {
        Self {
            category,
            connect: false,
            timeout: false,
            body: false,
            request: false,
        }
    }
    pub fn category(&self) -> NetworkError {
        self.category
    }
    pub fn is_connect(&self) -> bool {
        self.connect
    }
    pub fn is_timeout(&self) -> bool {
        self.timeout
    }
    pub fn is_body(&self) -> bool {
        self.body
    }
    pub fn is_request(&self) -> bool {
        self.request
    }
}

impl From<reqwest::Error> for HttpError {
    fn from(error: reqwest::Error) -> Self {
        let connect = error.is_connect();
        let timeout = error.is_timeout();
        let category = crate::connect::classify(
            &error,
            if timeout && connect {
                NetworkError::TcpTimeout
            } else {
                NetworkError::TlsProtocolRejected
            },
        );
        Self {
            category,
            connect,
            timeout: timeout
                || matches!(
                    category,
                    NetworkError::TcpTimeout
                        | NetworkError::TlsTimeout
                        | NetworkError::ProxyTimeout
                ),
            body: error.is_body(),
            request: error.is_request(),
        }
    }
}

impl fmt::Debug for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("HttpError").field(&self.category).finish()
    }
}
impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.category.fmt(f)
    }
}
impl Error for HttpError {}

fn allowed_headers(role: Role, headers: &HeaderMap) -> bool {
    !headers.contains_key(reqwest::header::PROXY_AUTHORIZATION)
        && (!matches!(role, Role::Cdn | Role::Upload)
            || (!headers.contains_key(reqwest::header::AUTHORIZATION)
                && !headers.contains_key(reqwest::header::COOKIE)))
}

struct PublicResolver {
    role: Role,
}
impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let role = self.role;
        Box::pin(async move {
            authorize(role, &host, 443)?;
            let addresses = resolve_public(&host, 0).await?;
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn numeric_private_and_credential_urls_fail_before_networking() {
        let http = NetworkContext::off()
            .http_builder(Role::Cdn)
            .unwrap()
            .build()
            .unwrap();
        for url in [
            "https://127.0.0.1/a?secret=canary",
            "https://[::1]/",
            "https://user:password@example.com/",
            "http://example.com/",
        ] {
            let error = http.get(url).send().await.unwrap_err();
            assert_eq!(error.category(), NetworkError::InvalidTarget);
            let rendered = format!("{error:?} {error}");
            for canary in ["canary", "password", "127.0.0.1"] {
                assert!(!rendered.contains(canary));
            }
        }
    }
    #[test]
    fn credential_free_clients_cannot_inherit_account_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_static("canary-secret"),
        );
        for role in [Role::Cdn, Role::Upload] {
            assert!(
                NetworkContext::off()
                    .http_builder(role)
                    .unwrap()
                    .default_headers(headers.clone())
                    .build()
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn diagnostic_debug_never_contains_a_signed_url() {
        let raw = reqwest::Client::new()
            .get("http://invalid host/?token=secret-canary")
            .build()
            .unwrap_err();
        let error = HttpError::from(raw);
        assert!(!format!("{error:?} {error}").contains("canary"));
        assert!(error.source().is_none());
    }
}
