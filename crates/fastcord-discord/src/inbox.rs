//! Inbox requests (SPEC §16, issue #77): the account's recent mentions
//! (`GET /users/@me/mentions?limit=25&roles=..&everyone=..[&before={id}]`)
//! and muting one conversation (`PATCH /users/@me/guilds/{guild|@me}/settings`
//! with `channel_overrides`). Both are sent once, on the user's request only;
//! a repeated mute or unmute sets the same state, so there is no hidden retry.
//! Unverified live (`docs/TESTING.md`).

use fastcord_model::{Message, Snowflake};
use reqwest::Method;

use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

/// Mentions per request (the `limit` asked for).
pub const MENTIONS_PAGE_SIZE: usize = 25;

/// Which mentions the list includes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MentionFilter {
    pub roles: bool,
    pub everyone: bool,
}

impl Default for MentionFilter {
    fn default() -> Self {
        Self {
            roles: true,
            everyone: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MentionsPage {
    pub items: Vec<Message>,
    /// A full page: older mentions may follow `before` the last item.
    pub has_more: bool,
}

fn mentions_route(before: Option<Snowflake>, filter: MentionFilter) -> Result<Route, RestError> {
    let mut path = format!(
        "/users/@me/mentions?limit={MENTIONS_PAGE_SIZE}&roles={}&everyone={}",
        filter.roles, filter.everyone
    );
    if let Some(before) = before {
        path.push_str(&format!("&before={before}"));
    }
    Route::new(Method::GET, &path)
}

fn mute_route(guild: Option<Snowflake>) -> Result<Route, RestError> {
    let scope = guild.map_or_else(|| "@me".to_owned(), |guild| guild.to_string());
    Route::new(
        Method::PATCH,
        &format!("/users/@me/guilds/{scope}/settings"),
    )
}

fn mute_body(channel: Snowflake, muted: bool) -> serde_json::Value {
    serde_json::json!({"channel_overrides": {channel.to_string(): {"muted": muted}}})
}

pub(crate) fn parse_mentions(body: &[u8]) -> Result<MentionsPage, RestError> {
    let items: Vec<Message> = serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    if items.len() > MENTIONS_PAGE_SIZE {
        return Err(RestError::InvalidJson);
    }
    Ok(MentionsPage {
        has_more: items.len() == MENTIONS_PAGE_SIZE,
        items,
    })
}

impl<C: Clock> RestClient<C> {
    /// Reads one page of mentions, newest first. `before` is the ID of the
    /// last mention already held.
    pub async fn recent_mentions(
        &self,
        before: Option<Snowflake>,
        filter: MentionFilter,
    ) -> Result<MentionsPage, RestError> {
        let request = RestRequest::new(mentions_route(before, filter)?, Priority::UserRead);
        let response = self.execute(request).await?;
        parse_mentions(response.body())
    }

    /// Mutes or unmutes one conversation (`guild` `None` for a DM or group
    /// DM). Discord echoes the change as `USER_GUILD_SETTINGS_UPDATE`, which
    /// is what updates the local state.
    pub async fn set_channel_muted(
        &self,
        guild: Option<Snowflake>,
        channel: Snowflake,
        muted: bool,
    ) -> Result<(), RestError> {
        let request = RestRequest::new(mute_route(guild)?, Priority::UserWrite)
            .json(&mute_body(channel, muted))?;
        self.execute(request).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = include_str!("../../../fixtures/rest/mentions-page.json");

    fn path(route: &Route) -> String {
        route
            .url
            .as_str()
            .strip_prefix("https://discord.com/api/v10")
            .unwrap()
            .to_owned()
    }

    #[test]
    fn routes_carry_the_filters_and_the_message_cursor() {
        assert_eq!(
            path(&mentions_route(None, MentionFilter::default()).unwrap()),
            "/users/@me/mentions?limit=25&roles=true&everyone=true"
        );
        let filter = MentionFilter {
            roles: false,
            everyone: true,
        };
        let route = mentions_route(Some(Snowflake(9)), filter).unwrap();
        assert_eq!(
            path(&route),
            "/users/@me/mentions?limit=25&roles=false&everyone=true&before=9"
        );
        assert_eq!(route.key.method(), &Method::GET);
    }

    #[test]
    fn mute_targets_the_guild_or_the_direct_messages_and_names_one_channel() {
        let route = mute_route(Some(Snowflake(40))).unwrap();
        assert_eq!(route.key.method(), &Method::PATCH);
        assert_eq!(path(&route), "/users/@me/guilds/40/settings");
        assert_eq!(
            path(&mute_route(None).unwrap()),
            "/users/@me/guilds/@me/settings"
        );
        assert_eq!(
            mute_body(Snowflake(500), true),
            serde_json::json!({"channel_overrides": {"500": {"muted": true}}})
        );
    }

    #[test]
    fn a_page_decodes_and_is_bounded() {
        let page = parse_mentions(PAGE.as_bytes()).unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(!page.has_more, "a short page is the last");
        assert_eq!(page.items[0].guild_id, Some(Snowflake(40)));
        assert_eq!(page.items[1].guild_id, None);
        let many = format!("[{}]", vec!["{}"; MENTIONS_PAGE_SIZE + 1].join(","));
        assert_eq!(parse_mentions(many.as_bytes()), Err(RestError::InvalidJson));
        assert_eq!(parse_mentions(b"{}"), Err(RestError::InvalidJson));
        assert!(parse_mentions(b"[]").unwrap().items.is_empty());
    }

    #[test]
    fn a_full_page_offers_more() {
        let item = serde_json::from_str::<serde_json::Value>(PAGE).unwrap()[0].clone();
        let body = serde_json::to_vec(&vec![item; MENTIONS_PAGE_SIZE]).unwrap();
        assert!(parse_mentions(&body).unwrap().has_more);
    }
}
