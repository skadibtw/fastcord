//! One sentence for a failed REST request (#192). Panels used to carry about
//! seventeen `match`es over `RestError` that disagreed with each other; this
//! decides the text and the retry advice once. A panel passes only the words
//! that genuinely differ: what a 403 means, what a 404 means, what to check
//! after an unconfirmed write, and what a 400 means.
//!
//! The text is fixed or Discord's own short refusal message: no request data,
//! tokens or URLs.

use fastcord_discord::{NetworkFailure, RestError, RetryableFailure};

use crate::i18n::{tr, trf};

#[derive(Clone, Copy, Debug)]
pub struct Wording<'a> {
    /// 403 / permission denied.
    pub forbidden: &'a str,
    /// 404 / gone.
    pub gone: &'a str,
    /// A timeout or 5xx: the write may have been applied; says what to check.
    pub unconfirmed: &'a str,
    /// A 400.
    pub rejected: &'a str,
}

pub const DEFAULT: Wording<'static> = Wording {
    forbidden: "Discord refused this for this account.",
    gone: "Discord could not find that.",
    unconfirmed: "Discord did not confirm. It may have been applied; check before trying again.",
    rejected: "Discord rejected the request.",
};

/// The retry advice class of an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Nothing left this machine; trying again is safe.
    NotSent,
    /// The request may have been received; check before trying again.
    Ambiguous,
    /// Discord answered and said no.
    Refused,
}

pub fn severity(error: &RestError) -> Severity {
    match error {
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection))
        | RestError::Disconnected
        | RestError::WriteNotVerified(_)
        | RestError::WritesPaused(_)
        | RestError::NotificationWritesPaused => Severity::NotSent,
        RestError::Retryable(_) | RestError::InvalidJson | RestError::ResponseBodyTooLarge => {
            Severity::Ambiguous
        }
        _ => Severity::Refused,
    }
}

fn status_of(error: &RestError) -> Option<u16> {
    match error {
        RestError::Refused(refusal) => Some(refusal.status.as_u16()),
        RestError::Http(status) => Some(status.as_u16()),
        RestError::AuthenticationRequired => Some(401),
        RestError::PermissionDenied => Some(403),
        RestError::ResourceGone => Some(404),
        _ => None,
    }
}

fn refused(refusal: &fastcord_discord::Refusal) -> String {
    trf(
        "Discord refused ({}{}): {}",
        &[
            &refusal.status.as_u16(),
            &refusal
                .code
                .map_or_else(String::new, |code| trf(", code {}", &[&code])),
            &refusal.message(),
        ],
    )
}

pub fn describe(error: RestError, w: &Wording) -> String {
    if fastcord_discord::is_mfa_required(&error) {
        return tr("Discord asks for two-factor verification for this change. fastcord does not answer that challenge: complete it in the official Discord client. Nothing was changed here.").to_owned();
    }
    match (&error, status_of(&error)) {
        (RestError::AccountVerificationRequired, _) => tr("Discord requires an account check. Requests are paused. Open the official Discord client, complete its verification, then reconnect fastcord.").to_owned(),
        (RestError::WritesPaused(_), _) => {
            trf("{}. Try again later.", &[&tr(&error.to_string())])
        }
        (RestError::WriteNotVerified(_), _) => tr(fastcord_discord::WRITE_NOT_VERIFIED).to_owned(),
        (RestError::RequestBodyTooLarge, _) => tr("That image is too large to send.").to_owned(),
        (
            RestError::InvalidRateLimit
            | RestError::RateLimited
            | RestError::SchedulerCapacityExceeded,
            _,
        )
        | (_, Some(429)) => {
            tr("Discord is rate limiting requests. Wait a moment and try again.").to_owned()
        }
        (_, Some(401)) => tr("Your session has expired. Log in again.").to_owned(),
        // A 403 with Discord's own specific reason (banned, verification
        // level...) keeps it; plain missing access/permission uses the panel's.
        (RestError::Refused(refusal), Some(403))
            if refusal.code.is_some_and(|c| !matches!(c, 50001 | 50013))
                && !refusal.message().is_empty() =>
        {
            refused(refusal)
        }
        (_, Some(403)) => tr(w.forbidden).to_owned(),
        (_, Some(404)) => tr(w.gone).to_owned(),
        _ => match severity(&error) {
            Severity::NotSent if matches!(error, RestError::Retryable(_)) => {
                tr("Could not reach Discord; nothing was sent.").to_owned()
            }
            Severity::Ambiguous => tr(w.unconfirmed).to_owned(),
            _ => match error {
                RestError::Refused(refusal)
                    if !refusal.message().is_empty() || refusal.status.as_u16() != 400 =>
                {
                    refused(&refusal)
                }
                RestError::Refused(_) => tr(w.rejected).to_owned(),
                RestError::Http(status) if status.as_u16() == 400 => tr(w.rejected).to_owned(),
                other => trf("The request failed: {}.", &[&tr(&other.to_string())]),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::{MFA_REQUIRED, PauseReason, Refusal, WriteGroup};
    use reqwest::StatusCode;

    fn status(code: u16) -> StatusCode {
        StatusCode::from_u16(code).unwrap()
    }

    /// Fails to compile when a `RestError` variant is added without being
    /// listed in [`all`].
    fn listed(error: &RestError) {
        match error {
            RestError::RateLimited
            | RestError::WritesPaused(_)
            | RestError::AccountVerificationRequired
            | RestError::NotificationWritesPaused
            | RestError::WriteNotVerified(_)
            | RestError::Refused(_)
            | RestError::AuthenticationRequired
            | RestError::PermissionDenied
            | RestError::ResourceGone
            | RestError::Retryable(_)
            | RestError::Http(_)
            | RestError::InvalidToken
            | RestError::InvalidRoute
            | RestError::InvalidJson
            | RestError::InvalidRateLimit
            | RestError::ClientConfiguration
            | RestError::RequestBodyTooLarge
            | RestError::ResponseBodyTooLarge
            | RestError::SchedulerCapacityExceeded
            | RestError::Disconnected => {}
        }
    }

    fn all() -> Vec<RestError> {
        let net = |k| RestError::Retryable(RetryableFailure::Network(k));
        let all = vec![
            RestError::RateLimited,
            RestError::WritesPaused(PauseReason::OpeningDirectMessagesTooFast),
            RestError::WritesPaused(PauseReason::ServiceRateLimited),
            RestError::AccountVerificationRequired,
            RestError::NotificationWritesPaused,
            RestError::WriteNotVerified(WriteGroup::ALL[0]),
            RestError::Refused(Refusal::new(status(400), None, "")),
            RestError::Refused(Refusal::new(status(400), Some(50035), "Invalid Form Body")),
            RestError::Refused(Refusal::new(
                status(403),
                Some(50013),
                "Missing Permissions",
            )),
            RestError::Refused(Refusal::new(status(404), None, "")),
            RestError::Refused(Refusal::new(status(429), None, "")),
            RestError::Refused(Refusal::new(
                status(403),
                Some(MFA_REQUIRED),
                "Multi-factor",
            )),
            RestError::AuthenticationRequired,
            RestError::PermissionDenied,
            RestError::ResourceGone,
            net(NetworkFailure::Connection),
            net(NetworkFailure::Timeout),
            net(NetworkFailure::Body),
            net(NetworkFailure::Other),
            RestError::Retryable(RetryableFailure::Server(status(502))),
            RestError::Http(status(400)),
            RestError::Http(status(429)),
            RestError::Http(status(418)),
            RestError::InvalidToken,
            RestError::InvalidRoute,
            RestError::InvalidJson,
            RestError::InvalidRateLimit,
            RestError::ClientConfiguration,
            RestError::RequestBodyTooLarge,
            RestError::ResponseBodyTooLarge,
            RestError::SchedulerCapacityExceeded,
            RestError::Disconnected,
        ];
        all.iter().for_each(listed);
        all
    }

    #[test]
    fn every_error_has_a_sentence_without_secrets_or_placeholders() {
        for error in all() {
            let text = describe(error, &DEFAULT);
            assert!(!text.is_empty(), "{error:?}");
            for bad in ["{}", "://", "Bearer", "mfa."] {
                assert!(!text.contains(bad), "{error:?} -> {text}");
            }
        }
    }

    #[test]
    fn the_same_error_reads_the_same_everywhere() {
        let w = Wording {
            forbidden: "F",
            gone: "G",
            unconfirmed: "U",
            rejected: "R",
        };
        let case = |e: RestError| describe(e, &w);
        for forbidden in [
            RestError::PermissionDenied,
            RestError::Refused(Refusal::new(status(403), None, "x")),
        ] {
            assert_eq!(case(forbidden), "F");
        }
        for gone in [
            RestError::ResourceGone,
            RestError::Refused(Refusal::new(status(404), None, "")),
        ] {
            assert_eq!(case(gone), "G");
        }
        assert_eq!(case(RestError::Http(status(400))), "R");
        let server = RetryableFailure::Server(status(500));
        assert_eq!(case(RestError::Retryable(server)), "U");
        let timeout = RetryableFailure::Network(NetworkFailure::Timeout);
        assert_eq!(case(RestError::Retryable(timeout)), "U");
        assert_eq!(case(RestError::InvalidJson), "U");
        // 60003 wins over the 403 it arrives with.
        let mfa = case(RestError::Refused(Refusal::new(
            status(403),
            Some(MFA_REQUIRED),
            "",
        )));
        assert!(mfa.contains("two-factor"));
        // 429 in every spelling.
        let rate = case(RestError::RateLimited);
        assert_eq!(case(RestError::Http(status(429))), rate);
        assert_eq!(
            case(RestError::Refused(Refusal::new(status(429), None, ""))),
            rate
        );
        assert_eq!(
            case(RestError::AuthenticationRequired),
            tr("Your session has expired. Log in again.")
        );
    }

    #[test]
    fn nothing_sent_is_not_called_ambiguous() {
        let down = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection));
        assert_eq!(severity(&down), Severity::NotSent);
        assert_eq!(
            describe(down, &DEFAULT),
            "Could not reach Discord; nothing was sent."
        );
        let server = RestError::Retryable(RetryableFailure::Server(status(500)));
        assert_eq!(severity(&server), Severity::Ambiguous);
        assert_eq!(severity(&RestError::PermissionDenied), Severity::Refused);
    }
}
