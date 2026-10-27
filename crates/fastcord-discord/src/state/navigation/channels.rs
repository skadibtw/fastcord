//! The channel list read model: which channels the account can open, in sidebar
//! order, with the local layout's folding and the permission context behind it.

use std::collections::HashSet;

use fastcord_model::{Channel, Snowflake};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::*;

pub(super) enum PermissionContext<'a> {
    Owner,
    Member {
        guild: GuildScope<'a>,
        member: MemberScope<'a>,
        /// Membership screening is not accepted: no writing until Discord says so.
        pending: bool,
    },
}

impl<'a> PermissionContext<'a> {
    pub(super) fn new(store: &'a Store, guild: &'a GuildEntry) -> Option<Self> {
        Self::with_clock(store, guild, OffsetDateTime::now_utc)
    }

    pub(super) fn with_clock(
        store: &'a Store,
        guild: &'a GuildEntry,
        now: impl FnOnce() -> OffsetDateTime,
    ) -> Option<Self> {
        let own = OwnScope::of(store, guild, now)?;
        Some(if own.owner {
            Self::Owner
        } else {
            Self::Member {
                guild: own.guild,
                member: own.member,
                pending: own.pending,
            }
        })
    }

    pub(super) fn permissions(&self, entry: &GuildEntry, channel: &Channel) -> PermissionSummary {
        // A thread has no overwrites of its own: it inherits its parent's, and
        // a missing parent leaves it inaccessible.
        let overwrites = if channel.kind.is_thread() {
            match channel.parent_id.and_then(|id| entry.channel(id)) {
                Some(parent) => &parent.permission_overwrites,
                None => return PermissionSummary::default(),
            }
        } else {
            &channel.permission_overwrites
        };
        let permissions = match self {
            Self::Owner => Permissions::ALL,
            Self::Member { guild, member, .. } => {
                channel_permissions(*guild, *member, overwrites, channel.kind.is_text_like())
            }
        };
        let mut summary = PermissionSummary::from(permissions);
        if matches!(self, Self::Member { pending: true, .. }) {
            summary.send_messages = false;
            summary.add_reactions = false;
            summary.create_threads = false;
            summary.attach_files = false;
        }
        if let Some(thread) = ThreadState::of(channel) {
            // Private threads exist only for members and thread managers.
            if channel.kind == ModelChannelKind::PrivateThread
                && !thread.joined
                && !summary.manage_threads
            {
                return PermissionSummary::default();
            }
            summary.send_messages = permissions.contains(Permissions::SEND_MESSAGES_IN_THREADS)
                && (!thread.locked || summary.manage_threads);
        }
        summary
    }
}

// Malformed expiry is unknown, not permission to send/connect. Timeouts are
// evaluated on demand; navigation never starts a periodic expiry poll.
pub(super) fn timeout_active(until: &str, now: OffsetDateTime) -> bool {
    OffsetDateTime::parse(until, &Rfc3339).map_or(true, |expiry| expiry > now)
}

pub(crate) fn active_timeout(until: &str) -> bool {
    timeout_active(until, OffsetDateTime::now_utc())
}

/// The channels of `guild` the account can open: what the sidebar lists,
/// categories excluded. The permission context is built once for the guild.
pub(in crate::state) fn readable_channels<'a>(
    store: &'a Store,
    guild: &'a GuildEntry,
) -> impl Iterator<Item = &'a Channel> + 'a {
    let context = PermissionContext::new(store, guild);
    guild.channels().filter(move |channel| {
        let (Some(context), Some(kind)) = (&context, ChannelKind::from_model(channel.kind)) else {
            return false;
        };
        context.permissions(guild, channel).enables(kind)
    })
}

pub(super) fn enabled_channel<'a>(
    store: &'a Store,
    guild: &'a GuildEntry,
    id: Snowflake,
) -> Option<(&'a Channel, ChannelKind, PermissionSummary)> {
    let channel = guild.channel(id)?;
    let kind = ChannelKind::from_model(channel.kind)?;
    let permissions = PermissionContext::new(store, guild)?.permissions(guild, channel);
    permissions
        .enables(kind)
        .then_some((channel, kind, permissions))
}

/// Category group, then the channel's slot, then (for a thread) its place
/// after the parent: `(0, 0)` for everything else, `(1, thread)` for threads.
pub(super) type ChannelOrder = (u8, i32, Snowflake, u8, i32, Snowflake, u8, Snowflake);

pub(super) struct VisibleChannel<'a> {
    pub(super) channel: &'a Channel,
    pub(super) kind: ChannelKind,
    pub(super) parent_id: Option<Snowflake>,
    pub(super) permissions: PermissionSummary,
    pub(super) order: ChannelOrder,
}

pub(super) fn visible_channels<'a>(
    store: &'a Store,
    guild: &'a GuildEntry,
    open: Option<Snowflake>,
) -> Vec<VisibleChannel<'a>> {
    let Some(context) = PermissionContext::new(store, guild) else {
        return Vec::new();
    };
    // Temporary borrowed metadata, not cloned account state or owned names.
    // The ID-sorted Store order also makes category lookup logarithmic.
    let mut visible: Vec<_> = guild
        .channels()
        .filter_map(|channel| {
            let kind = ChannelKind::from_model(channel.kind)?;
            let permissions = context.permissions(guild, channel);
            if !permissions.view_channel {
                return None;
            }
            // Archived threads are reached through the archive list, and forum
            // posts through the forum's post list; both stay in the sidebar
            // only while open.
            if (ThreadState::of(channel).is_some_and(|t| t.archived)
                || is_forum_post(guild, channel))
                && open != Some(channel.id)
            {
                return None;
            }
            let position = channel.position.unwrap_or(0);
            let order = if kind == ChannelKind::Category {
                (1, position, channel.id, 0, 0, channel.id, 0, Snowflake(0))
            } else {
                // Text-like channels above voice ones, each by position, as
                // Discord lists them and as the management panel does
                // (`AdminKind::bucket`) (#154).
                let bucket = 1 + u8::from(kind == ChannelKind::Voice);
                (
                    0,
                    0,
                    Snowflake(0),
                    bucket,
                    position,
                    channel.id,
                    0,
                    Snowflake(0),
                )
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
        if matches!(
            visible[at].kind,
            ChannelKind::Category | ChannelKind::Thread
        ) {
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
    // Threads follow their parent, which must itself be visible.
    let mut orphan = vec![false; visible.len()];
    for at in 0..visible.len() {
        if visible[at].kind != ChannelKind::Thread {
            continue;
        }
        let parent = visible[at].channel.parent_id.and_then(|parent| {
            visible
                .binary_search_by_key(&parent, |row| row.channel.id)
                .ok()
                .filter(|p| {
                    !matches!(
                        visible[*p].kind,
                        ChannelKind::Category | ChannelKind::Thread
                    )
                })
        });
        match parent {
            Some(parent) => {
                let mut order = visible[parent].order;
                order.6 = 1;
                order.7 = visible[at].channel.id;
                visible[at].order = order;
            }
            None => orphan[at] = true,
        }
    }
    let mut at = 0;
    visible.retain(|_| {
        at += 1;
        !orphan[at - 1]
    });
    visible.sort_unstable_by_key(|row| row.order);
    visible
}

impl Navigation {
    /// Drops the channels the local layout folds away: those in a collapsed
    /// category and, where the server asks for it, muted ones. The open
    /// channel, unread ones and voice channels with people in them stay.
    pub(super) fn fold<'a>(
        &self,
        store: &Store,
        guild: &GuildEntry,
        mut visible: Vec<VisibleChannel<'a>>,
    ) -> Vec<VisibleChannel<'a>> {
        let hide_muted = self.layout.hide_muted.contains(&guild.id());
        if !hide_muted && self.layout.collapsed_categories.is_empty() {
            return visible;
        }
        let mut dropped = HashSet::new();
        visible.retain(|row| {
            let id = row.channel.id;
            if row.kind == ChannelKind::Category || self.selected_channel == Some(id) {
                return true;
            }
            if row.kind == ChannelKind::Thread {
                return !row
                    .channel
                    .parent_id
                    .is_some_and(|parent| dropped.contains(&parent));
            }
            let unread = unread_of(store, row.channel);
            let occupied = row.kind == ChannelKind::Voice
                && guild
                    .voice_states()
                    .any(|state| state.channel_id == Some(id));
            let muted = hide_muted
                && store.is_muted(id, Some(guild.id()))
                && unread.mentions == 0
                && !occupied;
            let folded = row
                .parent_id
                .is_some_and(|parent| self.layout.collapsed_categories.contains(&parent))
                && !unread.any()
                && !occupied;
            if muted || folded {
                dropped.insert(id);
                return false;
            }
            true
        });
        visible
    }
}
