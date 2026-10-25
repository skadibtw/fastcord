use std::fmt;
use std::io::{self, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap,
    HeaderValue, ORIGIN, REFERER, USER_AGENT,
};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::attachment::{RefreshRequest, RefreshResponse, RefreshedAttachmentUrl};
use crate::error::{ERROR_BODY_BYTE_BUDGET, PauseReason, Refusal, status_error};
use crate::gateway::{ClientProperties, Identity};
use crate::rate_limit::{RateLimiter, ResponseLimits};
use crate::{
    Clock, MonotonicClock, NetworkFailure, RestError, RetryableFailure, Route, UserToken,
    WriteGroup,
};

const REQUEST_BODY_BYTE_BUDGET: usize = 1024 * 1024;
const RESPONSE_BODY_BYTE_BUDGET: usize = 8 * 1024 * 1024;
const RATE_LIMIT_BODY_BYTE_BUDGET: usize = 64 * 1024;

/// One request is not rescheduled after this many 429s in a row.
const MAX_CONSECUTIVE_429: u32 = 5;
/// How long writes pause after a "slow down" refusal that names no delay. A
/// minute is conservative for 40062 (the whole write side) and short enough
/// not to look like a freeze; reads are never paused by it.
const DEFAULT_WRITE_PAUSE: Duration = Duration::from_secs(60);
/// Opening DMs stays blocked this long after 40003 when Discord names no delay.
const DEFAULT_DM_PAUSE: Duration = Duration::from_secs(600);
/// A stated delay is honoured up to this long.
const MAX_PAUSE: Duration = Duration::from_secs(3600);

/// Write pauses as monotonic deadlines on the limiter's clock.
#[derive(Default)]
struct WritePause {
    all: Option<(Duration, PauseReason)>,
    direct_messages: Option<Duration>,
}

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
    /// Set only for a multipart body; JSON bodies leave it unset.
    content_type: Option<String>,
    error_detail: bool,
    audit_reason: Option<HeaderValue>,
    /// Set by a caller whose route is shared with another feature.
    group: Option<WriteGroup>,
}

/// Discord's audit-log reasons hold at most 512 characters.
pub const MAX_AUDIT_REASON_BYTES: usize = 512;

impl RestRequest {
    /// Adds `X-Audit-Log-Reason`: the text cut to 512 bytes, then percent-encoded.
    /// Blank text adds nothing. The reason is never part of `Debug` output.
    pub fn with_audit_reason(mut self, reason: &str) -> Self {
        let reason = reason.trim();
        let mut end = reason.len().min(MAX_AUDIT_REASON_BYTES);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        let mut encoded = String::new();
        for byte in reason[..end].bytes() {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                encoded.push(char::from(byte));
            } else {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
        self.audit_reason = HeaderValue::from_str(&encoded).ok().filter(|_| end > 0);
        self
    }

    /// Keeps Discord's error code and short message of a 4xx refusal
    /// (`RestError::Refused`) instead of the bare category.
    pub fn with_error_detail(mut self) -> Self {
        self.error_detail = true;
        self
    }

    /// Names the write group of a route that several features share
    /// (`PATCH /channels/{c}`, `PATCH /guilds/{g}`, `DELETE /webhooks/{id}`).
    pub fn in_group(mut self, group: WriteGroup) -> Self {
        self.group = Some(group);
        self
    }

    /// The write group this request belongs to; meaningful for non-GET only.
    pub fn write_group(&self) -> WriteGroup {
        self.group.unwrap_or_else(|| {
            // Only a JSON body is inspected; a multipart one is never a message.
            let body = if self.content_type.is_none() {
                self.body.as_deref()
            } else {
                None
            };
            WriteGroup::classify(
                self.route.key().method(),
                self.route.key().normalized(),
                body,
            )
        })
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
            content_type: None,
            error_detail: false,
            audit_reason: None,
            group: None,
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

    /// A prepared non-JSON body (multipart) within the same byte budget as JSON.
    pub(crate) fn raw(mut self, content_type: String, body: Vec<u8>) -> Result<Self, RestError> {
        if body.len() > REQUEST_BODY_BYTE_BUDGET {
            return Err(RestError::RequestBodyTooLarge);
        }
        self.content_type = Some(content_type);
        self.body = Some(body.into());
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
/// A 401, account verification/challenge, or explicit stop permanently cancels
/// queued and in-flight work, including reads and Gateway reconnection.
pub struct RestClient<C = MonotonicClock> {
    http: reqwest::Client,
    /// The profile this client and the account's Gateway share.
    identity: Identity,
    limiter: RateLimiter<C>,
    writes_restricted: Arc<AtomicBool>,
    /// Bitmask of enabled `WriteGroup`s, shared by clones.
    enabled_writes: Arc<AtomicU64>,
    write_pause: Arc<Mutex<WritePause>>,
}

impl<C> Clone for RestClient<C> {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            identity: self.identity.clone(),
            limiter: self.limiter.clone(),
            writes_restricted: self.writes_restricted.clone(),
            enabled_writes: self.enabled_writes.clone(),
            write_pause: self.write_pause.clone(),
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
        // Validate the whole identity once per client; the fixed part becomes
        // default headers, the part that rotates is added per request.
        auth_headers(&token, &properties)?;
        let mut headers = base_headers(&properties)?;
        headers.insert(AUTHORIZATION, authorization(&token)?);
        // The token wrapper is zeroized here; HTTP internals necessarily own
        // their header copy. No default Debug path exposes that sensitive header.
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .use_rustls_tls()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30));
        #[cfg(feature = "fake-discord")]
        let http = if crate::fake_origin::active() {
            http.https_only(false).no_proxy()
        } else {
            http
        };
        let http = http.build().map_err(|_| RestError::ClientConfiguration)?;
        Ok(Self {
            http,
            identity: Identity::new(properties),
            limiter: RateLimiter::new(clock),
            writes_restricted: Arc::new(AtomicBool::new(false)),
            enabled_writes: Arc::new(AtomicU64::new(WriteGroup::default_mask())),
            write_pause: Arc::default(),
        })
    }

    #[cfg(test)]
    pub(crate) fn limiter(&self) -> &RateLimiter<C> {
        &self.limiter
    }

    /// The profile shared with the account's Gateway.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn stop_authenticated_work(&self) {
        self.limiter.stop();
    }

    pub fn writes_restricted(&self) -> bool {
        self.writes_restricted.load(Ordering::Acquire)
    }

    /// Whether writes of `group` are sent (session-only; see `WriteGroup`).
    pub fn write_group_enabled(&self, group: WriteGroup) -> bool {
        group.in_mask(self.enabled_writes.load(Ordering::Acquire))
    }

    /// Turns one group of unverified writes on or off for this client and its clones.
    pub fn set_write_group(&self, group: WriteGroup, on: bool) {
        let _ = self
            .enabled_writes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |mask| {
                Some(group.set_in(mask, on))
            });
    }

    /// The one choke point for every non-GET request: a refused account stops
    /// all writes, and a group that is not enabled fails before the limiter.
    fn check_write(&self, request: &RestRequest) -> Result<(), RestError> {
        let method = request.route.key().method();
        if matches!(*method, Method::GET | Method::HEAD) {
            return Ok(());
        }
        if self.writes_restricted() {
            return Err(RestError::AccountVerificationRequired);
        }
        let now = self.limiter.now();
        let pause = self.write_pause.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((until, reason)) = pause.all
            && now < until
        {
            return Err(RestError::WritesPaused(reason));
        }
        if let Some(until) = pause.direct_messages
            && now < until
            && request.route.key().normalized() == "/users/@me/channels"
        {
            return Err(RestError::WritesPaused(
                PauseReason::OpeningDirectMessagesTooFast,
            ));
        }
        let group = request.write_group();
        if self.write_group_enabled(group) {
            Ok(())
        } else {
            Err(RestError::WriteNotVerified(group))
        }
    }

    /// 40003 and 40062 mean "slow down", not "stop": writes pause for a while
    /// (40003 also keeps DM opens blocked longer), reads carry on. Returns the
    /// error the refused request itself should report.
    fn observe_slow_down(&self, body: &[u8]) -> Option<RestError> {
        let value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        let named = value["retry_after"]
            .as_f64()
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
            .map(|wait| wait.min(MAX_PAUSE));
        let reason = match value["code"].as_u64()? {
            40003 => PauseReason::OpeningDirectMessagesTooFast,
            40062 => PauseReason::ServiceRateLimited,
            _ => return None,
        };
        let now = self.limiter.now();
        let mut pause = self.write_pause.lock().unwrap_or_else(|p| p.into_inner());
        pause.all = Some((now + named.unwrap_or(DEFAULT_WRITE_PAUSE), reason));
        if reason == PauseReason::OpeningDirectMessagesTooFast {
            pause.direct_messages = Some(now + named.unwrap_or(DEFAULT_DM_PAUSE));
        }
        Some(RestError::WritesPaused(reason))
    }

    fn observe_refusal(&self, status: StatusCode, body: &[u8]) -> bool {
        let value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        let challenge = status.is_client_error()
            && (matches!(value["code"].as_u64(), Some(40002 | 40012 | 50178))
                || value.get("captcha_key").is_some_and(|key| !key.is_null())
                || value
                    .get("captcha_sitekey")
                    .is_some_and(|key| !key.is_null()));
        if challenge {
            self.writes_restricted.store(true, Ordering::Release);
            self.limiter.stop();
        }
        challenge
    }

    /// Lets the account coordinator tear down other authenticated workers after
    /// REST observes a 401 or account restriction. Also completes after stop.
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
        let mut rate_limited = 0;
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
            crate::cookies::store(response.url(), response.headers());
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
                    } else if let Some(paused) = self.observe_slow_down(&body) {
                        error = paused;
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
                if self.observe_refusal(status, &body) {
                    reservation.complete(limits, None);
                    return Err(RestError::AccountVerificationRequired);
                }
                if let Some(paused) = self.observe_slow_down(&body) {
                    reservation.complete(limits, None);
                    return Err(paused);
                }
                let throttle = limits.throttle(&body)?;
                reservation.complete(limits, Some(throttle));
                rate_limited += 1;
                if rate_limited >= MAX_CONSECUTIVE_429 {
                    return Err(RestError::RateLimited);
                }
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
        let method = request.route.key().method().clone();
        let mut builder = if method == Method::GET || method == Method::HEAD {
            self.http.request(method, request.route.url.clone())
        } else {
            // Chromium adds Origin to same-origin requests that are not GET/HEAD.
            self.http
                .request(method, request.route.url.clone())
                .header(ORIGIN, DISCORD_ORIGIN)
        };
        if let Some(cookies) = crate::cookies::header(&request.route.url) {
            builder = builder.header(COOKIE, cookies);
        }
        if let Some(body) = &request.body {
            let content_type = request
                .content_type
                .as_deref()
                .unwrap_or("application/json");
            builder = builder
                .header(CONTENT_TYPE, content_type)
                .body(body.clone());
        }
        if let Some(reason) = &request.audit_reason {
            builder = builder.header("x-audit-log-reason", reason.clone());
        }
        let (super_properties, installation) = self.identity.headers();
        builder = builder.header("x-super-properties", super_properties);
        if let Some(installation) = installation {
            builder = builder.header("x-installation-id", installation);
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
    let mut headers = identity_headers(properties)?;
    headers.insert(AUTHORIZATION, authorization(token)?);
    Ok(headers)
}

fn authorization(token: &UserToken) -> Result<HeaderValue, RestError> {
    if token.expose_secret().is_empty() {
        return Err(RestError::InvalidToken);
    }
    let mut authorization =
        HeaderValue::from_str(token.expose_secret()).map_err(|_| RestError::InvalidToken)?;
    authorization.set_sensitive(true);
    Ok(authorization)
}

/// Everything but `Authorization`: also used by the pre-token password login.
pub(crate) fn identity_headers(properties: &ClientProperties) -> Result<HeaderMap, RestError> {
    let mut headers = base_headers(properties)?;
    // This is the exact properties object used by Gateway Identify.
    headers.insert(
        "x-super-properties",
        HeaderValue::from_str(&crate::gateway::profile::encode(properties))
            .map_err(|_| RestError::ClientConfiguration)?,
    );
    if let Some(installation) = &properties.installation_id {
        headers.insert(
            "x-installation-id",
            HeaderValue::from_str(installation).map_err(|_| RestError::ClientConfiguration)?,
        );
    }
    Ok(headers)
}

pub(crate) const DISCORD_ORIGIN: &str = "https://discord.com";

/// The headers that never change during a run: what the desktop client's
/// Chromium sends on a same-origin `fetch` from the Discord app, plus Discord's
/// own. The decoders behind `accept-encoding` are reqwest's; Chromium's `zstd`
/// is not offered because it is not decoded.
fn base_headers(properties: &ClientProperties) -> Result<HeaderMap, RestError> {
    let value =
        |text: &str| HeaderValue::from_str(text).map_err(|_| RestError::ClientConfiguration);
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(
        ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, deflate, br"),
    );
    headers.insert(ACCEPT_LANGUAGE, value(&properties.accept_language())?);
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://discord.com/channels/@me"),
    );
    headers.insert("sec-ch-ua", value(&properties.sec_ch_ua())?);
    headers.insert("sec-ch-ua-mobile", HeaderValue::from_static("?0"));
    headers.insert(
        "sec-ch-ua-platform",
        value(&format!("\"{}\"", properties.ua_platform()))?,
    );
    headers.insert("sec-fetch-dest", HeaderValue::from_static("empty"));
    headers.insert("sec-fetch-mode", HeaderValue::from_static("cors"));
    headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
    headers.insert("x-discord-locale", value(&properties.system_locale)?);
    if let Some(zone) = &properties.timezone {
        headers.insert("x-discord-timezone", value(zone)?);
    }
    headers.insert(USER_AGENT, value(&properties.browser_user_agent)?);
    // No X-Fingerprint here: the login requests that have no token add the one
    // `fingerprint` fetched for this process.
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
        let properties = ClientProperties::bundled(HostOs::Linux, "en-US");
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
    fn every_request_carries_the_identity_as_it_is_now() {
        let client = RestClient::new(
            UserToken::new("offline-test-credential".to_owned()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap();
        let request = RestRequest::new(
            Route::new(Method::GET, "/users/@me").unwrap(),
            Priority::UserRead,
        );
        let before = client.build_request(&request).unwrap();
        assert_eq!(
            before.headers()["x-super-properties"],
            crate::gateway::profile::encode(&client.identity().properties())
        );
        assert!(!before.headers().contains_key("x-installation-id"));
        // The Gateway adopts READY's installation id on the shared identity.
        let id = "1084179945133187083.JQddgNMmwJPghoBtFmaH7jTmdsw";
        client.clone().identity().set_installation_id(id);
        let after = client.build_request(&request).unwrap();
        assert_eq!(after.headers()["x-installation-id"], id);
        assert_eq!(
            after.headers()["x-super-properties"],
            crate::gateway::profile::encode(&client.identity().properties())
        );
        assert_ne!(
            after.headers()["x-super-properties"],
            before.headers()["x-super-properties"]
        );
    }

    #[test]
    fn the_chromium_header_set_comes_from_the_profile() {
        let properties = ClientProperties {
            timezone: Some("Europe/Moscow".to_owned()),
            ..ClientProperties::bundled(HostOs::Windows, "ru-RU")
        };
        let headers = auth_headers(&UserToken::new("t".to_owned()), &properties).unwrap();
        assert_eq!(headers[ACCEPT], "*/*");
        assert_eq!(headers[ACCEPT_ENCODING], "gzip, deflate, br");
        assert_eq!(headers[ACCEPT_LANGUAGE], "ru-RU,ru;q=0.9");
        assert_eq!(headers["sec-ch-ua"], properties.sec_ch_ua().as_str());
        assert_eq!(headers["sec-ch-ua-mobile"], "?0");
        assert_eq!(headers["sec-ch-ua-platform"], "\"Windows\"");
        assert_eq!(headers["sec-fetch-site"], "same-origin");
        assert_eq!(headers["x-discord-timezone"], "Europe/Moscow");
        assert_eq!(headers["x-discord-locale"], "ru-RU");
        assert!(!headers.contains_key(ORIGIN));
        assert!(
            !ClientProperties::bundled(HostOs::Linux, "en-US")
                .timezone
                .is_some()
        );

        let client = RestClient::new(UserToken::new("t".to_owned()), properties).unwrap();
        let get = RestRequest::new(
            Route::new(Method::GET, "/users/@me").unwrap(),
            Priority::UserRead,
        );
        assert!(
            !client
                .build_request(&get)
                .unwrap()
                .headers()
                .contains_key(ORIGIN)
        );
        let write = RestRequest::new(
            Route::new(Method::POST, "/channels/1/typing").unwrap(),
            Priority::UserWrite,
        );
        assert_eq!(
            client.build_request(&write).unwrap().headers()[ORIGIN],
            "https://discord.com"
        );
    }

    #[test]
    fn invalid_identity_headers_are_configuration_errors_without_secrets() {
        let token = UserToken::new("offline-test-credential".to_owned());
        for invalid in ["bad\r\nheader", "bad\0header"] {
            for field in ["browser_user_agent", "system_locale"] {
                let mut properties = ClientProperties::bundled(HostOs::Linux, "en-US");
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

    #[tokio::test]
    async fn an_account_challenge_stops_reads_writes_and_clones() {
        let client = RestClient::new(
            UserToken::new("offline-test-credential".to_owned()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
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
        assert_eq!(
            client.check_write(&join()),
            Err(RestError::WriteNotVerified(WriteGroup::InviteJoin))
        );
        client.set_write_group(WriteGroup::InviteJoin, true);
        assert!(client.check_write(&join()).is_ok());
        let captcha = br#"{"captcha_key":["captcha-required"],"captcha_sitekey":"k"}"#;
        assert!(client.observe_refusal(StatusCode::BAD_REQUEST, captcha));
        assert_eq!(
            client.check_write(&join()),
            Err(RestError::AccountVerificationRequired)
        );
        let clone = client.clone();
        clone.authentication_required().await;
        assert!(matches!(
            clone.execute(lookup).await,
            Err(RestError::AuthenticationRequired)
        ));
        assert!(matches!(
            clone.execute(join()).await,
            Err(RestError::AuthenticationRequired)
        ));
    }

    #[tokio::test]
    async fn verification_and_revocation_codes_cancel_queued_reads_without_dispatch() {
        use std::task::{Context, Poll, Waker};

        for code in [40002, 40012, 50178] {
            let client = RestClient::new(
                UserToken::new("offline-test-credential".to_owned()),
                ClientProperties::bundled(HostOs::Linux, "en-US"),
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
            let body = format!("{{\"code\":{code}}}");
            assert!(client.observe_refusal(StatusCode::FORBIDDEN, body.as_bytes()));
            assert!(matches!(
                queued.as_mut().poll(&mut context),
                Poll::Ready(Err(RestError::AuthenticationRequired))
            ));
            drop(active);
        }
    }

    #[test]
    fn resource_permissions_do_not_stop_the_account() {
        let client = RestClient::new(
            UserToken::new("offline-test-credential".to_owned()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap();
        for body in [
            br#"{"code":50013}"#.as_slice(),
            br#"{"code":40007}"#,
            b"invalid JSON",
        ] {
            assert!(!client.observe_refusal(StatusCode::FORBIDDEN, body));
        }
        assert!(!client.writes_restricted());
    }

    #[derive(Clone, Default)]
    struct ManualClock(Arc<std::sync::atomic::AtomicU64>);

    impl Clock for ManualClock {
        fn now(&self) -> Duration {
            Duration::from_secs(self.0.load(Ordering::SeqCst))
        }

        async fn sleep_until(&self, _deadline: Duration) {
            std::future::pending::<()>().await;
        }
    }

    #[test]
    fn slow_down_codes_pause_writes_not_reads_and_40003_also_blocks_dm_opens() {
        let clock = ManualClock::default();
        let client = RestClient::with_clock(
            UserToken::new("offline-test-credential".to_owned()),
            clock.clone(),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap();
        // A plain text message (on by default); opening DMs needs its group.
        client.set_write_group(crate::WriteGroup::OpenDm, true);
        let write = |path: &str| {
            RestRequest::new(Route::new(Method::POST, path).unwrap(), Priority::UserWrite)
                .json(&serde_json::json!({"content": "hi"}))
                .unwrap()
        };
        let read = RestRequest::new(
            Route::new(Method::GET, "/users/@me").unwrap(),
            Priority::UserRead,
        );
        // 40062: every write waits a minute; reads and the account carry on.
        assert_eq!(
            client.observe_slow_down(br#"{"code":40062}"#),
            Some(RestError::WritesPaused(PauseReason::ServiceRateLimited))
        );
        assert!(!client.writes_restricted());
        assert_eq!(
            client.check_write(&write("/channels/1/messages")),
            Err(RestError::WritesPaused(PauseReason::ServiceRateLimited))
        );
        assert!(client.check_write(&read).is_ok());
        clock.0.store(59, Ordering::SeqCst);
        assert!(client.check_write(&write("/channels/1/messages")).is_err());
        clock.0.store(61, Ordering::SeqCst);
        assert!(client.check_write(&write("/channels/1/messages")).is_ok());

        // 40003: writes pause briefly, opening DMs stays blocked for 10 minutes.
        assert_eq!(
            client.observe_slow_down(br#"{"code":40003,"message":"too fast"}"#),
            Some(RestError::WritesPaused(
                PauseReason::OpeningDirectMessagesTooFast
            ))
        );
        let open = write("/users/@me/channels");
        assert!(client.check_write(&open).is_err());
        clock.0.store(61 + 120, Ordering::SeqCst);
        assert!(client.check_write(&write("/channels/1/messages")).is_ok());
        assert_eq!(
            client.check_write(&open),
            Err(RestError::WritesPaused(
                PauseReason::OpeningDirectMessagesTooFast
            ))
        );
        clock.0.store(61 + 601, Ordering::SeqCst);
        assert!(client.check_write(&open).is_ok());

        // A stated retry_after replaces both defaults.
        client.observe_slow_down(br#"{"code":40003,"retry_after":5.5}"#);
        clock.0.store(61 + 601 + 6, Ordering::SeqCst);
        assert!(client.check_write(&open).is_ok());
        // Other codes and unreadable bodies are not slow-down signals.
        for body in [
            br#"{"code":50013}"#.as_slice(),
            br#"{"code":40002}"#,
            b"nope",
            b"",
        ] {
            assert_eq!(client.observe_slow_down(body), None);
        }
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

    #[test]
    fn an_audit_reason_is_percent_encoded_capped_and_never_printed() {
        let route = || Route::new(Method::DELETE, "/guilds/1/members/2").unwrap();
        let reason = |text: &str| {
            RestRequest::new(route(), Priority::UserWrite)
                .with_audit_reason(text)
                .audit_reason
        };
        assert_eq!(reason("spam & ads").unwrap(), "spam%20%26%20ads");
        assert_eq!(reason("caf\u{e9}").unwrap(), "caf%C3%A9");
        assert!(reason("  ").is_none());
        // The cut falls on a character boundary, before encoding.
        let long = "\u{e9}".repeat(MAX_AUDIT_REASON_BYTES);
        assert_eq!(reason(&long).unwrap().len(), 256 * 6);
        let request = RestRequest::new(route(), Priority::UserWrite).with_audit_reason("secret");
        assert!(!format!("{request:?}").contains("secret"));
    }

    #[tokio::test]
    async fn stopped_client_and_clones_never_dispatch_even_a_post() {
        let properties = ClientProperties::bundled(HostOs::Linux, "en-US");
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

        let properties = ClientProperties::bundled(HostOs::Linux, "en-US");
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
