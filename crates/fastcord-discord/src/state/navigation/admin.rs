//! Server and channel management read model (issue #93). A pure query over the
//! reducer's store for one guild: its settings, every channel the account may
//! see in display order with the settings an edit starts from, which of them
//! the account may manage (computed from the same permission context as the
//! sidebar), and the held people an ownership transfer can name. Nothing is
//! kept between calls, and every list and text is capped.

use fastcord_model::{
    ChannelKind as ModelChannelKind, GuildScope, MemberScope, PermissionOverwrite, Permissions,
    Snowflake, channel_permissions, guild_permissions,
};

use crate::{MAX_OVERWRITES, Placement};

use super::{PermissionContext, Store, active_timeout, capped_name, clip_bytes};

/// Channels listed (Discord allows 500 per server).
pub const MAX_ADMIN_CHANNELS: usize = 500;
/// People an ownership transfer can pick from.
pub const MAX_ADMIN_PEOPLE: usize = 256;
/// Roles listed (Discord allows 250 per server).
pub const MAX_ADMIN_ROLES: usize = 250;
const MAX_TOPIC_SHOWN: usize = 4096;

/// The channel kinds the management panel creates and edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminKind {
    Text,
    Announcement,
    Voice,
    Stage,
    Forum,
    Category,
}

impl AdminKind {
    fn from_model(kind: ModelChannelKind) -> Option<Self> {
        match kind {
            ModelChannelKind::GuildText => Some(Self::Text),
            ModelChannelKind::GuildAnnouncement => Some(Self::Announcement),
            ModelChannelKind::GuildVoice => Some(Self::Voice),
            ModelChannelKind::GuildStageVoice => Some(Self::Stage),
            ModelChannelKind::GuildForum => Some(Self::Forum),
            ModelChannelKind::GuildCategory => Some(Self::Category),
            _ => None,
        }
    }

    /// The Discord channel type a new channel of this kind is created with.
    pub const fn model(self) -> ModelChannelKind {
        match self {
            Self::Text => ModelChannelKind::GuildText,
            Self::Announcement => ModelChannelKind::GuildAnnouncement,
            Self::Voice => ModelChannelKind::GuildVoice,
            Self::Stage => ModelChannelKind::GuildStageVoice,
            Self::Forum => ModelChannelKind::GuildForum,
            Self::Category => ModelChannelKind::GuildCategory,
        }
    }

    pub const fn is_voice(self) -> bool {
        matches!(self, Self::Voice | Self::Stage)
    }

    /// Lists show text-like channels above voice ones inside a category.
    const fn bucket(self) -> u8 {
        if self.is_voice() { 1 } else { 0 }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Announcement => "Announcement",
            Self::Voice => "Voice",
            Self::Stage => "Stage",
            Self::Forum => "Forum",
            Self::Category => "Category",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminChannel {
    pub id: Snowflake,
    pub name: String,
    pub kind: AdminKind,
    /// The category this channel sits in, when it is held.
    pub parent_id: Option<Snowflake>,
    pub position: i32,
    pub topic: String,
    pub nsfw: bool,
    pub slowmode: u32,
    pub user_limit: u32,
    pub bitrate: u32,
    pub rtc_region: Option<String>,
    /// Voice video quality: 1 automatic, 2 full.
    pub video_quality: u8,
    /// MANAGE_CHANNELS in this channel (overwrites included).
    pub can_manage: bool,
    /// The channel's own overwrites, at most [`MAX_OVERWRITES`].
    pub overwrites: Vec<PermissionOverwrite>,
    /// MANAGE_ROLES in this channel (overwrites included).
    pub can_manage_permissions: bool,
    /// What the account itself may do here: no bit outside it can be allowed.
    pub mine: Permissions,
    /// For a channel in a category: whether its overwrites are exactly the
    /// category's. `None` outside a category.
    pub synced: Option<bool>,
}

/// Why a role cannot be changed, or `None` when it can.
pub const NEEDS_MANAGE_ROLES: &str = "You need Manage Roles.";
pub const ABOVE_YOUR_ROLE: &str = "This role is not below your highest role.";
pub const MANAGED_ROLE: &str = "An integration manages this role.";
pub const EVERYONE_ROLE: &str = "@everyone cannot be moved, removed or given.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminRole {
    pub id: Snowflake,
    pub name: String,
    pub color: u32,
    pub hoist: bool,
    pub mentionable: bool,
    pub permissions: Permissions,
    pub position: i32,
    pub managed: bool,
    /// The `@everyone` role: its ID is the server's.
    pub everyone: bool,
    /// Why this role's settings cannot be edited.
    pub edit_lock: Option<&'static str>,
    /// Why this role cannot be deleted or moved.
    pub move_lock: Option<&'static str>,
    /// Why this role cannot be given to or taken from a member.
    pub assign_lock: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuildAdmin {
    pub id: Snowflake,
    pub name: String,
    pub has_icon: bool,
    /// The account owns the server.
    pub owner: bool,
    pub community: bool,
    pub description: String,
    pub system_channel: Option<Snowflake>,
    pub afk_channel: Option<Snowflake>,
    pub afk_timeout: u32,
    pub notifications: u8,
    pub content_filter: u8,
    pub can_manage_guild: bool,
    /// Guild-wide MANAGE_CHANNELS: create channels and reorder them.
    pub can_manage_channels: bool,
    /// In display order: loose channels, then each category and its children.
    pub channels: Vec<AdminChannel>,
    /// Held members other than the account and bots, by name.
    pub people: Vec<(Snowflake, String)>,
    /// MANAGE_ROLES server-wide.
    pub can_manage_roles: bool,
    /// Highest first (position, then ID), `@everyone` last.
    pub roles: Vec<AdminRole>,
    /// What the account may do server-wide; with `grant_all` it can set any bit.
    pub mine: Permissions,
    /// Owner or Administrator: any permission may be granted.
    pub grant_all: bool,
    /// The account's highest role position; the owner is above every role.
    pub top_position: i32,
}

impl GuildAdmin {
    pub fn role(&self, id: Snowflake) -> Option<&AdminRole> {
        self.roles.iter().find(|r| r.id == id)
    }

    /// Whether the account may set this permission on a role or allow it in a
    /// channel (`held` is what it has there): it cannot hand out what it lacks.
    pub fn can_grant(&self, held: Permissions, bit: Permissions) -> bool {
        self.grant_all || held.contains(bit)
    }

    /// The new positions for moving a role one place up or down the shown
    /// order: the two roles swap their positions. `None` at either end, for
    /// `@everyone`, when either role may not be moved by the account, or when
    /// both share a position (Discord renumbers them on its own).
    pub fn plan_role_move(&self, id: Snowflake, down: bool) -> Option<Vec<Placement>> {
        let movable: Vec<&AdminRole> = self.roles.iter().filter(|r| !r.everyone).collect();
        let at = movable.iter().position(|r| r.id == id)?;
        let to = if down { at + 1 } else { at.checked_sub(1)? };
        let (this, other) = (*movable.get(at)?, *movable.get(to)?);
        if this.move_lock.is_some() || other.move_lock.is_some() || this.position == other.position
        {
            return None;
        }
        Some(vec![
            Placement {
                id: this.id,
                position: other.position,
            },
            Placement {
                id: other.id,
                position: this.position,
            },
        ])
    }

    pub fn channel(&self, id: Snowflake) -> Option<&AdminChannel> {
        self.channels.iter().find(|c| c.id == id)
    }

    /// The new positions for moving a channel one place up or down among its
    /// siblings (same category, and the same kind of list: categories, text
    /// or voice), numbered 0.. in the shown order; only channels whose
    /// position changes are listed. `None` at either end or for an unknown
    /// channel.
    pub fn plan_move(&self, id: Snowflake, down: bool) -> Option<Vec<Placement>> {
        let this = self.channel(id)?;
        let same = |c: &&AdminChannel| {
            c.parent_id == this.parent_id
                && (c.kind == AdminKind::Category) == (this.kind == AdminKind::Category)
                && c.kind.bucket() == this.kind.bucket()
        };
        let mut group: Vec<&AdminChannel> = self.channels.iter().filter(same).collect();
        let at = group.iter().position(|c| c.id == id)?;
        let to = if down { at + 1 } else { at.checked_sub(1)? };
        if to >= group.len() {
            return None;
        }
        group.swap(at, to);
        let moved: Vec<Placement> = group
            .iter()
            .enumerate()
            .filter(|(i, c)| c.position != *i as i32)
            .map(|(i, c)| Placement {
                id: c.id,
                position: i as i32,
            })
            .collect();
        (!moved.is_empty()).then_some(moved)
    }
}

impl PermissionContext<'_> {
    fn base(&self) -> Permissions {
        match self {
            Self::Owner => Permissions::ALL,
            Self::Member { guild, member, .. } => guild_permissions(*guild, *member),
        }
    }

    /// The account's permissions in a channel before any screening or thread
    /// rule: the bits an overwrite edit may allow.
    fn raw(&self, channel: &fastcord_model::Channel) -> Permissions {
        match self {
            Self::Owner => Permissions::ALL,
            Self::Member { guild, member, .. } => channel_permissions(
                *guild,
                *member,
                &channel.permission_overwrites,
                channel.kind.is_text_like(),
            ),
        }
    }
}

/// The same overwrites, in any order.
fn same_overwrites(a: &[PermissionOverwrite], b: &[PermissionOverwrite]) -> bool {
    a.len() == b.len() && a.iter().all(|o| b.contains(o))
}

/// The highest role position among `roles` (the `@everyone` role counts as 0).
fn top_of(guild: &super::GuildEntry, roles: &[Snowflake]) -> i32 {
    guild
        .roles()
        .iter()
        .filter(|r| roles.contains(&r.id))
        .map(|r| r.position)
        .max()
        .unwrap_or(0)
}

/// The management view of one held guild; `None` when it is unknown or the
/// account's own roles are not yet established (no permission is guessed).
pub fn guild_admin(store: &Store, guild_id: Snowflake) -> Option<GuildAdmin> {
    let guild = store.guild(guild_id)?;
    let me = store.current_user()?.id;
    let context = PermissionContext::new(store, guild)?;
    let base = context.base();
    let mut rows: Vec<AdminChannel> = guild
        .channels()
        .filter_map(|channel| {
            let kind = AdminKind::from_model(channel.kind)?;
            let summary = context.permissions(guild, channel);
            if !summary.view_channel {
                return None;
            }
            let mine = context.raw(channel);
            Some(AdminChannel {
                id: channel.id,
                name: capped_name(channel.name.as_deref(), "Unnamed channel"),
                kind,
                parent_id: channel.parent_id,
                position: channel.position.unwrap_or(0),
                topic: clip_bytes(channel.topic.as_deref().unwrap_or(""), MAX_TOPIC_SHOWN),
                nsfw: channel.nsfw,
                slowmode: u32::from(channel.rate_limit_per_user),
                user_limit: u32::from(channel.user_limit),
                bitrate: channel.bitrate,
                rtc_region: channel.rtc_region.as_deref().map(str::to_owned),
                video_quality: channel.video_quality_mode.unwrap_or(1),
                can_manage: summary.manage_channel,
                overwrites: channel
                    .permission_overwrites
                    .iter()
                    .take(MAX_OVERWRITES)
                    .cloned()
                    .collect(),
                can_manage_permissions: mine.contains(Permissions::MANAGE_ROLES),
                mine,
                synced: if kind == AdminKind::Category {
                    None
                } else {
                    channel
                        .parent_id
                        .and_then(|p| guild.channel(p))
                        .filter(|p| p.kind == ModelChannelKind::GuildCategory)
                        .map(|p| {
                            same_overwrites(
                                &channel.permission_overwrites,
                                &p.permission_overwrites,
                            )
                        })
                },
            })
        })
        .take(MAX_ADMIN_CHANNELS)
        .collect();
    // A parent that is not a held category leaves the channel loose.
    let categories: Vec<Snowflake> = rows
        .iter()
        .filter(|c| c.kind == AdminKind::Category)
        .map(|c| c.id)
        .collect();
    for row in &mut rows {
        if row.kind == AdminKind::Category
            || !row.parent_id.is_some_and(|p| categories.contains(&p))
        {
            row.parent_id = None;
        }
    }
    let key = |c: &AdminChannel| (c.kind.bucket(), c.position, c.id);
    let mut ordered: Vec<AdminChannel> = rows
        .iter()
        .filter(|c| c.kind != AdminKind::Category && c.parent_id.is_none())
        .cloned()
        .collect();
    ordered.sort_by_key(key);
    let mut categories: Vec<&AdminChannel> = rows
        .iter()
        .filter(|c| c.kind == AdminKind::Category)
        .collect();
    categories.sort_by_key(|c| (c.position, c.id));
    for category in categories {
        ordered.push(category.clone());
        let mut children: Vec<AdminChannel> = rows
            .iter()
            .filter(|c| c.parent_id == Some(category.id))
            .cloned()
            .collect();
        children.sort_by_key(key);
        ordered.extend(children);
    }
    let mut people: Vec<(Snowflake, String)> = guild
        .members()
        .filter(|m| m.user_id != me)
        .filter_map(|m| {
            let user = store.user(m.user_id)?;
            if user.bot {
                return None;
            }
            let name = m
                .nick
                .as_deref()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| user.display_name());
            Some((m.user_id, capped_name(Some(name), "Unknown user")))
        })
        .collect();
    people.sort_by_cached_key(|(id, name)| (name.to_lowercase(), *id));
    people.truncate(MAX_ADMIN_PEOPLE);
    let owner = guild.owner_id() == Some(me);
    let can_manage_roles = base.contains(Permissions::MANAGE_ROLES);
    let top_position = if owner {
        i32::MAX
    } else {
        guild.member(me).map_or(0, |m| top_of(guild, &m.roles))
    };
    let mut roles: Vec<AdminRole> = guild
        .roles()
        .iter()
        .take(MAX_ADMIN_ROLES)
        .map(|role| {
            let everyone = role.id == guild_id;
            let below = owner || role.position < top_position;
            let edit_lock = if !can_manage_roles {
                Some(NEEDS_MANAGE_ROLES)
            } else if !everyone && !below {
                Some(ABOVE_YOUR_ROLE)
            } else {
                None
            };
            let move_lock = edit_lock.or(if everyone {
                Some(EVERYONE_ROLE)
            } else if role.managed {
                Some(MANAGED_ROLE)
            } else {
                None
            });
            AdminRole {
                id: role.id,
                name: capped_name(Some(&role.name), "Role"),
                color: role.color,
                hoist: role.hoist,
                mentionable: role.mentionable,
                permissions: role.permissions,
                position: role.position,
                managed: role.managed,
                everyone,
                edit_lock,
                move_lock,
                assign_lock: move_lock,
            }
        })
        .collect();
    roles.sort_by_key(|r| (r.everyone, std::cmp::Reverse((r.position, r.id))));
    let settings = guild.settings();
    Some(GuildAdmin {
        id: guild_id,
        name: capped_name(Some(guild.name()), "Unnamed guild"),
        has_icon: guild.icon().is_some(),
        owner,
        community: settings.community,
        description: settings.description.clone().unwrap_or_default(),
        system_channel: settings.system_channel_id,
        afk_channel: settings.afk_channel_id,
        afk_timeout: settings.afk_timeout,
        notifications: settings.notifications,
        content_filter: settings.content_filter,
        can_manage_guild: base.contains(Permissions::MANAGE_GUILD),
        can_manage_channels: base.contains(Permissions::MANAGE_CHANNELS),
        channels: ordered,
        people,
        can_manage_roles,
        roles,
        mine: base,
        grant_all: base == Permissions::ALL,
        top_position,
    })
}

/// Everything the role and access panels need about one held member: which
/// roles they have, whether the account may change them, and what they can do
/// in each channel the account sees (the read-only permission inspection).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminMember {
    pub user: Snowflake,
    pub name: String,
    /// The account itself.
    pub me: bool,
    pub roles: Vec<Snowflake>,
    /// Why the account may not change this member's roles.
    pub lock: Option<&'static str>,
    /// Effective permissions per channel of [`GuildAdmin::channels`].
    pub effective: Vec<(Snowflake, Permissions)>,
}

impl AdminMember {
    pub fn effective_in(&self, channel: Snowflake) -> Option<Permissions> {
        self.effective
            .iter()
            .find(|(id, _)| *id == channel)
            .map(|(_, p)| *p)
    }
}

pub const MEMBER_IS_OWNER: &str = "The owner's roles cannot be changed.";
pub const MEMBER_ABOVE_YOU: &str = "This member's highest role is not below yours.";
pub const MEMBER_ROLES_UNKNOWN: &str = "This member's roles are not known yet.";

/// The role view of a held member of a guild; `None` when either is unknown
/// or the account's own roles are not yet established.
pub fn admin_member(store: &Store, admin: &GuildAdmin, user: Snowflake) -> Option<AdminMember> {
    let guild = store.guild(admin.id)?;
    let me = store.current_user()?.id;
    let member = guild.member(user)?;
    let owner_id = guild.owner_id()?;
    let name = store.user(user).map(|u| {
        member
            .nick
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| u.display_name())
    });
    let lock = if !member.roles_known {
        Some(MEMBER_ROLES_UNKNOWN)
    } else if user == me {
        None
    } else if user == owner_id {
        Some(MEMBER_IS_OWNER)
    } else if !admin.owner && top_of(guild, &member.roles) >= admin.top_position {
        Some(MEMBER_ABOVE_YOU)
    } else {
        None
    };
    let scope = GuildScope {
        id: guild.id(),
        owner_id,
        roles: guild.roles(),
    };
    let timed_out = member
        .communication_disabled_until
        .as_deref()
        .is_some_and(active_timeout);
    let who = MemberScope {
        user_id: user,
        role_ids: &member.roles,
        timed_out,
    };
    let effective = if member.roles_known {
        admin
            .channels
            .iter()
            .filter_map(|row| {
                let channel = guild.channel(row.id)?;
                Some((
                    row.id,
                    channel_permissions(
                        scope,
                        who,
                        &channel.permission_overwrites,
                        channel.kind.is_text_like(),
                    ),
                ))
            })
            .collect()
    } else {
        Vec::new()
    };
    Some(AdminMember {
        user,
        name: capped_name(name, "Unknown user"),
        me: user == me,
        roles: member.roles.clone(),
        lock,
        effective,
    })
}

/// Text channels offered as the destination of a follow.
pub const MAX_FOLLOW_TARGETS: usize = 256;

/// A text channel a followed announcement channel may post into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowTarget {
    pub guild: Snowflake,
    pub guild_name: String,
    pub channel: Snowflake,
    pub channel_name: String,
}

/// Where the announcement channel `source` may be followed from this account:
/// every other text channel it can see in a server where it has Manage
/// Webhooks (channel overwrites included), at most [`MAX_FOLLOW_TARGETS`], in
/// server and channel order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowTargets {
    pub source: Snowflake,
    pub targets: Vec<FollowTarget>,
}

/// `None` unless `source` is an announcement channel of `guild` that the
/// account can see; computed from the reducer's permissions, never guessed.
pub fn follow_targets(store: &Store, guild: Snowflake, source: Snowflake) -> Option<FollowTargets> {
    let entry = store.guild(guild)?;
    let channel = entry.channel(source)?;
    let context = PermissionContext::new(store, entry)?;
    (channel.kind == ModelChannelKind::GuildAnnouncement
        && context.permissions(entry, channel).view_channel)
        .then_some(())?;
    let mut targets = Vec::new();
    'guilds: for id in store.guild_ids() {
        let Some(entry) = store.guild(*id) else {
            continue;
        };
        let Some(context) = PermissionContext::new(store, entry) else {
            continue;
        };
        let mut channels: Vec<&fastcord_model::Channel> = entry
            .channels()
            .filter(|c| c.kind == ModelChannelKind::GuildText && c.id != source)
            .filter(|c| {
                context.permissions(entry, c).view_channel
                    && context.raw(c).contains(Permissions::MANAGE_WEBHOOKS)
            })
            .collect();
        channels.sort_by_key(|c| (c.position.unwrap_or(0), c.id));
        for c in channels {
            if targets.len() >= MAX_FOLLOW_TARGETS {
                break 'guilds;
            }
            targets.push(FollowTarget {
                guild: entry.id(),
                guild_name: capped_name(Some(entry.name()), "Unnamed server"),
                channel: c.id,
                channel_name: capped_name(c.name.as_deref(), "Unnamed channel"),
            });
        }
    }
    Some(FollowTargets { source, targets })
}
