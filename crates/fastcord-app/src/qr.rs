//! UI-side state of one QR login attempt (ADR 0006). The protocol, keys, and
//! tickets live in `fastcord_discord::RemoteAuth`; this module only holds what
//! the screen needs and is dropped, with the attempt it owns, on any exit.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use fastcord_discord::{RemoteAuth, RemoteAuthError, RemoteAuthEvent, RemoteUser, UserToken};
use iced::futures::{Stream, stream};
use iced::task::Handle;
use iced::widget::qr_code;

/// A worker stream for one attempt. The attempt starts when the stream is first
/// polled (inside iced's Tokio runtime) and ends, closing its socket and
/// dropping its keys, when the stream is dropped, which `Handle::abort` does.
pub fn events() -> impl Stream<Item = RemoteAuthEvent> {
    stream::unfold(None::<RemoteAuth>, |attempt| async move {
        let mut attempt = attempt.unwrap_or_else(RemoteAuth::start);
        let event = attempt.next_event().await?;
        Some((event, Some(attempt)))
    })
}

pub enum QrStage {
    Connecting,
    Showing {
        code: qr_code::Data,
        expires_in: Duration,
    },
    Scanned(RemoteUser),
    Ended(RemoteAuthError),
}

/// Result of applying an event that the caller must act on.
#[derive(Debug)]
pub enum Applied {
    Nothing,
    /// The phone user confirmed; run the normal validation and storage path.
    Authorized(Arc<UserToken>),
}

pub struct QrAttempt {
    /// Messages from earlier attempts (after regenerate/cancel) are ignored.
    pub id: u64,
    pub stage: QrStage,
    /// Aborts the attempt's worker when this state is dropped.
    _worker: Handle,
}

impl fmt::Debug for QrAttempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QrAttempt")
            .field("id", &self.id)
            .field(
                "stage",
                &match self.stage {
                    QrStage::Connecting => "Connecting",
                    QrStage::Showing { .. } => "Showing",
                    QrStage::Scanned(_) => "Scanned",
                    QrStage::Ended(_) => "Ended",
                },
            )
            .finish_non_exhaustive()
    }
}

impl QrAttempt {
    pub fn new(id: u64, worker: Handle) -> Self {
        Self {
            id,
            stage: QrStage::Connecting,
            _worker: worker.abort_on_drop(),
        }
    }

    pub fn apply(&mut self, event: RemoteAuthEvent) -> Applied {
        if matches!(self.stage, QrStage::Ended(_)) {
            return Applied::Nothing;
        }
        match event {
            RemoteAuthEvent::Qr(link) => {
                self.stage = match qr_code::Data::new(link.as_str()) {
                    Ok(code) => QrStage::Showing {
                        code,
                        expires_in: link.expires_in(),
                    },
                    Err(_) => QrStage::Ended(RemoteAuthError::Protocol),
                };
            }
            RemoteAuthEvent::PendingUser(user) => self.stage = QrStage::Scanned(user),
            RemoteAuthEvent::Authorized(token) => return Applied::Authorized(token),
            RemoteAuthEvent::Failed(error) => self.stage = QrStage::Ended(error),
        }
        Applied::Nothing
    }
}

/// "about 6 minutes" / "about 45 seconds": static text, so showing the code
/// needs no per-second redraw timer.
pub fn describe_lifetime(expires_in: Duration) -> String {
    let seconds = expires_in.as_secs();
    if seconds >= 90 {
        format!("about {} minutes", (seconds + 30) / 60)
    } else if seconds <= 1 {
        "about 1 second".to_owned()
    } else {
        format!("about {seconds} seconds")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::QrLink;
    use fastcord_model::Snowflake;
    use iced::Task;

    fn attempt() -> QrAttempt {
        let (_task, handle) = Task::<()>::none().abortable();
        QrAttempt::new(1, handle)
    }

    fn link() -> QrLink {
        QrLink::new(
            "UZ0-kOVzXDZTFVV5_QlpURSO2BQHrtkKWHNpIGoDI0k",
            Duration::from_secs(338),
        )
    }

    fn user() -> RemoteUser {
        RemoteUser {
            id: Snowflake(175_928_847_299_117_063),
            username: "alt_fixture".to_owned(),
            avatar: None,
        }
    }

    #[test]
    fn stages_follow_the_gateway_events() {
        let mut attempt = attempt();
        assert!(matches!(attempt.stage, QrStage::Connecting));
        assert!(matches!(
            attempt.apply(RemoteAuthEvent::Qr(link())),
            Applied::Nothing
        ));
        assert!(matches!(
            attempt.stage,
            QrStage::Showing { expires_in, .. } if expires_in == Duration::from_secs(338)
        ));
        let _ = attempt.apply(RemoteAuthEvent::PendingUser(user()));
        assert!(matches!(&attempt.stage, QrStage::Scanned(u) if u.username == "alt_fixture"));
        let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
        let Applied::Authorized(received) =
            attempt.apply(RemoteAuthEvent::Authorized(Arc::clone(&token)))
        else {
            panic!("authorization must be handed to the caller");
        };
        assert!(Arc::ptr_eq(&token, &received));
    }

    #[test]
    fn a_failed_attempt_is_terminal() {
        let mut attempt = attempt();
        let _ = attempt.apply(RemoteAuthEvent::Qr(link()));
        let _ = attempt.apply(RemoteAuthEvent::Failed(RemoteAuthError::Expired));
        assert!(matches!(
            attempt.stage,
            QrStage::Ended(RemoteAuthError::Expired)
        ));
        // Late events (including a token) can neither revive nor authorize it.
        let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
        assert!(matches!(
            attempt.apply(RemoteAuthEvent::Authorized(token)),
            Applied::Nothing
        ));
        let _ = attempt.apply(RemoteAuthEvent::Qr(link()));
        assert!(matches!(
            attempt.stage,
            QrStage::Ended(RemoteAuthError::Expired)
        ));
    }

    #[test]
    fn debug_output_never_includes_the_code_or_user() {
        let mut attempt = attempt();
        let _ = attempt.apply(RemoteAuthEvent::Qr(link()));
        let debug = format!("{attempt:?}");
        assert!(debug.contains("Showing"));
        assert!(!debug.contains("UZ0"));
        let _ = attempt.apply(RemoteAuthEvent::PendingUser(user()));
        assert!(!format!("{attempt:?}").contains("alt_fixture"));
    }

    #[test]
    fn lifetime_text_is_coarse_and_static() {
        assert_eq!(
            describe_lifetime(Duration::from_secs(338)),
            "about 6 minutes"
        );
        assert_eq!(
            describe_lifetime(Duration::from_secs(120)),
            "about 2 minutes"
        );
        assert_eq!(
            describe_lifetime(Duration::from_secs(89)),
            "about 89 seconds"
        );
        assert_eq!(describe_lifetime(Duration::ZERO), "about 1 second");
    }
}
