//! Server and channel management for communication (SPEC §16, issue #93):
//! create a server, edit its identity and conversation settings, transfer
//! or delete it, create/edit/delete/reorder channels and categories, and
//! list voice regions. Each function sends exactly one request on the user's
//! click and never retries; the server list and the channel list change only
//! when Discord's `GUILD_CREATE`/`GUILD_UPDATE`/`GUILD_DELETE` and
//! `CHANNEL_CREATE`/`CHANNEL_UPDATE`/`CHANNEL_DELETE` arrive. A challenge
//! (CAPTCHA, verification, two-factor) is never answered here: a CAPTCHA or
//! verification answer pauses every write (`AccountVerificationRequired`) and
//! any other refusal keeps Discord's own reason. Billing, boosts, vanity URLs,
//! monetization and paid media channels are not offered. Wire shapes come from
//! community documentation of the official client and are unverified against a
//! live account (`docs/TESTING.md`).

use fastcord_model::{ChannelKind, Snowflake};
use reqwest::Method;
use serde::Deserialize;

use crate::image_data::{ImageError, data_uri};
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

pub const MIN_GUILD_NAME_CHARS: usize = 2;
pub const MAX_GUILD_NAME_CHARS: usize = 100;
pub const MAX_CHANNEL_NAME_CHARS: usize = 100;
pub const MAX_CHANNEL_TOPIC_CHARS: usize = 1024;
pub const MAX_GUILD_DESCRIPTION_CHARS: usize = 120;
/// Voice user limit; 0 is no limit.
pub const MAX_VOICE_USER_LIMIT: u32 = 99;
pub const MIN_BITRATE: u32 = 8_000;
/// The bitrate every server allows. Discord does not report a server's own
/// maximum, so a higher value is offered only up to what a channel already has.
pub const BASE_BITRATE: u32 = 64_000;
/// Channels sent in one reorder request.
pub const MAX_PLACEMENTS: usize = 500;
/// Voice regions kept from one answer.
pub const MAX_REGIONS: usize = 32;
const MAX_REGION_TEXT: usize = 64;

/// The slowmode presets of the official client, in seconds.
pub const SLOWMODE: [(u32, &str); 14] = [
    (0, "Off"),
    (5, "5 seconds"),
    (10, "10 seconds"),
    (15, "15 seconds"),
    (30, "30 seconds"),
    (60, "1 minute"),
    (120, "2 minutes"),
    (300, "5 minutes"),
    (600, "10 minutes"),
    (900, "15 minutes"),
    (1_800, "30 minutes"),
    (3_600, "1 hour"),
    (7_200, "2 hours"),
    (21_600, "6 hours"),
];
/// Inactive timeouts a server may have, in seconds.
pub const AFK_TIMEOUTS: [(u32, &str); 5] = [
    (60, "1 minute"),
    (300, "5 minutes"),
    (900, "15 minutes"),
    (1_800, "30 minutes"),
    (3_600, "1 hour"),
];
pub const NOTIFICATION_LEVELS: [(u8, &str); 2] = [(0, "All messages"), (1, "Only @mentions")];
pub const CONTENT_FILTERS: [(u8, &str); 3] = [
    (0, "Do not scan"),
    (1, "Members without roles"),
    (2, "All members"),
];
/// Verification levels a server may require of new members (issue #96).
pub const VERIFICATION_CHOICES: [(u8, &str); 5] = [
    (0, "None: anyone can join and talk"),
    (1, "Low: a verified e-mail address"),
    (2, "Medium: registered on Discord for 5 minutes"),
    (3, "High: a member of this server for 10 minutes"),
    (4, "Highest: a verified phone number"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminError {
    /// A name outside the allowed length.
    BadName,
    TopicTooLong,
    DescriptionTooLong,
    /// Not a PNG or JPEG (an animated icon needs a boost), or empty.
    UnsupportedImage,
    ImageTooLarge,
    /// Announcement, Stage and forum channels need the Community feature.
    NeedsCommunity,
    /// A channel kind this panel does not create.
    UnsupportedKind,
    /// A value outside the presets or limits.
    BadValue,
    NothingToChange,
}

/// A failed management request: invalid input (nothing sent) or Discord's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminRequestError {
    Invalid(AdminError),
    Rest(RestError),
}

impl From<RestError> for AdminRequestError {
    fn from(error: RestError) -> Self {
        Self::Rest(error)
    }
}

/// A validated static server icon in the `data:` form the body carries.
#[derive(Clone, PartialEq, Eq)]
pub struct GuildIcon(String);

impl std::fmt::Debug for GuildIcon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GuildIcon({} bytes)", self.0.len())
    }
}

impl GuildIcon {
    /// PNG or JPEG of at most 512 KiB, judged by signature. GIF is refused
    /// because an animated server icon needs a boost.
    pub fn new(bytes: &[u8]) -> Result<Self, AdminError> {
        data_uri(bytes, false)
            .map(Self)
            .map_err(|error| match error {
                ImageError::Unsupported => AdminError::UnsupportedImage,
                ImageError::TooLarge => AdminError::ImageTooLarge,
            })
    }
}

fn within(text: &str, limit: usize, error: AdminError) -> Result<&str, AdminError> {
    let text = text.trim();
    if text.chars().count() > limit {
        return Err(error);
    }
    Ok(text)
}

fn name_within(text: &str, min: usize, max: usize) -> Result<&str, AdminError> {
    let text = text.trim();
    let count = text.chars().count();
    if count < min || count > max {
        return Err(AdminError::BadName);
    }
    Ok(text)
}

/// Whether Discord offers this channel kind only to Community servers.
pub fn needs_community(kind: ChannelKind) -> bool {
    matches!(
        kind,
        ChannelKind::GuildAnnouncement | ChannelKind::GuildStageVoice | ChannelKind::GuildForum
    )
}

/// A channel or category to create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewChannel {
    pub name: String,
    pub kind: ChannelKind,
    /// The category it goes in; none for the top level (and for a category).
    pub parent: Option<Snowflake>,
    /// Text, announcement and forum channels only; empty sends none.
    pub topic: String,
    /// Whether the server has the Community feature, from the reducer.
    pub community: bool,
}

fn new_channel_body(new: &NewChannel) -> Result<serde_json::Value, AdminError> {
    if !matches!(
        new.kind,
        ChannelKind::GuildText
            | ChannelKind::GuildVoice
            | ChannelKind::GuildCategory
            | ChannelKind::GuildAnnouncement
            | ChannelKind::GuildStageVoice
            | ChannelKind::GuildForum
    ) {
        return Err(AdminError::UnsupportedKind);
    }
    if needs_community(new.kind) && !new.community {
        return Err(AdminError::NeedsCommunity);
    }
    let name = name_within(&new.name, 1, MAX_CHANNEL_NAME_CHARS)?;
    let topic = within(
        &new.topic,
        MAX_CHANNEL_TOPIC_CHARS,
        AdminError::TopicTooLong,
    )?;
    let mut body = serde_json::json!({ "name": name, "type": new.kind.to_u8() });
    if new.kind != ChannelKind::GuildCategory
        && let Some(parent) = new.parent
    {
        body["parent_id"] = parent.to_string().into();
    }
    if !topic.is_empty() && !new.kind.is_voice() && new.kind != ChannelKind::GuildCategory {
        body["topic"] = topic.into();
    }
    Ok(body)
}

/// Changes to one channel; `None` leaves a field as it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelEdit {
    pub name: Option<String>,
    /// Empty clears the topic.
    pub topic: Option<String>,
    pub nsfw: Option<bool>,
    /// Seconds, one of [`SLOWMODE`].
    pub slowmode: Option<u32>,
    pub user_limit: Option<u32>,
    pub bitrate: Option<u32>,
    /// `Some(None)` is automatic.
    pub rtc_region: Option<Option<String>>,
    /// 1 automatic, 2 full.
    pub video_quality: Option<u8>,
    /// `Some(None)` moves it out of its category.
    pub parent: Option<Option<Snowflake>>,
}

/// The most a channel's bitrate may be set to: the base every server allows,
/// or what the channel already has because the server reported it.
pub fn bitrate_ceiling(current: u32) -> u32 {
    current.max(BASE_BITRATE)
}

fn channel_edit_body(edit: &ChannelEdit, ceiling: u32) -> Result<serde_json::Value, AdminError> {
    let mut body = serde_json::Map::new();
    if let Some(name) = &edit.name {
        body.insert(
            "name".into(),
            name_within(name, 1, MAX_CHANNEL_NAME_CHARS)?.into(),
        );
    }
    if let Some(topic) = &edit.topic {
        let topic = within(topic, MAX_CHANNEL_TOPIC_CHARS, AdminError::TopicTooLong)?;
        body.insert(
            "topic".into(),
            if topic.is_empty() {
                serde_json::Value::Null
            } else {
                topic.into()
            },
        );
    }
    if let Some(nsfw) = edit.nsfw {
        body.insert("nsfw".into(), nsfw.into());
    }
    if let Some(seconds) = edit.slowmode {
        if !SLOWMODE.iter().any(|(s, _)| *s == seconds) {
            return Err(AdminError::BadValue);
        }
        body.insert("rate_limit_per_user".into(), seconds.into());
    }
    if let Some(limit) = edit.user_limit {
        if limit > MAX_VOICE_USER_LIMIT {
            return Err(AdminError::BadValue);
        }
        body.insert("user_limit".into(), limit.into());
    }
    if let Some(bitrate) = edit.bitrate {
        if !(MIN_BITRATE..=ceiling).contains(&bitrate) {
            return Err(AdminError::BadValue);
        }
        body.insert("bitrate".into(), bitrate.into());
    }
    if let Some(region) = &edit.rtc_region {
        body.insert("rtc_region".into(), region.clone().into());
    }
    if let Some(quality) = edit.video_quality {
        if !(1..=2).contains(&quality) {
            return Err(AdminError::BadValue);
        }
        body.insert("video_quality_mode".into(), quality.into());
    }
    if let Some(parent) = edit.parent {
        body.insert(
            "parent_id".into(),
            parent.map_or(serde_json::Value::Null, |p| p.to_string().into()),
        );
    }
    if body.is_empty() {
        return Err(AdminError::NothingToChange);
    }
    Ok(body.into())
}

/// Changes to a server's settings; `None` leaves a field as it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuildEdit {
    pub name: Option<String>,
    /// `Some(None)` removes the icon.
    pub icon: Option<Option<GuildIcon>>,
    /// Empty clears the description.
    pub description: Option<String>,
    /// `Some(None)` clears the channel.
    pub system_channel: Option<Option<Snowflake>>,
    pub afk_channel: Option<Option<Snowflake>>,
    pub afk_timeout: Option<u32>,
    pub notifications: Option<u8>,
    pub content_filter: Option<u8>,
    pub verification_level: Option<u8>,
    /// `Some(None)` clears the safety alerts channel.
    pub safety_alerts_channel: Option<Option<Snowflake>>,
}

fn guild_edit_body(edit: &GuildEdit) -> Result<serde_json::Value, AdminError> {
    let mut body = serde_json::Map::new();
    if let Some(name) = &edit.name {
        body.insert(
            "name".into(),
            name_within(name, MIN_GUILD_NAME_CHARS, MAX_GUILD_NAME_CHARS)?.into(),
        );
    }
    if let Some(icon) = &edit.icon {
        body.insert(
            "icon".into(),
            icon.as_ref()
                .map_or(serde_json::Value::Null, |i| i.0.clone().into()),
        );
    }
    if let Some(text) = &edit.description {
        let text = within(
            text,
            MAX_GUILD_DESCRIPTION_CHARS,
            AdminError::DescriptionTooLong,
        )?;
        body.insert(
            "description".into(),
            if text.is_empty() {
                serde_json::Value::Null
            } else {
                text.into()
            },
        );
    }
    for (key, value) in [
        ("system_channel_id", edit.system_channel),
        ("afk_channel_id", edit.afk_channel),
    ] {
        if let Some(channel) = value {
            body.insert(
                key.into(),
                channel.map_or(serde_json::Value::Null, |c| c.to_string().into()),
            );
        }
    }
    if let Some(seconds) = edit.afk_timeout {
        if !AFK_TIMEOUTS.iter().any(|(s, _)| *s == seconds) {
            return Err(AdminError::BadValue);
        }
        body.insert("afk_timeout".into(), seconds.into());
    }
    if let Some(level) = edit.notifications {
        if !NOTIFICATION_LEVELS.iter().any(|(l, _)| *l == level) {
            return Err(AdminError::BadValue);
        }
        body.insert("default_message_notifications".into(), level.into());
    }
    if let Some(filter) = edit.content_filter {
        if !CONTENT_FILTERS.iter().any(|(f, _)| *f == filter) {
            return Err(AdminError::BadValue);
        }
        body.insert("explicit_content_filter".into(), filter.into());
    }
    if let Some(level) = edit.verification_level {
        if !VERIFICATION_CHOICES.iter().any(|(l, _)| *l == level) {
            return Err(AdminError::BadValue);
        }
        body.insert("verification_level".into(), level.into());
    }
    if let Some(channel) = edit.safety_alerts_channel {
        body.insert(
            "safety_alerts_channel_id".into(),
            channel.map_or(serde_json::Value::Null, |c| c.to_string().into()),
        );
    }
    if body.is_empty() {
        return Err(AdminError::NothingToChange);
    }
    Ok(body.into())
}

fn create_guild_body(
    name: &str,
    icon: Option<&GuildIcon>,
) -> Result<serde_json::Value, AdminError> {
    let name = name_within(name, MIN_GUILD_NAME_CHARS, MAX_GUILD_NAME_CHARS)?;
    let mut body = serde_json::json!({ "name": name });
    if let Some(icon) = icon {
        body["icon"] = icon.0.clone().into();
    }
    Ok(body)
}

/// One channel's new position in a reorder request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub id: Snowflake,
    pub position: i32,
}

pub(crate) fn placements_body(placements: &[Placement]) -> Result<serde_json::Value, AdminError> {
    if placements.is_empty() || placements.len() > MAX_PLACEMENTS {
        return Err(AdminError::BadValue);
    }
    Ok(placements
        .iter()
        .map(|p| serde_json::json!({ "id": p.id.to_string(), "position": p.position }))
        .collect())
}

/// A voice region a channel may be pinned to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceRegion {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
struct WireRegion {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    deprecated: bool,
    #[serde(default)]
    custom: bool,
}

/// Reads a `GET /guilds/{id}/regions` answer: usable regions, at most
/// [`MAX_REGIONS`], texts capped. `None` for anything that is not a list.
pub fn parse_regions(body: &[u8]) -> Option<Vec<VoiceRegion>> {
    let wire: Vec<WireRegion> = serde_json::from_slice(body).ok()?;
    Some(
        wire.into_iter()
            .filter(|r| !r.deprecated && !r.custom && !r.id.is_empty())
            .take(MAX_REGIONS)
            .map(|r| VoiceRegion {
                id: crate::invite::cut_at(r.id, MAX_REGION_TEXT),
                name: crate::invite::cut_at(r.name, MAX_REGION_TEXT),
            })
            .collect(),
    )
}

fn write(method: Method, path: &str) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(Route::new(method, path)?, Priority::UserWrite).with_error_detail())
}

impl<C: Clock> RestClient<C> {
    /// Creates a server (`POST /guilds`). It appears with `GUILD_CREATE`.
    pub async fn create_guild(
        &self,
        name: &str,
        icon: Option<&GuildIcon>,
    ) -> Result<(), AdminRequestError> {
        let body = create_guild_body(name, icon).map_err(AdminRequestError::Invalid)?;
        let request = write(Method::POST, "/guilds")?.json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Changes server settings (`PATCH /guilds/{id}`); the display follows
    /// `GUILD_UPDATE`.
    pub async fn edit_guild(
        &self,
        guild: Snowflake,
        edit: &GuildEdit,
    ) -> Result<(), AdminRequestError> {
        let body = guild_edit_body(edit).map_err(AdminRequestError::Invalid)?;
        let request = write(Method::PATCH, &format!("/guilds/{guild}"))?.json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Requires (or stops requiring) two-factor authentication of moderators
    /// (`POST /guilds/{id}/mfa`, owner only). Discord may demand two-factor
    /// verification itself; that answer is shown, never answered. The display
    /// follows `GUILD_UPDATE`.
    pub async fn set_mfa_level(&self, guild: Snowflake, required: bool) -> Result<(), RestError> {
        let request = write(Method::POST, &format!("/guilds/{guild}/mfa"))?
            .json(&serde_json::json!({ "level": u8::from(required) }))?;
        self.execute(request).await.map(drop)
    }

    /// Hands the server to another member (`PATCH /guilds/{id}` `owner_id`).
    /// Discord may answer with a two-factor or verification challenge; it is
    /// shown, never answered.
    pub async fn transfer_ownership(
        &self,
        guild: Snowflake,
        to: Snowflake,
    ) -> Result<(), RestError> {
        let request = write(Method::PATCH, &format!("/guilds/{guild}"))?
            .json(&serde_json::json!({ "owner_id": to.to_string() }))?;
        self.execute(request).await.map(drop)
    }

    /// Deletes the server for everyone (`DELETE /guilds/{id}`, owner only).
    pub async fn delete_guild(&self, guild: Snowflake) -> Result<(), RestError> {
        let request = write(Method::DELETE, &format!("/guilds/{guild}"))?;
        self.execute(request).await.map(drop)
    }

    /// Creates a channel or category (`POST /guilds/{id}/channels`); it
    /// appears with `CHANNEL_CREATE`.
    pub async fn create_channel(
        &self,
        guild: Snowflake,
        new: &NewChannel,
    ) -> Result<(), AdminRequestError> {
        let body = new_channel_body(new).map_err(AdminRequestError::Invalid)?;
        let request = write(Method::POST, &format!("/guilds/{guild}/channels"))?.json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Changes one channel (`PATCH /channels/{id}`); `ceiling` is
    /// [`bitrate_ceiling`] of the channel. The display follows `CHANNEL_UPDATE`.
    pub async fn edit_channel(
        &self,
        channel: Snowflake,
        edit: &ChannelEdit,
        ceiling: u32,
    ) -> Result<(), AdminRequestError> {
        let body = channel_edit_body(edit, ceiling).map_err(AdminRequestError::Invalid)?;
        let request = write(Method::PATCH, &format!("/channels/{channel}"))?.json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Deletes a channel or category (`DELETE /channels/{id}`).
    pub async fn delete_channel(&self, channel: Snowflake) -> Result<(), RestError> {
        let request = write(Method::DELETE, &format!("/channels/{channel}"))?;
        self.execute(request).await.map(drop)
    }

    /// Moves channels (`PATCH /guilds/{id}/channels`); each moved channel
    /// arrives as a `CHANNEL_UPDATE`.
    pub async fn reorder_channels(
        &self,
        guild: Snowflake,
        placements: &[Placement],
    ) -> Result<(), AdminRequestError> {
        let body = placements_body(placements).map_err(AdminRequestError::Invalid)?;
        let request = write(Method::PATCH, &format!("/guilds/{guild}/channels"))?.json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// The voice regions of a server, asked for when a voice channel is edited.
    pub async fn voice_regions(&self, guild: Snowflake) -> Result<Vec<VoiceRegion>, RestError> {
        let request = RestRequest::new(
            Route::new(Method::GET, &format!("/guilds/{guild}/regions"))?,
            Priority::UserRead,
        );
        parse_regions(self.execute(request).await?.body()).ok_or(RestError::InvalidJson)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/rest/guild-admin-synthetic.json"
        ))
        .unwrap()
    }

    fn new(kind: ChannelKind, community: bool) -> NewChannel {
        NewChannel {
            name: " general ".into(),
            kind,
            parent: Some(Snowflake(7)),
            topic: "hello".into(),
            community,
        }
    }

    #[test]
    fn a_new_channel_names_its_kind_parent_and_topic() {
        let text = new_channel_body(&new(ChannelKind::GuildText, false)).unwrap();
        assert_eq!(
            text,
            json!({"name": "general", "type": 0, "parent_id": "7", "topic": "hello"})
        );
        // Voice has no topic; a category has neither parent nor topic.
        let voice = new_channel_body(&new(ChannelKind::GuildVoice, false)).unwrap();
        assert_eq!(
            voice,
            json!({"name": "general", "type": 2, "parent_id": "7"})
        );
        let category = new_channel_body(&new(ChannelKind::GuildCategory, false)).unwrap();
        assert_eq!(category, json!({"name": "general", "type": 4}));
    }

    #[test]
    fn community_kinds_are_refused_without_the_feature_and_sent_with_it() {
        for kind in [
            ChannelKind::GuildAnnouncement,
            ChannelKind::GuildStageVoice,
            ChannelKind::GuildForum,
        ] {
            assert_eq!(
                new_channel_body(&new(kind, false)),
                Err(AdminError::NeedsCommunity)
            );
            assert_eq!(
                new_channel_body(&new(kind, true)).unwrap()["type"],
                kind.to_u8()
            );
        }
        assert_eq!(
            new_channel_body(&new(ChannelKind::GuildMedia, true)),
            Err(AdminError::UnsupportedKind)
        );
    }

    #[test]
    fn names_and_topics_are_checked_before_anything_is_sent() {
        let mut channel = new(ChannelKind::GuildText, false);
        channel.name = "   ".into();
        assert_eq!(new_channel_body(&channel), Err(AdminError::BadName));
        channel.name = "x".repeat(101);
        assert_eq!(new_channel_body(&channel), Err(AdminError::BadName));
        channel.name = "ok".into();
        channel.topic = "t".repeat(1025);
        assert_eq!(new_channel_body(&channel), Err(AdminError::TopicTooLong));
        assert_eq!(create_guild_body("a", None), Err(AdminError::BadName));
        assert_eq!(
            create_guild_body(" My server ", None).unwrap(),
            json!({"name": "My server"})
        );
    }

    #[test]
    fn a_server_icon_is_a_static_bounded_image() {
        let png = b"\x89PNG\r\n\x1a\nxxxx";
        let icon = GuildIcon::new(png).unwrap();
        let body = create_guild_body("Server", Some(&icon)).unwrap();
        assert!(
            body["icon"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(
            GuildIcon::new(b"GIF89a....").unwrap_err(),
            AdminError::UnsupportedImage
        );
        assert_eq!(
            GuildIcon::new(&vec![0xFF; 600 * 1024]).unwrap_err(),
            AdminError::UnsupportedImage
        );
        let mut big = png.to_vec();
        big.resize(600 * 1024, 0);
        assert_eq!(GuildIcon::new(&big).unwrap_err(), AdminError::ImageTooLarge);
        assert_eq!(
            format!("{icon:?}"),
            format!("GuildIcon({} bytes)", icon.0.len())
        );
    }

    #[test]
    fn a_channel_edit_sends_only_what_changed_within_the_presets() {
        let edit = ChannelEdit {
            topic: Some(String::new()),
            slowmode: Some(21_600),
            parent: Some(None),
            rtc_region: Some(Some("us-east".into())),
            ..ChannelEdit::default()
        };
        assert_eq!(
            channel_edit_body(&edit, BASE_BITRATE).unwrap(),
            json!({"topic": null, "rate_limit_per_user": 21600, "parent_id": null, "rtc_region": "us-east"})
        );
        assert_eq!(
            channel_edit_body(&ChannelEdit::default(), BASE_BITRATE),
            Err(AdminError::NothingToChange)
        );
        for bad in [
            ChannelEdit {
                slowmode: Some(7),
                ..ChannelEdit::default()
            },
            ChannelEdit {
                user_limit: Some(100),
                ..ChannelEdit::default()
            },
            ChannelEdit {
                video_quality: Some(3),
                ..ChannelEdit::default()
            },
            ChannelEdit {
                name: Some(String::new()),
                ..ChannelEdit::default()
            },
        ] {
            assert!(channel_edit_body(&bad, BASE_BITRATE).is_err());
        }
    }

    #[test]
    fn the_bitrate_is_capped_by_what_the_server_already_allows() {
        assert_eq!(bitrate_ceiling(0), BASE_BITRATE);
        assert_eq!(bitrate_ceiling(96_000), 96_000);
        let edit = |bitrate| ChannelEdit {
            bitrate: Some(bitrate),
            ..ChannelEdit::default()
        };
        assert!(channel_edit_body(&edit(64_000), BASE_BITRATE).is_ok());
        assert_eq!(
            channel_edit_body(&edit(64_001), BASE_BITRATE),
            Err(AdminError::BadValue)
        );
        assert!(channel_edit_body(&edit(96_000), bitrate_ceiling(96_000)).is_ok());
        assert_eq!(
            channel_edit_body(&edit(7_999), BASE_BITRATE),
            Err(AdminError::BadValue)
        );
    }

    #[test]
    fn a_guild_edit_sends_only_what_changed_and_clears_with_null() {
        let edit = GuildEdit {
            name: Some(" Renamed ".into()),
            icon: Some(None),
            description: Some(String::new()),
            system_channel: Some(Some(Snowflake(5))),
            afk_channel: Some(None),
            afk_timeout: Some(900),
            notifications: Some(1),
            content_filter: Some(2),
            verification_level: Some(3),
            safety_alerts_channel: Some(None),
        };
        assert_eq!(
            guild_edit_body(&edit).unwrap(),
            json!({
                "name": "Renamed", "icon": null, "description": null,
                "system_channel_id": "5", "afk_channel_id": null, "afk_timeout": 900,
                "default_message_notifications": 1, "explicit_content_filter": 2,
                "verification_level": 3, "safety_alerts_channel_id": null
            })
        );
        assert_eq!(
            guild_edit_body(&GuildEdit::default()),
            Err(AdminError::NothingToChange)
        );
        for bad in [
            GuildEdit {
                name: Some("x".into()),
                ..GuildEdit::default()
            },
            GuildEdit {
                description: Some("d".repeat(121)),
                ..GuildEdit::default()
            },
            GuildEdit {
                afk_timeout: Some(61),
                ..GuildEdit::default()
            },
            GuildEdit {
                notifications: Some(2),
                ..GuildEdit::default()
            },
            GuildEdit {
                content_filter: Some(3),
                ..GuildEdit::default()
            },
            GuildEdit {
                verification_level: Some(5),
                ..GuildEdit::default()
            },
        ] {
            assert!(guild_edit_body(&bad).is_err());
        }
    }

    #[test]
    fn a_reorder_lists_ids_as_strings_and_is_bounded() {
        let body = placements_body(&[
            Placement {
                id: Snowflake(3),
                position: 0,
            },
            Placement {
                id: Snowflake(4),
                position: 1,
            },
        ])
        .unwrap();
        assert_eq!(
            body,
            json!([{"id": "3", "position": 0}, {"id": "4", "position": 1}])
        );
        assert!(placements_body(&[]).is_err());
        let many = vec![
            Placement {
                id: Snowflake(1),
                position: 0
            };
            MAX_PLACEMENTS + 1
        ];
        assert!(placements_body(&many).is_err());
    }

    #[test]
    fn regions_skip_deprecated_and_custom_ones() {
        let regions = parse_regions(fixture()["regions"].to_string().as_bytes()).unwrap();
        assert_eq!(
            regions,
            [VoiceRegion {
                id: "us-east".into(),
                name: "US East".into()
            }]
        );
        assert!(parse_regions(b"{}").is_none());
    }

    #[test]
    fn the_presets_are_the_official_ones() {
        assert_eq!(SLOWMODE.len(), 14);
        assert_eq!(SLOWMODE[0].0, 0);
        assert_eq!(SLOWMODE[13], (21_600, "6 hours"));
        assert!(AFK_TIMEOUTS.iter().any(|(s, _)| *s == 300));
    }
}
