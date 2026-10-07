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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestError {
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
            Self::AuthenticationRequired => f.write_str("authentication is required"),
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
}
