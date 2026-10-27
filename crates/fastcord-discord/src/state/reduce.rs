//! The reducer: `Store::apply`, the `Dispatch` match and the per-event
//! handlers that mutate the entity tables through the ledgered helpers.

use super::*;

impl Store {
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

    pub(super) fn checked_changes(&mut self, mut changes: Changes) -> Changes {
        // Dropped read state changes every badge: the UI must hear of it.
        changes.read |= self.enforce_budget();
        if self.limit_exceeded {
            Changes {
                reset: true,
                ..Changes::default()
            }
        } else {
            changes
        }
    }

    pub(super) fn dispatch(&mut self, event: Dispatch) -> Changes {
        let mut changes = Changes::default();
        // Only these can make a user unreferenced; pruning scans the store.
        let unreferences = matches!(
            event,
            Dispatch::RelationshipRemove(_)
                | Dispatch::ChannelDelete(_)
                | Dispatch::ChannelRecipientRemove(_)
                | Dispatch::GuildDelete(_)
                | Dispatch::GuildMemberRemove(_)
        );
        match event {
            Dispatch::Ready(ready) => self.ready(*ready, &mut changes),
            Dispatch::ReadySupplemental(supplemental) => {
                self.ready_supplemental(*supplemental, &mut changes);
            }
            Dispatch::Resumed => {}
            Dispatch::MessageCreate(message) => {
                self.note_read_effects(&message, &mut changes);
                match message.guild_id {
                    Some(guild) => {
                        let marked = self.with_guild(guild, |entry| {
                            entry.mark_message(message.channel_id, message.id)
                        });
                        if marked == Some(true) {
                            changes.guilds.insert(guild);
                        }
                    }
                    None => self.private_message(&message, &mut changes),
                }
            }
            Dispatch::MessageAck(ack) => {
                if self
                    .read
                    .ack(ack.channel_id, ack.message_id, ack.mention_count)
                {
                    changes.read = true;
                }
            }
            Dispatch::PresenceUpdate(update) => {
                if self.presence_wanted(update.user) {
                    // Held for profile cards, but only the account, friends and
                    // direct-message recipients are drawn from the snapshot;
                    // churn of other members must not redraw the window (#212).
                    let applied = self.presences.apply(&update);
                    changes.presence = applied && self.presence_drawn(update.user);
                }
            }
            Dispatch::RelationshipAdd(relationship) => {
                changes.relationships = self.relationships.add(*relationship);
            }
            Dispatch::RelationshipUpdate(update) => {
                changes.relationships = self.relationships.update(update);
            }
            Dispatch::RelationshipRemove(id) => {
                changes.relationships = self.relationships.remove(id);
            }
            Dispatch::UserSettingsUpdate(update) => {
                if std::sync::Arc::make_mut(&mut self.privacy).apply(&update) {
                    changes.privacy = true;
                }
            }
            Dispatch::UserFoldersUpdate(folders) => {
                if *self.folders != *folders {
                    self.folders = std::sync::Arc::new(*folders);
                    changes.read = true;
                }
            }
            Dispatch::UserGuildSettingsUpdate(update) => {
                let muted = self.read.update_mutes(
                    update.guild_id,
                    update.guild_muted,
                    &update.channels,
                );
                changes.read |= self.read.update_levels(&update) | muted;
            }
            // Message bodies, reactions included, are the message store's;
            // none of these changes navigation metadata.
            Dispatch::MessageUpdate(_)
            | Dispatch::MessageDelete(_)
            | Dispatch::MessageDeleteBulk(_)
            | Dispatch::MessageReactionAdd(_)
            | Dispatch::MessageReactionRemove(_)
            | Dispatch::MessageReactionRemoveAll(_)
            | Dispatch::MessageReactionRemoveEmoji(_)
            | Dispatch::MessagePollVoteAdd(_)
            | Dispatch::MessagePollVoteRemove(_)
            // Interactions are tracked by the message worker.
            | Dispatch::InteractionSuccess(_)
            | Dispatch::InteractionFailure(_)
            | Dispatch::InteractionModalCreate(_) => {}
            Dispatch::GuildCreate(created) => self.guild_create(*created, &mut changes),
            Dispatch::GuildUpdate(updated) => self.guild_update(*updated, &mut changes),
            Dispatch::GuildEmojisUpdate(update) => {
                let guild_id = update.guild_id;
                if self
                    .with_guild(guild_id, |guild| {
                        guild.emojis = sorted_emojis(update.emojis);
                        guild.recompute_meta();
                    })
                    .is_some()
                {
                    changes.guilds.insert(guild_id);
                }
            }
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
            Dispatch::CallCreate(create) => changes.calls = self.calls.create(*create),
            Dispatch::CallUpdate(update) => changes.calls = self.calls.update(update),
            Dispatch::CallDelete(delete) => changes.calls = self.calls.delete(delete.channel_id),
            Dispatch::ChannelDelete(channel) => {
                changes.calls = self.calls.delete(channel.id);
                self.read.forget(channel.id);
                changes.read = true;
                self.channel_remove(&channel, &mut changes);
            }
            Dispatch::ThreadListSync(sync) => {
                let guild = sync.guild_id;
                if self
                    .with_guild(guild, |entry| entry.sync_threads(*sync))
                    .is_some()
                {
                    changes.guilds.insert(guild);
                }
            }
            Dispatch::ThreadMemberUpdate(update) => {
                if self
                    .with_guild(update.guild_id, |entry| {
                        entry.set_thread_joined(update.thread_id, true)
                    })
                    .unwrap_or(false)
                {
                    changes.guilds.insert(update.guild_id);
                }
            }
            Dispatch::ThreadMembersUpdate(update) => {
                let me = self.current_user;
                let joined = me.is_some_and(|me| update.added.contains(&me));
                let left = me.is_some_and(|me| update.removed.contains(&me));
                if (joined || left)
                    && self
                        .with_guild(update.guild_id, |entry| {
                            entry.set_thread_joined(update.thread_id, joined)
                        })
                        .unwrap_or(false)
                {
                    changes.guilds.insert(update.guild_id);
                }
            }
            Dispatch::ChannelRecipientAdd(event) => {
                self.recipient_add(event.channel_id, event.user, &mut changes);
            }
            Dispatch::ChannelRecipientRemove(event) => {
                self.recipient_remove(event.channel_id, event.user_id, &mut changes);
            }
            Dispatch::PassiveUpdate(update) => self.passive_update(*update, &mut changes),
            Dispatch::MemberListUpdate(update) => self.member_list_update(*update, &mut changes),
            Dispatch::GuildMemberAdd(event) => self.member_event(*event, true, &mut changes),
            Dispatch::GuildMemberUpdate(event) => self.member_event(*event, false, &mut changes),
            Dispatch::UserUpdate(user) => {
                // Only the account's own object arrives here, and it is whole:
                // replace it. A replay changes nothing.
                if self.current_user == Some(user.id) && self.users.get(&user.id) != Some(&*user) {
                    self.put_user(*user);
                    changes.account = true;
                }
            }
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
            Dispatch::EmbeddedActivity(update) => self.activity_event(update, &mut changes),
            Dispatch::StageInstanceUpsert(stage) => self.stage_event(stage, true, &mut changes),
            Dispatch::StageInstanceDelete(stage) => self.stage_event(stage, false, &mut changes),
            Dispatch::ScheduledEventUpsert(event) => self.event_upsert(*event, &mut changes),
            Dispatch::ScheduledEventDelete(key) => self.event_delete(key, &mut changes),
            Dispatch::ScheduledEventUser { user, added } => {
                self.event_user(user, added, &mut changes);
            }
            // Voice connection state is owned by the voice layer.
            Dispatch::VoiceServerUpdate(_)
            | Dispatch::StreamCreate(_)
            | Dispatch::StreamServerUpdate(_)
            | Dispatch::StreamUpdate(_)
            | Dispatch::StreamDelete(_)
            // Soundboard plays and catalog changes belong to the voice layer.
            | Dispatch::VoiceChannelSound(_)
            | Dispatch::SoundboardChanged(_)
            | Dispatch::StickersChanged(_)
            // AutoMod rules belong to the open panel, not the reducer.
            | Dispatch::AutoModeration(_)
            // Typing is transient and lives outside the store.
            | Dispatch::TypingStart(_) => {}
        }
        // A removal frees its users at once, so they never build up the
        // pressure that would cost read state later (#152).
        if unreferences {
            let references = self.user_references();
            self.sweep_users_with(&references);
            if self.prune_presences_with(references) {
                changes.presence = true;
            }
        }
        self.refresh_list_key(&mut changes);
        changes
    }

    /// Metadata can change the permission-list key without navigation. Never
    /// keep showing the old membership while awaiting a replacement SYNC.
    pub(super) fn refresh_list_key(&mut self, changes: &mut Changes) {
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

    pub(super) fn ready(&mut self, ready: Ready, changes: &mut Changes) {
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
        self.read.replace(ready.read.entries, ready.read.mutes);
        self.folders = std::sync::Arc::new(ready.read.folders.unwrap_or_default());
        self.relationships
            .replace(ready.relationships, ready.ignored);
        std::sync::Arc::make_mut(&mut self.privacy).reset();
        changes.privacy = true;
        self.calls.reset(Some(ready.user.id));
        changes.calls = true;
        self.presences.clear();
        changes.reset = true;
        changes.relationships = true;

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
        self.admit_presences(&ready.presences);
        changes.presence = true;
        // Deduplicated users may only acquire member references in
        // READY_SUPPLEMENTAL. Retain them until that startup phase completes.
    }

    pub(super) fn ready_supplemental(
        &mut self,
        supplemental: ReadySupplemental,
        changes: &mut Changes,
    ) {
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
        self.admit_presences(&supplemental.presences);
        changes.presence |= !supplemental.presences.is_empty();
        self.sweep_users();
    }

    pub(super) fn admit_presences(&mut self, updates: &[crate::gateway::PresenceUpdate]) {
        for update in updates {
            if self.presence_wanted(update.user) {
                self.presences.apply(update);
            }
        }
    }

    pub(super) fn guild_create(&mut self, created: GuildCreate, changes: &mut Changes) {
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
                changes.voice.insert(id);
            }
        }
    }

    pub(super) fn guild_update(&mut self, update: GuildUpdate, changes: &mut Changes) {
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
            if let Some(settings) = update.settings {
                entry.settings = settings;
            }
            entry.recompute_meta();
        });
        if known.is_some() {
            changes.guilds.insert(update.id);
        }
    }

    pub(super) fn install_guild(&mut self, guild: Guild) {
        let id = guild.id;
        self.remove_guild(id);
        let entry = GuildEntry::new(guild, self.tick);
        self.guilds_bytes += entry.bytes();
        self.guilds.insert(id, entry);
        self.guild_order.push(id);
    }

    /// A GUILD_CREATE for a guild we already hold: its description is
    /// replaced, what we learned since (members, voice, lists) is kept.
    pub(super) fn refresh_guild(&mut self, guild: Guild) {
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
            entry.emojis = sorted_emojis(guild.emojis);
            entry.settings = guild.settings;
            let mut channels = sorted_channels(guild.channels);
            for channel in &mut channels {
                if let Some(old) = entry.channel(channel.id) {
                    channel.last_message_id = newest(old.last_message_id, channel.last_message_id);
                }
            }
            entry.channels = channels;
            entry.stages = guild
                .stage_instances
                .into_iter()
                .take(MAX_STAGES)
                .map(capped_stage)
                .collect();
            entry.trim_threads();
            entry.set_events(guild.scheduled_events);
            for member in guild.members {
                entry.put_member(member, tick);
            }
            // GUILD_CREATE carries the guild's voice states as they are now;
            // a payload that stays silent leaves what we know alone.
            if let Some(states) = guild.voice_states {
                let stale: Vec<Snowflake> = entry.voice.keys().copied().collect();
                for user in stale {
                    entry.take_voice(user);
                }
                for state in states {
                    entry.put_voice(state);
                }
            }
        });
    }

    pub(super) fn remove_guild(&mut self, id: Snowflake) {
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
    pub(super) fn with_guild<R>(
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

    pub(super) fn put_user(&mut self, user: User) {
        let cost = user_bytes(&user);
        let old = self.users.insert(user.id, user);
        self.users_bytes += cost;
        if let Some(old) = old {
            self.users_bytes -= user_bytes(&old);
        }
    }

    /// Moves the user objects a channel embeds into the user table.
    pub(super) fn normalize_channel(&mut self, mut channel: Channel) -> Channel {
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

    pub(super) fn channel_upsert(&mut self, channel: Channel, changes: &mut Changes) {
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

    pub(super) fn channel_remove(&mut self, channel: &Channel, changes: &mut Changes) {
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
                    let id = channel.id;
                    entry
                        .channels
                        .retain(|c| !(c.kind.is_thread() && c.parent_id == Some(id)));
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
                self.take_private_channel(channel.id, changes);
            }
        }
    }

    pub(super) fn role_upsert(&mut self, event: GuildRoleEvent, changes: &mut Changes) {
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

    pub(super) fn role_remove(&mut self, event: GuildRoleDelete, changes: &mut Changes) {
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

    pub(super) fn passive_update(&mut self, update: PassiveUpdate, changes: &mut Changes) {
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

    /// A Stage session started, changed or ended. Replayed or reordered events
    /// settle on the last one: a delete of an unknown session changes nothing.
    pub(super) fn stage_event(
        &mut self,
        stage: StageInstance,
        present: bool,
        changes: &mut Changes,
    ) {
        let guild = stage.guild_id;
        let changed = self.with_guild(guild, |entry| {
            let at = entry
                .stages
                .iter()
                .position(|held| held.channel_id == stage.channel_id);
            let changed = match (at, present) {
                (Some(at), true) => {
                    let stage = capped_stage(stage);
                    let same = entry.stages[at] == stage;
                    entry.stages[at] = stage;
                    !same
                }
                (None, true) if entry.stages.len() < MAX_STAGES => {
                    entry.stages.push(capped_stage(stage));
                    true
                }
                (Some(at), false) => {
                    entry.stages.remove(at);
                    entry.stages.shrink_to_fit();
                    true
                }
                _ => false,
            };
            entry.recompute_meta();
            changed
        });
        if changed == Some(true) {
            changes.guilds.insert(guild);
            changes.voice.insert(guild);
        }
    }

    pub(super) fn voice_update(&mut self, update: VoiceStateUpdate, changes: &mut Changes) {
        let Some(guild) = update.state.guild_id else {
            for user in update.users {
                self.put_user(user);
            }
            changes.calls = self.calls.voice(&update.state);
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

    pub(super) fn member_event(
        &mut self,
        event: GuildMemberEvent,
        added: bool,
        changes: &mut Changes,
    ) {
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
                // An omitted field keeps what was known.
                if member.pending.is_some() {
                    previous.member.pending = member.pending;
                }
                if member.flags.is_some() {
                    previous.member.flags = member.flags;
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

    pub(super) fn member_list_update(&mut self, update: MemberListUpdate, changes: &mut Changes) {
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
}
