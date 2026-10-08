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
//! first when the budget is exceeded, in this order: member lists other than
//! the visible one, members nobody references (oldest write first), users
//! nobody references. A member is referenced while it is the current user,
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

mod list_id;
mod member_list;
pub mod navigation;
#[cfg(test)]
mod regressions;
#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::mem::size_of;

use fastcord_model::{
    Channel, Guild, GuildMember, OverwriteKind, PermissionOverwrite, Role, Snowflake, User,
    VoiceState,
};

pub use member_list::{MAX_ROWS, MemberList};

use crate::gateway::{
    Dispatch, GatewayEvent, GuildCreate, GuildMemberEvent, GuildRoleDelete, GuildRoleEvent,
    GuildUpdate, MemberListUpdate, PassiveUpdate, Ready, ReadySupplemental, SubscriptionTarget,
    VoiceStateUpdate,
};

/// Budget for guild/channel/role metadata, users, members, voice states, and
/// visible member ranges together (SPEC §4.5).
pub const METADATA_BUDGET: usize = 12 * 1024 * 1024;

/// Shedding stops at this share of the budget, so a store at its limit does
/// not shed on every event.
const LOW_WATER_PERCENT: usize = 80;

/// Member lists kept per guild: the visible one plus the one it just replaced,
/// whose late events must not resurrect an unbounded set.
const MAX_LISTS_PER_GUILD: usize = 2;

/// Estimated hash-table storage from its usable capacity, including the
/// load-factor reserve and control bytes. The map's inline owner is charged
/// by its enclosing entity; live values are charged by their own ledgers.
fn table_bytes<K, V>(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = (capacity * 8).div_ceil(7).next_power_of_two();
    buckets * (size_of::<(K, V)>() + 1) + 16
}

fn map_overhead<K, V>(map: &HashMap<K, V>) -> usize {
    table_bytes::<K, V>(map.capacity()) - map.len() * size_of::<V>()
}

fn compact_map<K: Eq + std::hash::Hash, V>(map: &mut HashMap<K, V>) {
    // Amortize ordinary deletes; bulk eviction also compacts once at its end.
    if map.capacity() > map.len().saturating_mul(2) {
        map.shrink_to_fit();
    }
}

fn text(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, String::capacity)
}

fn user_bytes(user: &User) -> usize {
    size_of::<User>() + user.username.capacity() + text(&user.global_name) + text(&user.avatar)
}

fn role_bytes(role: &Role) -> usize {
    size_of::<Role>() + role.name.capacity()
}

fn channel_bytes(channel: &Channel) -> usize {
    size_of::<Channel>()
        + text(&channel.name)
        + channel.permission_overwrites.capacity() * size_of::<PermissionOverwrite>()
        + (channel.recipients.capacity() - channel.recipients.len()) * size_of::<User>()
        + channel.recipients.iter().map(user_bytes).sum::<usize>()
        + channel.recipient_ids.capacity() * size_of::<Snowflake>()
}

fn member_bytes(member: &GuildMember) -> usize {
    size_of::<MemberSlot>()
        + text(&member.nick)
        + member.roles.capacity() * size_of::<Snowflake>()
        + text(&member.communication_disabled_until)
}

fn voice_bytes(state: &VoiceState) -> usize {
    size_of::<VoiceState>() + state.session_id.capacity()
}

/// Dense channel metadata avoids paying for a full Channel in every unused
/// hash-table bucket. Stable sorting preserves the previous last-write-wins
/// behavior for duplicate IDs in a guild snapshot.
fn sorted_channels(mut channels: Vec<Channel>) -> Vec<Channel> {
    channels.sort_by_key(|channel| channel.id);
    channels.dedup_by(|later, earlier| {
        if later.id == earlier.id {
            std::mem::swap(later, earlier);
            true
        } else {
            false
        }
    });
    channels.shrink_to_fit();
    channels
}

/// Role updates replace one authoritative ID, never OR stale duplicate grants.
fn sorted_roles(mut roles: Vec<Role>) -> Vec<Role> {
    roles.sort_by_key(|role| role.id);
    roles.dedup_by(|later, earlier| {
        if later.id == earlier.id {
            std::mem::swap(later, earlier);
            true
        } else {
            false
        }
    });
    roles.shrink_to_fit();
    roles
}

struct MemberSlot {
    member: GuildMember,
    /// Store tick of the last write; the oldest unreferenced member goes first.
    touched: u64,
}

/// One guild as the store holds it.
pub struct GuildEntry {
    id: Snowflake,
    name: String,
    icon: Option<String>,
    owner_id: Option<Snowflake>,
    member_count: u32,
    roles: Vec<Role>,
    channels: Vec<Channel>,
    members: HashMap<Snowflake, MemberSlot>,
    voice: HashMap<Snowflake, VoiceState>,
    lists: Vec<MemberList>,
    /// Cached sizes of the parts that change one element at a time.
    meta_bytes: usize,
    members_bytes: usize,
    voice_bytes: usize,
}

impl GuildEntry {
    fn new(guild: Guild, tick: u64) -> Self {
        let mut entry = Self {
            id: guild.id,
            name: guild.name,
            icon: guild.icon,
            owner_id: guild.owner_id,
            member_count: guild.member_count,
            roles: sorted_roles(guild.roles),
            channels: sorted_channels(guild.channels),
            members: HashMap::new(),
            voice: HashMap::new(),
            lists: Vec::new(),
            meta_bytes: 0,
            members_bytes: 0,
            voice_bytes: 0,
        };
        entry.recompute_meta();
        for member in guild.members {
            entry.put_member(member, tick);
        }
        entry
    }

    pub fn id(&self) -> Snowflake {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn icon(&self) -> Option<&str> {
        self.icon.as_deref()
    }

    pub fn owner_id(&self) -> Option<Snowflake> {
        self.owner_id
    }

    /// Members of the guild as Discord last reported it (not how many we hold).
    pub fn member_count(&self) -> u32 {
        self.member_count
    }

    pub fn roles(&self) -> &[Role] {
        &self.roles
    }

    pub fn channels(&self) -> impl Iterator<Item = &Channel> {
        self.channels.iter()
    }

    pub fn channel(&self, id: Snowflake) -> Option<&Channel> {
        self.channels
            .binary_search_by_key(&id, |channel| channel.id)
            .ok()
            .map(|at| &self.channels[at])
    }

    /// A member we currently hold. Absence does not mean the user left.
    pub fn member(&self, user: Snowflake) -> Option<&GuildMember> {
        self.members.get(&user).map(|slot| &slot.member)
    }

    pub fn members_held(&self) -> usize {
        self.members.len()
    }

    pub fn voice_states(&self) -> impl Iterator<Item = &VoiceState> {
        self.voice.values()
    }

    pub fn voice_state(&self, user: Snowflake) -> Option<&VoiceState> {
        self.voice.get(&user)
    }

    /// The visible member list: the most recently updated one.
    pub fn member_list(&self) -> Option<&MemberList> {
        self.lists.iter().max_by_key(|list| list.updated)
    }

    fn base_bytes(&self) -> usize {
        size_of::<Self>()
            + self.name.capacity()
            + text(&self.icon)
            + (self.roles.capacity() - self.roles.len()) * size_of::<Role>()
            + (self.channels.capacity() - self.channels.len()) * size_of::<Channel>()
            + map_overhead(&self.members)
            + map_overhead(&self.voice)
            + (self.lists.capacity() - self.lists.len()) * size_of::<MemberList>()
    }

    fn bytes(&self) -> usize {
        self.base_bytes()
            + self.meta_bytes
            + self.members_bytes
            + self.voice_bytes
            + self.lists.iter().map(MemberList::bytes).sum::<usize>()
    }

    /// The same total computed from the contents alone.
    fn recount(&self) -> usize {
        self.base_bytes()
            + self.roles.iter().map(role_bytes).sum::<usize>()
            + self.channels.iter().map(channel_bytes).sum::<usize>()
            + self
                .members
                .values()
                .map(|slot| member_bytes(&slot.member))
                .sum::<usize>()
            + self.voice.values().map(voice_bytes).sum::<usize>()
            + self.lists.iter().map(MemberList::recount).sum::<usize>()
    }

    fn recompute_meta(&mut self) {
        self.meta_bytes = self.roles.iter().map(role_bytes).sum::<usize>()
            + self.channels.iter().map(channel_bytes).sum::<usize>();
    }

    fn put_member(&mut self, member: GuildMember, tick: u64) {
        let cost = member_bytes(&member);
        let old = self.members.insert(
            member.user_id,
            MemberSlot {
                member,
                touched: tick,
            },
        );
        self.members_bytes += cost;
        if let Some(old) = old {
            self.members_bytes -= member_bytes(&old.member);
        }
    }

    fn take_member(&mut self, user: Snowflake) -> bool {
        match self.members.remove(&user) {
            Some(slot) => {
                self.members_bytes -= member_bytes(&slot.member);
                compact_map(&mut self.members);
                true
            }
            None => false,
        }
    }

    fn put_voice(&mut self, state: VoiceState) {
        let cost = voice_bytes(&state);
        let old = self.voice.insert(state.user_id, state);
        self.voice_bytes += cost;
        if let Some(old) = old {
            self.voice_bytes -= voice_bytes(&old);
        }
    }

    fn take_voice(&mut self, user: Snowflake) {
        if let Some(old) = self.voice.remove(&user) {
            self.voice_bytes -= voice_bytes(&old);
            compact_map(&mut self.voice);
        }
    }

    fn put_channel(&mut self, mut channel: Channel) {
        // CHANNEL_UPDATE and GUILD_CREATE may omit the newest message marker;
        // never let that regress what we know.
        match self
            .channels
            .binary_search_by_key(&channel.id, |channel| channel.id)
        {
            Ok(at) => {
                channel.last_message_id =
                    newest(self.channels[at].last_message_id, channel.last_message_id);
                self.channels[at] = channel;
            }
            Err(at) => self.channels.insert(at, channel),
        }
        self.recompute_meta();
    }

    /// Records a newer last-message marker for a channel we hold.
    fn mark_message(&mut self, channel: Snowflake, message: Snowflake) -> bool {
        let Ok(at) = self
            .channels
            .binary_search_by_key(&channel, |channel| channel.id)
        else {
            return false;
        };
        let channel = &mut self.channels[at];
        if channel.last_message_id < Some(message) {
            channel.last_message_id = Some(message);
            true
        } else {
            false
        }
    }

    /// Everything the guild's pinned members are: the ones that must not be
    /// shed, as user IDs.
    fn pinned_members(
        &self,
        current_user: Option<Snowflake>,
        asked: Option<&HashSet<Snowflake>>,
    ) -> HashSet<Snowflake> {
        let mut pinned: HashSet<Snowflake> = self.voice.keys().copied().collect();
        pinned.extend(asked.into_iter().flatten());
        pinned.extend(current_user);
        for list in &self.lists {
            pinned.extend(list.member_ids());
        }
        pinned
    }
}

fn newest(old: Option<Snowflake>, new: Option<Snowflake>) -> Option<Snowflake> {
    old.max(new)
}

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
    focus: Focus,
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
            focus: Focus::default(),
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
        self.allocation_overhead() + self.users_bytes + self.guilds_bytes + self.private_bytes
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
    }

    pub fn budget(&self) -> usize {
        self.budget
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

    // ----- apply -----

    /// Applies one Gateway event. Connection states change nothing here.
    pub fn apply(&mut self, event: GatewayEvent) -> Changes {
        if self.limit_exceeded {
            return Changes::default();
        }
        let GatewayEvent::Dispatch { event, .. } = event else {
            return Changes::default();
        };
        self.tick += 1;
        let changes = self.dispatch(event);
        self.checked_changes(changes)
    }

    fn checked_changes(&mut self, changes: Changes) -> Changes {
        self.enforce_budget();
        if self.limit_exceeded {
            Changes {
                reset: true,
                ..Changes::default()
            }
        } else {
            changes
        }
    }

    fn dispatch(&mut self, event: Dispatch) -> Changes {
        let mut changes = Changes::default();
        match event {
            Dispatch::Ready(ready) => self.ready(*ready, &mut changes),
            Dispatch::ReadySupplemental(supplemental) => {
                self.ready_supplemental(*supplemental, &mut changes);
            }
            Dispatch::Resumed => {}
            Dispatch::MessageCreate(message) => match message.guild_id {
                Some(guild) => {
                    let marked = self.with_guild(guild, |entry| {
                        entry.mark_message(message.channel_id, message.id)
                    });
                    if marked == Some(true) {
                        changes.guilds.insert(guild);
                    }
                }
                None => {
                    if let Some(channel) = self.private_channels.get_mut(&message.channel_id)
                        && channel.last_message_id < Some(message.id)
                    {
                        channel.last_message_id = Some(message.id);
                        changes.private_channels = true;
                    }
                }
            },
            Dispatch::MessageUpdate(_)
            | Dispatch::MessageDelete(_)
            | Dispatch::MessageDeleteBulk(_) => {}
            Dispatch::GuildCreate(created) => self.guild_create(*created, &mut changes),
            Dispatch::GuildUpdate(updated) => self.guild_update(*updated, &mut changes),
            Dispatch::GuildDelete(deleted) => {
                self.remove_guild(deleted.id);
                if deleted.unavailable {
                    self.unavailable.insert(deleted.id);
                } else {
                    self.unavailable.remove(&deleted.id);
                }
                changes.guilds.insert(deleted.id);
            }
            Dispatch::ChannelCreate(channel) | Dispatch::ChannelUpdate(channel) => {
                self.channel_upsert(*channel, &mut changes);
            }
            Dispatch::ChannelDelete(channel) => self.channel_remove(&channel, &mut changes),
            Dispatch::ChannelRecipientAdd(event) => {
                if self.private_channels.contains_key(&event.channel_id) {
                    let user_id = event.user.id;
                    let user_changed = self.users.get(&user_id) != Some(&event.user);
                    if user_changed {
                        self.put_user(event.user);
                    }
                    let (added, cost_delta) = {
                        let channel = self.private_channels.get_mut(&event.channel_id).unwrap();
                        let before = channel_bytes(channel);
                        let added = !channel.recipient_ids.contains(&user_id);
                        if added {
                            channel.recipient_ids.push(user_id);
                        }
                        (added, channel_bytes(channel) - before)
                    };
                    self.private_bytes += cost_delta;
                    changes.private_channels |= user_changed || added;
                }
            }
            Dispatch::ChannelRecipientRemove(event) => {
                if self.current_user == Some(event.user_id) {
                    if let Some(channel) = self.private_channels.remove(&event.channel_id) {
                        self.private_bytes -= channel_bytes(&channel);
                        compact_map(&mut self.private_channels);
                        changes.private_channels = true;
                        self.sweep_users();
                    }
                } else if let Some(channel) = self.private_channels.get_mut(&event.channel_id) {
                    let before = channel.recipient_ids.len();
                    channel.recipient_ids.retain(|&id| id != event.user_id);
                    changes.private_channels |= before != channel.recipient_ids.len();
                    self.sweep_users();
                }
            }
            Dispatch::PassiveUpdate(update) => self.passive_update(*update, &mut changes),
            Dispatch::MemberListUpdate(update) => self.member_list_update(*update, &mut changes),
            Dispatch::GuildMemberAdd(event) => self.member_event(*event, true, &mut changes),
            Dispatch::GuildMemberUpdate(event) => self.member_event(*event, false, &mut changes),
            Dispatch::GuildMemberRemove(removed) => {
                let known = self.with_guild(removed.guild_id, |entry| {
                    entry.take_member(removed.user_id);
                    entry.member_count = entry.member_count.saturating_sub(1);
                });
                if known.is_some() {
                    changes.guilds.insert(removed.guild_id);
                }
            }
            Dispatch::GuildRoleCreate(event) | Dispatch::GuildRoleUpdate(event) => {
                self.role_upsert(*event, &mut changes);
            }
            Dispatch::GuildRoleDelete(event) => self.role_remove(event, &mut changes),
            Dispatch::VoiceStateUpdate(update) => self.voice_update(*update, &mut changes),
            // Voice connection state is owned by the voice layer.
            Dispatch::VoiceServerUpdate(_) => {}
        }
        self.refresh_list_key(&mut changes);
        changes
    }

    /// Metadata can change the permission-list key without navigation. Never
    /// keep showing the old membership while awaiting a replacement SYNC.
    fn refresh_list_key(&mut self, changes: &mut Changes) {
        if self.focus.list_id.is_some() {
            return;
        }
        let (Some(guild), Some(channel)) = (self.focus.guild, self.focus.channel) else {
            return;
        };
        let expected = self.guilds.get(&guild).and_then(|entry| {
            entry
                .channel(channel)
                .map(|channel| list_id::for_channel(entry.id, &entry.roles, channel))
        });
        let released = self.with_guild(guild, |entry| {
            let before = entry.lists.len();
            entry
                .lists
                .retain(|list| Some(list.id()) == expected.as_ref());
            before != entry.lists.len()
        });
        if released == Some(true) {
            changes.member_lists.insert(guild);
        }
        self.focus.list_id = expected;
    }

    fn ready(&mut self, ready: Ready, changes: &mut Changes) {
        // A READY replaces everything derived from a previous session; what
        // the consumer is looking at stays.
        self.focus.list_id = None;
        self.users = HashMap::new();
        self.users_bytes = 0;
        self.guilds = HashMap::new();
        self.guild_order = Vec::new();
        self.guilds_bytes = 0;
        self.unavailable.clear();
        self.private_channels = HashMap::new();
        self.private_bytes = 0;
        changes.reset = true;

        self.current_user = Some(ready.user.id);
        for user in ready.users {
            self.put_user(user);
        }
        self.put_user(ready.user);
        for guild in ready.guilds {
            self.install_guild(guild);
        }
        self.unavailable = ready.unavailable_guilds.into_iter().collect();
        for channel in ready.private_channels {
            self.put_private_channel(channel);
        }
        // Deduplicated users may only acquire member references in
        // READY_SUPPLEMENTAL. Retain them until that startup phase completes.
    }

    fn ready_supplemental(&mut self, supplemental: ReadySupplemental, changes: &mut Changes) {
        for user in supplemental.users {
            self.put_user(user);
        }
        let tick = self.tick;
        for guild in supplemental.guilds {
            let known = self.with_guild(guild.id, |entry| {
                let stale: Vec<Snowflake> = entry.voice.keys().copied().collect();
                for user in stale {
                    entry.take_voice(user);
                }
                for state in guild.voice_states {
                    entry.put_voice(state);
                }
                for member in guild.members {
                    entry.put_member(member, tick);
                }
            });
            if known.is_some() {
                changes.guilds.insert(guild.id);
                changes.voice.insert(guild.id);
            }
        }
        for channel in supplemental.lazy_private_channels {
            self.put_private_channel(channel);
            changes.private_channels = true;
        }
        self.sweep_users();
    }

    fn guild_create(&mut self, created: GuildCreate, changes: &mut Changes) {
        match created {
            GuildCreate::Unavailable(id) => {
                self.remove_guild(id);
                self.unavailable.insert(id);
                changes.guilds.insert(id);
            }
            GuildCreate::Available { guild, users } => {
                for user in users {
                    self.put_user(user);
                }
                let id = guild.id;
                self.unavailable.remove(&id);
                if self.guilds.contains_key(&id) {
                    self.refresh_guild(guild);
                } else {
                    self.install_guild(guild);
                }
                changes.guilds.insert(id);
            }
        }
    }

    fn guild_update(&mut self, update: GuildUpdate, changes: &mut Changes) {
        if self.focus.guild == Some(update.id) && update.roles_present {
            self.focus.list_id = None;
        }
        let known = self.with_guild(update.id, |entry| {
            if update.name_present {
                entry.name = update.name.unwrap_or_default();
            }
            if update.icon_present {
                entry.icon = update.icon;
            }
            if update.owner_present {
                entry.owner_id = update.owner_id;
            }
            if update.roles_present {
                entry.roles = sorted_roles(update.roles);
            }
            if let Some(count) = update.member_count {
                entry.member_count = count;
            }
            entry.recompute_meta();
        });
        if known.is_some() {
            changes.guilds.insert(update.id);
        }
    }

    fn install_guild(&mut self, guild: Guild) {
        let id = guild.id;
        self.remove_guild(id);
        let entry = GuildEntry::new(guild, self.tick);
        self.guilds_bytes += entry.bytes();
        self.guilds.insert(id, entry);
        self.guild_order.push(id);
    }

    /// A GUILD_CREATE for a guild we already hold: its description is
    /// replaced, what we learned since (members, voice, lists) is kept.
    fn refresh_guild(&mut self, guild: Guild) {
        if self.focus.guild == Some(guild.id) {
            self.focus.list_id = None;
        }
        let tick = self.tick;
        self.with_guild(guild.id, |entry| {
            entry.name = guild.name;
            entry.icon = guild.icon;
            entry.owner_id = guild.owner_id;
            entry.member_count = guild.member_count;
            entry.roles = sorted_roles(guild.roles);
            let mut channels = sorted_channels(guild.channels);
            for channel in &mut channels {
                if let Some(old) = entry.channel(channel.id) {
                    channel.last_message_id = newest(old.last_message_id, channel.last_message_id);
                }
            }
            entry.channels = channels;
            entry.recompute_meta();
            for member in guild.members {
                entry.put_member(member, tick);
            }
        });
    }

    fn remove_guild(&mut self, id: Snowflake) {
        if self.focus.guild == Some(id) {
            self.focus.list_id = None;
        }
        if let Some(entry) = self.guilds.remove(&id) {
            self.guilds_bytes -= entry.bytes();
            self.guild_order.retain(|known| *known != id);
            compact_map(&mut self.guilds);
            if self.guild_order.capacity() > self.guild_order.len().saturating_mul(2) {
                self.guild_order.shrink_to_fit();
            }
        }
    }

    /// Runs `change` on a guild and keeps the ledger exact.
    fn with_guild<R>(
        &mut self,
        id: Snowflake,
        change: impl FnOnce(&mut GuildEntry) -> R,
    ) -> Option<R> {
        let entry = self.guilds.get_mut(&id)?;
        let before = entry.bytes();
        let result = change(entry);
        let after = entry.bytes();
        self.guilds_bytes = self.guilds_bytes + after - before;
        Some(result)
    }

    fn put_user(&mut self, user: User) {
        let cost = user_bytes(&user);
        let old = self.users.insert(user.id, user);
        self.users_bytes += cost;
        if let Some(old) = old {
            self.users_bytes -= user_bytes(&old);
        }
    }

    /// Moves the user objects a channel embeds into the user table.
    fn normalize_channel(&mut self, mut channel: Channel) -> Channel {
        if !channel.recipients.is_empty() {
            channel.recipient_ids = std::mem::take(&mut channel.recipients)
                .into_iter()
                .map(|user| {
                    let id = user.id;
                    self.put_user(user);
                    id
                })
                .collect();
        }
        channel
    }

    fn put_private_channel(&mut self, channel: Channel) {
        let mut channel = self.normalize_channel(channel);
        if let Some(old) = self.private_channels.get(&channel.id) {
            channel.last_message_id = newest(old.last_message_id, channel.last_message_id);
        }
        let cost = channel_bytes(&channel);
        let old = self.private_channels.insert(channel.id, channel);
        self.private_bytes += cost;
        if let Some(old) = old {
            self.private_bytes -= channel_bytes(&old);
        }
    }

    fn channel_upsert(&mut self, channel: Channel, changes: &mut Changes) {
        if self.focus.channel == Some(channel.id) {
            self.focus.list_id = None;
        }
        match channel.guild_id {
            Some(guild) => {
                if self.guilds.contains_key(&guild) {
                    let channel = self.normalize_channel(channel);
                    self.with_guild(guild, |entry| entry.put_channel(channel));
                    changes.guilds.insert(guild);
                }
            }
            None => {
                self.put_private_channel(channel);
                changes.private_channels = true;
            }
        }
    }

    fn channel_remove(&mut self, channel: &Channel, changes: &mut Changes) {
        if self.focus.channel == Some(channel.id) {
            self.focus.list_id = None;
        }
        match channel.guild_id {
            Some(guild) => {
                let removed = self.with_guild(guild, |entry| {
                    let Ok(at) = entry
                        .channels
                        .binary_search_by_key(&channel.id, |channel| channel.id)
                    else {
                        return false;
                    };
                    entry.channels.remove(at);
                    if entry.channels.capacity() > entry.channels.len().saturating_mul(2) {
                        entry.channels.shrink_to_fit();
                    }
                    entry.recompute_meta();
                    true
                });
                if removed == Some(true) {
                    changes.guilds.insert(guild);
                }
            }
            None => {
                if let Some(old) = self.private_channels.remove(&channel.id) {
                    self.private_bytes -= channel_bytes(&old);
                    compact_map(&mut self.private_channels);
                    changes.private_channels = true;
                }
            }
        }
    }

    fn role_upsert(&mut self, event: GuildRoleEvent, changes: &mut Changes) {
        let guild = event.guild_id;
        if self.focus.guild == Some(guild) {
            self.focus.list_id = None;
        }
        let known = self.with_guild(guild, |entry| {
            match entry
                .roles
                .binary_search_by_key(&event.role.id, |role| role.id)
            {
                Ok(at) => entry.roles[at] = event.role,
                Err(at) => entry.roles.insert(at, event.role),
            }
            entry.recompute_meta();
        });
        if known.is_some() {
            changes.guilds.insert(guild);
        }
    }

    fn role_remove(&mut self, event: GuildRoleDelete, changes: &mut Changes) {
        let guild = event.guild_id;
        if self.focus.guild == Some(guild) {
            self.focus.list_id = None;
        }
        let removed = self.with_guild(guild, |entry| {
            let before = entry.roles.len();
            entry.roles.retain(|role| role.id != event.role_id);
            let mut changed = before != entry.roles.len();
            for member in entry.members.values_mut() {
                let before = member.member.roles.len();
                member.member.roles.retain(|role| *role != event.role_id);
                changed |= before != member.member.roles.len();
            }
            for channel in &mut entry.channels {
                let before = channel.permission_overwrites.len();
                channel.permission_overwrites.retain(|overwrite| {
                    overwrite.kind != OverwriteKind::Role || overwrite.id != event.role_id
                });
                changed |= before != channel.permission_overwrites.len();
            }
            if entry.roles.capacity() > entry.roles.len().saturating_mul(2) {
                entry.roles.shrink_to_fit();
            }
            entry.recompute_meta();
            changed
        });
        if removed == Some(true) {
            changes.guilds.insert(guild);
        }
    }

    fn passive_update(&mut self, update: PassiveUpdate, changes: &mut Changes) {
        if !self.guilds.contains_key(&update.guild_id) {
            return;
        }
        for user in update.users {
            self.put_user(user);
        }
        let tick = self.tick;
        self.with_guild(update.guild_id, |entry| {
            for channel in &update.channels {
                if let Some(message) = channel.last_message_id {
                    entry.mark_message(channel.id, message);
                }
            }
            for state in update.updated_voice_states {
                if state.channel_id.is_some() {
                    entry.put_voice(state);
                } else {
                    entry.take_voice(state.user_id);
                }
            }
            for user in &update.removed_voice_states {
                entry.take_voice(*user);
            }
            for member in update.members {
                entry.put_member(member, tick);
            }
        });
        changes.guilds.insert(update.guild_id);
        changes.voice.insert(update.guild_id);
    }

    fn voice_update(&mut self, update: VoiceStateUpdate, changes: &mut Changes) {
        let Some(guild) = update.state.guild_id else {
            // Private calls belong to the voice milestones.
            return;
        };
        if !self.guilds.contains_key(&guild) {
            return;
        }
        for user in update.users {
            self.put_user(user);
        }
        let tick = self.tick;
        self.with_guild(guild, |entry| {
            if let Some(member) = update.member {
                entry.put_member(member, tick);
            }
            if update.state.channel_id.is_some() {
                entry.put_voice(update.state);
            } else {
                entry.take_voice(update.state.user_id);
            }
        });
        changes.voice.insert(guild);
    }

    fn member_event(&mut self, event: GuildMemberEvent, added: bool, changes: &mut Changes) {
        if !self.guilds.contains_key(&event.guild_id) {
            return;
        }
        for user in event.users {
            if !added && let Some(previous) = self.users.get_mut(&user.id) {
                let before = user_bytes(previous);
                if event.username_present {
                    previous.username = user.username;
                }
                if event.global_name_present {
                    previous.global_name = user.global_name;
                }
                if event.avatar_present {
                    previous.avatar = user.avatar;
                }
                if event.bot_present {
                    previous.bot = user.bot;
                }
                self.users_bytes = self.users_bytes + user_bytes(previous) - before;
            } else {
                self.put_user(user);
            }
        }
        let tick = self.tick;
        self.with_guild(event.guild_id, |entry| {
            let member = event.member;
            if !added && let Some(previous) = entry.members.get_mut(&member.user_id) {
                let before = member_bytes(&previous.member);
                if event.nick_present {
                    previous.member.nick = member.nick;
                }
                if event.roles_present {
                    previous.member.roles = member.roles;
                    previous.member.roles_known = member.roles_known;
                }
                if event.timeout_present {
                    previous.member.communication_disabled_until =
                        member.communication_disabled_until;
                }
                previous.touched = tick;
                entry.members_bytes = entry.members_bytes + member_bytes(&previous.member) - before;
            } else {
                let new = !entry.members.contains_key(&member.user_id);
                entry.put_member(member, tick);
                if added && new {
                    entry.member_count = entry.member_count.saturating_add(1);
                }
            }
        });
        changes.guilds.insert(event.guild_id);
    }

    fn member_list_update(&mut self, update: MemberListUpdate, changes: &mut Changes) {
        // Lists exist only for the guild on screen; anything else is a late
        // event for a subscription we already left.
        if self.focus.guild != Some(update.guild_id) || !self.guilds.contains_key(&update.guild_id)
        {
            return;
        }
        let synchronizes = update
            .ops
            .iter()
            .any(|op| matches!(op, crate::gateway::MemberListOp::Sync { .. }));
        let entry = &self.guilds[&update.guild_id];
        let Some(channel) = self.focus.channel.and_then(|id| entry.channel(id)) else {
            return;
        };
        let expected = self
            .focus
            .list_id
            .get_or_insert_with(|| list_id::for_channel(entry.id, &entry.roles, channel));
        if expected != &update.list_id {
            return;
        }
        // A late incremental update cannot introduce a list after navigation
        // or bring an older list back to the foreground.
        if !synchronizes && !entry.lists.iter().any(|list| list.id() == &update.list_id) {
            return;
        }
        let active = entry
            .member_list()
            .is_some_and(|list| list.id() == &update.list_id);
        for user in update.users {
            self.put_user(user);
        }
        let tick = self.tick;
        let MemberListUpdate {
            guild_id,
            list_id,
            member_count,
            online_count,
            groups,
            ops,
            members,
            ..
        } = update;
        let ranges = std::mem::take(&mut self.focus.ranges);
        self.with_guild(guild_id, |entry| {
            for member in members {
                entry.put_member(member, tick);
            }
            let at = match entry.lists.iter().position(|list| list.id() == &list_id) {
                Some(at) => at,
                None => {
                    if entry.lists.len() >= MAX_LISTS_PER_GUILD
                        && let Some(oldest) = entry
                            .lists
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, list)| list.updated)
                            .map(|(at, _)| at)
                    {
                        entry.lists.swap_remove(oldest);
                    }
                    entry.lists.push(MemberList::new(list_id));
                    entry.lists.len() - 1
                }
            };
            let list = &mut entry.lists[at];
            list.set_header(member_count, online_count, groups);
            for op in ops {
                list.apply(op);
            }
            list.retain_ranges(&ranges);
            if synchronizes || active {
                list.updated = tick;
            }
            if let Some(member_count) = member_count {
                entry.member_count = member_count;
            }
        });
        self.focus.ranges = ranges;
        changes.member_lists.insert(guild_id);
    }

    // ----- retention -----

    fn enforce_budget(&mut self) {
        if self.bytes() <= self.budget {
            return;
        }
        // Spare buckets alone must not force eviction of live detail.
        self.compact_optional();
        if self.bytes() <= self.budget {
            return;
        }
        let target = self.budget / 100 * LOW_WATER_PERCENT;
        self.shed_lists();
        self.sweep_users();
        if self.bytes() > target {
            self.shed_members(target);
        }
        if self.bytes() > self.budget {
            let budget = self.budget;
            *self = Self::with_budget(budget);
            self.limit_exceeded = true;
        }
    }

    fn compact_optional(&mut self) {
        for entry in self.guilds.values_mut() {
            let before = entry.bytes();
            entry.members.shrink_to_fit();
            entry.voice.shrink_to_fit();
            entry.lists.shrink_to_fit();
            let after = entry.bytes();
            self.guilds_bytes = self.guilds_bytes + after - before;
        }
        self.users.shrink_to_fit();
    }

    /// Keeps only the visible (newest) member list of each guild.
    fn shed_lists(&mut self) {
        let ids: Vec<Snowflake> = self.guilds.keys().copied().collect();
        for id in ids {
            self.with_guild(id, |entry| {
                if entry.lists.len() > 1
                    && let Some(newest) = entry.lists.iter().map(|list| list.updated).max()
                {
                    entry.lists.retain(|list| list.updated == newest);
                    entry.lists.truncate(1);
                    entry.lists.shrink_to_fit();
                }
            });
        }
    }

    /// Drops unreferenced members, oldest write first, until `target` is met.
    /// A user nothing else references goes with its member.
    fn shed_members(&mut self, target: usize) {
        let mut references = self.user_references();
        let mut candidates: Vec<(u64, Snowflake, Snowflake)> = Vec::new();
        for (id, entry) in &self.guilds {
            let pinned = entry.pinned_members(self.current_user, self.focus.asked.get(id));
            candidates.extend(
                entry
                    .members
                    .iter()
                    .filter(|(user, _)| !pinned.contains(user))
                    .map(|(user, slot)| (slot.touched, *id, *user)),
            );
        }
        candidates.sort_unstable();
        for (_, guild, user) in candidates {
            if self.bytes() <= target {
                break;
            }
            if self.with_guild(guild, |entry| entry.take_member(user)) != Some(true) {
                continue;
            }
            let left = references.entry(user).or_insert(1);
            *left = left.saturating_sub(1);
            if *left == 0
                && let Some(old) = self.users.remove(&user)
            {
                self.users_bytes -= user_bytes(&old);
                compact_map(&mut self.users);
            }
        }
        // Release the final partially empty optional tables too, not merely
        // their values. Their changing capacities are part of the ledger.
        self.compact_optional();
    }

    /// How many things hold each user: a member slot, a voice state, a list
    /// row, a direct-message recipient, or being the current user.
    fn user_references(&self) -> HashMap<Snowflake, usize> {
        let mut references: HashMap<Snowflake, usize> = HashMap::new();
        let mut count = |user: Snowflake| *references.entry(user).or_insert(0) += 1;
        self.current_user.into_iter().for_each(&mut count);
        for entry in self.guilds.values() {
            entry.members.keys().copied().for_each(&mut count);
            entry.voice.keys().copied().for_each(&mut count);
            for list in &entry.lists {
                list.member_ids().for_each(&mut count);
            }
        }
        for channel in self.private_channels.values() {
            channel.recipient_ids.iter().copied().for_each(&mut count);
        }
        references
    }

    /// Drops every user nothing references any more.
    fn sweep_users(&mut self) {
        let references = self.user_references();
        let mut freed = 0;
        self.users.retain(|id, user| {
            let keep = references.contains_key(id);
            if !keep {
                freed += user_bytes(user);
            }
            keep
        });
        self.users_bytes -= freed;
        self.users.shrink_to_fit();
    }
}
