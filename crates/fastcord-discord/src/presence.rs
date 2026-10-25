//! Setting the account's own status (SPEC §16 milestone 46).
//!
//! One explicit user action is one `PATCH /users/@me/settings`, the call the
//! web client's legacy settings use: `{"status": "idle"}` or
//! `{"custom_status": {"text": "..."}}` (`null` clears it). Discord stores the
//! setting and echoes it to every session as PRESENCE_UPDATE, so no Gateway
//! opcode 3 is sent and the status survives reconnects. Unverified live
//! (`docs/TESTING.md`).

use fastcord_model::{MAX_CUSTOM_STATUS_CHARS, Status};
use reqwest::Method;

use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

/// What the account asked to change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusChange {
    Status(Status),
    /// `None` or blank clears the custom status.
    Custom(Option<String>),
}

fn body(change: &StatusChange) -> Result<serde_json::Value, RestError> {
    Ok(match change {
        // "offline" is never something an account can choose.
        StatusChange::Status(Status::Offline) => return Err(RestError::InvalidRoute),
        StatusChange::Status(status) => serde_json::json!({"status": status.wire()}),
        StatusChange::Custom(text) => {
            let text = text.as_deref().map(str::trim).filter(|t| !t.is_empty());
            serde_json::json!({"custom_status": text.map(|t| serde_json::json!({
                "text": t.chars().take(MAX_CUSTOM_STATUS_CHARS).collect::<String>()
            }))})
        }
    })
}

impl<C: Clock> RestClient<C> {
    pub async fn set_status(&self, change: &StatusChange) -> Result<(), RestError> {
        let request = RestRequest::new(
            Route::new(Method::PATCH, "/users/@me/settings")?,
            Priority::UserWrite,
        )
        .json(&body(change)?)?;
        self.execute(request).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_match_the_settings_shape() {
        assert_eq!(
            body(&StatusChange::Status(Status::Dnd)).unwrap(),
            serde_json::json!({"status": "dnd"})
        );
        assert_eq!(
            body(&StatusChange::Status(Status::Invisible)).unwrap(),
            serde_json::json!({"status": "invisible"})
        );
        assert!(body(&StatusChange::Status(Status::Offline)).is_err());
        assert_eq!(
            body(&StatusChange::Custom(Some("  at lunch ".into()))).unwrap(),
            serde_json::json!({"custom_status": {"text": "at lunch"}})
        );
        for clear in [None, Some("   ".to_owned())] {
            assert_eq!(
                body(&StatusChange::Custom(clear)).unwrap(),
                serde_json::json!({"custom_status": null})
            );
        }
        let long = body(&StatusChange::Custom(Some("x".repeat(500)))).unwrap();
        assert_eq!(
            long["custom_status"]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_CUSTOM_STATUS_CHARS
        );
    }

    #[test]
    fn the_route_is_the_settings_patch() {
        let route = Route::new(Method::PATCH, "/users/@me/settings").unwrap();
        assert_eq!(route.key().method(), Method::PATCH);
        assert_eq!(route.url.path(), "/api/v9/users/@me/settings");
    }
}
