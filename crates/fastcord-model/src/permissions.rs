//! Permission bitsets and channel permission computation.
//!
//! Follows Discord's documented algorithm: base guild permissions from
//! `@everyone` and member roles, then channel overwrites in the order
//! `@everyone` → roles (deny before allow) → member, then implicit rules.

use std::ops::{BitAnd, BitOr, Not};

use crate::{Role, Snowflake};

/// A Discord permission bitset (decimal string on the wire).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Permissions(pub u64);

impl Permissions {
    pub const NONE: Self = Self(0);
    pub const ALL: Self = Self(u64::MAX);

    pub const CREATE_INSTANT_INVITE: Self = Self(1 << 0);
    pub const ADMINISTRATOR: Self = Self(1 << 3);
    pub const KICK_MEMBERS: Self = Self(1 << 1);
    pub const BAN_MEMBERS: Self = Self(1 << 2);
    pub const MANAGE_CHANNELS: Self = Self(1 << 4);
    pub const MANAGE_GUILD: Self = Self(1 << 5);
    pub const MANAGE_ROLES: Self = Self(1 << 28);
    pub const MANAGE_WEBHOOKS: Self = Self(1 << 29);
    pub const REQUEST_TO_SPEAK: Self = Self(1 << 32);
    pub const MUTE_MEMBERS: Self = Self(1 << 22);
    pub const DEAFEN_MEMBERS: Self = Self(1 << 23);
    pub const MOVE_MEMBERS: Self = Self(1 << 24);
    pub const ADD_REACTIONS: Self = Self(1 << 6);
    pub const PRIORITY_SPEAKER: Self = Self(1 << 8);
    pub const STREAM: Self = Self(1 << 9);
    pub const VIEW_CHANNEL: Self = Self(1 << 10);
    pub const SEND_MESSAGES: Self = Self(1 << 11);
    pub const SEND_TTS_MESSAGES: Self = Self(1 << 12);
    pub const MANAGE_MESSAGES: Self = Self(1 << 13);
    pub const EMBED_LINKS: Self = Self(1 << 14);
    pub const ATTACH_FILES: Self = Self(1 << 15);
    pub const READ_MESSAGE_HISTORY: Self = Self(1 << 16);
    pub const MENTION_EVERYONE: Self = Self(1 << 17);
    pub const USE_EXTERNAL_EMOJIS: Self = Self(1 << 18);
    pub const CONNECT: Self = Self(1 << 20);
    pub const SPEAK: Self = Self(1 << 21);
    pub const USE_VAD: Self = Self(1 << 25);
    pub const USE_SOUNDBOARD: Self = Self(1 << 42);
    pub const MANAGE_THREADS: Self = Self(1 << 34);
    pub const MANAGE_EVENTS: Self = Self(1 << 33);
    pub const CREATE_EVENTS: Self = Self(1 << 44);
    pub const CREATE_PUBLIC_THREADS: Self = Self(1 << 35);
    pub const SEND_MESSAGES_IN_THREADS: Self = Self(1 << 38);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn apply(self, allow: Self, deny: Self) -> Self {
        (self & !deny) | allow
    }
}

impl BitOr for Permissions {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitAnd for Permissions {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl Not for Permissions {
    type Output = Self;
    fn not(self) -> Self {
        Self(!self.0)
    }
}

impl serde::Serialize for Permissions {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        crate::wire::serialize_u64_str(self.0, s)
    }
}

impl<'de> serde::Deserialize<'de> for Permissions {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::wire::deserialize_u64_str(d).map(Self)
    }
}

/// Where a permission can be set: on a role only, or also in a channel.
pub mod scope {
    /// Server-wide: a role has it, a channel overwrite cannot.
    pub const ROLE: u8 = 1;
    /// Any channel and category.
    pub const CHANNEL: u8 = 2;
    /// Text, announcement and forum channels (and categories).
    pub const TEXT: u8 = 4;
    /// Voice and Stage channels (and categories).
    pub const VOICE: u8 = 8;
}

/// One named permission bit of the official client's role editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PermissionInfo {
    pub bit: u8,
    pub name: &'static str,
    pub scope: u8,
}

const fn info(bit: u8, name: &'static str, scope: u8) -> PermissionInfo {
    PermissionInfo { bit, name, scope }
}

impl PermissionInfo {
    pub const fn flag(self) -> Permissions {
        Permissions(1 << self.bit)
    }
}

/// The free permissions grouped like the official client. Paid, game-activity
/// and creator-monetization bits are left out; the bits of a role that are not
/// listed are kept as they are when it is edited.
pub const PERMISSION_GROUPS: [(&str, &[PermissionInfo]); 5] = [
    (
        "General",
        &[
            info(10, "View channels", scope::CHANNEL),
            info(4, "Manage channels", scope::CHANNEL),
            info(28, "Manage roles", scope::CHANNEL),
            info(30, "Manage expressions", scope::ROLE),
            info(7, "View audit log", scope::ROLE),
            info(19, "View server insights", scope::ROLE),
            info(29, "Manage webhooks", scope::CHANNEL),
            info(5, "Manage server", scope::ROLE),
        ],
    ),
    (
        "Membership",
        &[
            info(0, "Create invite", scope::CHANNEL),
            info(26, "Change nickname", scope::ROLE),
            info(27, "Manage nicknames", scope::ROLE),
            info(1, "Kick members", scope::ROLE),
            info(2, "Ban members", scope::ROLE),
            info(40, "Timeout members", scope::ROLE),
        ],
    ),
    (
        "Text channels",
        &[
            info(11, "Send messages", scope::TEXT),
            info(38, "Send messages in threads", scope::TEXT),
            info(35, "Create public threads", scope::TEXT),
            info(36, "Create private threads", scope::TEXT),
            info(14, "Embed links", scope::TEXT),
            info(15, "Attach files", scope::TEXT),
            info(6, "Add reactions", scope::TEXT),
            info(18, "Use external emoji", scope::TEXT),
            info(37, "Use external stickers", scope::TEXT),
            info(17, "Mention @everyone, @here and all roles", scope::TEXT),
            info(13, "Manage messages", scope::TEXT),
            info(34, "Manage threads", scope::TEXT),
            info(16, "Read message history", scope::TEXT),
            info(12, "Send text-to-speech messages", scope::TEXT),
            info(31, "Use application commands", scope::TEXT),
            info(46, "Send voice messages", scope::TEXT),
            info(49, "Create polls", scope::TEXT),
        ],
    ),
    (
        "Voice channels",
        &[
            info(20, "Connect", scope::VOICE),
            info(21, "Speak", scope::VOICE),
            info(9, "Video", scope::VOICE),
            info(42, "Use soundboard", scope::VOICE),
            info(43, "Use external sounds", scope::VOICE),
            info(25, "Use voice activity", scope::VOICE),
            info(8, "Priority speaker", scope::VOICE),
            info(22, "Mute members", scope::VOICE),
            info(23, "Deafen members", scope::VOICE),
            info(24, "Move members", scope::VOICE),
            info(32, "Request to speak", scope::VOICE),
        ],
    ),
    (
        "Events and advanced",
        &[
            info(44, "Create events", scope::CHANNEL),
            info(33, "Manage events", scope::CHANNEL),
            info(3, "Administrator", scope::ROLE),
        ],
    ),
];

/// Whether an overwrite targets a role or a single member.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(u8)]
pub enum OverwriteKind {
    Role = 0,
    Member = 1,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PermissionOverwrite {
    pub id: Snowflake,
    #[serde(rename = "type")]
    pub kind: OverwriteKind,
    pub allow: Permissions,
    pub deny: Permissions,
}

/// Guild data needed for permission computation.
#[derive(Clone, Copy, Debug)]
pub struct GuildScope<'a> {
    pub id: Snowflake,
    pub owner_id: Snowflake,
    /// All guild roles, including `@everyone` (whose ID equals the guild ID).
    pub roles: &'a [Role],
}

/// The member whose permissions are computed.
#[derive(Clone, Copy, Debug)]
pub struct MemberScope<'a> {
    pub user_id: Snowflake,
    pub role_ids: &'a [Snowflake],
    /// `communication_disabled_until` is in the future.
    pub timed_out: bool,
}

/// Guild-level permissions (no channel overwrites).
pub fn guild_permissions(guild: GuildScope<'_>, member: MemberScope<'_>) -> Permissions {
    if member.user_id == guild.owner_id {
        return Permissions::ALL;
    }
    let base = guild
        .roles
        .iter()
        .filter(|r| r.id == guild.id || member.role_ids.contains(&r.id))
        .fold(Permissions::NONE, |acc, r| acc | r.permissions);
    if base.contains(Permissions::ADMINISTRATOR) {
        Permissions::ALL
    } else {
        base
    }
}

/// Effective permissions in a guild channel.
///
/// `overwrites` are the channel's own overwrites; for threads pass the parent
/// channel's overwrites. `text_like` enables the implicit rule that losing
/// `SEND_MESSAGES` also removes mention/TTS/attach/embed permissions.
pub fn channel_permissions(
    guild: GuildScope<'_>,
    member: MemberScope<'_>,
    overwrites: &[PermissionOverwrite],
    text_like: bool,
) -> Permissions {
    let base = guild_permissions(guild, member);
    if base == Permissions::ALL {
        return Permissions::ALL;
    }

    let mut perms = base;
    if let Some(everyone) = overwrites
        .iter()
        .find(|o| o.kind == OverwriteKind::Role && o.id == guild.id)
    {
        perms = perms.apply(everyone.allow, everyone.deny);
    }

    let (allow, deny) = overwrites
        .iter()
        .filter(|o| o.kind == OverwriteKind::Role && member.role_ids.contains(&o.id))
        .fold((Permissions::NONE, Permissions::NONE), |(a, d), o| {
            (a | o.allow, d | o.deny)
        });
    perms = perms.apply(allow, deny);

    if let Some(own) = overwrites
        .iter()
        .find(|o| o.kind == OverwriteKind::Member && o.id == member.user_id)
    {
        perms = perms.apply(own.allow, own.deny);
    }

    if member.timed_out {
        perms = perms & (Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY);
    }
    if !perms.contains(Permissions::VIEW_CHANNEL) {
        return Permissions::NONE;
    }
    if text_like && !perms.contains(Permissions::SEND_MESSAGES) {
        perms = perms
            & !(Permissions::MENTION_EVERYONE
                | Permissions::SEND_TTS_MESSAGES
                | Permissions::ATTACH_FILES
                | Permissions::EMBED_LINKS);
    }
    perms
}
