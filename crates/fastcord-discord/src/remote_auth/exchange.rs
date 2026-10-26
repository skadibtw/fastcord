//! `POST /users/@me/remote-auth/login`: trades the gateway's ticket for the
//! token encrypted to this attempt's public key. The request is deliberately
//! unauthenticated; there is no token yet.

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use super::RemoteAuthError;
use crate::gateway::ClientProperties;
use crate::route::api_base;

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
    http: fastcord_net::HttpClient,
}

impl HttpTicketExchange {
    /// The exchange carries the same identity headers as every other request,
    /// like the password login.
    pub(crate) fn new(
        network: &fastcord_net::NetworkContext,
        properties: &ClientProperties,
    ) -> Result<Self, RemoteAuthError> {
        #[cfg(feature = "fake-discord")]
        if crate::fake_origin::active() {
            return Err(RemoteAuthError::Connect);
        }
        let http =
            crate::rest::login_client(network, properties).ok_or(RemoteAuthError::Connect)?;
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
        let mut request = self
            .http
            .post(format!("{}/users/@me/remote-auth/login", api_base()))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ORIGIN, crate::rest::DISCORD_ORIGIN);
        if let Some(fingerprint) = crate::fingerprint::fingerprint(&self.http).await {
            request = request.header("x-fingerprint", fingerprint);
        }
        let request = request
            .body(body)
            .build()
            .map_err(|_| RemoteAuthError::Protocol)?;
        let response = crate::cookies::send(&self.http, request)
            .await
            .map_err(|_| RemoteAuthError::ExchangeNetwork)?;
        let status = response.status();
        let bytes = crate::rest::read_body(response, EXCHANGE_BODY_BYTE_BUDGET)
            .await
            .map_err(|error| match error {
                crate::RestError::ResponseBodyTooLarge => RemoteAuthError::Protocol,
                _ => RemoteAuthError::ExchangeNetwork,
            })?;
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
