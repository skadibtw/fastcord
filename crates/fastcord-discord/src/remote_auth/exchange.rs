//! `POST /users/@me/remote-auth/login`: trades the gateway's ticket for the
//! token encrypted to this attempt's public key. The request is deliberately
//! unauthenticated; there is no token yet.

use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use super::RemoteAuthError;
use crate::route::API_BASE;

const EXCHANGE_BODY_BYTE_BUDGET: usize = 16 * 1024;

pub(crate) trait TicketExchange: Send + Sync {
    /// Returns the base64 `encrypted_token`. The ticket is a one-shot credential:
    /// implementations must not retry or log it.
    fn exchange(
        &self,
        ticket: &str,
    ) -> impl Future<Output = Result<String, RemoteAuthError>> + Send;
}

pub(crate) struct HttpTicketExchange {
    http: reqwest::Client,
}

impl HttpTicketExchange {
    pub(crate) fn new() -> Result<Self, RemoteAuthError> {
        let http = reqwest::Client::builder()
            .use_rustls_tls()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| RemoteAuthError::Connect)?;
        Ok(Self { http })
    }
}

#[derive(Serialize)]
struct ExchangeRequest<'a> {
    ticket: &'a str,
}

#[derive(Deserialize)]
struct ExchangeResponse {
    encrypted_token: String,
}

impl TicketExchange for HttpTicketExchange {
    async fn exchange(&self, ticket: &str) -> Result<String, RemoteAuthError> {
        let body = serde_json::to_vec(&ExchangeRequest { ticket })
            .map_err(|_| RemoteAuthError::Protocol)?;
        let mut response = self
            .http
            .post(format!("{API_BASE}/users/@me/remote-auth/login"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|_| RemoteAuthError::ExchangeNetwork)?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|len| len > EXCHANGE_BODY_BYTE_BUDGET as u64)
        {
            return Err(RemoteAuthError::Protocol);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| RemoteAuthError::ExchangeNetwork)?
        {
            if chunk.len() > EXCHANGE_BODY_BYTE_BUDGET - bytes.len() {
                return Err(RemoteAuthError::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        classify_response(status, &bytes)
    }
}

/// Maps a response to the encrypted token or a categorical error. Response
/// bodies are inspected but never copied into an error: they may echo input.
pub(crate) fn classify_response(
    status: StatusCode,
    body: &[u8],
) -> Result<String, RemoteAuthError> {
    if status.is_success() {
        return serde_json::from_slice::<ExchangeResponse>(body)
            .map(|response| response.encrypted_token)
            .map_err(|_| RemoteAuthError::Protocol);
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(RemoteAuthError::RateLimited);
    }
    if requires_captcha(body) {
        return Err(RemoteAuthError::Captcha);
    }
    Err(RemoteAuthError::ExchangeRejected(status.as_u16()))
}

fn requires_captcha(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|object| {
            object.contains_key("captcha_key")
                || object.contains_key("captcha_sitekey")
                || object.contains_key("captcha_service")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_yields_the_encrypted_token_and_rejects_malformed_bodies() {
        assert_eq!(
            classify_response(StatusCode::OK, br#"{"encrypted_token":"QUJD"}"#).unwrap(),
            "QUJD"
        );
        for bad in [&b""[..], b"{}", b"[]", br#"{"encrypted_token":null}"#] {
            assert_eq!(
                classify_response(StatusCode::OK, bad).unwrap_err(),
                RemoteAuthError::Protocol
            );
        }
    }

    #[test]
    fn captcha_challenge_is_a_distinct_stop() {
        let fixture =
            include_bytes!("../../../../fixtures/remote-auth/exchange-captcha-required.json");
        assert_eq!(
            classify_response(StatusCode::BAD_REQUEST, fixture).unwrap_err(),
            RemoteAuthError::Captcha
        );
        // Any non-success status carrying a challenge stops; success never does.
        assert_eq!(
            classify_response(StatusCode::FORBIDDEN, br#"{"captcha_sitekey":"x"}"#).unwrap_err(),
            RemoteAuthError::Captcha
        );
    }

    #[test]
    fn other_failures_are_categorical() {
        assert_eq!(
            classify_response(StatusCode::TOO_MANY_REQUESTS, br#"{"retry_after":1.5}"#)
                .unwrap_err(),
            RemoteAuthError::RateLimited
        );
        assert_eq!(
            classify_response(
                StatusCode::BAD_REQUEST,
                br#"{"message":"Invalid ticket","code":50035}"#
            )
            .unwrap_err(),
            RemoteAuthError::ExchangeRejected(400)
        );
        assert_eq!(
            classify_response(StatusCode::BAD_GATEWAY, b"<html>").unwrap_err(),
            RemoteAuthError::ExchangeRejected(502)
        );
    }
}
