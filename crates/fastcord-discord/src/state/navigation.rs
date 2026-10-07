//! Permission-gated guild/channel navigation over the reducer's authoritative store.
//!
//! Reconcile after each ordered Store mutation. Selection and per-guild memory use
//! IDs, never row indices; a fresh READY for the same account preserves valid IDs.
//! Windows own at most 128 capped names, not guilds, members, or message histories.
//! Queries perform no I/O, subscriptions request no member-list viewport, and no
//! timer is needed. Unsupported channel kinds (threads, forums, DMs) are not part
//! of guild navigation.

use std::collections::BTreeMap;

use fastcord_model::{
    Channel, ChannelKind as ModelChannelKind, GuildScope, MemberScope, Permissions, Snowflake,
    channel_permissions,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{GuildEntry, Store};
use crate::gateway::SubscriptionTarget;

/// Absolute response row cap, independent of a caller's requested viewport.
pub const MAX_WINDOW_ROWS: usize = 128;
/// UTF-8 byte cap for each name copied into a read model.
pub const MAX_NAME_BYTES: usize = 256;

/// An owned, immutable-by-convention slice of a list, including its full length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window<T> {
    /// Requested offset clamped to `total`; row indices begin here.
    pub offset: usize,
    pub total: usize,
    pub rows: Vec<T>,
}

impl<T> Default for Window<T> {
    fn default() -> Self {
        Self {
            offset: 0,
            total: 0,
            rows: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuildRow {
    pub id: Snowflake,
    pub name: String,
    /// Outage guilds have only an ID and cannot be explicitly selected.
    pub available: bool,
    pub selected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    Category,
    Text,
    Announcement,
    Voice,
}

impl ChannelKind {
    fn from_model(kind: ModelChannelKind) -> Option<Self> {
        match kind {
            ModelChannelKind::GuildCategory => Some(Self::Category),
            ModelChannelKind::GuildText => Some(Self::Text),
            ModelChannelKind::GuildAnnouncement => Some(Self::Announcement),
            ModelChannelKind::GuildVoice | ModelChannelKind::GuildStageVoice => Some(Self::Voice),
            _ => None,
        }
    }
}

/// Effective model permissions, including overwrites and implicit restrictions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PermissionSummary {
    pub view_channel: bool,
    pub read_history: bool,
    pub send_messages: bool,
    pub connect: bool,
    pub speak: bool,
}

impl From<Permissions> for PermissionSummary {
    fn from(permissions: Permissions) -> Self {
        Self {
            view_channel: permissions.contains(Permissions::VIEW_CHANNEL),
            read_history: permissions.contains(Permissions::READ_MESSAGE_HISTORY),
            send_messages: permissions.contains(Permissions::SEND_MESSAGES),
            connect: permissions.contains(Permissions::CONNECT),
            speak: permissions.contains(Permissions::SPEAK),
        }
    }
}

impl PermissionSummary {
    fn enables(self, kind: ChannelKind) -> bool {
        self.view_channel
            && match kind {
                ChannelKind::Category => false,
                ChannelKind::Voice => self.connect,
                ChannelKind::Text | ChannelKind::Announcement => true,
            }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelRow {
    pub id: Snowflake,
    pub name: String,
    pub kind: ChannelKind,
    /// Visible category ID, or none for uncategorized/orphaned channels.
    pub parent_id: Option<Snowflake>,
    /// Categories are headers; voice without CONNECT is visible but disabled.
    pub enabled: bool,
    pub selected: bool,
    pub permissions: PermissionSummary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelSelection {
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
    pub name: String,
    pub kind: ChannelKind,
    pub permissions: PermissionSummary,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NavigationSnapshot {
    pub guilds: Window<GuildRow>,
    pub channels: Window<ChannelRow>,
    pub selected_guild: Option<Snowflake>,
    pub selection: Option<ChannelSelection>,
    /// Available guild lacks authoritative own-member/role/owner data.
    pub permissions_pending: bool,
}

/// Reducer-owned selection. Memory has at most one channel ID per joined guild
/// (including temporarily unavailable guilds), pruned on each reconciliation.
#[derive(Debug, Default)]
pub struct Navigation {
    account: Option<Snowflake>,
    selected_guild: Option<Snowflake>,
    selected_channel: Option<Snowflake>,
    remembered: BTreeMap<Snowflake, Snowflake>,
}

impl Navigation {
    pub fn selected_guild(&self) -> Option<Snowflake> {
        self.selected_guild
    }

    pub fn selected_channel(&self) -> Option<Snowflake> {
        self.selected_channel
    }

    /// Preserves valid IDs, clears revoked/deleted channels, and prunes memory.
    /// Outages retain the guild and remembered ID, but never an open channel.
    /// No channel is opened automatically; returning to a guild can restore its
    /// last still-accessible selection. With no retained guild, the first
    /// available READY-order guild becomes selected.
    pub fn reconcile(&mut self, store: &Store) {
        let account = store.current_user().map(|user| user.id);
        if self.account != account {
            self.account = account;
            self.selected_guild = None;
            self.selected_channel = None;
            self.remembered = BTreeMap::new();
        }
        self.remembered.retain(|guild_id, channel_id| {
            if let Some(guild) = store.guild(*guild_id) {
                enabled_channel(store, guild, *channel_id).is_some()
            } else {
                store.unavailable.contains(guild_id)
            }
        });
        if !self
            .selected_guild
            .is_some_and(|guild| known_guild(store, guild))
        {
            self.selected_guild = store.guild_ids().first().copied();
        }
        self.selected_channel = self.selected_guild.and_then(|guild| {
            store.guild(guild)?;
            self.remembered.get(&guild).copied()
        });
    }

    /// Rejects unknown/outage guilds without changing selection.
    pub fn select_guild(&mut self, store: &Store, guild_id: Snowflake) -> bool {
        if store.guild(guild_id).is_none() {
            return false;
        }
        self.reconcile(store);
        self.selected_guild = Some(guild_id);
        self.selected_channel = self.remembered.get(&guild_id).copied();
        true
    }

    /// Opens only an enabled channel of the currently selected guild. A stale
    /// UI row or injected ID is never sufficient authorization.
    pub fn select_channel(&mut self, store: &Store, channel_id: Snowflake) -> bool {
        if self.account != store.current_user().map(|user| user.id) {
            return false;
        }
        let Some(guild_id) = self.selected_guild else {
            return false;
        };
        let Some(guild) = store.guild(guild_id) else {
            return false;
        };
        if enabled_channel(store, guild, channel_id).is_none() {
            return false;
        }
        self.selected_channel = Some(channel_id);
        self.remembered.insert(guild_id, channel_id);
        true
    }

    /// READY/GUILD_CREATE order for available guilds, then outage IDs in numeric
    /// order. Outage names are decimal IDs because no name is authoritative.
    pub fn guild_window(&self, store: &Store, offset: usize, count: usize) -> Window<GuildRow> {
        let ids = || {
            store.guild_ids().iter().copied().chain(
                store
                    .unavailable_guilds()
                    .filter(|id| store.guild(*id).is_none()),
            )
        };
        let total = store.guild_ids().len()
            + store
                .unavailable_guilds()
                .filter(|id| store.guild(*id).is_none())
                .count();
        let offset = offset.min(total);
        let rows = ids()
            .skip(offset)
            .take(count.min(MAX_WINDOW_ROWS))
            .map(|id| {
                let guild = store.guild(id);
                GuildRow {
                    id,
                    name: guild.map_or_else(
                        || id.to_string(),
                        |guild| capped_name(Some(guild.name()), "Unnamed guild"),
                    ),
                    available: guild.is_some(),
                    selected: self.account == store.current_user().map(|user| user.id)
                        && self.selected_guild == Some(id),
                }
            })
            .collect();
        Window {
            offset,
            total,
            rows,
        }
    }

    /// Selected guild only. Uncategorized channels come first, then category
    /// groups in `(position, ID)` order, each header followed by children in
    /// `(position, ID)` order. Missing positions are zero. A hidden/missing
    /// category never hides a child with its own VIEW_CHANNEL; that child is
    /// uncategorized and no forbidden category name is copied.
    pub fn channel_window(&self, store: &Store, offset: usize, count: usize) -> Window<ChannelRow> {
        let visible = self
            .selected_guild
            .filter(|_| self.account == store.current_user().map(|user| user.id))
            .and_then(|id| store.guild(id))
            .map_or_else(Vec::new, |guild| visible_channels(store, guild));
        let total = visible.len();
        let offset = offset.min(total);
        let rows = visible
            .into_iter()
            .skip(offset)
            .take(count.min(MAX_WINDOW_ROWS))
            .map(|row| ChannelRow {
                id: row.channel.id,
                name: capped_name(row.channel.name.as_deref(), "Unnamed channel"),
                kind: row.kind,
                parent_id: row.parent_id,
                enabled: row.permissions.enables(row.kind),
                selected: self.selected_channel == Some(row.channel.id)
                    && row.permissions.enables(row.kind),
                permissions: row.permissions,
            })
            .collect();
        Window {
            offset,
            total,
            rows,
        }
    }

    /// Rechecks authorization before exposing an open channel or its summary.
    pub fn selection(&self, store: &Store) -> Option<ChannelSelection> {
        if self.account != store.current_user().map(|user| user.id) {
            return None;
        }
        let guild_id = self.selected_guild?;
        let guild = store.guild(guild_id)?;
        let channel_id = self.selected_channel?;
        let (channel, kind, permissions) = enabled_channel(store, guild, channel_id)?;
        Some(ChannelSelection {
            guild_id,
            channel_id,
            name: capped_name(channel.name.as_deref(), "Unnamed channel"),
            kind,
            permissions,
        })
    }

    pub fn snapshot(
        &self,
        store: &Store,
        guild_offset: usize,
        guild_count: usize,
        channel_offset: usize,
        channel_count: usize,
    ) -> NavigationSnapshot {
        let selected_guild = self.selected_guild.filter(|id| {
            self.account == store.current_user().map(|user| user.id) && known_guild(store, *id)
        });
        let permissions_pending = selected_guild
            .and_then(|id| store.guild(id))
            .is_some_and(|guild| PermissionContext::new(store, guild).is_none());
        NavigationSnapshot {
            guilds: self.guild_window(store, guild_offset, guild_count),
            channels: self.channel_window(store, channel_offset, channel_count),
            selected_guild,
            selection: self.selection(store),
            permissions_pending,
        }
    }

    /// Declarative guild/own-member interest only, with no history request and
    /// no member-list range. Outages are never subscribed. Verified owners need
    /// no member fetch to establish access; an omitted/null own roles array does.
    pub fn subscription_target(&self, store: &Store) -> SubscriptionTarget {
        let mut target = SubscriptionTarget::default();
        if self.account != store.current_user().map(|user| user.id) {
            return target;
        }
        let Some(guild_id) = self.selected_guild else {
            return target;
        };
        let Some(guild) = store.guild(guild_id) else {
            return target;
        };
        target.select_guild(guild_id);
        if let Some(channel_id) = self.selected_channel
            && enabled_channel(store, guild, channel_id).is_some()
        {
            target.select_channel(guild_id, channel_id);
            target.clear_member_viewport();
        }
        if let Some(user) = store.current_user()
            && guild.owner_id() != Some(user.id)
            && !guild
                .member(user.id)
                .is_some_and(|member| member.roles_known)
        {
            target.set_member_interest(guild_id, [user.id]);
        }
        target
    }
}

fn known_guild(store: &Store, id: Snowflake) -> bool {
    store.guild(id).is_some() || store.unavailable.contains(&id)
}

fn capped_name(value: Option<&str>, fallback: &str) -> String {
    let value = value.filter(|name| !name.is_empty()).unwrap_or(fallback);
    let mut end = value.len().min(MAX_NAME_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

enum PermissionContext<'a> {
    Owner,
    Member {
        guild: GuildScope<'a>,
        member: MemberScope<'a>,
    },
}

impl<'a> PermissionContext<'a> {
    fn new(store: &'a Store, guild: &'a GuildEntry) -> Option<Self> {
        Self::with_clock(store, guild, OffsetDateTime::now_utc)
    }

    fn with_clock(
        store: &'a Store,
        guild: &'a GuildEntry,
        now: impl FnOnce() -> OffsetDateTime,
    ) -> Option<Self> {
        let user = store.current_user()?.id;
        let owner_id = guild.owner_id()?;
        if user == owner_id {
            return Some(Self::Owner);
        }
        let own = guild.member(user)?;
        if !own.roles_known
            || guild
                .roles()
                .binary_search_by_key(&guild.id(), |role| role.id)
                .is_err()
            || own.roles.iter().any(|id| {
                guild
                    .roles()
                    .binary_search_by_key(id, |role| role.id)
                    .is_err()
            })
        {
            return None;
        }
        Some(Self::Member {
            guild: GuildScope {
                id: guild.id(),
                owner_id,
                roles: guild.roles(),
            },
            member: MemberScope {
                user_id: user,
                role_ids: &own.roles,
                timed_out: own
                    .communication_disabled_until
                    .as_deref()
                    .is_some_and(|until| timeout_active(until, now())),
            },
        })
    }

    fn permissions(&self, channel: &Channel) -> PermissionSummary {
        let permissions = match self {
            Self::Owner => Permissions::ALL,
            Self::Member { guild, member } => channel_permissions(
                *guild,
                *member,
                &channel.permission_overwrites,
                channel.kind.is_text_like(),
            ),
        };
        permissions.into()
    }
}

// Malformed expiry is unknown, not permission to send/connect. Timeouts are
// evaluated on demand; navigation never starts a periodic expiry poll.
fn timeout_active(until: &str, now: OffsetDateTime) -> bool {
    OffsetDateTime::parse(until, &Rfc3339).map_or(true, |expiry| expiry > now)
}

fn enabled_channel<'a>(
    store: &'a Store,
    guild: &'a GuildEntry,
    id: Snowflake,
) -> Option<(&'a Channel, ChannelKind, PermissionSummary)> {
    let channel = guild.channel(id)?;
    let kind = ChannelKind::from_model(channel.kind)?;
    let permissions = PermissionContext::new(store, guild)?.permissions(channel);
    permissions
        .enables(kind)
        .then_some((channel, kind, permissions))
}

type ChannelOrder = (u8, i32, Snowflake, u8, i32, Snowflake);

struct VisibleChannel<'a> {
    channel: &'a Channel,
    kind: ChannelKind,
    parent_id: Option<Snowflake>,
    permissions: PermissionSummary,
    order: ChannelOrder,
}

fn visible_channels<'a>(store: &'a Store, guild: &'a GuildEntry) -> Vec<VisibleChannel<'a>> {
    let Some(context) = PermissionContext::new(store, guild) else {
        return Vec::new();
    };
    // Temporary borrowed metadata, not cloned account state or owned names.
    // The ID-sorted Store order also makes category lookup logarithmic.
    let mut visible: Vec<_> = guild
        .channels()
        .filter_map(|channel| {
            let kind = ChannelKind::from_model(channel.kind)?;
            let permissions = context.permissions(channel);
            if !permissions.view_channel {
                return None;
            }
            let position = channel.position.unwrap_or(0);
            let order = if kind == ChannelKind::Category {
                (1, position, channel.id, 0, 0, channel.id)
            } else {
                (0, 0, Snowflake(0), 1, position, channel.id)
            };
            Some(VisibleChannel {
                channel,
                kind,
                parent_id: None,
                permissions,
                order,
            })
        })
        .collect();
    for at in 0..visible.len() {
        if visible[at].kind == ChannelKind::Category {
            continue;
        }
        let category = visible[at]
            .channel
            .parent_id
            .and_then(|parent| {
                visible
                    .binary_search_by_key(&parent, |row| row.channel.id)
                    .ok()
            })
            .filter(|parent| visible[*parent].kind == ChannelKind::Category);
        if let Some(category) = category {
            let parent = visible[category].channel;
            visible[at].parent_id = Some(parent.id);
            visible[at].order.0 = 1;
            visible[at].order.1 = parent.position.unwrap_or(0);
            visible[at].order.2 = parent.id;
        }
    }
    visible.sort_unstable_by_key(|row| row.order);
    visible
}

#[cfg(test)]
mod tests;
