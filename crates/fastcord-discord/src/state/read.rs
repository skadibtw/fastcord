//! Read cursors, mention counts and mutes (SPEC M41).
//!
//! A channel is unread when its newest message is newer than the read cursor
//! the account last acknowledged. The cursor is the account's own
//! (`read_state` in READY, `MESSAGE_ACK` from any client), never the channel's
//! `last_message_id`: that only says what exists. A channel with no read state
//! is treated as read, so a fresh account does not light up every channel.
//!
//! The table is bounded ([`MAX_READ_ENTRIES`] cursors, [`MAX_MUTED`] mutes per
//! kind) and charged to the store's byte ledger.

use std::collections::{HashMap, HashSet};

use fastcord_model::Snowflake;

use super::table_bytes;
use crate::gateway::{GuildMuteUpdate, MuteSettings, NotifyLevel, ReadStateEntry};

/// Read cursors kept; the rest of a larger READY is ignored (those channels
/// simply show no marker).
pub const MAX_READ_ENTRIES: usize = 16_384;
/// Muted guilds and muted channels kept, each.
pub const MAX_MUTED: usize = 4_096;

/// What a channel, DM or guild shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unread {
    /// Unmuted messages the account has not read.
    pub unread: bool,
    /// Mentions since the cursor (muted places still count them).
    pub mentions: u32,
}

impl Unread {
    pub fn merge(self, other: Unread) -> Unread {
        Unread {
            unread: self.unread || other.unread,
            mentions: self.mentions.saturating_add(other.mentions),
        }
    }

    pub fn any(self) -> bool {
        self.unread || self.mentions > 0
    }
}

#[derive(Clone, Copy)]
struct Entry {
    acked: Snowflake,
    mentions: u32,
}

#[derive(Default)]
pub struct ReadStates {
    entries: HashMap<Snowflake, Entry>,
    muted_guilds: HashSet<Snowflake>,
    muted_channels: HashSet<Snowflake>,
    /// Explicit notification levels; absent means inherit.
    guild_levels: HashMap<Snowflake, NotifyLevel>,
    channel_levels: HashMap<Snowflake, NotifyLevel>,
    suppress_everyone: HashSet<Snowflake>,
    suppress_roles: HashSet<Snowflake>,
}

impl ReadStates {
    /// A new session replaces everything.
    pub fn replace(&mut self, entries: Vec<ReadStateEntry>, mutes: MuteSettings) {
        self.entries = entries
            .into_iter()
            .take(MAX_READ_ENTRIES)
            .map(|entry| {
                (
                    entry.channel_id,
                    Entry {
                        acked: entry.last_acked,
                        mentions: entry.mention_count,
                    },
                )
            })
            .collect();
        self.muted_guilds = mutes.guilds.into_iter().take(MAX_MUTED).collect();
        self.muted_channels = mutes.channels.into_iter().take(MAX_MUTED).collect();
        self.guild_levels.clear();
        self.channel_levels.clear();
        self.suppress_everyone.clear();
        self.suppress_roles.clear();
        for entry in &mutes.entries {
            self.update_levels(entry);
        }
    }

    /// The levels and suppressions of one settings entry (READY's or an
    /// update's). Returns whether anything changed.
    pub fn update_levels(&mut self, update: &GuildMuteUpdate) -> bool {
        let mut changed = false;
        if let Some(guild) = update.guild_id {
            changed |= put(&mut self.guild_levels, guild, update.level);
            changed |= set(&mut self.suppress_everyone, guild, update.suppress_everyone);
            changed |= set(&mut self.suppress_roles, guild, update.suppress_roles);
        }
        for &(channel, level) in &update.channel_levels {
            changed |= put(&mut self.channel_levels, channel, level);
        }
        changed
    }

    /// What a channel notifies about: the nearest override in `lineage` (the
    /// channel, then its parents), else the server's, else the server's
    /// default (`guild_default`: Discord's 0 or 1).
    pub fn level(
        &self,
        lineage: impl IntoIterator<Item = Snowflake>,
        guild: Option<Snowflake>,
        guild_default: Option<NotifyLevel>,
    ) -> NotifyLevel {
        lineage
            .into_iter()
            .find_map(|channel| self.channel_levels.get(&channel))
            .or_else(|| guild.and_then(|guild| self.guild_levels.get(&guild)))
            .copied()
            .or(guild_default)
            .unwrap_or_default()
    }

    /// The server's own explicit level, if it set one.
    pub fn guild_level(&self, guild: Snowflake) -> Option<NotifyLevel> {
        self.guild_levels.get(&guild).copied()
    }

    /// The channel's own override, if it has one.
    pub fn channel_level(&self, channel: Snowflake) -> Option<NotifyLevel> {
        self.channel_levels.get(&channel).copied()
    }

    pub fn guild_muted(&self, guild: Snowflake) -> bool {
        self.muted_guilds.contains(&guild)
    }

    pub fn suppresses_everyone(&self, guild: Snowflake) -> bool {
        self.suppress_everyone.contains(&guild)
    }

    pub fn suppresses_roles(&self, guild: Snowflake) -> bool {
        self.suppress_roles.contains(&guild)
    }

    /// `USER_GUILD_SETTINGS_UPDATE` for one guild (or the DMs, `None`).
    pub fn update_mutes(
        &mut self,
        guild: Option<Snowflake>,
        guild_muted: bool,
        channels: &[(Snowflake, bool)],
    ) -> bool {
        let mut changed = false;
        if let Some(guild) = guild {
            changed |= set(&mut self.muted_guilds, guild, guild_muted);
        }
        for &(channel, muted) in channels {
            changed |= set(&mut self.muted_channels, channel, muted);
        }
        changed
    }

    /// The cursor moved (this client or another acknowledged). Returns whether
    /// anything changed.
    pub fn ack(&mut self, channel: Snowflake, message: Snowflake, mentions: u32) -> bool {
        if !self.entries.contains_key(&channel) && self.entries.len() >= MAX_READ_ENTRIES {
            return false;
        }
        let new = Entry {
            acked: message,
            mentions,
        };
        match self.entries.insert(channel, new) {
            Some(old) => old.acked != message || old.mentions != mentions,
            None => true,
        }
    }

    /// A message that mentions the account arrived. Counted once per message
    /// newer than the cursor; a channel without a cursor gets one just before
    /// the message.
    pub fn note_mention(&mut self, channel: Snowflake, message: Snowflake) -> bool {
        if let Some(entry) = self.entries.get_mut(&channel) {
            if message > entry.acked {
                entry.mentions = entry.mentions.saturating_add(1);
                return true;
            }
            return false;
        }
        if self.entries.len() >= MAX_READ_ENTRIES {
            return false;
        }
        self.entries.insert(
            channel,
            Entry {
                acked: Snowflake(message.0.saturating_sub(1)),
                mentions: 1,
            },
        );
        true
    }

    /// Whether `message` would make the channel unread (it has a cursor, the
    /// message is newer, and the place is not muted).
    pub fn would_flag(
        &self,
        channel: Snowflake,
        muted: bool,
        message: Snowflake,
        level: NotifyLevel,
    ) -> bool {
        !muted
            && level == NotifyLevel::All
            && self
                .entries
                .get(&channel)
                .is_some_and(|entry| message > entry.acked)
    }

    pub fn acked(&self, channel: Snowflake) -> Option<Snowflake> {
        self.entries.get(&channel).map(|entry| entry.acked)
    }

    /// Whether any channel of `lineage` (the channel, then its parents) or
    /// the guild is muted.
    pub fn is_muted(
        &self,
        lineage: impl IntoIterator<Item = Snowflake>,
        guild: Option<Snowflake>,
    ) -> bool {
        lineage
            .into_iter()
            .any(|channel| self.muted_channels.contains(&channel))
            || guild.is_some_and(|guild| self.muted_guilds.contains(&guild))
    }

    /// Whether the channel itself (not only its guild) is muted.
    pub fn channel_muted(&self, channel: Snowflake) -> bool {
        self.muted_channels.contains(&channel)
    }

    pub fn unread(
        &self,
        channel: Snowflake,
        muted: bool,
        last_message: Option<Snowflake>,
        level: NotifyLevel,
    ) -> Unread {
        let Some(entry) = self.entries.get(&channel) else {
            return Unread::default();
        };
        Unread {
            unread: !muted
                && level == NotifyLevel::All
                && last_message.is_some_and(|last| last > entry.acked),
            // "Nothing" hides the badge too; mentions are still counted so a
            // change of level shows them.
            mentions: if level == NotifyLevel::Nothing {
                0
            } else {
                entry.mentions
            },
        }
    }

    /// Forgets a channel that no longer exists, so a late event cannot bring
    /// its state back.
    pub fn forget(&mut self, channel: Snowflake) {
        self.entries.remove(&channel);
        self.muted_channels.remove(&channel);
        self.channel_levels.remove(&channel);
    }

    /// Forgets a server the account left: its mute, level and suppressions.
    pub fn forget_guild(&mut self, guild: Snowflake) {
        self.muted_guilds.remove(&guild);
        self.guild_levels.remove(&guild);
        self.suppress_everyone.remove(&guild);
        self.suppress_roles.remove(&guild);
    }

    pub fn bytes(&self) -> usize {
        table_bytes::<Snowflake, Entry>(self.entries.capacity())
            + table_bytes::<Snowflake, ()>(self.muted_guilds.capacity())
            + table_bytes::<Snowflake, ()>(self.muted_channels.capacity())
            + table_bytes::<Snowflake, NotifyLevel>(
                self.guild_levels.capacity() + self.channel_levels.capacity(),
            )
            + table_bytes::<Snowflake, ()>(
                self.suppress_everyone.capacity() + self.suppress_roles.capacity(),
            )
    }

    #[cfg(test)]
    fn cursors(&self) -> usize {
        self.entries.len()
    }
}

/// Sets or clears one level; the table is bounded like the mutes.
fn put(
    levels: &mut HashMap<Snowflake, NotifyLevel>,
    id: Snowflake,
    level: Option<NotifyLevel>,
) -> bool {
    match level {
        Some(level) if levels.len() < MAX_MUTED || levels.contains_key(&id) => {
            levels.insert(id, level) != Some(level)
        }
        Some(_) => false,
        None => levels.remove(&id).is_some(),
    }
}

fn set(ids: &mut HashSet<Snowflake>, id: Snowflake, present: bool) -> bool {
    if present {
        ids.len() < MAX_MUTED && ids.insert(id)
    } else {
        ids.remove(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Snowflake = Snowflake(100);
    const G: Snowflake = Snowflake(7);

    fn all(
        read: &ReadStates,
        channel: Snowflake,
        guild: Option<Snowflake>,
        last: Option<Snowflake>,
    ) -> Unread {
        read.unread(
            channel,
            read.is_muted([channel], guild),
            last,
            NotifyLevel::All,
        )
    }

    fn entry(
        guild: u64,
        level: Option<NotifyLevel>,
        channel: Option<(u64, NotifyLevel)>,
    ) -> GuildMuteUpdate {
        GuildMuteUpdate {
            guild_id: Some(Snowflake(guild)),
            guild_muted: false,
            channels: Vec::new(),
            level,
            suppress_everyone: true,
            suppress_roles: false,
            channel_levels: channel
                .map(|(c, l)| (Snowflake(c), Some(l)))
                .into_iter()
                .collect(),
        }
    }

    #[test]
    fn levels_resolve_channel_then_server_then_default_and_hide_badges() {
        let mut read = states(50, 2);
        let default = Some(NotifyLevel::Mentions);
        assert_eq!(read.level([A], Some(G), default), NotifyLevel::Mentions);
        assert_eq!(read.level([A], Some(G), None), NotifyLevel::All);
        assert!(read.update_levels(&entry(
            7,
            Some(NotifyLevel::All),
            Some((100, NotifyLevel::Nothing))
        )));
        assert!(!read.update_levels(&entry(
            7,
            Some(NotifyLevel::All),
            Some((100, NotifyLevel::Nothing))
        )));
        assert_eq!(read.level([A], Some(G), default), NotifyLevel::Nothing);
        assert!(read.suppresses_everyone(G) && !read.suppresses_roles(G));
        let last = Some(Snowflake(60));
        let unread = |level| read.unread(A, false, last, level);
        assert_eq!(
            unread(NotifyLevel::All),
            Unread {
                unread: true,
                mentions: 2
            }
        );
        assert_eq!(
            unread(NotifyLevel::Mentions),
            Unread {
                unread: false,
                mentions: 2
            }
        );
        assert_eq!(unread(NotifyLevel::Nothing), Unread::default());
        assert!(!read.would_flag(A, false, Snowflake(60), NotifyLevel::Mentions));
        // Inheriting again clears the override; a new READY forgets everything.
        let mut inherit = entry(7, None, None);
        inherit.channel_levels = vec![(A, None)];
        inherit.suppress_everyone = false;
        assert!(read.update_levels(&inherit));
        assert_eq!(read.level([A], Some(G), None), NotifyLevel::All);
        read.update_levels(&entry(7, Some(NotifyLevel::Nothing), None));
        read.replace(Vec::new(), MuteSettings::default());
        assert_eq!(read.guild_level(G), None);
    }

    fn states(acked: u64, mentions: u32) -> ReadStates {
        let mut read = ReadStates::default();
        read.replace(
            vec![ReadStateEntry {
                channel_id: A,
                last_acked: Snowflake(acked),
                mention_count: mentions,
            }],
            MuteSettings::default(),
        );
        read
    }

    #[test]
    fn a_channel_is_unread_only_past_the_account_cursor() {
        let read = states(50, 2);
        assert_eq!(
            all(&read, A, Some(G), Some(Snowflake(60))),
            Unread {
                unread: true,
                mentions: 2
            }
        );
        // Newest equals the cursor: read. The channel's own last id alone says nothing.
        assert!(!all(&read, A, Some(G), Some(Snowflake(50))).unread);
        assert!(!all(&read, A, Some(G), None).unread);
        // No cursor, no marker.
        assert_eq!(
            all(&read, Snowflake(5), None, Some(Snowflake(999))),
            Unread::default()
        );
    }

    #[test]
    fn mutes_hide_the_dot_but_not_mentions() {
        let mut read = states(50, 1);
        read.update_mutes(Some(G), true, &[]);
        let muted = all(&read, A, Some(G), Some(Snowflake(60)));
        assert_eq!((muted.unread, muted.mentions), (false, 1));
        read.update_mutes(Some(G), false, &[(A, true)]);
        assert!(!all(&read, A, Some(G), Some(Snowflake(60))).unread);
        read.update_mutes(None, false, &[(A, false)]);
        assert!(all(&read, A, Some(G), Some(Snowflake(60))).unread);
    }

    #[test]
    fn acknowledgements_move_the_cursor_and_report_real_changes_only() {
        let mut read = states(50, 3);
        assert!(!read.ack(A, Snowflake(50), 3), "same cursor, same count");
        assert!(read.ack(A, Snowflake(70), 0));
        assert_eq!(read.acked(A), Some(Snowflake(70)));
        assert_eq!(all(&read, A, None, Some(Snowflake(70))), Unread::default());
        // A cursor for a channel not seen before is added.
        assert!(read.ack(Snowflake(200), Snowflake(1), 0));
    }

    #[test]
    fn mentions_count_once_per_new_message_and_create_a_cursor_when_none_exists() {
        let mut read = states(50, 0);
        assert!(read.note_mention(A, Snowflake(60)));
        assert!(
            !read.note_mention(A, Snowflake(40)),
            "older than the cursor"
        );
        assert_eq!(all(&read, A, None, Some(Snowflake(60))).mentions, 1);
        assert!(read.note_mention(Snowflake(300), Snowflake(900)));
        assert!(all(&read, Snowflake(300), None, Some(Snowflake(900))).unread);
        read.forget(A);
        assert_eq!(all(&read, A, None, Some(Snowflake(60))), Unread::default());
    }

    #[test]
    fn the_table_is_bounded_and_its_bytes_are_charged() {
        let mut read = ReadStates::default();
        let entries: Vec<_> = (1..=(MAX_READ_ENTRIES as u64 + 100))
            .map(|n| ReadStateEntry {
                channel_id: Snowflake(n),
                last_acked: Snowflake(n),
                mention_count: 0,
            })
            .collect();
        read.replace(entries, MuteSettings::default());
        assert_eq!(read.cursors(), MAX_READ_ENTRIES);
        assert!(!read.ack(Snowflake(u64::MAX), Snowflake(1), 0), "full");
        assert!(read.bytes() > MAX_READ_ENTRIES * size_of::<Entry>());
    }
}
