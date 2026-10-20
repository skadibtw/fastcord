//! Message requests, Ignore and communication privacy (SPEC §16, issue #80).
//!
//! Every function sends exactly one request on the user's click and never
//! retries; the state changes only when Discord's matching Gateway event
//! (`CHANNEL_UPDATE`/`CHANNEL_DELETE`, `RELATIONSHIP_*`, `USER_SETTINGS_UPDATE`)
//! arrives. Ignore is its own relationship flag (`user_ignored`), never a block.
//!
//! - preview a request: `GET /channels/{id}/messages?limit=5` (a read; nothing
//!   is acknowledged or accepted by it)
//! - accept a request: `PUT /channels/{id}/recipients/@me`
//! - decline a request: `DELETE /channels/{id}` (closes the conversation)
//! - privacy settings: `GET`/`PATCH /users/@me/settings`, only the changed
//!   known fields are sent
//!
//! Wire shapes are from community documentation of the official client and are
//! unverified against a live account (`docs/TESTING.md`).

use fastcord_model::{Message, Snowflake};
use reqwest::Method;
use serde::Deserialize;

use crate::history::{HistoryCursor, page_route, parse_page};
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

/// Messages shown when previewing a request.
pub const PREVIEW_MESSAGES: usize = 5;
/// Server overrides held: more is treated as "not known" and not editable.
pub const MAX_RESTRICTED_GUILDS: usize = 256;

/// Who may send the account a friend request (`friend_source_flags`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FriendSources {
    pub everyone: bool,
    pub mutual_friends: bool,
    pub mutual_guilds: bool,
}

/// What Discord says about the account's privacy settings. A field Discord
/// did not send, or sent in a shape this client does not know, is `None` and
/// is never written back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrivacySettings {
    pub friend_sources: Option<FriendSources>,
    /// Direct messages from members of new servers are off by default.
    pub default_guilds_restricted: Option<bool>,
    /// Servers whose members may not message the account directly.
    pub restricted_guilds: Option<Vec<Snowflake>>,
    /// Scan direct messages for explicit images: 0 off, 1 from non-friends,
    /// 2 from everyone. Another value is kept as unknown.
    pub explicit_filter: Option<u8>,
}

impl PrivacySettings {
    /// Lenient: fields in an unexpected shape are simply unknown.
    pub fn from_value(value: &serde_json::Value) -> Self {
        let friend_sources = value.get("friend_source_flags").and_then(|flags| {
            Some(FriendSources {
                everyone: flags.get("all")?.as_bool()?,
                mutual_friends: flags.get("mutual_friends")?.as_bool()?,
                mutual_guilds: flags.get("mutual_guilds")?.as_bool()?,
            })
        });
        let restricted_guilds = value
            .get("restricted_guilds")
            .and_then(serde_json::Value::as_array)
            .filter(|list| list.len() <= MAX_RESTRICTED_GUILDS)
            .and_then(|list| {
                list.iter()
                    .map(|id| match id {
                        serde_json::Value::String(text) => text.parse::<u64>().ok().map(Snowflake),
                        _ => None,
                    })
                    .collect()
            });
        Self {
            friend_sources,
            default_guilds_restricted: value
                .get("default_guilds_restricted")
                .and_then(serde_json::Value::as_bool),
            restricted_guilds,
            explicit_filter: value
                .get("explicit_content_filter")
                .and_then(serde_json::Value::as_u64)
                .and_then(|level| u8::try_from(level).ok()),
        }
    }

    /// Takes every field `newer` knows, keeping the rest.
    pub fn overlay(&mut self, newer: &Self) {
        if newer.friend_sources.is_some() {
            self.friend_sources = newer.friend_sources;
        }
        if newer.default_guilds_restricted.is_some() {
            self.default_guilds_restricted = newer.default_guilds_restricted;
        }
        if newer.restricted_guilds.is_some() {
            self.restricted_guilds.clone_from(&newer.restricted_guilds);
        }
        if newer.explicit_filter.is_some() {
            self.explicit_filter = newer.explicit_filter;
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The settings as Gateway events have set them since READY, with the order
/// each field last changed in. A panel that read the settings over REST puts
/// back only the fields whose stamp is newer than its request, so a late
/// answer never restores an older value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrivacyLive {
    pub settings: PrivacySettings,
    /// Last change per field: friend sources, default restriction,
    /// restricted servers, explicit filter.
    pub stamps: [u64; 4],
    /// The newest stamp handed out; stamps are never reused or reset.
    pub latest: u64,
}

impl PrivacyLive {
    /// A new session forgets the event overlay but keeps counting.
    pub fn reset(&mut self) {
        *self = Self {
            latest: self.latest,
            ..Self::default()
        };
    }

    /// Applies a `USER_SETTINGS_UPDATE`; returns whether any known field
    /// actually changed (a replay or an unrelated update changes nothing).
    pub fn apply(&mut self, update: &PrivacySettings) -> bool {
        let before = self.settings.clone();
        self.settings.overlay(update);
        let now = [
            self.settings.friend_sources != before.friend_sources,
            self.settings.default_guilds_restricted != before.default_guilds_restricted,
            self.settings.restricted_guilds != before.restricted_guilds,
            self.settings.explicit_filter != before.explicit_filter,
        ];
        if !now.contains(&true) {
            return false;
        }
        self.latest += 1;
        for (stamp, changed) in self.stamps.iter_mut().zip(now) {
            if changed {
                *stamp = self.latest;
            }
        }
        true
    }

    /// `loaded` as it stands after the events newer than `since`.
    pub fn newer_than(&self, since: u64, loaded: &mut PrivacySettings) {
        let s = &self.settings;
        if self.stamps[0] > since {
            loaded.friend_sources = s.friend_sources;
        }
        if self.stamps[1] > since {
            loaded.default_guilds_restricted = s.default_guilds_restricted;
        }
        if self.stamps[2] > since {
            loaded.restricted_guilds.clone_from(&s.restricted_guilds);
        }
        if self.stamps[3] > since {
            loaded.explicit_filter = s.explicit_filter;
        }
    }
}

/// The fields of one `PATCH /users/@me/settings`: only what the user changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrivacyEdit {
    pub friend_sources: Option<FriendSources>,
    pub default_guilds_restricted: Option<bool>,
    /// The whole list as it should be afterwards (the setting is replaced as a
    /// whole), so it is only built from a list that was read completely.
    pub restricted_guilds: Option<Vec<Snowflake>>,
    /// 0 to 2.
    pub explicit_filter: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivacyError {
    /// Nothing changed, so nothing is sent.
    Empty,
    /// A value this client does not offer.
    OutOfRange,
}

/// A failed settings write: invalid input (nothing sent) or Discord's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivacyRequestError {
    Invalid(PrivacyError),
    Rest(RestError),
}

impl From<RestError> for PrivacyRequestError {
    fn from(error: RestError) -> Self {
        Self::Rest(error)
    }
}

pub(crate) fn settings_body(edit: &PrivacyEdit) -> Result<serde_json::Value, PrivacyError> {
    let mut body = serde_json::Map::new();
    if let Some(sources) = edit.friend_sources {
        body.insert(
            "friend_source_flags".into(),
            serde_json::json!({
                "all": sources.everyone,
                "mutual_friends": sources.mutual_friends,
                "mutual_guilds": sources.mutual_guilds,
            }),
        );
    }
    if let Some(restricted) = edit.default_guilds_restricted {
        body.insert("default_guilds_restricted".into(), restricted.into());
    }
    if let Some(list) = &edit.restricted_guilds {
        if list.len() > MAX_RESTRICTED_GUILDS {
            return Err(PrivacyError::OutOfRange);
        }
        body.insert(
            "restricted_guilds".into(),
            list.iter().map(ToString::to_string).collect(),
        );
    }
    if let Some(level) = edit.explicit_filter {
        if level > 2 {
            return Err(PrivacyError::OutOfRange);
        }
        body.insert("explicit_content_filter".into(), level.into());
    }
    if body.is_empty() {
        return Err(PrivacyError::Empty);
    }
    Ok(body.into())
}

fn parse_settings(body: &[u8]) -> Result<PrivacySettings, RestError> {
    #[derive(Deserialize)]
    struct Object(serde_json::Map<String, serde_json::Value>);
    let Object(map) = serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    Ok(PrivacySettings::from_value(&map.into()))
}

fn accept_route(channel: Snowflake) -> Result<Route, RestError> {
    Route::new(Method::PUT, &format!("/channels/{channel}/recipients/@me"))
}

fn decline_route(channel: Snowflake) -> Result<Route, RestError> {
    Route::new(Method::DELETE, &format!("/channels/{channel}"))
}

impl<C: Clock> RestClient<C> {
    /// The newest few messages of a request, newest first. Reading them does
    /// not accept or acknowledge anything.
    pub async fn message_request_preview(
        &self,
        channel: Snowflake,
    ) -> Result<Vec<Message>, RestError> {
        let route = page_route(channel, HistoryCursor::Latest, PREVIEW_MESSAGES)?;
        let response = self
            .execute(RestRequest::new(route, Priority::UserRead))
            .await?;
        let mut page = parse_page(response.body(), channel, HistoryCursor::Latest)?;
        page.truncate(PREVIEW_MESSAGES);
        Ok(page)
    }

    /// Accepts a message request; the conversation joins the ordinary list
    /// when Discord's `CHANNEL_UPDATE` says so.
    pub async fn accept_message_request(&self, channel: Snowflake) -> Result<(), RestError> {
        let request = RestRequest::new(accept_route(channel)?, Priority::UserWrite);
        self.execute(request).await.map(drop)
    }

    /// Declines a message request by closing it.
    pub async fn decline_message_request(&self, channel: Snowflake) -> Result<(), RestError> {
        let request = RestRequest::new(decline_route(channel)?, Priority::UserWrite);
        self.execute(request).await.map(drop)
    }

    /// The account's privacy settings (`GET /users/@me/settings`), read only
    /// when the user opens the section.
    pub async fn privacy_settings(&self) -> Result<PrivacySettings, RestError> {
        let request = RestRequest::new(
            Route::new(Method::GET, "/users/@me/settings")?,
            Priority::UserRead,
        );
        parse_settings(self.execute(request).await?.body())
    }

    /// Writes the changed fields and returns the settings as Discord now has
    /// them. A failure may still have been applied; the caller must not
    /// repeat it by itself.
    pub async fn edit_privacy(
        &self,
        edit: &PrivacyEdit,
    ) -> Result<PrivacySettings, PrivacyRequestError> {
        let body = settings_body(edit).map_err(PrivacyRequestError::Invalid)?;
        let request = RestRequest::new(
            Route::new(Method::PATCH, "/users/@me/settings")?,
            Priority::UserWrite,
        )
        .json(&body)?;
        Ok(parse_settings(self.execute(request).await?.body())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../fixtures/rest/privacy-settings.json");

    fn fixture() -> serde_json::Value {
        serde_json::from_str(FIXTURE).unwrap()
    }

    #[test]
    fn settings_are_read_leniently_and_unknown_shapes_stay_unknown() {
        let full = PrivacySettings::from_value(&fixture()["full"]);
        assert_eq!(
            full.friend_sources,
            Some(FriendSources {
                everyone: false,
                mutual_friends: true,
                mutual_guilds: true
            })
        );
        assert_eq!(full.default_guilds_restricted, Some(true));
        assert_eq!(
            full.restricted_guilds,
            Some(vec![Snowflake(7), Snowflake(9)])
        );
        assert_eq!(full.explicit_filter, Some(1));
        // Empty, null and partial shapes are unknown, not "off".
        assert!(PrivacySettings::from_value(&fixture()["empty"]).is_empty());
        let odd = PrivacySettings::from_value(&fixture()["odd"]);
        assert!(odd.is_empty(), "{odd:?}");
        // A list that is too long is not trusted (it would be written back whole).
        let long = serde_json::json!({
            "restricted_guilds": (1..=MAX_RESTRICTED_GUILDS + 1).map(|n| n.to_string()).collect::<Vec<_>>()
        });
        assert_eq!(PrivacySettings::from_value(&long).restricted_guilds, None);
    }

    #[test]
    fn only_changed_known_fields_are_written() {
        let edit = PrivacyEdit {
            default_guilds_restricted: Some(false),
            ..PrivacyEdit::default()
        };
        assert_eq!(settings_body(&edit).unwrap(), fixture()["patch_default"]);
        let edit = PrivacyEdit {
            friend_sources: Some(FriendSources {
                everyone: false,
                mutual_friends: true,
                mutual_guilds: false,
            }),
            restricted_guilds: Some(vec![Snowflake(7)]),
            explicit_filter: Some(2),
            ..PrivacyEdit::default()
        };
        assert_eq!(settings_body(&edit).unwrap(), fixture()["patch_several"]);
        assert_eq!(
            settings_body(&PrivacyEdit::default()),
            Err(PrivacyError::Empty)
        );
        assert_eq!(
            settings_body(&PrivacyEdit {
                explicit_filter: Some(3),
                ..PrivacyEdit::default()
            }),
            Err(PrivacyError::OutOfRange)
        );
    }

    #[test]
    fn overlay_keeps_what_the_newer_settings_do_not_say() {
        let mut held = PrivacySettings::from_value(&fixture()["full"]);
        held.overlay(&PrivacySettings {
            default_guilds_restricted: Some(false),
            ..PrivacySettings::default()
        });
        assert_eq!(held.default_guilds_restricted, Some(false));
        assert_eq!(held.explicit_filter, Some(1));
        assert!(held.friend_sources.is_some());
    }

    #[test]
    fn live_settings_stamp_only_real_changes_and_never_reach_back() {
        let mut live = PrivacyLive::default();
        let flip = |restricted| PrivacySettings {
            default_guilds_restricted: Some(restricted),
            ..PrivacySettings::default()
        };
        assert!(live.apply(&flip(true)));
        assert!(!live.apply(&flip(true)), "a replay changes nothing");
        assert!(!live.apply(&PrivacySettings::default()), "unrelated update");
        let request_started = live.latest;
        assert!(live.apply(&flip(false)));
        // An answer read before the second event keeps its older value for
        // fields the event did not touch, and takes the newer one for the rest.
        let mut loaded = PrivacySettings {
            default_guilds_restricted: Some(true),
            explicit_filter: Some(1),
            ..PrivacySettings::default()
        };
        live.newer_than(request_started, &mut loaded);
        assert_eq!(loaded.default_guilds_restricted, Some(false));
        assert_eq!(loaded.explicit_filter, Some(1));
        // A new session drops the overlay but never reuses a stamp.
        let before = live.latest;
        live.reset();
        assert!(live.settings.is_empty() && live.latest == before);
        assert!(live.apply(&flip(true)) && live.stamps[1] > before);
    }

    #[test]
    fn answers_that_are_not_objects_are_invalid() {
        assert_eq!(parse_settings(b"[]"), Err(RestError::InvalidJson));
        assert_eq!(parse_settings(b"nonsense"), Err(RestError::InvalidJson));
        assert!(parse_settings(b"{}").unwrap().is_empty());
    }

    #[test]
    fn request_routes_name_the_conversation() {
        let key = accept_route(Snowflake(5)).unwrap();
        assert_eq!(key.key().method(), &Method::PUT);
        assert!(key.key().normalized().ends_with("/recipients/@me"));
        let key = decline_route(Snowflake(5)).unwrap();
        assert_eq!(key.key().method(), &Method::DELETE);
        assert_eq!(key.key().major().channel_id(), Some(Snowflake(5)));
    }
}
