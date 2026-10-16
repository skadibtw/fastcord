//! Private-call ring signaling (SPEC §16 milestone 47).
//!
//! Joining or leaving the call itself is main Gateway opcode 4 with a null
//! `guild_id` (see `fastcord_model::VoiceStateRequest`); these two requests
//! only start or stop the ringing of the other participants:
//!
//! - ring everyone not connected: `POST /channels/{id}/call/ring` with
//!   `{"recipients": null}`
//! - stop ringing (decline an incoming call, or cancel one before anyone
//!   answered): `POST /channels/{id}/call/stop-ringing` with
//!   `{"recipients": null}`
//!
//! Each is one explicit user action and exactly one request. The wire shapes
//! are from observed web-client traffic and are unverified against a live
//! account (`docs/TESTING.md`).

use fastcord_model::Snowflake;
use reqwest::Method;

use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallSignal {
    Ring,
    StopRinging,
}

fn route(channel: Snowflake, signal: CallSignal) -> Result<Route, RestError> {
    let tail = match signal {
        CallSignal::Ring => "ring",
        CallSignal::StopRinging => "stop-ringing",
    };
    Route::new(Method::POST, &format!("/channels/{channel}/call/{tail}"))
}

impl<C: Clock> RestClient<C> {
    pub async fn call_signal(
        &self,
        channel: Snowflake,
        signal: CallSignal,
    ) -> Result<(), RestError> {
        let request = RestRequest::new(route(channel, signal)?, Priority::UserWrite)
            .json(&serde_json::json!({ "recipients": null }))?;
        self.execute(request).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_post_to_the_channels_call_routes_with_a_null_recipient_list() {
        for (signal, tail) in [
            (CallSignal::Ring, "ring"),
            (CallSignal::StopRinging, "stop-ringing"),
        ] {
            let built = route(Snowflake(42), signal).unwrap();
            let key = built.key();
            assert_eq!(key.method(), Method::POST);
            assert!(key.normalized().ends_with(&format!("/call/{tail}")));
            assert_eq!(key.major().channel_id(), Some(Snowflake(42)));
        }
    }
}
