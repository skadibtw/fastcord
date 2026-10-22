use crate::{Channel, Permissions, ScheduledEvent, Snowflake, StageInstance};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Role {
    pub id: Snowflake,
    pub name: String,
    pub permissions: Permissions,
    #[serde(default)]
    pub position: i32,
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
}
