//! Acknowledging messages (`POST /channels/{id}/messages/{id}/ack`): moving the
//! account's read cursor.
//!
//! An acknowledgement is idempotent and needs no nonce: repeating one moves the
//! cursor to the same place. It is never retried by the transport.

use fastcord_model::Snowflake;
use serde::{Deserialize, Serialize};

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route};

#[derive(Serialize)]
#[serde(untagged)]
enum AckBody {
    /// The user marked the channel read on purpose.
    Manual { manual: bool, mention_count: u32 },
    /// Reading moved the cursor along. `token` is null for the first
    /// acknowledgement of a channel; afterwards it is the one Discord's last
    /// answer for that channel carried.
    Automatic { token: Option<String> },
}

/// The longest ack token kept; Discord's are far shorter.
const MAX_TOKEN_BYTES: usize = 1024;

#[derive(Deserialize)]
struct AckAnswer {
    token: Option<String>,
}

/// The token in an ack answer, if it has a usable one.
fn answer_token(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<AckAnswer>(body)
        .ok()?
        .token
        .filter(|token| !token.is_empty() && token.len() <= MAX_TOKEN_BYTES)
}

fn ack_route(channel: Snowflake, message: Snowflake) -> Result<Route, RestError> {
    Route::new(
        Method::POST,
        &format!("/channels/{channel}/messages/{message}/ack"),
    )
}

impl<C: Clock> RestClient<C> {
    /// Marks `message` (and everything before it) of `channel` read. `manual`
    /// is the user's explicit "mark as read"; otherwise the cursor moved
    /// because the newest message was on screen, and `token` is the one the
    /// previous answer for this channel carried. Returns the token of this
    /// answer, to send with the next acknowledgement.
    pub async fn ack_message(
        &self,
        channel: Snowflake,
        message: Snowflake,
        manual: bool,
        token: Option<&str>,
    ) -> Result<Option<String>, RestError> {
        let body = if manual {
            AckBody::Manual {
                manual: true,
                mention_count: 0,
            }
        } else {
            AckBody::Automatic {
                token: token.map(str::to_owned),
            }
        };
        let request =
            RestRequest::new(ack_route(channel, message)?, Priority::UserWrite).json(&body)?;
        let answer = self.execute(request).await?;
        Ok(answer_token(answer.body()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_names_the_channel_and_message_and_bodies_have_the_known_shape() {
        let route = ack_route(Snowflake(5), Snowflake(9)).unwrap();
        assert_eq!(
            route.key().normalized(),
            "/channels/{channel_id}/messages/{id}/ack"
        );
        assert_eq!(
            serde_json::to_value(AckBody::Manual {
                manual: true,
                mention_count: 0
            })
            .unwrap(),
            serde_json::json!({"manual": true, "mention_count": 0})
        );
        assert_eq!(
            serde_json::to_value(AckBody::Automatic { token: None }).unwrap(),
            serde_json::json!({"token": null})
        );
        assert_eq!(
            serde_json::to_value(AckBody::Automatic {
                token: Some("t1".to_owned())
            })
            .unwrap(),
            serde_json::json!({"token": "t1"})
        );
    }

    #[test]
    fn the_token_of_an_answer_is_kept_only_when_usable() {
        assert_eq!(answer_token(br#"{"token":"abc"}"#).as_deref(), Some("abc"));
        for body in [
            br#"{"token":null}"#.as_slice(),
            br#"{"token":""}"#,
            br#"{}"#,
            b"not json",
            b"",
        ] {
            assert_eq!(answer_token(body), None);
        }
        let long = format!("{{\"token\":\"{}\"}}", "a".repeat(MAX_TOKEN_BYTES + 1));
        assert_eq!(answer_token(long.as_bytes()), None);
    }
}
