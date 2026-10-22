use crate::{Channel, Permissions, ScheduledEvent, Snowflake, StageInstance};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Role {
    pub id: Snowflake,
    pub name: String,
    pub permissions: Permissions,
    #[serde(default)]
    pub position: i32,
    /// RGB colour; 0 is no colour.
    #[serde(default)]
    pub color: u32,
    /// Listed apart in the member list.
    #[serde(default)]
    pub hoist: bool,
    #[serde(default)]
    pub mentionable: bool,
    /// Owned by an integration or bot: it cannot be given to a member by hand.
    #[serde(default)]
    pub managed: bool,
}

/// A guild member, referencing its user by ID; the user object is stored once
/// in the user table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuildMember {
    pub user_id: Snowflake,
    pub nick: Option<String>,
    pub roles: Vec<Snowflake>,
    /// Only an explicit roles array establishes the member's role set.
    /// Missing/null startup fields must not authorize channel access.
    pub roles_known: bool,
    /// Timeout expiry (ISO 8601); a time in the past means the timeout ended.
    pub communication_disabled_until: Option<String>,
    /// `Some(true)`: membership screening (rules) is not accepted yet. `None`:
    /// the Gateway did not say.
    pub pending: Option<bool>,
    /// Member flags (`COMPLETED_ONBOARDING` is bit 1, `STARTED_ONBOARDING` bit 3).
    pub flags: Option<u32>,
}

/// Guild-owned custom emoji metadata. `available=false` and explicit role
/// restrictions are meaningful; missing account entitlements are not assumed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GuildEmoji {
    pub id: Snowflake,
    pub name: Option<String>,
    #[serde(default)]
    pub animated: bool,
    #[serde(default)]
    pub roles: Vec<Snowflake>,
    #[serde(default = "emoji_available")]
    pub available: bool,
}

fn emoji_available() -> bool {
    true
}

/// Server settings a conversation depends on, as the Gateway reports them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuildSettings {
    /// At most [`MAX_DESCRIPTION_BYTES`].
    pub description: Option<String>,
    /// The `COMMUNITY` feature is on.
    pub community: bool,
    pub system_channel_id: Option<Snowflake>,
    pub afk_channel_id: Option<Snowflake>,
    /// Seconds; 0 when Discord did not say.
    pub afk_timeout: u32,
    /// `default_message_notifications`: 0 all messages, 1 only mentions.
    pub notifications: u8,
    /// `explicit_content_filter`: 0 off, 1 members without roles, 2 everyone.
    pub content_filter: u8,
    /// The channel Community rules live in.
    pub rules_channel_id: Option<Snowflake>,
    /// The channel Discord posts Community updates to.
    pub public_updates_channel_id: Option<Snowflake>,
    /// `verification_level`: 0 none, 1 low, 2 medium, 3 high, 4 highest.
    pub verification_level: u8,
    /// Feature flags (at most [`MAX_FEATURES`] of [`MAX_FEATURE_BYTES`]), kept
    /// so a Community change can send them back.
    pub features: Vec<String>,
}

/// Feature flags kept per server; a real server has far fewer.
pub const MAX_FEATURES: usize = 128;
pub const MAX_FEATURE_BYTES: usize = 64;

/// Longest server description kept.
pub const MAX_DESCRIPTION_BYTES: usize = 512;

/// A normalized, available guild as delivered by the Gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guild {
    pub id: Snowflake,
    pub name: String,
    pub icon: Option<String>,
    pub owner_id: Option<Snowflake>,
    pub roles: Vec<Role>,
    /// Channels always carry their `guild_id`.
    pub channels: Vec<Channel>,
    pub emojis: Vec<GuildEmoji>,
    /// Initial members: just the current user's own member until
    /// READY_SUPPLEMENTAL (or lazy member requests) provides more.
    pub members: Vec<GuildMember>,
    pub member_count: u32,
    /// Live Stage sessions delivered with the guild.
    pub stage_instances: Vec<StageInstance>,
    /// Scheduled events delivered with the guild.
    pub scheduled_events: Vec<ScheduledEvent>,
    pub settings: GuildSettings,
}
