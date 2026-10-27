//! The one place that says what an entity's removal takes with it (#166).
//!
//! Every removal event routes through [`Store::on_removed`]: it clears the
//! tables that were keyed by the entity (read cursors, mutes and levels,
//! calls, voice states, stages, activities, scheduled events, the consumer's
//! asked-for members), removes the entity itself, then releases the users and
//! presences nothing references any more, so a removal can neither leave a
//! dependent entry behind nor skip the sweep.

use super::*;

/// What was removed, with what the clean-up needs to find its dependents.
pub(super) enum Removed {
    /// A server left (`gone`) or in an outage (kept account state: it returns).
    Guild { id: Snowflake, gone: bool },
    /// A channel, thread or conversation (`guild` `None`).
    Channel {
        guild: Option<Snowflake>,
        id: Snowflake,
    },
    /// A member left a server.
    Member { guild: Snowflake, user: Snowflake },
    /// A friend, block or request was removed.
    Relationship(Snowflake),
    /// Someone else left a group conversation.
    Recipient { channel: Snowflake, user: Snowflake },
}

impl Store {
    /// Clears every dependent table of a removal, removes the entity, then
    /// frees the users and presences it released.
    pub(super) fn on_removed(&mut self, what: Removed, changes: &mut Changes) {
        match what {
            Removed::Guild { id, gone } => {
                if gone {
                    let channels: Vec<Snowflake> = self
                        .guilds
                        .get(&id)
                        .map(|entry| entry.channels().map(|channel| channel.id).collect())
                        .unwrap_or_default();
                    for channel in channels {
                        self.read.forget(channel);
                    }
                    self.read.forget_guild(id);
                    self.focus.asked.remove(&id);
                    changes.read = true;
                }
                self.remove_guild(id);
                changes.guilds.insert(id);
            }
            Removed::Channel { guild, id } => {
                // A channel takes its threads with it.
                let mut gone = vec![id];
                if let Some(entry) = guild.and_then(|guild| self.guilds.get(&guild)) {
                    gone.extend(
                        entry
                            .channels()
                            .filter(|c| c.kind.is_thread() && c.parent_id == Some(id))
                            .map(|c| c.id),
                    );
                }
                for &channel in &gone {
                    changes.calls |= self.calls.delete(channel);
                    self.read.forget(channel);
                }
                changes.read = true;
                if let Some(guild) = guild {
                    let inside = self.with_guild(guild, |entry| {
                        let users: Vec<Snowflake> = entry
                            .voice
                            .values()
                            .filter(|state| state.channel_id.is_some_and(|c| gone.contains(&c)))
                            .map(|state| state.user_id)
                            .collect();
                        for user in &users {
                            entry.take_voice(*user);
                        }
                        let (stages, activities, events) = (
                            entry.stages.len(),
                            entry.activities.len(),
                            entry.events.len(),
                        );
                        entry.stages.retain(|s| !gone.contains(&s.channel_id));
                        entry.activities.retain(|a| !gone.contains(&a.channel_id));
                        entry
                            .events
                            .retain(|e| !e.channel_id.is_some_and(|c| gone.contains(&c)));
                        entry.recompute_meta();
                        !users.is_empty()
                            || stages != entry.stages.len()
                            || activities != entry.activities.len()
                            || events != entry.events.len()
                    });
                    if inside == Some(true) {
                        changes.guilds.insert(guild);
                        changes.voice.insert(guild);
                    }
                }
                self.channel_remove(guild, id, changes);
            }
            Removed::Member { guild, user } => {
                let voiced = self.with_guild(guild, |entry| {
                    let had = entry.voice.contains_key(&user);
                    entry.take_voice(user);
                    had
                });
                if voiced == Some(true) {
                    changes.voice.insert(guild);
                }
                if let Some(asked) = self.focus.asked.get_mut(&guild) {
                    asked.remove(&user);
                }
                let known = self.with_guild(guild, |entry| {
                    entry.take_member(user);
                    entry.member_count = entry.member_count.saturating_sub(1);
                });
                if known.is_some() {
                    changes.guilds.insert(guild);
                }
            }
            Removed::Relationship(id) => {
                changes.relationships = self.relationships.remove(id);
            }
            Removed::Recipient { channel, user } => {
                if let Some(channel) = self.private_channels.get_mut(&channel) {
                    let before = channel.recipient_ids.len();
                    channel.recipient_ids.retain(|&id| id != user);
                    changes.private_channels |= before != channel.recipient_ids.len();
                }
            }
        }
        // A removal frees its users at once, so they never build up the
        // pressure that would cost read state later (#152).
        let references = self.user_references();
        self.sweep_users_with(&references);
        if self.prune_presences_with(references) {
            changes.presence = true;
        }
    }
}
