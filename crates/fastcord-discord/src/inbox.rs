//! Inbox requests (SPEC §16, issue #77): the account's recent mentions
//! (`GET /users/@me/mentions?limit=25&roles=..&everyone=..[&before={id}]`)
//! and the notification settings of a server or one conversation
//! (`PATCH /users/@me/guilds/{guild|@me}/settings`: mute with a duration,
//! `message_notifications`, `suppress_everyone`/`suppress_roles`, issue #106).
//! Notification writes are currently quarantined after an account-restriction
//! incident reported while using Alerts. The cause is unconfirmed. Mention
//! reads are unchanged; the settings wire helpers remain for offline validation.

use fastcord_model::{Message, Snowflake};
use reqwest::Method;

use crate::gateway::NotifyLevel;
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

/// Mentions per request (the `limit` asked for).
pub const MENTIONS_PAGE_SIZE: usize = 25;

/// Re-enable only after notification requests have been independently validated.
/// There is intentionally no runtime bypass for this quarantine.
pub const NOTIFICATION_WRITES_ENABLED: bool = false;

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

/// How long a mute lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MuteFor {
    Minutes15,
    Hour1,
    Hours3,
    Hours8,
    Hours24,
    /// Until the user turns it back on.
    Forever,
}

impl MuteFor {
    pub const ALL: [Self; 6] = [
        Self::Minutes15,
        Self::Hour1,
        Self::Hours3,
        Self::Hours8,
        Self::Hours24,
        Self::Forever,
    ];

    fn seconds(self) -> Option<i64> {
        match self {
            Self::Minutes15 => Some(15 * 60),
            Self::Hour1 => Some(3600),
            Self::Hours3 => Some(3 * 3600),
            Self::Hours8 => Some(8 * 3600),
            Self::Hours24 => Some(24 * 3600),
            Self::Forever => None,
        }
    }
}

impl std::fmt::Display for MuteFor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Minutes15 => "For 15 minutes",
            Self::Hour1 => "For 1 hour",
            Self::Hours3 => "For 3 hours",
            Self::Hours8 => "For 8 hours",
            Self::Hours24 => "For 24 hours",
            Self::Forever => "Until I turn it back on",
        })
    }
}

/// One notification setting of a server or conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifySetting {
    Mute(MuteFor),
    Unmute,
    /// `None` inherits (a channel follows its server, a server its default).
    Level(Option<NotifyLevel>),
    /// Server only.
    SuppressEveryone(bool),
    /// Server only.
    SuppressRoles(bool),
}

/// `2026-01-02T03:04:05.000Z`, which is what `end_time` carries.
fn rfc3339(unix: i64) -> String {
    let Ok(t) = time::OffsetDateTime::from_unix_timestamp(unix) else {
        return String::new();
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

fn setting_fields(setting: NotifySetting, now: i64) -> serde_json::Value {
    match setting {
        NotifySetting::Mute(length) => serde_json::json!({
            "muted": true,
            "mute_config": match length.seconds() {
                Some(seconds) => serde_json::json!({
                    "selected_time_window": seconds,
                    "end_time": rfc3339(now.saturating_add(seconds)),
                }),
                None => serde_json::json!({"selected_time_window": -1, "end_time": null}),
            },
        }),
        NotifySetting::Unmute => serde_json::json!({"muted": false, "mute_config": null}),
        // 3 is "use the server's setting".
        NotifySetting::Level(level) => {
            serde_json::json!({"message_notifications": level.map_or(3, NotifyLevel::wire)})
        }
        NotifySetting::SuppressEveryone(on) => serde_json::json!({"suppress_everyone": on}),
        NotifySetting::SuppressRoles(on) => serde_json::json!({"suppress_roles": on}),
    }
}

/// The body for the server itself (`channel` `None`) or one of its channels.
fn setting_body(channel: Option<Snowflake>, setting: NotifySetting, now: i64) -> serde_json::Value {
    let fields = setting_fields(setting, now);
    match channel {
        Some(channel) => serde_json::json!({"channel_overrides": {channel.to_string(): fields}}),
        None => fields,
    }
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
        let setting = if muted {
            NotifySetting::Mute(MuteFor::Forever)
        } else {
            NotifySetting::Unmute
        };
        self.set_notification(guild, Some(channel), setting).await
    }

    /// Changes one notification setting of a server (`channel` `None`) or of
    /// a conversation (`guild` `None` for a DM or group DM). Server-only
    /// settings on a channel, and channel settings of the DMs without a
    /// channel, are refused here before anything is sent.
    pub async fn set_notification(
        &self,
        guild: Option<Snowflake>,
        channel: Option<Snowflake>,
        setting: NotifySetting,
    ) -> Result<(), RestError> {
        let server_only = matches!(
            setting,
            NotifySetting::SuppressEveryone(_) | NotifySetting::SuppressRoles(_)
        );
        if (guild.is_none() && channel.is_none()) || (server_only && channel.is_some()) {
            return Err(RestError::InvalidJson);
        }
        if !NOTIFICATION_WRITES_ENABLED {
            return Err(RestError::NotificationWritesPaused);
        }
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let request = RestRequest::new(mute_route(guild)?, Priority::UserWrite)
            .json(&setting_body(channel, setting, now))?;
        self.execute(request).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn notification_writes_are_rejected_before_any_dispatch() {
        use crate::UserToken;
        use crate::gateway::{ClientProperties, HostOs};

        let client = RestClient::new(
            UserToken::new("offline-test-credential".to_owned()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap();
        for (guild, channel) in [
            (Some(Snowflake(1)), None),
            (Some(Snowflake(1)), Some(Snowflake(2))),
            (None, Some(Snowflake(2))),
        ] {
            assert_eq!(
                client
                    .set_notification(guild, channel, NotifySetting::Mute(MuteFor::Forever))
                    .await,
                Err(RestError::NotificationWritesPaused),
            );
            if let Some(channel) = channel {
                assert_eq!(
                    client.set_channel_muted(guild, channel, false).await,
                    Err(RestError::NotificationWritesPaused)
                );
            }
        }
    }

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
    }

    #[test]
    fn mutes_carry_a_window_and_an_end_time() {
        // 2026-01-02T03:04:05Z
        let now = 1_767_323_045;
        let body = setting_body(
            Some(Snowflake(500)),
            NotifySetting::Mute(MuteFor::Hour1),
            now,
        );
        assert_eq!(
            body,
            serde_json::json!({"channel_overrides": {"500": {"muted": true, "mute_config": {
                "selected_time_window": 3600, "end_time": "2026-01-02T04:04:05.000Z"}}}})
        );
        assert_eq!(
            setting_body(None, NotifySetting::Mute(MuteFor::Forever), now),
            serde_json::json!({"muted": true, "mute_config": {"selected_time_window": -1, "end_time": null}})
        );
        assert_eq!(
            setting_body(Some(Snowflake(5)), NotifySetting::Unmute, now),
            serde_json::json!({"channel_overrides": {"5": {"muted": false, "mute_config": null}}})
        );
        for length in MuteFor::ALL {
            assert_eq!(length.seconds().is_none(), length == MuteFor::Forever);
        }
    }

    #[test]
    fn levels_and_suppressions_are_plain_fields_and_inherit_is_three() {
        let level = |l| setting_body(None, NotifySetting::Level(l), 0);
        assert_eq!(
            level(Some(NotifyLevel::Mentions)),
            serde_json::json!({"message_notifications": 1})
        );
        assert_eq!(level(None), serde_json::json!({"message_notifications": 3}));
        assert_eq!(
            setting_body(None, NotifySetting::SuppressRoles(true), 0),
            serde_json::json!({"suppress_roles": true})
        );
        assert_eq!(
            setting_body(
                Some(Snowflake(9)),
                NotifySetting::Level(Some(NotifyLevel::Nothing)),
                0
            ),
            serde_json::json!({"channel_overrides": {"9": {"message_notifications": 2}}})
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
