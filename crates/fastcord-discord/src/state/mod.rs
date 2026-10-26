//! Bounded, single-writer normalized Discord state (SPEC §4.5).
//!
//! One [`Store`] is owned by one reducer task. It applies the Gateway's ordered
//! events (`apply`) and answers read-only queries; nothing else mutates it. IDs
//! reference entities, entities are stored once (users in one table, members
//! by user ID per guild), and the whole store is accounted in estimated bytes
//! against the 12 MiB metadata budget.
//!
//! Retention. Budget eviction never sheds navigable guild/channel/role
//! identity, permission data, or the current user's own member.
//! Everything else is optional detail that is cheap to fetch again and goes
//! first when the budget is exceeded, in this order: users nobody references
//! (removals also release theirs at once), member lists other than the
//! visible one, scheduled events, members nobody references (oldest write
//! first), and last the read state (every unread marker and mention count,
//! reported as a read change; READY restores it). A member is referenced while it is the current user,
//! has a voice state, was asked for by the consumer (visible authors, reply
//! targets, permission computation), or is a row of a retained member list.
//! Member lists exist only for the guild currently selected in the consumer's
//! [`SubscriptionTarget`] and are released as soon as it navigates away.
//! If required identity or pinned detail alone cannot fit, the store releases
//! account state and enters terminal [`Store::limit_exceeded`] failure rather
//! than retaining an unchecked overflow or exposing incomplete permissions.
//!
//! Sizes are estimates of entity inline size plus owned allocation capacities,
//! including unused vector slots and hash-table buckets, not allocator
//! measurements. [`Store::recount`] recomputes the total from scratch so tests
//! can prove the incremental ledger never drifts.

mod activities;
mod calls;
mod events;
mod guild;
mod list_id;
mod member_list;
pub mod navigation;
mod presences;
mod read;
mod reduce;
#[cfg(test)]
mod regressions;
mod relationships;
mod retention;
#[cfg(test)]
mod safety_tests;
pub use calls::{CallInfo, MAX_CALL_USERS, MAX_CALLS};
pub use events::MAX_EVENTS;
pub use relationships::Suppression;
#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::mem::size_of;

use fastcord_model::{Channel, Guild, Message, OverwriteKind, Snowflake, StageInstance, User};

use crate::gateway::{
    Dispatch, GatewayEvent, GuildCreate, GuildMemberEvent, GuildRoleDelete, GuildRoleEvent,
    GuildUpdate, MemberListUpdate, NotifyLevel, PassiveUpdate, Ready, ReadySupplemental,
    SubscriptionTarget, ThreadListSync, VoiceStateUpdate,
};
use crate::stage::MAX_TOPIC_CHARS;
#[cfg(test)]
use fastcord_model::{GuildEmoji, GuildMember};
pub use guild::GuildEntry;
use guild::*;
pub use member_list::{MAX_ROWS, MemberList};
pub use read::{MAX_MUTED, MAX_READ_ENTRIES, ReadStates, Unread};

/// Budget for guild/channel/role metadata, users, members, voice states, and
/// visible member ranges together (SPEC §4.5).
pub const METADATA_BUDGET: usize = 12 * 1024 * 1024;

/// Shedding stops at this share of the budget, so a store at its limit does
/// not shed on every event.
const LOW_WATER_PERCENT: usize = 80;

/// Threads kept per guild. Past it, threads the account has not joined and
/// archived ones go first, then the oldest by ID.
pub const MAX_THREADS_PER_GUILD: usize = 1024;

/// Member lists kept per guild: the visible one plus the one it just replaced,
/// whose late events must not resurrect an unbounded set.
const MAX_LISTS_PER_GUILD: usize = 2;

/// What the consumer is looking at; decides what is pinned and which member
/// lists may exist. Set from the same [`SubscriptionTarget`] the Gateway
/// subscribes with.
#[derive(Default)]
struct Focus {
    guild: Option<Snowflake>,
    channel: Option<Snowflake>,
    list_id: Option<crate::gateway::MemberListId>,
    asked: HashMap<Snowflake, HashSet<Snowflake>>,
    ranges: Vec<crate::gateway::MemberRange>,
}

impl Focus {
    fn bytes(&self) -> usize {
        self.list_id.as_ref().map_or(0, |id| id.0.capacity())
            + self.ranges.capacity() * size_of::<crate::gateway::MemberRange>()
            + table_bytes::<Snowflake, HashSet<Snowflake>>(self.asked.capacity())
            + self
                .asked
                .values()
                .map(|members| table_bytes::<Snowflake, ()>(members.capacity()))
                .sum::<usize>()
    }
}

/// What an applied event changed, coalesced: sets, never a queue, so the
/// consumer's backlog is bounded by the number of guilds, not events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// A new session replaced everything (READY).
    pub reset: bool,
    /// Guilds whose metadata, channels, or members changed.
    pub guilds: BTreeSet<Snowflake>,
    /// Guilds whose visible member list changed.
    pub member_lists: BTreeSet<Snowflake>,
    /// Guilds whose voice membership changed.
    pub voice: BTreeSet<Snowflake>,
    /// Direct-message channels changed.
    pub private_channels: bool,
    /// Read cursors, mention counts or mutes changed, so unread markers may
    /// have: navigation is rebuilt even for message events.
    pub read: bool,
    /// Friends, blocks or pending requests changed.
    pub relationships: bool,
    /// Private calls (ringing or connected users) changed.
    pub calls: bool,
    /// A referenced user's presence changed.
    pub presence: bool,
    /// The account's own user object changed.
    pub account: bool,
    /// A known privacy setting changed.
    pub privacy: bool,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The reducer's state.
pub struct Store {
    budget: usize,
    tick: u64,
    limit_exceeded: bool,
    current_user: Option<Snowflake>,
    users: HashMap<Snowflake, User>,
    users_bytes: usize,
    guilds: HashMap<Snowflake, GuildEntry>,
    guild_order: Vec<Snowflake>,
    guilds_bytes: usize,
    unavailable: BTreeSet<Snowflake>,
    private_channels: HashMap<Snowflake, Channel>,
    private_bytes: usize,
    relationships: relationships::Relationships,
    privacy: std::sync::Arc<crate::PrivacyLive>,
    calls: calls::Calls,
    presences: presences::Presences,
    focus: Focus,
    read: ReadStates,
    folders: std::sync::Arc<crate::user_settings::GuildFolders>,
}

impl fmt::Debug for Store {
    // Names counts only: the contents are account data.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("guilds", &self.guilds.len())
            .field("users", &self.users.len())
            .field("bytes", &self.bytes())
            .finish_non_exhaustive()
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        Self::with_budget(METADATA_BUDGET)
    }

    /// A store with a smaller budget, for tests of the retention rules.
    pub fn with_budget(budget: usize) -> Self {
        Self {
            budget,
            tick: 0,
            limit_exceeded: false,
            current_user: None,
            users: HashMap::new(),
            users_bytes: 0,
            guilds: HashMap::new(),
            guild_order: Vec::new(),
            guilds_bytes: 0,
            unavailable: BTreeSet::new(),
            private_channels: HashMap::new(),
            private_bytes: 0,
            relationships: relationships::Relationships::default(),
            privacy: Default::default(),
            calls: calls::Calls::default(),
            presences: presences::Presences::default(),
            focus: Focus::default(),
            read: ReadStates::default(),
            folders: Default::default(),
        }
    }

    // ----- reads -----

    pub fn current_user(&self) -> Option<&User> {
        self.current_user.and_then(|id| self.users.get(&id))
    }

    pub fn user(&self, id: Snowflake) -> Option<&User> {
        self.users.get(&id)
    }

    pub fn users_held(&self) -> usize {
        self.users.len()
    }

    /// Guild IDs in the order READY and GUILD_CREATE introduced them.
    pub fn guild_ids(&self) -> &[Snowflake] {
        &self.guild_order
    }

    pub fn guild(&self, id: Snowflake) -> Option<&GuildEntry> {
        self.guilds.get(&id)
    }

    /// Joined guilds currently in an outage (known by ID only).
    pub fn unavailable_guilds(&self) -> impl Iterator<Item = Snowflake> + '_ {
        self.unavailable.iter().copied()
    }

    pub fn private_channel(&self, id: Snowflake) -> Option<&Channel> {
        self.private_channels.get(&id)
    }

    pub fn private_channels(&self) -> impl Iterator<Item = &Channel> {
        self.private_channels.values()
    }

    /// Estimated bytes held.
    pub fn bytes(&self) -> usize {
        self.allocation_overhead()
            + self.users_bytes
            + self.guilds_bytes
            + self.private_bytes
            + self.relationships.bytes()
            + self.calls.bytes()
            + self.presences.bytes()
    }

    /// Friends, blocks and pending requests, sorted by kind then name.
    pub fn relationships(&self) -> std::sync::Arc<[fastcord_model::Relationship]> {
        self.relationships.rows()
    }

    /// Live private calls, sorted by channel.
    pub fn calls(&self) -> std::sync::Arc<[CallInfo]> {
        self.calls.rows()
    }

    /// Whether the user's messages, mentions, notifications and calls are
    /// withheld, and why. The one rule every consumer reads; the account's own
    /// ID is never suppressed.
    pub fn suppression(&self, id: Snowflake) -> Option<Suppression> {
        if self.current_user == Some(id) {
            return None;
        }
        self.relationships.suppression(id)
    }

    /// Privacy settings as Gateway events set them since READY.
    pub fn privacy(&self) -> std::sync::Arc<crate::PrivacyLive> {
        std::sync::Arc::clone(&self.privacy)
    }

    /// Every suppressed user, shared and rebuilt only when it changes.
    pub fn suppressed(&self) -> std::sync::Arc<std::collections::BTreeMap<Snowflake, Suppression>> {
        self.relationships.suppressed()
    }

    pub fn relationship(&self, id: Snowflake) -> Option<fastcord_model::RelationshipKind> {
        self.relationships.kind(id)
    }

    /// Presence of referenced users (friends, direct-conversation recipients,
    /// visible members and the account), shared with snapshots.
    pub fn presences(&self) -> std::sync::Arc<HashMap<Snowflake, fastcord_model::Presence>> {
        self.presences.shared()
    }

    pub fn presences_held(&self) -> usize {
        self.presences.len()
    }

    /// Whether anything on screen or in a list still refers to this user.
    /// Whether the navigation snapshot draws this user's presence.
    fn presence_drawn(&self, id: Snowflake) -> bool {
        self.current_user == Some(id)
            || self.relationships.kind(id).is_some()
            || self
                .private_channels
                .values()
                .any(|channel| channel.recipient_ids.contains(&id))
    }

    fn presence_wanted(&self, id: Snowflake) -> bool {
        self.current_user == Some(id)
            || self.relationships.kind(id).is_some()
            || self
                .private_channels
                .values()
                .any(|channel| channel.recipient_ids.contains(&id))
            || self.guilds.values().any(|entry| {
                entry.members.contains_key(&id)
                    || entry
                        .lists
                        .iter()
                        .any(|list| list.member_ids().any(|m| m == id))
            })
    }

    fn prune_presences_with(&mut self, mut wanted: HashMap<Snowflake, usize>) -> bool {
        wanted.extend(self.relationships.ids().map(|id| (id, 1)));
        self.presences.retain(|id| wanted.contains_key(&id))
    }

    pub fn relationships_held(&self) -> usize {
        self.relationships.len()
    }

    fn allocation_overhead(&self) -> usize {
        map_overhead(&self.users)
            + map_overhead(&self.guilds)
            + map_overhead(&self.private_channels)
            + self.guild_order.capacity() * size_of::<Snowflake>()
            // BTreeSet has no capacity API. Charge each ID with an allowance
            // for its share of a partially occupied tree node.
            + self.unavailable.len() * (size_of::<Snowflake>() + 48)
            + self.focus.bytes()
            + self.read.bytes()
            + self.folders.bytes()
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    // ----- read state -----

    /// What a channel (or DM, `guild` `None`) shows, given its newest message.
    pub fn unread(
        &self,
        channel: Snowflake,
        guild: Option<Snowflake>,
        last_message: Option<Snowflake>,
    ) -> Unread {
        self.read.unread(
            channel,
            self.is_muted(channel, guild),
            last_message,
            self.notify_level(channel, guild),
        )
    }

    /// The channel, its parent and (for a thread) the parent's category:
    /// where a mute or notification level is inherited from (#148). Bounded,
    /// so a parent cycle from the wire cannot loop.
    fn lineage(
        &self,
        channel: Snowflake,
        guild: Option<Snowflake>,
    ) -> impl Iterator<Item = Snowflake> + '_ {
        let entry = guild.and_then(|guild| self.guild(guild));
        let mut next = Some(channel);
        std::iter::from_fn(move || {
            let id = next?;
            next = entry
                .and_then(|entry| entry.channel(id))
                .and_then(|channel| channel.parent_id);
            Some(id)
        })
        .take(3)
    }

    /// What a channel notifies about: its override, its category's (a
    /// thread: its channel's, then that category's), the server's level, or
    /// the server's default (`default_message_notifications`).
    pub fn notify_level(&self, channel: Snowflake, guild: Option<Snowflake>) -> NotifyLevel {
        let default = guild
            .and_then(|guild| self.guild(guild))
            .and_then(|guild| NotifyLevel::from_wire(guild.settings().notifications.into()));
        self.read
            .level(self.lineage(channel, guild), guild, default)
    }

    /// Whether the server itself is muted.
    pub fn guild_muted(&self, guild: Snowflake) -> bool {
        self.read.guild_muted(guild)
    }

    /// The server's explicit level (`None`: its default applies).
    pub fn guild_notify_level(&self, guild: Snowflake) -> Option<NotifyLevel> {
        self.read.guild_level(guild)
    }

    /// The channel's own override (`None`: inherits).
    pub fn channel_notify_level(&self, channel: Snowflake) -> Option<NotifyLevel> {
        self.read.channel_level(channel)
    }

    /// Whether `@everyone`/`@here` and role mentions of a server are muted.
    pub fn suppressions(&self, guild: Snowflake) -> (bool, bool) {
        (
            self.read.suppresses_everyone(guild),
            self.read.suppresses_roles(guild),
        )
    }

    /// The account's server folders and order (shared, replaced whole).
    pub fn folders(&self) -> std::sync::Arc<crate::user_settings::GuildFolders> {
        self.folders.clone()
    }

    /// What a guild shows: the unread flags and mention counts of the channels
    /// the account can open, the same ones the sidebar and the inbox list. A
    /// hidden channel could never be read, so it never lights the icon (#159).
    pub fn guild_unread(&self, guild: Snowflake) -> Unread {
        self.guild(guild).map_or_else(Unread::default, |entry| {
            navigation::readable_channels(self, entry).fold(Unread::default(), |total, channel| {
                total.merge(self.unread(channel.id, Some(guild), channel.last_message_id))
            })
        })
    }

    /// The newest message Discord reported for a channel (`guild` `None` for a
    /// direct message), whatever the account has read.
    pub fn last_message(&self, guild: Option<Snowflake>, channel: Snowflake) -> Option<Snowflake> {
        match guild {
            Some(guild) => self.guild(guild)?.channel(channel)?.last_message_id,
            None => self.private_channel(channel)?.last_message_id,
        }
    }

    /// Whether the channel, its category (a thread: its channel or that
    /// category) or its guild is muted by the account.
    pub fn is_muted(&self, channel: Snowflake, guild: Option<Snowflake>) -> bool {
        self.read.is_muted(self.lineage(channel, guild), guild)
    }

    /// Whether the channel itself is muted (a muted guild does not count).
    pub fn channel_muted(&self, channel: Snowflake) -> bool {
        self.read.channel_muted(channel)
    }

    /// The account's read cursor for a channel, if it has one.
    pub fn acked(&self, channel: Snowflake) -> Option<Snowflake> {
        self.read.acked(channel)
    }

    /// Applies an acknowledgement this client made once Discord accepted it,
    /// without waiting for the echo.
    pub fn ack_local(&mut self, channel: Snowflake, message: Snowflake) -> Changes {
        let mut changes = Changes::default();
        // Only moves the cursor forward: a stale answer cannot mark newer
        // messages read.
        if self
            .read
            .acked(channel)
            .is_some_and(|acked| message > acked)
            && self.read.ack(channel, message, 0)
        {
            changes.read = true;
        }
        changes
    }

    /// Reads and mutes that follow from a new message: the sender's own
    /// message reads the channel, a mention counts, anything newer than the
    /// cursor in an unmuted channel makes it unread.
    fn note_read_effects(&mut self, message: &Message, changes: &mut Changes) {
        let Some(me) = self.current_user else {
            return;
        };
        let channel = message.channel_id;
        if message.author.id == me {
            if self.read.ack(channel, message.id, 0) {
                changes.read = true;
            }
            return;
        }
        // A blocked or ignored author's messages are withheld everywhere,
        // badges included (#162).
        if self.suppression(message.author.id).is_some() {
            return;
        }
        let mentioned = message.guild_id.is_none() || self.notifies_account(message);
        let moved = if mentioned {
            self.read.note_mention(channel, message.id)
        } else {
            let level = self.notify_level(channel, message.guild_id);
            let muted = self.is_muted(channel, message.guild_id);
            self.read.would_flag(channel, muted, message.id, level)
        };
        changes.read |= moved;
    }

    /// Whether the message addresses the account: a direct mention, one of its
    /// roles, or `@everyone`/`@here` as Discord flagged it. A plain direct
    /// message does not count (that is only an unread-badge rule).
    pub fn mentions_account(&self, message: &Message) -> bool {
        self.mentions_with(message, true, true)
    }

    /// [`Self::mentions_account`] as the account's server settings count it:
    /// suppressed `@everyone`/`@here` and role mentions do not.
    pub fn notifies_account(&self, message: &Message) -> bool {
        let (everyone, roles) = message
            .guild_id
            .map_or((false, false), |guild| self.suppressions(guild));
        self.mentions_with(message, !everyone, !roles)
    }

    fn mentions_with(&self, message: &Message, everyone: bool, roles: bool) -> bool {
        let Some(me) = self.current_user else {
            return false;
        };
        (everyone && message.mention_everyone)
            || message.mentions.iter().any(|user| user.id == me)
            || message.guild_id.is_some_and(|guild| {
                roles
                    && !message.mention_roles.is_empty()
                    && self
                        .guild(guild)
                        .and_then(|entry| entry.member(me))
                        .is_some_and(|member| {
                            message
                                .mention_roles
                                .iter()
                                .any(|role| member.roles.contains(role))
                        })
            })
    }

    /// Whether the estimated retained state currently exceeds the budget.
    pub fn over_budget(&self) -> bool {
        self.bytes() > self.budget
    }

    /// Identity or required pins could not fit. Account state has been
    /// released and this store rejects further mutations until replaced.
    pub fn limit_exceeded(&self) -> bool {
        self.limit_exceeded
    }

    /// The total recomputed from the contents, ignoring the running ledger.
    pub fn recount(&self) -> usize {
        self.allocation_overhead()
            + self.users.values().map(user_bytes).sum::<usize>()
            + self.relationships.recount()
            + self.calls.bytes()
            + self.presences.recount()
            + self.guilds.values().map(GuildEntry::recount).sum::<usize>()
            + self
                .private_channels
                .values()
                .map(channel_bytes)
                .sum::<usize>()
    }

    // ----- focus -----

    /// Aligns the store with what the consumer wants subscribed and shown:
    /// member lists of other guilds, and of another channel, are released.
    pub fn focus(&mut self, target: &SubscriptionTarget) -> Changes {
        if self.limit_exceeded {
            return Changes::default();
        }
        let guild = target.selected_guild();
        let channel = target.selected_channel();
        let moved = self.focus.guild != guild || self.focus.channel != channel;
        let ranges = target.ranges();
        let mut changes = Changes::default();
        if moved {
            let ids: Vec<Snowflake> = self.guilds.keys().copied().collect();
            for id in ids {
                let released = self.with_guild(id, |entry| {
                    let had = !entry.lists.is_empty();
                    entry.lists = Vec::new();
                    had
                });
                if released == Some(true) {
                    changes.member_lists.insert(id);
                }
            }
        }
        if !moved
            && ranges != self.focus.ranges
            && let Some(guild) = guild
        {
            let clipped = self.with_guild(guild, |entry| {
                let mut changed = false;
                for list in &mut entry.lists {
                    let before = list.known_rows();
                    list.retain_ranges(&ranges);
                    changed |= before != list.known_rows();
                }
                changed
            });
            if clipped == Some(true) {
                changes.member_lists.insert(guild);
            }
        }
        let mut asked = HashMap::new();
        for id in [guild, target.voice_guild()].into_iter().flatten() {
            asked
                .entry(id)
                .or_insert_with(|| target.member_interest(id).collect());
        }
        self.focus = Focus {
            guild,
            channel,
            list_id: if moved {
                None
            } else {
                self.focus.list_id.take()
            },
            asked,
            ranges,
        };
        self.checked_changes(changes)
    }
}
