//! Budget retention: shedding order, compaction and the user sweep (see the
//! module docs of `state` for the order).

use super::*;

impl Store {
    // ----- retention -----

    /// Sheds detail until the store is under budget; true when read state
    /// (every unread marker and mention count) had to go.
    pub(super) fn enforce_budget(&mut self) -> bool {
        if self.bytes() <= self.budget {
            return false;
        }
        // Spare buckets alone must not force eviction of live detail.
        self.compact_optional();
        if self.bytes() <= self.budget {
            return false;
        }
        let target = self.budget / 100 * LOW_WATER_PERCENT;
        // Users nothing references are free garbage: they go first (#152).
        self.sweep_users();
        if self.bytes() > target {
            self.shed_lists();
        }
        if self.bytes() > target {
            self.shed_events();
        }
        if self.bytes() > target {
            self.shed_members(target);
        }
        // Unread markers go last: losing them silently clears every badge
        // until the next READY brings them back.
        let mut read_dropped = false;
        if self.bytes() > target {
            self.read = ReadStates::default();
            read_dropped = true;
        }
        if self.bytes() > self.budget {
            let budget = self.budget;
            *self = Self::with_budget(budget);
            self.limit_exceeded = true;
        }
        read_dropped
    }

    pub(super) fn compact_optional(&mut self) {
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
    pub(super) fn shed_lists(&mut self) {
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
    pub(super) fn shed_members(&mut self, target: usize) {
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
    pub(super) fn user_references(&self) -> HashMap<Snowflake, usize> {
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
    pub(super) fn sweep_users(&mut self) {
        let references = self.user_references();
        self.sweep_users_with(&references);
    }

    pub(super) fn sweep_users_with(&mut self, references: &HashMap<Snowflake, usize>) {
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
