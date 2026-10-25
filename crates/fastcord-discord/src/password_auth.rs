//! Email/password login: `POST /auth/login`, optional `POST /auth/mfa/totp`.
//!
//! The requests carry no token (there is none yet) but the same official-client
//! identity headers as every other request (the official desktop profile), plus
//! the `X-Fingerprint` from `GET /experiments`. A CAPTCHA is never solved here: it
//! is returned as a challenge for the caller to show to the person, who then
//! repeats the login with the key their own browser view produced. Response
//! bodies are inspected but never copied into errors or logs: they can echo the
//! login, and success bodies hold the token.

use std::fmt;
use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::UserToken;
use crate::error::{ERROR_BODY_BYTE_BUDGET, clean};
use crate::gateway::ClientProperties;
use crate::rest::identity_headers;
use crate::route::api_base;

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
    /// Any other refusal, e.g. new-location email verification.
    Refused(LoginRefusal),
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
            Self::Refused(refusal) => refusal.fmt(f),
            Self::Protocol => f.write_str("Discord sent an unexpected login response."),
        }
    }
}

impl std::error::Error for PasswordAuthError {}

/// Why Discord refused a login, from its JSON error body: the HTTP status, the
/// body's numeric `code` and `message`, and the nested `errors..._errors[]`
/// codes. Everything is sanitized (see `error::clean`); the body is never kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginRefusal {
    pub status: u16,
    pub code: Option<u32>,
    pub message: String,
    /// Up to four `(CODE, message)` pairs, e.g. `ACCOUNT_LOGIN_VERIFICATION_EMAIL`.
    pub reasons: Vec<(String, String)>,
}

impl LoginRefusal {
    pub(crate) fn from_body(status: StatusCode, body: &[u8]) -> Self {
        let value: serde_json::Value = if body.len() <= ERROR_BODY_BYTE_BUDGET {
            serde_json::from_slice(body).unwrap_or_default()
        } else {
            serde_json::Value::Null
        };
        let mut reasons = Vec::new();
        collect_reasons(&value["errors"], 0, &mut reasons);
        Self {
            status: status.as_u16(),
            code: value["code"].as_u64().and_then(|c| u32::try_from(c).ok()),
            message: clean(value["message"].as_str().unwrap_or("")),
            reasons,
        }
    }

    fn has(&self, code: &str) -> bool {
        self.reasons.iter().any(|(c, _)| c == code)
    }
}

fn collect_reasons(value: &serde_json::Value, depth: usize, out: &mut Vec<(String, String)>) {
    let Some(map) = value.as_object() else {
        return;
    };
    if depth > 4 {
        return;
    }
    for (key, child) in map {
        if key == "_errors" {
            for item in child.as_array().into_iter().flatten() {
                let code = item["code"].as_str().unwrap_or("");
                let enum_like = !code.is_empty()
                    && code.len() <= 64
                    && code
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
                if enum_like && out.len() < 4 {
                    out.push((
                        code.to_owned(),
                        clean(item["message"].as_str().unwrap_or("")),
                    ));
                }
            }
        } else {
            collect_reasons(child, depth + 1, out);
        }
    }
}

impl fmt::Display for LoginRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = self.status;
        if self.has("ACCOUNT_LOGIN_VERIFICATION_EMAIL") {
            write!(
                f,
                "Discord detected a new login location (HTTP {status}). Check the account's e-mail and approve this login, then try again."
            )
        } else if self.has("ACCOUNT_DISABLED") || self.has("ACCOUNT_SCHEDULED_FOR_DELETION") {
            write!(
                f,
                "Discord says this account is disabled (HTTP {status}). Resolve it in an official client."
            )
        } else if self.has("INVALID_LOGIN") || self.has("INVALID_TWO_FACTOR_CODE") {
            write!(
                f,
                "Discord rejected those credentials or that code (HTTP {status})."
            )
        } else if self.has("ACCOUNT_COMPROMISED_RESET_PASSWORD") {
            write!(
                f,
                "Discord requires a password reset for this account (HTTP {status}). Do it in an official client."
            )
        } else {
            write!(f, "Discord refused the login (HTTP {status}")?;
            if let Some(code) = self.code {
                write!(f, ", code {code}")?;
            }
            f.write_str(")")?;
            match self.reasons.first() {
                Some((code, message)) if !message.is_empty() => {
                    write!(f, ": {message} ({code}).")
                }
                Some((code, _)) => write!(f, ": {code}."),
                None if !self.message.is_empty() => write!(f, ": {}.", self.message),
                None => f.write_str("; it may require verification in an official client."),
            }
        }
    }
}

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
        #[cfg(feature = "fake-discord")]
        if crate::fake_origin::active() {
            return Err(PasswordAuthError::Configuration);
        }
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
        let mut request = self.http.post(format!("{}/auth/login", api_base()));
        if let Some(fingerprint) = crate::fingerprint::fingerprint(&self.http).await {
            request = request.header("x-fingerprint", fingerprint);
        }
        if let Some(solution) = captcha {
            request = request.header("x-captcha-key", &solution.key);
            if let Some(rqtoken) = &solution.rqtoken {
                request = request.header("x-captcha-rqtoken", rqtoken);
            }
        }
        let (status, bytes) = send(&self.http, request.body(body)).await?;
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
        let mut request = self
            .http
            .post(format!("{}/auth/mfa/totp", api_base()))
            .body(body);
        if let Some(fingerprint) = crate::fingerprint::fingerprint(&self.http).await {
            request = request.header("x-fingerprint", fingerprint);
        }
        let (status, bytes) = send(&self.http, request).await?;
        match classify_login(status, &bytes)? {
            PasswordLogin::Token(token) => Ok(token),
            _ => Err(PasswordAuthError::Protocol),
        }
    }
}

async fn send(
    http: &reqwest::Client,
    request: reqwest::RequestBuilder,
) -> Result<(StatusCode, Vec<u8>), PasswordAuthError> {
    let request = request
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ORIGIN, crate::rest::DISCORD_ORIGIN)
        .build()
        .map_err(|_| PasswordAuthError::Configuration)?;
    let mut request = request;
    if let Some(cookies) = crate::cookies::header(request.url()) {
        request
            .headers_mut()
            .insert(reqwest::header::COOKIE, cookies);
    }
    let mut response = http
        .execute(request)
        .await
        .map_err(|_| PasswordAuthError::Network)?;
    crate::cookies::store(response.url(), response.headers());
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
    Err(PasswordAuthError::Refused(LoginRefusal::from_body(
        status, body,
    )))
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
        assert!(matches!(
            classify_login(StatusCode::FORBIDDEN, b"not json").unwrap_err(),
            PasswordAuthError::Refused(r) if r.status == 403 && r.reasons.is_empty()
        ));
        for bad in [&b""[..], b"{}", br#"{"token":""}"#, br#"{"mfa":true}"#] {
            assert_eq!(
                classify_login(StatusCode::OK, bad).unwrap_err(),
                PasswordAuthError::Protocol
            );
        }
    }

    fn refusal_text(status: StatusCode, body: &[u8]) -> String {
        classify_login(status, body).unwrap_err().to_string()
    }

    #[test]
    fn new_location_and_disabled_accounts_get_specific_sentences() {
        let text = refusal_text(
            StatusCode::BAD_REQUEST,
            br#"{"code":50035,"message":"Invalid Form Body","errors":{"login":{"_errors":[{"code":"ACCOUNT_LOGIN_VERIFICATION_EMAIL","message":"New login location detected, please check your e-mail."}]}}}"#,
        );
        assert!(
            text.contains("approve this login") && text.contains("HTTP 400"),
            "{text}"
        );
        let text = refusal_text(
            StatusCode::BAD_REQUEST,
            br#"{"errors":{"login":{"_errors":[{"code":"ACCOUNT_DISABLED","message":"x"}]}}}"#,
        );
        assert!(text.contains("disabled"), "{text}");
        // Unmapped: Discord's own wording and codes, nothing else.
        let text = refusal_text(
            StatusCode::BAD_REQUEST,
            br#"{"code":1,"errors":{"login":{"_errors":[{"code":"SOMETHING_NEW","message":"Try later"}]}}}"#,
        );
        assert_eq!(
            text,
            "Discord refused the login (HTTP 400, code 1): Try later (SOMETHING_NEW)."
        );
    }

    #[test]
    fn hostile_bodies_are_bounded_and_not_echoed() {
        // Oversized: ignored, status only.
        let big = format!(r#"{{"message":"{}"}}"#, "a".repeat(10_000));
        let text = refusal_text(StatusCode::BAD_REQUEST, big.as_bytes());
        assert!(text.ends_with("official client."), "{text}");
        // Control characters and secret-shaped words are removed.
        let text = refusal_text(
            StatusCode::FORBIDDEN,
            br#"{"message":"bad\u0007\u001b[31m login me@example.test sk.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA end"}"#,
        );
        assert!(
            !text.contains('\u{7}') && !text.contains('\u{1b}'),
            "{text:?}"
        );
        assert!(
            !text.contains("example") && !text.contains("AAAA"),
            "{text}"
        );
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
