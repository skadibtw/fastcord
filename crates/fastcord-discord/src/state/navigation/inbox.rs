//! The unified inbox (issue #77): every conversation the account can open
//! that is unread or has mentions, from the same read table that draws the
//! channel badges, so the two cannot disagree. Pure query over the store: no
//! I/O, no timer, nothing kept between calls. At most [`MAX_INBOX_ROWS`] rows
//! with capped names are copied; `total` counts all of them.
//!
//! Order: conversations with mentions first, then the newest message first.
//! Threads and voice/Stage chats are listed like channels; categories are
//! not conversations here. A channel the account cannot open is left out (the
//! guild badge may still count it).

use fastcord_model::Snowflake;

use super::{
    ChannelKind, Navigation, Store, capped_name, enabled_channel, is_private_conversation,
    private_name,
};

pub const MAX_INBOX_ROWS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxRow {
    pub channel: Snowflake,
    /// `None` for a DM or group DM.
    pub guild: Option<Snowflake>,
    /// Empty for a DM.
    pub guild_name: String,
    pub name: String,
    pub unread: bool,
    pub mentions: u32,
    /// The channel or its guild is muted.
    pub muted: bool,
    /// The channel itself is muted (what the mute control toggles).
    pub channel_muted: bool,
    pub last_message: Option<Snowflake>,
    /// The account's read cursor (where "first unread" starts).
    pub acked: Option<Snowflake>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inbox {
    /// Rows that matched, before the cap.
    pub total: usize,
    /// Mentions over all matching conversations, not only the rows kept.
    pub mentions: u32,
    pub rows: Vec<InboxRow>,
    /// Ignored and blocked users, so the mention list can leave their
    /// messages out (the same set every other consumer reads).
    pub suppressed:
        std::sync::Arc<std::collections::BTreeMap<Snowflake, crate::state::Suppression>>,
}

impl Navigation {
    /// The inbox, at most `limit` rows (capped at [`MAX_INBOX_ROWS`]).
    pub fn inbox(&self, store: &Store, limit: usize) -> Inbox {
        if limit == 0 || self.account != store.current_user().map(|user| user.id) {
            return Inbox::default();
        }
        let mut rows = Vec::new();
        for &guild_id in store.guild_ids() {
            let Some(guild) = store.guild(guild_id) else {
                continue;
            };
            for channel in guild.channels() {
                let unread = store.unread(channel.id, Some(guild_id), channel.last_message_id);
                if !unread.any() {
                    continue;
                }
                let Some((channel, kind, _)) = enabled_channel(store, guild, channel.id) else {
                    continue;
                };
                if matches!(kind, ChannelKind::Category) {
                    continue;
                }
                rows.push(InboxRow {
                    channel: channel.id,
                    guild: Some(guild_id),
                    guild_name: capped_name(Some(guild.name()), "Unnamed guild"),
                    name: capped_name(channel.name.as_deref(), "Unnamed channel"),
                    unread: unread.unread,
                    mentions: unread.mentions,
                    muted: store.is_muted(channel.id, Some(guild_id)),
                    channel_muted: store.channel_muted(channel.id),
                    last_message: channel.last_message_id,
                    acked: store.acked(channel.id),
                });
            }
        }
        for channel in store
            .private_channels()
            .filter(|channel| is_private_conversation(channel))
        {
            let unread = store.unread(channel.id, None, channel.last_message_id);
            if unread.any() {
                rows.push(InboxRow {
                    channel: channel.id,
                    guild: None,
                    guild_name: String::new(),
                    name: private_name(store, channel),
                    unread: unread.unread,
                    mentions: unread.mentions,
                    muted: store.is_muted(channel.id, None),
                    channel_muted: store.channel_muted(channel.id),
                    last_message: channel.last_message_id,
                    acked: store.acked(channel.id),
                });
            }
        }
        rows.sort_by_key(|row| {
            (
                row.mentions == 0,
                std::cmp::Reverse(row.last_message),
                row.channel,
            )
        });
        let total = rows.len();
        let mentions = rows
            .iter()
            .fold(0u32, |sum, row| sum.saturating_add(row.mentions));
        rows.truncate(limit.min(MAX_INBOX_ROWS));
        Inbox {
            total,
            mentions,
            rows,
            suppressed: store.suppressed(),
        }
    }

    /// Opens a conversation by ID alone, wherever it lives: a held DM or an
    /// enabled channel of a held guild (the guild is selected first). Used by
    /// jumps that know only the channel. Nothing is opened for a channel the
    /// account cannot open.
    pub fn open_channel(&mut self, store: &Store, channel: Snowflake) -> bool {
        if store.private_channel(channel).is_some() {
            return self.select_private_channel(store, channel);
        }
        let guild = store.guild_ids().iter().copied().find(|id| {
            store
                .guild(*id)
                .is_some_and(|guild| enabled_channel(store, guild, channel).is_some())
        });
        match guild {
            Some(guild) => self.select_guild(store, guild) && self.select_channel(store, channel),
            None => false,
        }
    }
}
