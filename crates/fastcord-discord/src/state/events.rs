//! Scheduled events (issue #92) in the store: at most [`MAX_EVENTS`] per guild,
//! charged to the guild's metadata bytes. Gateway events settle on the last
//! one seen: a replayed or late update of a deleted event changes nothing,
//! because the ids of recently deleted events are remembered. The REST list
//! replaces a guild's events and never revives one of those.

use std::mem::size_of;

use fastcord_model::{ScheduledEvent, ScheduledEventKey, ScheduledEventUser, Snowflake};

use super::{Changes, GuildEntry, Store, text};

/// Events kept per guild (Discord's own limit for events that are not over).
pub const MAX_EVENTS: usize = 100;
/// Deleted event ids remembered per guild.
const MAX_GONE: usize = 32;

pub(super) fn event_bytes(event: &ScheduledEvent) -> usize {
    size_of::<ScheduledEvent>()
        + event.creator.capacity()
        + event.name.capacity()
        + event.description.capacity()
        + event.location.capacity()
        + text(&event.recurrence)
}

impl GuildEntry {
    pub fn scheduled_events(&self) -> &[ScheduledEvent] {
        &self.events
    }

    /// Replaces the events with a complete list. What only this client knows
    /// (`interested`, a count the list leaves out) carries over.
    pub(super) fn set_events(&mut self, events: Vec<ScheduledEvent>) {
        let mut kept: Vec<ScheduledEvent> = Vec::new();
        for mut event in events {
            if self.gone_events.contains(&event.id) {
                continue;
            }
            if let Some(old) = self.events.iter().find(|old| old.id == event.id) {
                event.interested = old.interested;
                event.user_count = event.user_count.or(old.user_count);
            }
            match kept.iter().position(|held| held.id == event.id) {
                Some(at) => kept[at] = event,
                None => kept.push(event),
            }
        }
        self.events = kept;
        self.trim_events();
        self.recompute_meta();
    }

    /// Keeps [`MAX_EVENTS`]: events that are over go first, oldest first.
    fn trim_events(&mut self) {
        if let Some(excess) = self.events.len().checked_sub(MAX_EVENTS) {
            self.events
                .sort_by_key(|event| (!event.status.is_over(), event.start_ms));
            self.events.drain(..excess);
        }
        self.events.shrink_to_fit();
    }

    fn upsert_event(&mut self, mut event: ScheduledEvent) -> bool {
        if self.gone_events.contains(&event.id) {
            return false;
        }
        let changed = match self.events.iter().position(|held| held.id == event.id) {
            Some(at) => {
                // An update does not say who is interested.
                event.interested = self.events[at].interested;
                event.user_count = event.user_count.or(self.events[at].user_count);
                let same = self.events[at] == event;
                self.events[at] = event;
                !same
            }
            None => {
                self.events.push(event);
                true
            }
        };
        self.trim_events();
        self.recompute_meta();
        changed
    }

    fn remove_event(&mut self, id: Snowflake) -> bool {
        if self.gone_events.len() == MAX_GONE {
            self.gone_events.remove(0);
        }
        if !self.gone_events.contains(&id) {
            self.gone_events.push(id);
        }
        let Some(at) = self.events.iter().position(|held| held.id == id) else {
            return false;
        };
        self.events.remove(at);
        self.events.shrink_to_fit();
        self.recompute_meta();
        true
    }

    fn event_user(&mut self, user: ScheduledEventUser, added: bool, me: Option<Snowflake>) -> bool {
        let Some(event) = self.events.iter_mut().find(|held| held.id == user.event_id) else {
            return false;
        };
        if Some(user.user_id) == me {
            // The same answer again (a replay) counts once.
            if event.interested == Some(added) {
                return false;
            }
            event.interested = Some(added);
        }
        if let Some(count) = &mut event.user_count {
            *count = if added {
                count.saturating_add(1)
            } else {
                count.saturating_sub(1)
            };
        }
        true
    }
}

impl Store {
    pub(super) fn event_upsert(&mut self, event: ScheduledEvent, changes: &mut Changes) {
        let guild = event.guild_id;
        if self.with_guild(guild, |entry| entry.upsert_event(event)) == Some(true) {
            changes.guilds.insert(guild);
        }
    }

    pub(super) fn event_delete(&mut self, key: ScheduledEventKey, changes: &mut Changes) {
        if self.with_guild(key.guild_id, |entry| entry.remove_event(key.id)) == Some(true) {
            changes.guilds.insert(key.guild_id);
        }
    }

    pub(super) fn event_user(
        &mut self,
        user: ScheduledEventUser,
        added: bool,
        changes: &mut Changes,
    ) {
        let me = self.current_user;
        if self.with_guild(user.guild_id, |entry| entry.event_user(user, added, me)) == Some(true) {
            changes.guilds.insert(user.guild_id);
        }
    }

    /// The complete event list of a guild as the REST list reports it.
    pub fn replace_scheduled_events(
        &mut self,
        guild: Snowflake,
        events: Vec<ScheduledEvent>,
    ) -> Changes {
        let mut changes = Changes::default();
        let events: Vec<_> = events.into_iter().filter(|e| e.guild_id == guild).collect();
        if self
            .with_guild(guild, |entry| entry.set_events(events))
            .is_some()
        {
            changes.guilds.insert(guild);
        }
        self.checked_changes(changes)
    }

    /// Events are optional detail that a list request brings back: under
    /// memory pressure every guild but the focused one gives them up.
    pub(super) fn shed_events(&mut self) {
        let keep = self.focus.guild;
        let ids: Vec<Snowflake> = self.guilds.keys().copied().collect();
        for id in ids.into_iter().filter(|id| Some(*id) != keep) {
            self.with_guild(id, |entry| {
                entry.events = Vec::new();
                entry.gone_events = Vec::new();
                entry.recompute_meta();
            });
        }
    }
}
