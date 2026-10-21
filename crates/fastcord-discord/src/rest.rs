use base64::Engine;
use std::fmt;
use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::attachment::{RefreshRequest, RefreshResponse, RefreshedAttachmentUrl};
use crate::error::{ERROR_BODY_BYTE_BUDGET, Refusal, status_error};
use crate::gateway::{CLIENT_FINGERPRINT, ClientProperties};
use crate::rate_limit::{RateLimiter, ResponseLimits};
use crate::{Clock, MonotonicClock, NetworkFailure, RestError, RetryableFailure, Route, UserToken};

const REQUEST_BODY_BYTE_BUDGET: usize = 1024 * 1024;
const RESPONSE_BODY_BYTE_BUDGET: usize = 8 * 1024 * 1024;
const RATE_LIMIT_BODY_BYTE_BUDGET: usize = 64 * 1024;

/// FIFO within each priority; blocked writes do not stall unrelated reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    UserWrite,
    UserRead,
    SpeculativeRead,
}

/// A bounded JSON request. Its Debug output omits both the body and actual URL.
pub struct RestRequest {
    route: Route,
    priority: Priority,
    body: Option<Bytes>,
    error_detail: bool,
}

impl RestRequest {
    /// Keeps Discord's error code and short message of a 4xx refusal
    /// (`RestError::Refused`) instead of the bare category.
    pub fn with_error_detail(mut self) -> Self {
        self.error_detail = true;
        self
    }

    pub fn new(route: Route, priority: Priority) -> Self {
        let priority =
            if route.key().method() == Method::GET || route.key().method() == Method::HEAD {
                priority
            } else {
                Priority::UserWrite
            };
        Self {
            route,
            priority,
            body: None,
            error_detail: false,
        }
    }

    pub fn json<T: Serialize + ?Sized>(mut self, value: &T) -> Result<Self, RestError> {
        let mut output = JsonBuffer {
            bytes: Vec::new(),
            exceeded: false,
        };
        if serde_json::to_writer(&mut output, value).is_err() {
            return Err(if output.exceeded {
                RestError::RequestBodyTooLarge
            } else {
                RestError::InvalidJson
            });
        }
        self.body = Some(output.bytes.into());
        Ok(self)
    }

    fn queued_bytes(&self) -> usize {
        // Covers the request and scheduler records as well as retained URL/body
        // buffers. Responses have a separate, streaming-enforced byte budget.
        1024 + self.route.url.as_str().len() + self.body.as_ref().map_or(0, Bytes::len)
    }
}

impl fmt::Debug for RestRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestRequest")
            .field("route", &self.route)
            .field("priority", &self.priority)
            .field("body_bytes", &self.body.as_ref().map_or(0, Bytes::len))
            .finish()
    }
}

struct JsonBuffer {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for JsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > REQUEST_BODY_BYTE_BUDGET.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("request body byte budget exhausted"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct RestResponse {
    status: StatusCode,
    body: Bytes,
}

impl RestResponse {
    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn into_body(self) -> Bytes {
        self.body
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T, RestError> {
        serde_json::from_slice(&self.body).map_err(|_| RestError::InvalidJson)
    }
}

impl fmt::Debug for RestResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestResponse")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Fixed-origin, rustls-backed REST transport. Cloning does not create another
/// account scheduler. Create a new client only after obtaining a new credential.
/// A 401 (or explicit stop) permanently cancels queued and in-flight work.
pub struct RestClient<C = MonotonicClock> {
    http: reqwest::Client,
    limiter: RateLimiter<C>,
    writes_restricted: Arc<AtomicBool>,
}

impl<C> Clone for RestClient<C> {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            limiter: self.limiter.clone(),
            writes_restricted: self.writes_restricted.clone(),
        }
    }
}

impl RestClient {
    pub fn new(token: UserToken, properties: ClientProperties) -> Result<Self, RestError> {
        Self::with_clock(token, MonotonicClock::default(), properties)
    }
}

impl<C: Clock> RestClient<C> {
    pub fn with_clock(
        token: UserToken,
        clock: C,
        properties: ClientProperties,
    ) -> Result<Self, RestError> {
        // Encode and validate the shared identity once per client, not per request.
        let headers = auth_headers(&token, &properties)?;
        // The token wrapper is zeroized here; HTTP internals necessarily own
        // their header copy. No default Debug path exposes that sensitive header.
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .use_rustls_tls()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| RestError::ClientConfiguration)?;
        Ok(Self {
            http,
            limiter: RateLimiter::new(clock),
            writes_restricted: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn stop_authenticated_work(&self) {
        self.limiter.stop();
    }

    pub fn writes_restricted(&self) -> bool {
        self.writes_restricted.load(Ordering::Acquire)
    }

    fn check_write(&self, request: &RestRequest) -> Result<(), RestError> {
        if self.writes_restricted()
            && !matches!(*request.route.key().method(), Method::GET | Method::HEAD)
        {
            Err(RestError::AccountVerificationRequired)
        } else {
            Ok(())
        }
    }

    fn observe_refusal(&self, status: StatusCode, body: &[u8]) -> bool {
        let value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        let challenge = status.is_client_error()
            && (value["code"].as_u64() == Some(40002)
                || value.get("captcha_key").is_some_and(|key| !key.is_null())
                || value
                    .get("captcha_sitekey")
                    .is_some_and(|key| !key.is_null()));
        if challenge {
            self.writes_restricted.store(true, Ordering::Release);
        }
        challenge
    }

    /// Lets the account coordinator tear down other authenticated workers after
    /// REST observes a 401. Also completes after explicit logout/stop.
    pub async fn authentication_required(&self) {
        self.limiter.authentication_required().await;
    }

    pub async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        priority: Priority,
    ) -> Result<T, RestError> {
        let request = RestRequest::new(Route::new(Method::GET, path)?, priority);
        self.execute(request).await?.json()
    }

    /// Obtains fresh signed CDN URLs through Discord's authenticated
    /// `POST /attachments/refresh-urls`. Callers keep their attachment identity
    /// and replace only the URL they fetch.
    pub async fn refresh_attachment_urls(
        &self,
        urls: &[String],
    ) -> Result<Vec<RefreshedAttachmentUrl>, RestError> {
        let body = RefreshRequest {
            attachment_urls: urls,
        };
        let request =
            RestRequest::new(attachment_refresh_route()?, Priority::UserRead).json(&body)?;
        let response = self.execute(request).await?;
        Ok(response.json::<RefreshResponse>()?.refreshed_urls)
    }

    /// Only confirmed 429 rejections are rescheduled automatically. A network or
    /// 5xx error returns immediately, even for GET: caller policy owns retries,
    /// and an ambiguous POST must be reconciled rather than blindly repeated.
    pub async fn execute(&self, request: RestRequest) -> Result<RestResponse, RestError> {
        tokio::select! {
            biased;
            () = self.limiter.authentication_required() => Err(RestError::AuthenticationRequired),
            response = self.execute_scheduled(request) => response,
        }
    }

    async fn execute_scheduled(&self, request: RestRequest) -> Result<RestResponse, RestError> {
        loop {
            self.check_write(&request)?;
            let reservation = self
                .limiter
                .acquire(
                    request.route.key().clone(),
                    request.priority,
                    request.queued_bytes(),
                )
                .await?;
            self.check_write(&request)?;
            let response = self
                .http
                .execute(self.build_request(&request)?)
                .await
                .map_err(network_error)?;
            let status = response.status();
            let error = self.observe_status(status);
            if error == Some(RestError::AuthenticationRequired) {
                return Err(RestError::AuthenticationRequired);
            }
            let limits = ResponseLimits::from_headers(response.headers(), self.limiter.now());
            if let Some(mut error) = error {
                if status.is_client_error()
                    && let Ok(body) = read_body(response, ERROR_BODY_BYTE_BUDGET).await
                {
                    if self.observe_refusal(status, &body) {
                        error = RestError::AccountVerificationRequired;
                    } else if request.error_detail {
                        error = RestError::Refused(Refusal::from_body(status, &body));
                    }
                }
                reservation.complete(limits, None);
                return Err(error);
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                self.limiter
                    .pause_from_headers(request.route.key(), &limits);
                let body = read_body(response, RATE_LIMIT_BODY_BYTE_BUDGET).await?;
                let throttle = limits.throttle(&body)?;
                reservation.complete(limits, Some(throttle));
                continue;
            }
            let body = read_body(response, RESPONSE_BODY_BYTE_BUDGET).await?;
            reservation.complete(limits, None);
            return Ok(RestResponse { status, body });
        }
    }

    fn observe_status(&self, status: StatusCode) -> Option<RestError> {
        let error = status_error(status);
        if error == Some(RestError::AuthenticationRequired) {
            // Do not wait for an error body before canceling the account.
            self.limiter.stop();
        }
        error
    }

    fn build_request(&self, request: &RestRequest) -> Result<reqwest::Request, RestError> {
        let mut builder = self.http.request(
            request.route.key().method().clone(),
            request.route.url.clone(),
        );
        if let Some(body) = &request.body {
            builder = builder
                .header(CONTENT_TYPE, "application/json")
                .body(body.clone());
        }
        builder.build().map_err(|_| RestError::ClientConfiguration)
    }
}

fn attachment_refresh_route() -> Result<Route, RestError> {
    Route::new(Method::POST, "/attachments/refresh-urls")
}
pub(crate) fn auth_headers(
    token: &UserToken,
    properties: &ClientProperties,
) -> Result<HeaderMap, RestError> {
    if token.expose_secret().is_empty() {
        return Err(RestError::InvalidToken);
    }
    let mut authorization =
        HeaderValue::from_str(token.expose_secret()).map_err(|_| RestError::InvalidToken)?;
    authorization.set_sensitive(true);
    let mut headers = identity_headers(properties)?;
    headers.insert(AUTHORIZATION, authorization);
    Ok(headers)
}

/// Everything but `Authorization`: also used by the pre-token password login.
pub(crate) fn identity_headers(properties: &ClientProperties) -> Result<HeaderMap, RestError> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

    // This is the exact properties object used by Gateway Identify.
    let properties_json =
        serde_json::to_string(properties).map_err(|_| RestError::ClientConfiguration)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&properties_json);
    headers.insert(
        "x-super-properties",
        HeaderValue::from_str(&encoded).map_err(|_| RestError::ClientConfiguration)?,
    );

    headers.insert(
        "x-discord-locale",
        HeaderValue::from_str(&properties.system_locale)
            .map_err(|_| RestError::ClientConfiguration)?,
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&properties.browser_user_agent)
            .map_err(|_| RestError::ClientConfiguration)?,
    );
    headers.insert(
        "x-fingerprint",
        HeaderValue::from_static(CLIENT_FINGERPRINT),
    );

    Ok(headers)
}

fn network_error(error: reqwest::Error) -> RestError {
    RestError::Retryable(RetryableFailure::Network(NetworkFailure::from_reqwest(
        &error,
    )))
}

#[derive(Default)]
struct BodyBuffer {
    first: Option<Bytes>,
    combined: Option<BytesMut>,
    len: usize,
}

impl BodyBuffer {
    fn push(&mut self, chunk: Bytes, budget: usize) -> Result<(), RestError> {
        if chunk.len() > budget.saturating_sub(self.len) {
            return Err(RestError::ResponseBodyTooLarge);
        }
        self.len += chunk.len();
        if let Some(combined) = &mut self.combined {
            combined.extend_from_slice(&chunk);
        } else if let Some(first) = self.first.take() {
            let mut combined = BytesMut::with_capacity(self.len);
            combined.extend_from_slice(&first);
            combined.extend_from_slice(&chunk);
            self.combined = Some(combined);
        } else {
            // The common single-chunk response needs no extra allocation/copy.
            self.first = Some(chunk);
        }
        Ok(())
    }

    fn finish(self) -> Bytes {
        self.combined
            .map(BytesMut::freeze)
            .or(self.first)
            .unwrap_or_default()
    }
}

async fn read_body(mut response: reqwest::Response, budget: usize) -> Result<Bytes, RestError> {
    if response
        .content_length()
        .is_some_and(|len| len > budget as u64)
    {
        return Err(RestError::ResponseBodyTooLarge);
    }
    let mut buffer = BodyBuffer::default();
    while let Some(chunk) = response.chunk().await.map_err(network_error)? {
        buffer.push(chunk, budget)?;
    }
    Ok(buffer.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::HostOs;

    #[test]
    fn authorization_is_raw_and_sensitive() {
        let properties = ClientProperties::web(HostOs::Linux, "en-US", 1);
        let token = UserToken::new("offline-test-credential".to_owned());
        let headers = auth_headers(&token, &properties).unwrap();
        assert_eq!(
            headers[AUTHORIZATION].as_bytes(),
            b"offline-test-credential"
        );
        assert!(headers[AUTHORIZATION].is_sensitive());
        assert!(!format!("{headers:?}").contains("offline-test-credential"));
        assert!(matches!(
            auth_headers(&UserToken::new("bad\r\nheader".to_owned()), &properties),
            Err(RestError::InvalidToken)
        ));
        assert!(matches!(
            auth_headers(&UserToken::new(String::new()), &properties),
            Err(RestError::InvalidToken)
        ));
    }

    #[test]
    fn invalid_identity_headers_are_configuration_errors_without_secrets() {
        let token = UserToken::new("offline-test-credential".to_owned());
        for invalid in ["bad\r\nheader", "bad\0header"] {
            for field in ["browser_user_agent", "system_locale"] {
                let mut properties = ClientProperties::web(HostOs::Linux, "en-US", 1);
                match field {
                    "browser_user_agent" => properties.browser_user_agent = invalid.to_owned(),
                    "system_locale" => properties.system_locale = invalid.to_owned(),
                    _ => unreachable!(),
                }
                let error = auth_headers(&token, &properties).unwrap_err();
                assert_eq!(error, RestError::ClientConfiguration);
                assert!(!format!("{error:?}: {error}").contains(token.expose_secret()));
                assert!(matches!(
                    RestClient::new(UserToken::new(token.expose_secret().to_owned()), properties,),
                    Err(RestError::ClientConfiguration)
                ));
            }
        }
    }

    #[test]
    fn a_join_challenge_pauses_writes_but_not_lookups() {
        let client = RestClient::new(
            UserToken::new("offline-test-credential".to_owned()),
            ClientProperties::web(HostOs::Linux, "en-US", 1),
        )
        .unwrap();
        let join = || {
            RestRequest::new(
                Route::new(Method::POST, "/invites/abc123").unwrap(),
                Priority::UserWrite,
            )
        };
        let lookup = RestRequest::new(
            Route::new(Method::GET, "/invites/abc123?with_counts=true").unwrap(),
            Priority::SpeculativeRead,
        );
        assert!(client.check_write(&join()).is_ok());
        let captcha = br#"{"captcha_key":["captcha-required"],"captcha_sitekey":"k"}"#;
        assert!(client.observe_refusal(StatusCode::BAD_REQUEST, captcha));
        assert_eq!(
            client.check_write(&join()),
            Err(RestError::AccountVerificationRequired)
        );
        assert!(client.check_write(&lookup).is_ok());
    }

    #[test]
    fn expired_attachment_urls_refresh_on_the_global_authenticated_route() {
        let route = attachment_refresh_route().unwrap();
        assert_eq!(route.key().normalized(), "/attachments/refresh-urls");
        assert_eq!(route.key().method(), &Method::POST);
        assert_eq!(route.key().major().channel_id(), None);
    }

    #[test]
    fn json_and_streaming_body_budgets_are_enforced() {
        let route = Route::new(Method::POST, "/channels/1/messages").unwrap();
        let request = RestRequest::new(route.clone(), Priority::SpeculativeRead)
            .json(&serde_json::json!({"content": "hello"}))
            .unwrap();
        assert_eq!(request.priority, Priority::UserWrite);
        assert!(!format!("{request:?}").contains("hello"));
        assert_eq!(request.body.unwrap().as_ref(), b"{\"content\":\"hello\"}");
        let huge = "a".repeat(REQUEST_BODY_BYTE_BUDGET);
        assert!(matches!(
            RestRequest::new(route, Priority::UserWrite).json(&huge),
            Err(RestError::RequestBodyTooLarge)
        ));
        let mut buffer = BodyBuffer::default();
        buffer.push(Bytes::from_static(b"abc"), 5).unwrap();
        buffer.push(Bytes::from_static(b"de"), 5).unwrap();
        assert_eq!(
            buffer.push(Bytes::from_static(b"f"), 5),
            Err(RestError::ResponseBodyTooLarge)
        );
        assert_eq!(buffer.finish().as_ref(), b"abcde");
    }

    #[tokio::test]
    async fn stopped_client_and_clones_never_dispatch_even_a_post() {
        let properties = ClientProperties::web(HostOs::Linux, "en-US", 1);
        let client = RestClient::new(
            UserToken::new("offline-test-credential".to_owned()),
            properties,
        )
        .unwrap();
        let clone = client.clone();
        client.stop_authenticated_work();
        let request = RestRequest::new(
            Route::new(Method::POST, "/channels/1/messages").unwrap(),
            Priority::UserWrite,
        );
        assert!(matches!(
            clone.execute(request).await,
            Err(RestError::AuthenticationRequired)
        ));
        clone.authentication_required().await;
    }

    #[tokio::test]
    async fn observing_401_cancels_queued_requests_before_dispatch() {
        use std::task::{Context, Poll, Waker};

        let properties = ClientProperties::web(HostOs::Linux, "en-US", 1);
        let client = RestClient::new(
            UserToken::new("offline-test-credential".to_owned()),
            properties,
        )
        .unwrap();
        let route = Route::new(Method::GET, "/users/@me").unwrap();
        let active = client
            .limiter
            .acquire(route.key().clone(), Priority::UserRead, 1024)
            .await
            .unwrap();
        let clone = client.clone();
        let mut queued = Box::pin(clone.execute(RestRequest::new(route, Priority::UserRead)));
        let mut context = Context::from_waker(Waker::noop());
        assert!(queued.as_mut().poll(&mut context).is_pending());
        assert_eq!(
            client.observe_status(StatusCode::UNAUTHORIZED),
            Some(RestError::AuthenticationRequired)
        );
        assert!(matches!(
            queued.as_mut().poll(&mut context),
            Poll::Ready(Err(RestError::AuthenticationRequired))
        ));
        drop(active);
        assert!(matches!(
            client
                .get::<serde_json::Value>("/gateway", Priority::UserRead)
                .await,
            Err(RestError::AuthenticationRequired)
        ));
    }
}
