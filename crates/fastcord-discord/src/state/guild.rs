//! Guild entity storage and its byte ledger (SPEC §4.5): `GuildEntry`, the
//! size estimators and the sort/dedup normalizers.

use std::collections::HashMap;
use std::mem::size_of;

use fastcord_model::*;

use super::*;

/// Estimated hash-table storage from its usable capacity, including the
/// load-factor reserve and control bytes. The map's inline owner is charged
/// by its enclosing entity; live values are charged by their own ledgers.
pub(super) fn table_bytes<K, V>(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = (capacity * 8).div_ceil(7).next_power_of_two();
    buckets * (size_of::<(K, V)>() + 1) + 16
}

pub(super) fn map_overhead<K, V>(map: &HashMap<K, V>) -> usize {
    table_bytes::<K, V>(map.capacity()) - map.len() * size_of::<V>()
}

/// Most participants one conversation keeps (Discord allows ten; the margin
/// only bounds a flood of add events).
pub(super) const MAX_GROUP_MEMBERS: usize = 64;

pub(super) fn compact_map<K: Eq + std::hash::Hash, V>(map: &mut HashMap<K, V>) {
    // Amortize ordinary deletes; bulk eviction also compacts once at its end.
    if map.capacity() > map.len().saturating_mul(2) {
        map.shrink_to_fit();
    }
}

pub(super) fn text(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, String::capacity)
}

pub(super) fn user_bytes(user: &User) -> usize {
    size_of::<User>() + user.username.capacity() + text(&user.global_name) + text(&user.avatar)
}

pub(super) fn role_bytes(role: &Role) -> usize {
    size_of::<Role>() + role.name.capacity()
}

pub(super) fn channel_bytes(channel: &Channel) -> usize {
    size_of::<Channel>()
        + text(&channel.name)
        + text(&channel.icon)
        + channel.topic.as_ref().map_or(0, |t| t.len())
        + channel.rtc_region.as_ref().map_or(0, |t| t.len())
        + channel.permission_overwrites.capacity() * size_of::<PermissionOverwrite>()
        + (channel.recipients.capacity() - channel.recipients.len()) * size_of::<User>()
        + channel.recipients.iter().map(user_bytes).sum::<usize>()
        + channel.recipient_ids.capacity() * size_of::<Snowflake>()
}

pub(super) fn member_bytes(member: &GuildMember) -> usize {
    size_of::<MemberSlot>()
        + text(&member.nick)
        + member.roles.capacity() * size_of::<Snowflake>()
        + text(&member.communication_disabled_until)
}

pub(super) fn voice_bytes(state: &VoiceState) -> usize {
    size_of::<VoiceState>() + state.session_id.capacity() + text(&state.request_to_speak_timestamp)
}

/// Most live Stage sessions kept per guild.
pub(super) const MAX_STAGES: usize = 64;

pub(super) fn stage_bytes(stage: &StageInstance) -> usize {
    size_of::<StageInstance>() + stage.topic.capacity()
}

pub(super) fn capped_stage(mut stage: StageInstance) -> StageInstance {
    if stage.topic.chars().count() > MAX_TOPIC_CHARS {
        stage.topic = stage.topic.chars().take(MAX_TOPIC_CHARS).collect();
    }
    stage
}

/// Dense channel metadata avoids paying for a full Channel in every unused
/// hash-table bucket. Stable sorting preserves the previous last-write-wins
/// behavior for duplicate IDs in a guild snapshot.
pub(super) fn sorted_channels(mut channels: Vec<Channel>) -> Vec<Channel> {
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
pub(super) fn sorted_roles(mut roles: Vec<Role>) -> Vec<Role> {
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

pub(super) struct MemberSlot {
    pub(super) member: GuildMember,
    /// Store tick of the last write; the oldest unreferenced member goes first.
    pub(super) touched: u64,
}

/// One guild as the store holds it.
pub struct GuildEntry {
    pub(super) id: Snowflake,
    pub(super) name: String,
    pub(super) icon: Option<String>,
    pub(super) owner_id: Option<Snowflake>,
    pub(super) member_count: u32,
    pub(super) roles: Vec<Role>,
    pub(super) channels: Vec<Channel>,
    pub(super) emojis: Vec<GuildEmoji>,
    pub(super) settings: GuildSettings,
    pub(super) members: HashMap<Snowflake, MemberSlot>,
    pub(super) voice: HashMap<Snowflake, VoiceState>,
    pub(super) stages: Vec<StageInstance>,
    pub(super) activities: Vec<ActivityUpdate>,
    pub(super) events: Vec<ScheduledEvent>,
    /// Ids of recently deleted events, so a late update cannot revive one.
    pub(super) gone_events: Vec<Snowflake>,
    pub(super) lists: Vec<MemberList>,
    /// Cached sizes of the parts that change one element at a time.
    pub(super) meta_bytes: usize,
    pub(super) members_bytes: usize,
    pub(super) voice_bytes: usize,
}

impl GuildEntry {
    pub(super) fn new(guild: Guild, tick: u64) -> Self {
        let mut entry = Self {
            id: guild.id,
            name: guild.name,
            icon: guild.icon,
            owner_id: guild.owner_id,
            member_count: guild.member_count,
            roles: sorted_roles(guild.roles),
            channels: sorted_channels(guild.channels),
            emojis: sorted_emojis(guild.emojis),
            settings: guild.settings,
            members: HashMap::new(),
            voice: HashMap::new(),
            stages: guild
                .stage_instances
                .into_iter()
                .take(MAX_STAGES)
                .map(capped_stage)
                .collect(),
            activities: Vec::new(),
            events: Vec::new(),
            gone_events: Vec::new(),
            lists: Vec::new(),
            meta_bytes: 0,
            members_bytes: 0,
            voice_bytes: 0,
        };
        entry.trim_threads();
        entry.set_events(guild.scheduled_events);
        for member in guild.members {
            entry.put_member(member, tick);
        }
        // READY (and GUILD_CREATE) list the guild's current occupants; without
        // them a room looks empty until everyone happens to move again.
        if let Some(states) = guild.voice_states {
            for state in states {
                entry.put_voice(state);
            }
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

    pub fn settings(&self) -> &GuildSettings {
        &self.settings
    }

    pub fn emojis(&self) -> &[GuildEmoji] {
        &self.emojis
    }

    pub fn emoji_usable(&self, emoji: &GuildEmoji, user: Snowflake) -> bool {
        emoji.available
            && (emoji.roles.is_empty()
                || emoji.roles.contains(&self.id)
                || self.owner_id == Some(user)
                || self.member(user).is_some_and(|member| {
                    member.roles_known && emoji.roles.iter().any(|role| member.roles.contains(role))
                }))
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

    /// The members currently held (a subset of the guild, never all of it).
    pub fn members(&self) -> impl Iterator<Item = &GuildMember> {
        self.members.values().map(|slot| &slot.member)
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

    /// The live Stage session of a Stage channel, if one runs.
    pub fn stage_instance(&self, channel: Snowflake) -> Option<&StageInstance> {
        self.stages.iter().find(|stage| stage.channel_id == channel)
    }

    /// The visible member list: the most recently updated one.
    pub fn member_list(&self) -> Option<&MemberList> {
        self.lists.iter().max_by_key(|list| list.updated)
    }

    pub(super) fn base_bytes(&self) -> usize {
        size_of::<Self>()
            + self.name.capacity()
            + text(&self.icon)
            + (self.roles.capacity() - self.roles.len()) * size_of::<Role>()
            + (self.channels.capacity() - self.channels.len()) * size_of::<Channel>()
            + (self.emojis.capacity() - self.emojis.len()) * size_of::<GuildEmoji>()
            + map_overhead(&self.members)
            + map_overhead(&self.voice)
            + (self.lists.capacity() - self.lists.len()) * size_of::<MemberList>()
            + (self.events.capacity() - self.events.len()) * size_of::<ScheduledEvent>()
            + self.gone_events.capacity() * size_of::<Snowflake>()
    }

    pub(super) fn bytes(&self) -> usize {
        self.base_bytes()
            + self.meta_bytes
            + self.members_bytes
            + self.voice_bytes
            + self.lists.iter().map(MemberList::bytes).sum::<usize>()
    }

    /// The same total computed from the contents alone.
    pub(super) fn recount(&self) -> usize {
        self.base_bytes()
            + self.roles.iter().map(role_bytes).sum::<usize>()
            + self.channels.iter().map(channel_bytes).sum::<usize>()
            + self.emojis.iter().map(emoji_bytes).sum::<usize>()
            + self.stages.iter().map(stage_bytes).sum::<usize>()
            + self.activities.iter().map(activities::bytes).sum::<usize>()
            + self.events.iter().map(events::event_bytes).sum::<usize>()
            + text(&self.settings.description)
            + self
                .members
                .values()
                .map(|slot| member_bytes(&slot.member))
                .sum::<usize>()
            + self.voice.values().map(voice_bytes).sum::<usize>()
            + self.lists.iter().map(MemberList::recount).sum::<usize>()
    }

    pub(super) fn recompute_meta(&mut self) {
        self.meta_bytes = self.roles.iter().map(role_bytes).sum::<usize>()
            + self.channels.iter().map(channel_bytes).sum::<usize>()
            + self.emojis.iter().map(emoji_bytes).sum::<usize>()
            + self.stages.iter().map(stage_bytes).sum::<usize>()
            + self.activities.iter().map(activities::bytes).sum::<usize>()
            + self.events.iter().map(events::event_bytes).sum::<usize>()
            + text(&self.settings.description);
    }

    pub(super) fn put_member(&mut self, mut member: GuildMember, tick: u64) {
        // Voice and list members may omit the screening and onboarding
        // fields; that is not "accepted".
        if let Some(old) = self.members.get(&member.user_id) {
            member.pending = member.pending.or(old.member.pending);
            member.flags = member.flags.or(old.member.flags);
        }
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

    pub(super) fn take_member(&mut self, user: Snowflake) -> bool {
        match self.members.remove(&user) {
            Some(slot) => {
                self.members_bytes -= member_bytes(&slot.member);
                compact_map(&mut self.members);
                true
            }
            None => false,
        }
    }

    pub(super) fn put_voice(&mut self, state: VoiceState) {
        let cost = voice_bytes(&state);
        let old = self.voice.insert(state.user_id, state);
        self.voice_bytes += cost;
        if let Some(old) = old {
            self.voice_bytes -= voice_bytes(&old);
        }
    }

    pub(super) fn take_voice(&mut self, user: Snowflake) {
        if let Some(old) = self.voice.remove(&user) {
            self.voice_bytes -= voice_bytes(&old);
            compact_map(&mut self.voice);
        }
    }

    pub(super) fn put_channel(&mut self, mut channel: Channel) {
        // CHANNEL_UPDATE and GUILD_CREATE may omit the newest message marker;
        // never let that regress what we know.
        match self
            .channels
            .binary_search_by_key(&channel.id, |channel| channel.id)
        {
            Ok(at) => {
                channel.last_message_id =
                    newest(self.channels[at].last_message_id, channel.last_message_id);
                // Only some thread payloads say whether the account joined;
                // silence is not leaving.
                if channel.member.is_none() {
                    channel.member = self.channels[at].member.take();
                }
                self.channels[at] = channel;
            }
            Err(at) => self.channels.insert(at, channel),
        }
        self.trim_threads();
        self.recompute_meta();
    }

    /// Keeps at most [`MAX_THREADS_PER_GUILD`] threads: unjoined and archived
    /// ones are dropped first, then the oldest.
    pub(super) fn trim_threads(&mut self) {
        let archived = |c: &Channel| c.thread_metadata.as_ref().is_some_and(|m| m.archived);
        let count = self.channels.iter().filter(|c| c.kind.is_thread()).count();
        let Some(excess) = count.checked_sub(MAX_THREADS_PER_GUILD).filter(|n| *n > 0) else {
            return;
        };
        let mut order: Vec<_> = self
            .channels
            .iter()
            .filter(|c| c.kind.is_thread())
            .map(|c| (c.member.is_some(), !archived(c), c.id))
            .collect();
        order.sort_unstable();
        let doomed: HashSet<Snowflake> = order.iter().take(excess).map(|key| key.2).collect();
        self.channels.retain(|c| !doomed.contains(&c.id));
        self.channels.shrink_to_fit();
    }

    /// Marks the account as (not) a member of a thread held here.
    pub(super) fn set_thread_joined(&mut self, thread: Snowflake, joined: bool) -> bool {
        let Ok(at) = self.channels.binary_search_by_key(&thread, |c| c.id) else {
            return false;
        };
        let channel = &mut self.channels[at];
        if !channel.kind.is_thread() || channel.member.is_some() == joined {
            return false;
        }
        channel.member = joined.then_some(ThreadMember {});
        true
    }

    /// THREAD_LIST_SYNC: replaces the threads of `parents` (every thread when
    /// empty) with `threads`, remembering what was read.
    pub(super) fn sync_threads(&mut self, sync: ThreadListSync) {
        let mut read = HashMap::new();
        self.channels.retain(|c| {
            let doomed = c.kind.is_thread()
                && (sync.parent_ids.is_empty()
                    || c.parent_id.is_some_and(|p| sync.parent_ids.contains(&p)));
            if doomed {
                read.insert(c.id, c.last_message_id);
            }
            !doomed
        });
        for mut thread in sync.threads {
            if !thread.kind.is_thread() {
                continue;
            }
            thread.last_message_id = newest(
                read.get(&thread.id).copied().flatten(),
                thread.last_message_id,
            );
            if sync.joined.contains(&thread.id) {
                thread.member = Some(ThreadMember {});
            }
            let at = self
                .channels
                .binary_search_by_key(&thread.id, |c| c.id)
                .unwrap_or_else(|at| at);
            if self.channels.get(at).is_some_and(|c| c.id == thread.id) {
                self.channels[at] = thread;
            } else {
                self.channels.insert(at, thread);
            }
        }
        self.trim_threads();
        self.channels.shrink_to_fit();
        self.recompute_meta();
    }

    /// Records a newer last-message marker for a channel we hold.
    pub(super) fn mark_message(&mut self, channel: Snowflake, message: Snowflake) -> bool {
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
    pub(super) fn pinned_members(
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

pub(super) fn newest(old: Option<Snowflake>, new: Option<Snowflake>) -> Option<Snowflake> {
    old.max(new)
}

pub(super) fn emoji_bytes(emoji: &GuildEmoji) -> usize {
    size_of::<GuildEmoji>() + text(&emoji.name) + emoji.roles.capacity() * size_of::<Snowflake>()
}

pub(super) fn sorted_emojis(mut emojis: Vec<GuildEmoji>) -> Vec<GuildEmoji> {
    emojis.sort_unstable_by_key(|emoji| emoji.id);
    emojis.dedup_by_key(|emoji| emoji.id);
    emojis.shrink_to_fit();
    for emoji in &mut emojis {
        if let Some(name) = &mut emoji.name {
            name.shrink_to_fit();
        }
        emoji.roles.shrink_to_fit();
    }
    emojis
}
