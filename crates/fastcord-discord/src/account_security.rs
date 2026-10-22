//! Account sessions and the read-only security overview (SPEC §16, issue #107).
//!
//! - list the account's sessions: `GET /auth/sessions` (client info and last use)
//! - log sessions out: `POST /auth/sessions/logout` with `{"session_id_hashes":[...]}`
//! - security overview: `GET /users/@me` (`mfa_enabled`, `verified`, whether a `phone` exists)
//!
//! Both session routes are unverified against a live account (`docs/PROTOCOL.md`).
//! Discord may answer the logout with a 401/403, code 60003 and an `mfa`
//! ticket; that is reported as a refusal and never answered here (no MFA
//! re-authentication, no retry). The phone number and email are read only as
//! "present"; they are never kept.

use reqwest::Method;
use serde::Deserialize;

use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

/// Sessions kept from one list.
pub const MAX_SESSIONS: usize = 50;
/// Longest kept client-info text, in characters.
const MAX_INFO_CHARS: usize = 64;
/// Discord's "multi-factor authentication required" error code.
pub const MFA_REQUIRED: u32 = 60003;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthSession {
    /// Opaque id used to log the session out. Not a credential.
    pub id_hash: String,
    pub os: String,
    pub platform: String,
    pub location: String,
    /// Discord's approximate last-use time, ISO 8601, as sent.
    pub last_used: Option<String>,
}

fn cut(text: Option<&str>) -> String {
    text.unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_INFO_CHARS)
        .collect()
}

/// Lenient: a session without an id hash cannot be acted on and is skipped.
pub fn parse_sessions(body: &[u8]) -> Result<Vec<AuthSession>, RestError> {
    #[derive(Deserialize)]
    struct List {
        user_sessions: Vec<serde_json::Value>,
    }
    let list: List = serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    Ok(list
        .user_sessions
        .iter()
        .filter_map(|session| {
            let id_hash = session["id_hash"].as_str().filter(|h| !h.is_empty())?;
            let info = &session["client_info"];
            Some(AuthSession {
                id_hash: id_hash.chars().take(128).collect(),
                os: cut(info["os"].as_str()),
                platform: cut(info["platform"].as_str()),
                location: cut(info["location"].as_str()),
                last_used: session["approx_last_used_time"]
                    .as_str()
                    .map(|t| t.chars().take(40).collect()),
            })
        })
        .take(MAX_SESSIONS)
        .collect())
}

/// The security facts shown read-only. A field Discord did not send is `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SecurityOverview {
    pub mfa_enabled: Option<bool>,
    pub email_verified: Option<bool>,
    /// Whether a phone number is on the account; the number itself is dropped.
    pub phone_present: Option<bool>,
}

pub fn parse_overview(body: &[u8]) -> Result<SecurityOverview, RestError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    Ok(SecurityOverview {
        mfa_enabled: value["mfa_enabled"].as_bool(),
        email_verified: value["verified"].as_bool(),
        phone_present: value.get("phone").map(|phone| phone.is_string()),
    })
}

/// The body of one logout: the hashes exactly as the list gave them.
pub fn logout_body(hashes: &[String]) -> Result<serde_json::Value, RestError> {
    if hashes.is_empty() || hashes.len() > MAX_SESSIONS || hashes.iter().any(String::is_empty) {
        return Err(RestError::InvalidJson);
    }
    Ok(serde_json::json!({ "session_id_hashes": hashes }))
}

/// The refusal that asks for multi-factor or password re-authentication.
pub fn is_mfa_required(error: &RestError) -> bool {
    matches!(error, RestError::Refused(r) if r.code == Some(MFA_REQUIRED))
}

impl<C: Clock> RestClient<C> {
    pub async fn auth_sessions(&self) -> Result<Vec<AuthSession>, RestError> {
        let request = RestRequest::new(
            Route::new(Method::GET, "/auth/sessions")?,
            Priority::UserRead,
        );
        parse_sessions(self.execute(request).await?.body())
    }

    pub async fn security_overview(&self) -> Result<SecurityOverview, RestError> {
        let request = RestRequest::new(Route::new(Method::GET, "/users/@me")?, Priority::UserRead);
        parse_overview(self.execute(request).await?.body())
    }

    /// One logout request, sent once. A 401/403 with code 60003 arrives as
    /// `RestError::Refused` (see [`is_mfa_required`]); it is never retried here.
    pub async fn logout_sessions(&self, hashes: &[String]) -> Result<(), RestError> {
        let request = RestRequest::new(
            Route::new(Method::POST, "/auth/sessions/logout")?,
            Priority::UserWrite,
        )
        .with_error_detail()
        .json(&logout_body(hashes)?)?;
        self.execute(request).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    const SESSIONS: &str = include_str!("../../../fixtures/rest/auth-sessions-synthetic.json");
    const MFA: &str =
        include_str!("../../../fixtures/rest/auth-sessions-mfa-required-synthetic.json");

    #[test]
    fn the_session_list_parses_and_skips_rows_without_an_id() {
        let list = parse_sessions(SESSIONS.as_bytes()).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].id_hash, "synthetic-hash-aaaa");
        assert_eq!(list[0].os, "Windows");
        assert_eq!(list[0].location, "Exampleville, Exampleland");
        assert!(
            list[0]
                .last_used
                .as_deref()
                .unwrap()
                .starts_with("2026-10-20")
        );
        assert_eq!(list[2].os, "");
        assert_eq!(list[2].last_used, None);
        assert_eq!(parse_sessions(b"{}"), Err(RestError::InvalidJson));
    }

    #[test]
    fn the_list_is_capped_and_text_is_cleaned() {
        let rows: Vec<_> = (0..80)
            .map(|i| serde_json::json!({"id_hash": format!("h{i}"), "client_info": {"os": "a\nb"}}))
            .collect();
        let body = serde_json::json!({ "user_sessions": rows }).to_string();
        let list = parse_sessions(body.as_bytes()).unwrap();
        assert_eq!(list.len(), MAX_SESSIONS);
        assert_eq!(list[0].os, "ab");
    }

    #[test]
    fn the_overview_keeps_only_booleans() {
        let body = br#"{"mfa_enabled":true,"verified":false,"phone":"+15550100","email":"x@example.invalid"}"#;
        let overview = parse_overview(body).unwrap();
        assert_eq!(
            overview,
            SecurityOverview {
                mfa_enabled: Some(true),
                email_verified: Some(false),
                phone_present: Some(true),
            }
        );
        let none = parse_overview(br#"{"phone":null}"#).unwrap();
        assert_eq!(none.phone_present, Some(false));
        assert_eq!(parse_overview(b"{}").unwrap(), SecurityOverview::default());
        assert!(!format!("{overview:?}").contains("5550100"));
    }

    #[test]
    fn the_logout_body_carries_the_hashes_and_rejects_nothing_to_send() {
        let body = logout_body(&["a".to_owned(), "b".to_owned()]).unwrap();
        assert_eq!(body.to_string(), r#"{"session_id_hashes":["a","b"]}"#);
        assert_eq!(logout_body(&[]), Err(RestError::InvalidJson));
        assert_eq!(logout_body(&[String::new()]), Err(RestError::InvalidJson));
    }

    #[test]
    fn code_60003_is_recognised_in_a_401_or_403_refusal() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let refusal = crate::Refusal::from_body(status, MFA.as_bytes());
            let error = RestError::Refused(refusal);
            assert!(is_mfa_required(&error));
            assert_eq!(refusal.message(), "Multi-factor authentication required");
        }
        let other = crate::Refusal::from_body(StatusCode::FORBIDDEN, br#"{"code":50013}"#);
        assert!(!is_mfa_required(&RestError::Refused(other)));
        assert!(!is_mfa_required(&RestError::PermissionDenied));
    }
}
