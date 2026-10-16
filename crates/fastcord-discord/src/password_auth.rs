//! Email/password login: `POST /auth/login`, optional `POST /auth/mfa/totp`.
//!
//! The requests carry no token (there is none yet) but the same web-client
//! identity headers as every other request. A CAPTCHA is never solved here: it
//! is returned as a challenge for the caller to show to the person, who then
//! repeats the login with the key their own browser view produced. Response
//! bodies are inspected but never copied into errors or logs: they can echo the
//! login, and success bodies hold the token.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::UserToken;
use crate::gateway::ClientProperties;
use crate::rest::identity_headers;
use crate::route::API_BASE;

const BODY_BYTE_BUDGET: usize = 16 * 1024;
/// Discord accepts far less; this only bounds what is put on the wire.
pub const MAX_CREDENTIAL_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasswordAuthError {
    InvalidInput,
    Configuration,
    Network,
    RateLimited,
    /// Wrong login/password (or wrong two-factor code for `mfa`).
    Rejected,
    /// Any other refusal, e.g. new-location email verification (HTTP status only).
    Refused(u16),
    Protocol,
}

impl fmt::Display for PasswordAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput => f.write_str("Enter an email or phone and a password."),
            Self::Configuration => f.write_str("The client identity could not be built."),
            Self::Network => f.write_str("Could not reach Discord. Login stopped; try again."),
            Self::RateLimited => {
                f.write_str("Discord is rate limiting logins. Wait and try again.")
            }
            Self::Rejected => f.write_str("Discord rejected those credentials or that code."),
            Self::Refused(status) => write!(
                f,
                "Discord refused the login (HTTP {status}); it may require verification in an official client."
            ),
            Self::Protocol => f.write_str("Discord sent an unexpected login response."),
        }
    }
}

impl std::error::Error for PasswordAuthError {}

/// A challenge to show to the person. Never solved or bypassed by this crate.
#[derive(Clone, PartialEq, Eq)]
pub struct CaptchaChallenge {
    pub sitekey: String,
    pub service: String,
    pub rqdata: Option<String>,
    pub rqtoken: Option<String>,
}

impl fmt::Debug for CaptchaChallenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptchaChallenge")
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

/// The response key the person's own CAPTCHA widget produced.
#[derive(Clone)]
pub struct CaptchaSolution {
    pub key: String,
    pub rqtoken: Option<String>,
}

impl fmt::Debug for CaptchaSolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CaptchaSolution(..)")
    }
}

/// Second factor requested after a correct password. `ticket` is one-shot.
#[derive(Clone, PartialEq, Eq)]
pub struct MfaChallenge {
    ticket: String,
    pub totp: bool,
}

impl fmt::Debug for MfaChallenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MfaChallenge")
            .field("totp", &self.totp)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum PasswordLogin {
    Token(UserToken),
    Mfa(MfaChallenge),
    Captcha(CaptchaChallenge),
}

#[derive(Serialize)]
struct LoginRequest<'a> {
    login: &'a str,
    password: &'a str,
    undelete: bool,
    login_source: Option<&'a str>,
    gift_code_sku_id: Option<&'a str>,
}

#[derive(Serialize)]
struct MfaRequest<'a> {
    code: &'a str,
    ticket: &'a str,
    login_source: Option<&'a str>,
    gift_code_sku_id: Option<&'a str>,
}

#[derive(Deserialize)]
struct LoginResponse {
    token: Option<String>,
    #[serde(default)]
    mfa: bool,
    ticket: Option<String>,
    #[serde(default)]
    totp: bool,
}

#[derive(Deserialize)]
struct CaptchaResponse {
    captcha_sitekey: Option<String>,
    captcha_service: Option<String>,
    captcha_rqdata: Option<String>,
    captcha_rqtoken: Option<String>,
}

pub struct PasswordAuth {
    http: reqwest::Client,
}

impl PasswordAuth {
    pub fn new(properties: &ClientProperties) -> Result<Self, PasswordAuthError> {
        let headers = identity_headers(properties).map_err(|_| PasswordAuthError::Configuration)?;
        let http = reqwest::Client::builder()
            .use_rustls_tls()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| PasswordAuthError::Network)?;
        Ok(Self { http })
    }

    /// One attempt; never retried, since a repeat could trip account protection.
    pub async fn login(
        &self,
        login: &str,
        password: &str,
        captcha: Option<&CaptchaSolution>,
    ) -> Result<PasswordLogin, PasswordAuthError> {
        if login.is_empty()
            || password.is_empty()
            || login.len() > MAX_CREDENTIAL_BYTES
            || password.len() > MAX_CREDENTIAL_BYTES
        {
            return Err(PasswordAuthError::InvalidInput);
        }
        let body = serde_json::to_vec(&LoginRequest {
            login,
            password,
            undelete: false,
            login_source: None,
            gift_code_sku_id: None,
        })
        .map_err(|_| PasswordAuthError::Protocol)?;
        let mut request = self.http.post(format!("{API_BASE}/auth/login"));
        if let Some(solution) = captcha {
            request = request.header("x-captcha-key", &solution.key);
            if let Some(rqtoken) = &solution.rqtoken {
                request = request.header("x-captcha-rqtoken", rqtoken);
            }
        }
        let (status, bytes) = send(request.body(body)).await?;
        classify_login(status, &bytes)
    }

    /// Completes a `PasswordLogin::Mfa` with an authenticator (TOTP) code.
    pub async fn totp(
        &self,
        challenge: &MfaChallenge,
        code: &str,
    ) -> Result<UserToken, PasswordAuthError> {
        if !challenge.totp
            || code.is_empty()
            || code.len() > 16
            || !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(PasswordAuthError::InvalidInput);
        }
        let body = serde_json::to_vec(&MfaRequest {
            code,
            ticket: &challenge.ticket,
            login_source: None,
            gift_code_sku_id: None,
        })
        .map_err(|_| PasswordAuthError::Protocol)?;
        let request = self
            .http
            .post(format!("{API_BASE}/auth/mfa/totp"))
            .body(body);
        let (status, bytes) = send(request).await?;
        match classify_login(status, &bytes)? {
            PasswordLogin::Token(token) => Ok(token),
            _ => Err(PasswordAuthError::Protocol),
        }
    }
}

async fn send(
    request: reqwest::RequestBuilder,
) -> Result<(StatusCode, Vec<u8>), PasswordAuthError> {
    let mut response = request
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .send()
        .await
        .map_err(|_| PasswordAuthError::Network)?;
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|len| len > BODY_BYTE_BUDGET as u64)
    {
        return Err(PasswordAuthError::Protocol);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| PasswordAuthError::Network)?
    {
        if chunk.len() > BODY_BYTE_BUDGET - bytes.len() {
            return Err(PasswordAuthError::Protocol);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((status, bytes))
}

pub(crate) fn classify_login(
    status: StatusCode,
    body: &[u8],
) -> Result<PasswordLogin, PasswordAuthError> {
    if status.is_success() {
        let response: LoginResponse =
            serde_json::from_slice(body).map_err(|_| PasswordAuthError::Protocol)?;
        return match (response.token, response.mfa, response.ticket) {
            (Some(token), _, _) if !token.is_empty() => {
                Ok(PasswordLogin::Token(UserToken::new(token)))
            }
            (None, true, Some(ticket)) if !ticket.is_empty() => {
                Ok(PasswordLogin::Mfa(MfaChallenge {
                    ticket,
                    totp: response.totp,
                }))
            }
            _ => Err(PasswordAuthError::Protocol),
        };
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(PasswordAuthError::RateLimited);
    }
    if let Ok(challenge) = serde_json::from_slice::<CaptchaResponse>(body)
        && let (Some(sitekey), Some(service)) =
            (challenge.captcha_sitekey, challenge.captcha_service)
    {
        return Ok(PasswordLogin::Captcha(CaptchaChallenge {
            sitekey,
            service,
            rqdata: challenge.captcha_rqdata,
            rqtoken: challenge.captcha_rqtoken,
        }));
    }
    if status == StatusCode::BAD_REQUEST && mentions_invalid_credentials(body) {
        return Err(PasswordAuthError::Rejected);
    }
    Err(PasswordAuthError::Refused(status.as_u16()))
}

fn mentions_invalid_credentials(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let text = value.to_string();
    text.contains("INVALID_LOGIN")
        || text.contains("INVALID_TWO_FACTOR_CODE")
        || value["code"] == 60008
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_mfa_and_captcha_responses_are_distinguished() {
        let token = classify_login(StatusCode::OK, br#"{"token":"abc","user_id":"1"}"#).unwrap();
        assert!(matches!(token, PasswordLogin::Token(t) if t.expose_secret() == "abc"));

        let mfa = classify_login(
            StatusCode::OK,
            br#"{"mfa":true,"ticket":"t","totp":true,"sms":false}"#,
        )
        .unwrap();
        assert!(matches!(mfa, PasswordLogin::Mfa(m) if m.totp && m.ticket == "t"));

        let captcha = classify_login(
            StatusCode::BAD_REQUEST,
            br#"{"captcha_key":["captcha-required"],"captcha_sitekey":"k","captcha_service":"hcaptcha","captcha_rqdata":"d","captcha_rqtoken":"q"}"#,
        )
        .unwrap();
        assert!(matches!(captcha, PasswordLogin::Captcha(c)
            if c.sitekey == "k" && c.service == "hcaptcha" && c.rqtoken.as_deref() == Some("q")));
    }

    #[test]
    fn failures_are_categorical_and_never_echo_the_body() {
        assert_eq!(
            classify_login(StatusCode::TOO_MANY_REQUESTS, b"{}").unwrap_err(),
            PasswordAuthError::RateLimited
        );
        assert_eq!(
            classify_login(
                StatusCode::BAD_REQUEST,
                br#"{"code":50035,"errors":{"login":{"_errors":[{"code":"INVALID_LOGIN"}]}}}"#
            )
            .unwrap_err(),
            PasswordAuthError::Rejected
        );
        assert_eq!(
            classify_login(StatusCode::FORBIDDEN, br#"{"message":"secret-login"}"#).unwrap_err(),
            PasswordAuthError::Refused(403)
        );
        for bad in [&b""[..], b"{}", br#"{"token":""}"#, br#"{"mfa":true}"#] {
            assert_eq!(
                classify_login(StatusCode::OK, bad).unwrap_err(),
                PasswordAuthError::Protocol
            );
        }
    }

    #[test]
    fn debug_output_hides_tickets_and_captcha_material() {
        let mfa = MfaChallenge {
            ticket: "SECRET-TICKET".into(),
            totp: true,
        };
        let solution = CaptchaSolution {
            key: "SECRET-KEY".into(),
            rqtoken: None,
        };
        let text = format!("{mfa:?} {solution:?}");
        assert!(!text.contains("SECRET"));
    }
}
