use std::fmt;

use reqwest::StatusCode;

/// Network details are deliberately categorical: third-party error strings can
/// contain request URLs, webhook credentials, or other sensitive input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkFailure {
    Timeout,
    Connection,
    Body,
    Other,
}

impl NetworkFailure {
    pub(crate) fn from_reqwest(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else if error.is_connect() {
            Self::Connection
        } else if error.is_body() {
            Self::Body
        } else {
            Self::Other
        }
    }
}

/// The caller decides whether/how to retry, particularly after an ambiguous
/// POST failure. The transport never retries network failures or server errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryableFailure {
    Network(NetworkFailure),
    Server(StatusCode),
}

/// Longest Discord `message` kept from an error body.
pub const REFUSAL_MESSAGE_BYTES: usize = 96;
/// At most this much of an error body is read and parsed.
pub(crate) const ERROR_BODY_BYTE_BUDGET: usize = 4 * 1024;

/// A Discord error message made safe to show: control characters become
/// spaces, anything address- or token-shaped is dropped, and it is cut to
/// `REFUSAL_MESSAGE_BYTES` at a character boundary.
pub(crate) fn clean(message: &str) -> String {
    let spaced: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out = String::new();
    for word in spaced.split_whitespace() {
        let word = if word.contains('@') || word.chars().count() > 32 {
            "..."
        } else {
            word
        };
        let sep = usize::from(!out.is_empty());
        if out.len() + sep + word.len() > REFUSAL_MESSAGE_BYTES {
            break;
        }
        if sep == 1 {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// What Discord said about a refused request: its numeric `code` and a short,
/// control-free `message`, nothing else (no request data, no headers).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    pub status: StatusCode,
    pub code: Option<u32>,
    len: u8,
    text: [u8; REFUSAL_MESSAGE_BYTES],
}

impl Refusal {
    pub fn new(status: StatusCode, code: Option<u32>, message: &str) -> Self {
        let message = clean(message);
        let mut text = [0; REFUSAL_MESSAGE_BYTES];
        let len = message.len();
        text[..len].copy_from_slice(message.as_bytes());
        Self {
            status,
            code,
            len: len as u8,
            text,
        }
    }

    /// Parses a Discord error body; an unparsable one still yields the status.
    pub fn from_body(status: StatusCode, body: &[u8]) -> Self {
        let value: serde_json::Value = if body.len() <= ERROR_BODY_BYTE_BUDGET {
            serde_json::from_slice(body).unwrap_or_default()
        } else {
            serde_json::Value::Null
        };
        let code = value["code"].as_u64().and_then(|c| u32::try_from(c).ok());
        Self::new(status, code, value["message"].as_str().unwrap_or(""))
    }

    pub fn message(&self) -> &str {
        std::str::from_utf8(&self.text[..usize::from(self.len)]).unwrap_or("")
    }
}

impl fmt::Debug for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Refusal")
            .field("status", &self.status.as_u16())
            .field("code", &self.code)
            .field("message", &self.message())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestError {
    /// Discord requires account verification or a challenge. All authenticated
    /// work stops for this client; use the official client before reconnecting.
    AccountVerificationRequired,
    /// Notification-setting writes are quarantined pending live validation.
    NotificationWritesPaused,
    /// A 4xx refusal whose Discord error body was kept; only requests that
    /// opt in (`RestRequest::with_error_detail`) produce it.
    Refused(Refusal),
    AuthenticationRequired,
    PermissionDenied,
    ResourceGone,
    Retryable(RetryableFailure),
    Http(StatusCode),
    InvalidToken,
    InvalidRoute,
    InvalidJson,
    InvalidRateLimit,
    ClientConfiguration,
    RequestBodyTooLarge,
    ResponseBodyTooLarge,
    SchedulerCapacityExceeded,
}

impl fmt::Display for RestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccountVerificationRequired => f.write_str("Discord requires account verification; open the official client before reconnecting"),
            Self::NotificationWritesPaused => f.write_str("Notification changes are paused pending validation; use the official Discord client"),
            Self::AuthenticationRequired => f.write_str("authentication is required"),
            Self::Refused(r) => write!(f, "request refused ({})", r.status),
            Self::PermissionDenied => f.write_str("permission denied for this resource"),
            Self::ResourceGone => f.write_str("the requested resource is gone"),
            Self::Retryable(RetryableFailure::Network(kind)) => {
                write!(
                    f,
                    "network failure ({kind:?}); caller must reconcile before retrying"
                )
            }
            Self::Retryable(RetryableFailure::Server(status)) => {
                write!(
                    f,
                    "server failure ({status}); caller decides whether to retry"
                )
            }
            Self::Http(status) => write!(f, "REST request failed ({status})"),
            Self::InvalidToken => f.write_str("token cannot be used as an authorization header"),
            Self::InvalidRoute => f.write_str("expected an absolute Discord API resource path"),
            Self::InvalidJson => f.write_str("invalid REST JSON payload"),
            Self::InvalidRateLimit => f.write_str("429 response has no usable retry deadline"),
            Self::ClientConfiguration => f.write_str("could not configure the REST client"),
            Self::RequestBodyTooLarge => f.write_str("REST request exceeds the body byte budget"),
            Self::ResponseBodyTooLarge => f.write_str("REST response exceeds the body byte budget"),
            Self::SchedulerCapacityExceeded => f.write_str("REST scheduler byte budget exhausted"),
        }
    }
}

impl std::error::Error for RestError {}

pub(crate) fn status_error(status: StatusCode) -> Option<RestError> {
    match status {
        StatusCode::UNAUTHORIZED => Some(RestError::AuthenticationRequired),
        StatusCode::FORBIDDEN => Some(RestError::PermissionDenied),
        StatusCode::NOT_FOUND => Some(RestError::ResourceGone),
        _ if status.is_server_error() => {
            Some(RestError::Retryable(RetryableFailure::Server(status)))
        }
        _ if status.is_success() || status == StatusCode::TOO_MANY_REQUESTS => None,
        _ => Some(RestError::Http(status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_actionable_and_do_not_hide_server_failures() {
        assert_eq!(
            status_error(StatusCode::UNAUTHORIZED),
            Some(RestError::AuthenticationRequired)
        );
        assert_eq!(
            status_error(StatusCode::FORBIDDEN),
            Some(RestError::PermissionDenied)
        );
        assert_eq!(
            status_error(StatusCode::NOT_FOUND),
            Some(RestError::ResourceGone)
        );
        assert_eq!(
            status_error(StatusCode::BAD_GATEWAY),
            Some(RestError::Retryable(RetryableFailure::Server(
                StatusCode::BAD_GATEWAY
            )))
        );
        assert_eq!(
            status_error(StatusCode::BAD_REQUEST),
            Some(RestError::Http(StatusCode::BAD_REQUEST))
        );
        assert_eq!(status_error(StatusCode::NO_CONTENT), None);
        assert_eq!(status_error(StatusCode::TOO_MANY_REQUESTS), None);
    }

    #[test]
    fn refusal_keeps_only_code_and_a_clean_short_message() {
        let r = Refusal::from_body(
            StatusCode::FORBIDDEN,
            br#"{"message":"You need to verify your account","code":40002}"#,
        );
        assert_eq!(
            (r.code, r.message()),
            (Some(40002), "You need to verify your account")
        );

        // Unparsable and oversized bodies keep the status only.
        let r = Refusal::from_body(StatusCode::FORBIDDEN, b"<html>nope</html>");
        assert_eq!((r.code, r.message()), (None, ""));
        let big = format!(r#"{{"code":1,"message":"{}"}}"#, "x ".repeat(4000));
        let r = Refusal::from_body(StatusCode::FORBIDDEN, big.as_bytes());
        assert_eq!((r.code, r.message()), (None, ""));

        // Controls become spaces; address- and token-shaped words are dropped;
        // long text is cut at a character boundary.
        let r = Refusal::from_body(
            StatusCode::FORBIDDEN,
            br#"{"code":5,"message":"a\u0000b\n\u001b[0m me@example.test AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA ok"}"#,
        );
        assert_eq!(r.message(), "a b [0m ... ... ok");
        let long = Refusal::new(StatusCode::FORBIDDEN, None, &"\u{e9}".repeat(500));
        assert!(long.message().len() <= REFUSAL_MESSAGE_BYTES);
        assert!(!format!("{r:?}").contains("example"));
    }
}
