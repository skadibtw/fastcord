//! Acknowledging messages (`POST /channels/{id}/messages/{id}/ack`): moving the
//! account's read cursor.
//!
//! An acknowledgement is idempotent and needs no nonce: repeating one moves the
//! cursor to the same place. It is never retried by the transport.

use fastcord_model::Snowflake;
use serde::Serialize;

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route};

#[derive(Serialize)]
#[serde(untagged)]
enum AckBody {
    /// The user marked the channel read on purpose.
    Manual { manual: bool, mention_count: u32 },
    /// Reading moved the cursor along. `token` is null for the first
    /// acknowledgement of a session.
    Automatic { token: Option<String> },
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
    /// because the newest message was on screen.
    pub async fn ack_message(
        &self,
        channel: Snowflake,
        message: Snowflake,
        manual: bool,
    ) -> Result<(), RestError> {
        let body = if manual {
            AckBody::Manual {
                manual: true,
                mention_count: 0,
            }
        } else {
            AckBody::Automatic { token: None }
        };
        let request =
            RestRequest::new(ack_route(channel, message)?, Priority::UserWrite).json(&body)?;
        self.execute(request).await.map(|_| ())
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
    }
}
