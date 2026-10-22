//! Server and channel management read model (issue #93). A pure query over the
//! reducer's store for one guild: its settings, every channel the account may
//! see in display order with the settings an edit starts from, which of them
//! the account may manage (computed from the same permission context as the
//! sidebar), and the held people an ownership transfer can name. Nothing is
//! kept between calls, and every list and text is capped.

use fastcord_model::{ChannelKind as ModelChannelKind, Permissions, Snowflake, guild_permissions};

use crate::Placement;

use super::{PermissionContext, Store, capped_name, clip_bytes};

/// Channels listed (Discord allows 500 per server).
pub const MAX_ADMIN_CHANNELS: usize = 500;
/// People an ownership transfer can pick from.
pub const MAX_ADMIN_PEOPLE: usize = 256;
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
}

impl GuildAdmin {
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
    let settings = guild.settings();
    Some(GuildAdmin {
        id: guild_id,
        name: capped_name(Some(guild.name()), "Unnamed guild"),
        has_icon: guild.icon().is_some(),
        owner: guild.owner_id() == Some(me),
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
    })
}
