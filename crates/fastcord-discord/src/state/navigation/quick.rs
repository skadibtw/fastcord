//! Quick switching and keyboard stepping (milestone 50). Pure queries over the
//! reducer's store: no I/O, no message history, nothing kept between calls.
//! A query copies at most [`MAX_QUICK_RESULTS`] capped names; matching runs over
//! the names the store already holds (accessible channels, held threads, DMs
//! and group DMs), never over archives or message pages.

use std::borrow::Cow;

use fastcord_model::{ChannelKind as ModelChannelKind, Snowflake};

use super::{
    ChannelKind, Navigation, Store, capped_name, is_private_conversation, private_name,
    visible_channels,
};

/// Results of one quick-switch query.
pub const MAX_QUICK_RESULTS: usize = 50;
/// Characters of a query that are matched.
pub const MAX_QUICK_QUERY_CHARS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuickKind {
    Channel,
    /// A voice or Stage channel: selecting it opens its text chat, never the call.
    Voice,
    Thread,
    Direct,
    Group,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickEntry {
    pub channel: Snowflake,
    /// `None` for a DM or group DM.
    pub guild: Option<Snowflake>,
    /// Empty for a DM.
    pub guild_name: String,
    pub name: String,
    pub kind: QuickKind,
    pub unread: bool,
    pub mentions: u32,
}

/// What a keyboard step moves between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepScope {
    /// The next or previous conversation in the sidebar.
    Channel,
    /// The next or previous conversation with unread messages.
    Unread,
    /// The next or previous server.
    Server,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub scope: StepScope,
    pub forward: bool,
}

/// How well `text` matches a lowercase query: a prefix beats a word start,
/// which beats a substring, which beats a subsequence. `None`: no match.
pub fn match_score(query: &[char], text: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let at = |from: usize| text.get(from..from + query.len()) == Some(query);
    if at(0) {
        return Some(1000 + 100u32.saturating_sub(text.len() as u32));
    }
    let starts: Vec<usize> = (1..text.len()).filter(|&i| at(i)).collect();
    if starts.iter().any(|&i| !text[i - 1].is_alphanumeric()) {
        return Some(800);
    }
    if !starts.is_empty() {
        return Some(600);
    }
    let mut wanted = query.iter();
    let mut next = wanted.next();
    for c in &text {
        if next == Some(c) {
            next = wanted.next();
        }
    }
    next.is_none().then_some(300)
}

/// The query as lowercase characters, without a leading `#` or `@`.
pub fn query_chars(query: &str) -> Vec<char> {
    query
        .trim()
        .trim_start_matches(['#', '@'])
        .chars()
        .take(MAX_QUICK_QUERY_CHARS)
        .flat_map(char::to_lowercase)
        .collect()
}

struct Candidate<'a> {
    score: u32,
    channel: Snowflake,
    guild: Option<Snowflake>,
    guild_name: &'a str,
    name: Cow<'a, str>,
    kind: QuickKind,
    unread: bool,
    mentions: u32,
    last_message: Option<Snowflake>,
}

impl Navigation {
    /// Conversations matching `query`, best first: the score, then those with
    /// mentions, then unread, then the newest activity. An empty query lists
    /// the unread ones and the most recent. At most `limit` (and
    /// [`MAX_QUICK_RESULTS`]) are copied.
    pub fn quick_switch(&self, store: &Store, query: &str, limit: usize) -> Vec<QuickEntry> {
        if self.account != store.current_user().map(|user| user.id) {
            return Vec::new();
        }
        let query = query_chars(query);
        let mut found: Vec<Candidate<'_>> = Vec::new();
        for &guild_id in store.guild_ids() {
            let Some(guild) = store.guild(guild_id) else {
                continue;
            };
            for row in visible_channels(store, guild, None) {
                if row.kind == ChannelKind::Category || !row.permissions.enables(row.kind) {
                    continue;
                }
                let name = row.channel.name.as_deref().unwrap_or("");
                // "server channel": the server's name may take part, as a weak match.
                let Some(score) = match_score(&query, name).or_else(|| {
                    match_score(&query, &format!("{} {name}", guild.name())).map(|s| s.min(250))
                }) else {
                    continue;
                };
                let unread =
                    store.unread(row.channel.id, Some(guild_id), row.channel.last_message_id);
                found.push(Candidate {
                    score,
                    channel: row.channel.id,
                    guild: Some(guild_id),
                    guild_name: guild.name(),
                    name: Cow::Borrowed(name),
                    kind: match row.kind {
                        ChannelKind::Voice => QuickKind::Voice,
                        ChannelKind::Thread => QuickKind::Thread,
                        _ => QuickKind::Channel,
                    },
                    unread: unread.unread,
                    mentions: unread.mentions,
                    last_message: row.channel.last_message_id,
                });
            }
        }
        for channel in store
            .private_channels()
            .filter(|channel| is_private_conversation(channel))
        {
            let name = private_name(store, channel);
            let Some(score) = match_score(&query, &name) else {
                continue;
            };
            let unread = store.unread(channel.id, None, channel.last_message_id);
            found.push(Candidate {
                score,
                channel: channel.id,
                guild: None,
                guild_name: "",
                name: Cow::Owned(name),
                kind: if channel.kind == ModelChannelKind::GroupDm {
                    QuickKind::Group
                } else {
                    QuickKind::Direct
                },
                unread: unread.unread,
                mentions: unread.mentions,
                last_message: channel.last_message_id,
            });
        }
        found.sort_by_key(|c| {
            (
                std::cmp::Reverse(c.score),
                c.mentions == 0,
                !c.unread,
                std::cmp::Reverse(c.last_message),
                c.channel,
            )
        });
        found
            .into_iter()
            .take(limit.min(MAX_QUICK_RESULTS))
            .map(|c| QuickEntry {
                channel: c.channel,
                guild: c.guild,
                guild_name: capped_name(Some(c.guild_name), ""),
                name: capped_name(Some(&c.name), "Unnamed channel"),
                kind: c.kind,
                unread: c.unread,
                mentions: c.mentions,
            })
            .collect()
    }

    /// Moves the selection one step, wrapping around. Channel and unread steps
    /// stay in what the sidebar shows (the selected server's channels, or the
    /// direct messages); a server step moves between servers. Returns whether
    /// anything was selected.
    pub fn step(&mut self, store: &Store, step: Step) -> bool {
        if self.account != store.current_user().map(|user| user.id) {
            return false;
        }
        if step.scope == StepScope::Server {
            let guilds = store.guild_ids();
            let at = self
                .selected_guild
                .and_then(|id| guilds.iter().position(|g| *g == id));
            let Some(next) = cycle(guilds.len(), at, step.forward) else {
                return false;
            };
            return self.select_guild(store, guilds[next]);
        }
        let private = self.selected_private_channel.is_some() || self.selected_guild.is_none();
        // (conversation, unread and not muted) in sidebar order.
        let list: Vec<(Snowflake, bool)> = if private {
            let mut channels: Vec<_> = store
                .private_channels()
                .filter(|channel| is_private_conversation(channel))
                .collect();
            channels
                .sort_by_key(|channel| (std::cmp::Reverse(channel.last_message_id), channel.id));
            channels
                .into_iter()
                .map(|c| {
                    let unread = store.unread(c.id, None, c.last_message_id);
                    (c.id, unread.any() && !store.is_muted(c.id, None))
                })
                .collect()
        } else {
            let Some(guild) = self.selected_guild.and_then(|id| store.guild(id)) else {
                return false;
            };
            let guild_id = guild.id();
            visible_channels(store, guild, self.selected_channel)
                .into_iter()
                .filter(|row| {
                    row.kind != ChannelKind::Category && row.permissions.enables(row.kind)
                })
                .map(|row| {
                    let unread =
                        store.unread(row.channel.id, Some(guild_id), row.channel.last_message_id);
                    (
                        row.channel.id,
                        unread.any() && !store.is_muted(row.channel.id, Some(guild_id)),
                    )
                })
                .collect()
        };
        let current = if private {
            self.selected_private_channel
        } else {
            self.selected_channel
        };
        let n = list.len();
        let at = current.and_then(|id| list.iter().position(|(c, _)| *c == id));
        let target = if step.scope == StepScope::Unread && n > 0 {
            let from = at.unwrap_or(if step.forward { n - 1 } else { 0 });
            (1..=n)
                .map(|k| {
                    if step.forward {
                        (from + k) % n
                    } else {
                        (from + n - k % n) % n
                    }
                })
                .find(|pos| Some(*pos) != at && list[*pos].1)
        } else {
            cycle(n, at, step.forward)
        };
        let Some(target) = target else {
            return false;
        };
        let id = list[target].0;
        if private {
            self.select_private_channel(store, id)
        } else {
            self.select_channel(store, id)
        }
    }
}

/// The index after (or before) `at` in a ring of `len`; with no current one,
/// the first (or last).
fn cycle(len: usize, at: Option<usize>, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (at, forward) {
        (None, true) => 0,
        (None, false) => len - 1,
        (Some(i), true) => (i + 1) % len,
        (Some(i), false) => (i + len - 1) % len,
    })
}
