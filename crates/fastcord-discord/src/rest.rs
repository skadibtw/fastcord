use std::fmt;
use std::io::{self, Write};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::status_error;
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
}

impl RestRequest {
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
}

impl<C> Clone for RestClient<C> {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            limiter: self.limiter.clone(),
        }
    }
}

impl RestClient {
    pub fn new(token: UserToken) -> Result<Self, RestError> {
        Self::with_clock(token, MonotonicClock::default())
    }
}

impl<C: Clock> RestClient<C> {
    pub fn with_clock(token: UserToken, clock: C) -> Result<Self, RestError> {
        let headers = auth_headers(&token)?;
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
        })
    }

    pub fn stop_authenticated_work(&self) {
        self.limiter.stop();
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
            let reservation = self
                .limiter
                .acquire(
                    request.route.key().clone(),
                    request.priority,
                    request.queued_bytes(),
                )
                .await?;
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
            if let Some(error) = error {
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

fn auth_headers(token: &UserToken) -> Result<HeaderMap, RestError> {
    if token.expose_secret().is_empty() {
        return Err(RestError::InvalidToken);
    }
    let mut authorization =
        HeaderValue::from_str(token.expose_secret()).map_err(|_| RestError::InvalidToken)?;
    authorization.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, authorization);
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
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

    #[test]
    fn authorization_is_raw_and_sensitive() {
        let token = UserToken::new("offline-test-credential".to_owned());
        let headers = auth_headers(&token).unwrap();
        assert_eq!(
            headers[AUTHORIZATION].as_bytes(),
            b"offline-test-credential"
        );
        assert!(headers[AUTHORIZATION].is_sensitive());
        assert!(!format!("{headers:?}").contains("offline-test-credential"));
        assert!(matches!(
            auth_headers(&UserToken::new("bad\r\nheader".to_owned())),
            Err(RestError::InvalidToken)
        ));
        assert!(matches!(
            auth_headers(&UserToken::new(String::new())),
            Err(RestError::InvalidToken)
        ));
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
        let client = RestClient::new(UserToken::new("offline-test-credential".to_owned())).unwrap();
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

        let client = RestClient::new(UserToken::new("offline-test-credential".to_owned())).unwrap();
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
