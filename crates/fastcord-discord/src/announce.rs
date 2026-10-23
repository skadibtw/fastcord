//! Announcement channels (SPEC §16, issue #98): publish one message
//! (`POST /channels/{c}/messages/{m}/crosspost`), follow a channel into a text
//! channel (`POST /channels/{c}/followers`), list the Channel Follower
//! webhooks of a destination (`GET /channels/{c}/webhooks`) and remove one
//! (`DELETE /webhooks/{id}`, which is how a channel is unfollowed). Each
//! function sends exactly one request on the user's click and never retries.
//! The listing keeps type 2 only and never declares a webhook `token`, so no
//! credential of another webhook type can reach the UI or a log. Wire shapes
//! come from community documentation and are unverified against a live account
//! (`docs/TESTING.md`).

use fastcord_model::Snowflake;
use reqwest::Method;
use serde::Deserialize;

use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route, WriteGroup};

/// Followed destinations kept from one answer.
pub const MAX_FOLLOWERS: usize = 100;
const MAX_NAME_CHARS: usize = 100;
/// `type` of a Channel Follower webhook.
const FOLLOWER_TYPE: u8 = 2;

/// One Channel Follower webhook of a destination channel: what it follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Follower {
    pub id: Snowflake,
    /// Source server and channel names, empty when Discord did not say.
    pub source_guild: String,
    pub source_channel: String,
}

impl Follower {
    /// "Server #channel", as shown to the user.
    pub fn label(&self) -> String {
        match (self.source_guild.is_empty(), self.source_channel.is_empty()) {
            (true, true) => "An unknown channel".to_owned(),
            (false, true) => self.source_guild.clone(),
            (true, false) => format!("#{}", self.source_channel),
            (false, false) => format!("{} #{}", self.source_guild, self.source_channel),
        }
    }
}

#[derive(Deserialize)]
struct WireName {
    #[serde(default)]
    name: Option<String>,
}

// No `token` field is declared: serde drops it at parse time.
#[derive(Deserialize)]
struct WireWebhook {
    id: Snowflake,
    #[serde(rename = "type", default)]
    kind: u8,
    #[serde(default)]
    source_guild: Option<WireName>,
    #[serde(default)]
    source_channel: Option<WireName>,
}

/// Reads a `GET /channels/{id}/webhooks` answer: Channel Follower webhooks
/// only, at most [`MAX_FOLLOWERS`], texts capped. `None` for a non-list.
pub fn parse_followers(body: &[u8]) -> Option<Vec<Follower>> {
    let wire: Vec<WireWebhook> = serde_json::from_slice(body).ok()?;
    let name = |n: Option<WireName>| {
        crate::invite::cut_at(n.and_then(|n| n.name).unwrap_or_default(), MAX_NAME_CHARS)
    };
    Some(
        wire.into_iter()
            .filter(|w| w.kind == FOLLOWER_TYPE)
            .take(MAX_FOLLOWERS)
            .map(|w| Follower {
                id: w.id,
                source_guild: name(w.source_guild),
                source_channel: name(w.source_channel),
            })
            .collect(),
    )
}

fn write(method: Method, path: &str) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(Route::new(method, path)?, Priority::UserWrite).with_error_detail())
}

fn crosspost_path(channel: Snowflake, message: Snowflake) -> String {
    format!("/channels/{channel}/messages/{message}/crosspost")
}

fn followers_path(channel: Snowflake) -> String {
    format!("/channels/{channel}/followers")
}

fn webhooks_path(channel: Snowflake) -> String {
    format!("/channels/{channel}/webhooks")
}

fn webhook_path(webhook: Snowflake) -> String {
    format!("/webhooks/{webhook}")
}

fn follow_body(destination: Snowflake) -> serde_json::Value {
    serde_json::json!({ "webhook_channel_id": destination.to_string() })
}

impl<C: Clock> RestClient<C> {
    /// Publishes one announcement to every server following its channel. The
    /// `CROSSPOSTED` flag arrives with `MESSAGE_UPDATE`.
    pub async fn crosspost(&self, channel: Snowflake, message: Snowflake) -> Result<(), RestError> {
        let request = write(Method::POST, &crosspost_path(channel, message))?;
        self.execute(request).await.map(drop)
    }

    /// Follows an announcement channel into `destination`, a text channel
    /// where the account has Manage Webhooks.
    pub async fn follow_channel(
        &self,
        channel: Snowflake,
        destination: Snowflake,
    ) -> Result<(), RestError> {
        let request =
            write(Method::POST, &followers_path(channel))?.json(&follow_body(destination))?;
        self.execute(request).await.map(drop)
    }

    /// The channels a destination follows (needs Manage Webhooks there).
    pub async fn followed_channels(&self, channel: Snowflake) -> Result<Vec<Follower>, RestError> {
        let request = RestRequest::new(
            Route::new(Method::GET, &webhooks_path(channel))?,
            Priority::UserRead,
        );
        parse_followers(self.execute(request).await?.body()).ok_or(RestError::InvalidJson)
    }

    /// Unfollows by deleting the follower webhook.
    pub async fn delete_follower(&self, webhook: Snowflake) -> Result<(), RestError> {
        let request =
            write(Method::DELETE, &webhook_path(webhook))?.in_group(WriteGroup::Announcements);
        self.execute(request).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../fixtures/rest/channel-webhooks-synthetic.json");

    #[test]
    fn only_follower_webhooks_are_listed_with_their_source_and_no_token() {
        let list = parse_followers(FIXTURE.as_bytes()).unwrap();
        assert_eq!(list.len(), 2, "the type 1 webhook is dropped");
        assert_eq!(list[0].id, Snowflake(7001));
        assert_eq!(list[0].label(), "Source Server #news");
        assert_eq!(list[1].label(), "An unknown channel");
        assert!(!format!("{list:?}").contains("credential"));
    }

    #[test]
    fn bad_and_oversized_answers_are_bounded() {
        assert_eq!(parse_followers(b"{}"), None);
        assert_eq!(parse_followers(b"[]").unwrap().len(), 0);
        let many = format!(
            "[{}]",
            vec![r#"{"id":"1","type":2}"#; MAX_FOLLOWERS + 5].join(",")
        );
        assert_eq!(
            parse_followers(many.as_bytes()).unwrap().len(),
            MAX_FOLLOWERS
        );
        let long = format!(
            r#"[{{"id":"1","type":2,"source_guild":{{"name":"{}"}}}}]"#,
            "g".repeat(500)
        );
        assert_eq!(
            parse_followers(long.as_bytes()).unwrap()[0]
                .source_guild
                .chars()
                .count(),
            MAX_NAME_CHARS
        );
    }

    #[test]
    fn routes_and_the_follow_body() {
        assert_eq!(
            crosspost_path(Snowflake(5), Snowflake(6)),
            "/channels/5/messages/6/crosspost"
        );
        assert_eq!(followers_path(Snowflake(5)), "/channels/5/followers");
        assert_eq!(webhooks_path(Snowflake(5)), "/channels/5/webhooks");
        assert_eq!(webhook_path(Snowflake(7001)), "/webhooks/7001");
        assert_eq!(
            follow_body(Snowflake(9)).to_string(),
            r#"{"webhook_channel_id":"9"}"#
        );
        for path in [
            crosspost_path(Snowflake(5), Snowflake(6)),
            followers_path(Snowflake(5)),
            webhook_path(Snowflake(7)),
        ] {
            assert!(write(Method::POST, &path).is_ok());
        }
    }
}
