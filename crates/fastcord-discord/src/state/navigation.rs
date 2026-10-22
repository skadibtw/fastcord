//! Permission-gated guild/channel navigation over the reducer's authoritative store.
//!
//! Reconcile after each ordered Store mutation. Selection and per-guild memory use
//! IDs, never row indices; a fresh READY for the same account preserves valid IDs.
//! Windows own at most 128 capped names, not guilds, members, or message histories.
//! Queries perform no I/O, subscriptions request no member-list viewport, and no
//! timer is needed. Unsupported channel kinds (DMs) are not part of guild
//! navigation.
//!
//! Threads (milestone 48) are channels the store holds under their parent. They
//! inherit the parent's overwrites, are listed right after the parent while
//! active (an archived one only while open), a private thread only for its
//! members and thread managers, and a thread whose parent is not viewable or
//! not held is never exposed.

use std::collections::BTreeMap;
use std::ops::Range;

use fastcord_model::{
    Channel, ChannelKind as ModelChannelKind, FLAG_PINNED, FLAG_REQUIRE_TAG, GuildScope,
    MemberScope, Permissions, Snowflake, channel_permissions,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{GuildEntry, Store, Unread};
use crate::gateway::SubscriptionTarget;

/// Held members copied into one snapshot, and roles listed with them.
pub const MAX_SNAPSHOT_MEMBERS: usize = 1024;
pub const MAX_SNAPSHOT_ROLES: usize = 250;
const MAX_NICK_BYTES: usize = 128;

/// What a message author or profile card needs of a held guild member.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemberView {
    pub nick: Option<String>,
    pub roles: Vec<Snowflake>,
    pub moderation: crate::MemberPermissions,
}

/// Absolute response row cap, independent of a caller's requested viewport.
pub const MAX_WINDOW_ROWS: usize = 128;
/// UTF-8 byte cap for each name copied into a read model.
pub const MAX_NAME_BYTES: usize = 256;

/// An owned, immutable-by-convention slice of a list, including its full length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window<T> {
    /// Requested offset clamped to `total`; row indices begin here.
    pub offset: usize,
    pub total: usize,
    pub rows: Vec<T>,
}

impl<T> Default for Window<T> {
    fn default() -> Self {
        Self {
            offset: 0,
            total: 0,
            rows: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuildRow {
    pub id: Snowflake,
    pub name: String,
    /// The icon hash for the public CDN; none without an icon.
    pub icon: Option<String>,
    /// Outage guilds have only an ID and cannot be explicitly selected.
    pub available: bool,
    /// The account owns the server, so it cannot leave it.
    pub owned: bool,
    pub selected: bool,
    /// Unmuted messages not yet read in some channel, and mentions in all.
    pub unread: bool,
    pub mentions: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    Category,
    Text,
    Announcement,
    Voice,
    /// A public, announcement or private thread.
    Thread,
    /// A forum or media channel: its posts are listed in `forum` of the
    /// snapshot, not in the sidebar.
    Forum,
}

impl ChannelKind {
    fn from_model(kind: ModelChannelKind) -> Option<Self> {
        match kind {
            ModelChannelKind::GuildCategory => Some(Self::Category),
            ModelChannelKind::GuildText => Some(Self::Text),
            ModelChannelKind::GuildAnnouncement => Some(Self::Announcement),
            ModelChannelKind::GuildVoice | ModelChannelKind::GuildStageVoice => Some(Self::Voice),
            kind if kind.is_thread() => Some(Self::Thread),
            kind if kind.is_forum() => Some(Self::Forum),
            _ => None,
        }
    }
}

/// Effective model permissions, including overwrites and implicit restrictions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PermissionSummary {
    pub view_channel: bool,
    pub read_history: bool,
    pub send_messages: bool,
    pub add_reactions: bool,
    pub manage_messages: bool,
    pub use_external_emojis: bool,
    pub connect: bool,
    pub speak: bool,
    /// Voice activity may open the microphone; without it the channel requires push-to-talk.
    pub use_vad: bool,
    /// The soundboard may be played in this voice channel.
    pub use_soundboard: bool,
    pub create_threads: bool,
    pub manage_threads: bool,
    pub attach_files: bool,
    pub create_invite: bool,
    /// MANAGE_GUILD: may list and revoke every invite of the server.
    pub manage_guild: bool,
    /// MANAGE_CHANNELS: may list and revoke this channel's invites.
    pub manage_channel: bool,
}

impl From<Permissions> for PermissionSummary {
    fn from(permissions: Permissions) -> Self {
        Self {
            view_channel: permissions.contains(Permissions::VIEW_CHANNEL),
            read_history: permissions.contains(Permissions::READ_MESSAGE_HISTORY),
            send_messages: permissions.contains(Permissions::SEND_MESSAGES),
            add_reactions: permissions.contains(Permissions::ADD_REACTIONS),
            manage_messages: permissions.contains(Permissions::MANAGE_MESSAGES),
            use_external_emojis: permissions.contains(Permissions::USE_EXTERNAL_EMOJIS),
            connect: permissions.contains(Permissions::CONNECT),
            speak: permissions.contains(Permissions::SPEAK),
            use_vad: permissions.contains(Permissions::USE_VAD),
            use_soundboard: permissions.contains(Permissions::USE_SOUNDBOARD),
            create_threads: permissions.contains(Permissions::CREATE_PUBLIC_THREADS),
            manage_threads: permissions.contains(Permissions::MANAGE_THREADS),
            attach_files: permissions.contains(Permissions::ATTACH_FILES),
            create_invite: permissions.contains(Permissions::CREATE_INSTANT_INVITE),
            manage_guild: permissions.contains(Permissions::MANAGE_GUILD),
            manage_channel: permissions.contains(Permissions::MANAGE_CHANNELS),
        }
    }
}

impl PermissionSummary {
    fn enables(self, kind: ChannelKind) -> bool {
        self.view_channel
            && match kind {
                ChannelKind::Category => false,
                // Voice and Stage channels carry a text chat that needs no CONNECT;
                // joining the call is gated separately on `connect`.
                ChannelKind::Voice
                | ChannelKind::Text
                | ChannelKind::Announcement
                | ChannelKind::Forum
                | ChannelKind::Thread => true,
            }
    }
}

/// What a thread's row or header shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadState {
    pub parent_id: Snowflake,
    pub archived: bool,
    /// Only thread managers can write to a locked thread.
    pub locked: bool,
    /// The account is a member (it follows the thread).
    pub joined: bool,
    /// A post of a forum or media channel (set where the parent is known).
    pub forum_post: bool,
}

impl ThreadState {
    fn of(channel: &Channel) -> Option<Self> {
        channel.kind.is_thread().then(|| {
            let metadata = channel.thread_metadata.as_ref();
            Self {
                parent_id: channel.parent_id.unwrap_or(Snowflake(0)),
                archived: metadata.is_some_and(|m| m.archived),
                locked: metadata.is_some_and(|m| m.locked),
                joined: channel.member.is_some(),
                forum_post: false,
            }
        })
    }

    /// Like [`Self::of`], knowing whether the parent is a forum.
    fn in_guild(guild: &GuildEntry, channel: &Channel) -> Option<Self> {
        Self::of(channel).map(|mut state| {
            state.forum_post = is_forum_post(guild, channel);
            state
        })
    }
}

/// A thread whose parent is a forum or media channel held in `guild`.
fn is_forum_post(guild: &GuildEntry, channel: &Channel) -> bool {
    channel.kind.is_thread()
        && channel
            .parent_id
            .and_then(|parent| guild.channel(parent))
            .is_some_and(|parent| parent.kind.is_forum())
}

/// Posts of one forum listed at most.
pub const MAX_FORUM_POSTS: usize = 200;

/// One tag of the open forum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagView {
    pub id: Snowflake,
    pub name: String,
    pub moderated: bool,
    pub emoji: Option<String>,
}

/// One active post of the open forum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostRow {
    pub id: Snowflake,
    pub title: String,
    pub tags: Vec<Snowflake>,
    /// Messages after the first, as Discord counts them.
    pub replies: u32,
    pub owner: Option<Snowflake>,
    /// The owner's name when the user is known, otherwise empty.
    pub author: String,
    /// The newest message ID (the post's own ID without replies): snowflakes
    /// order by time.
    pub last_activity: Snowflake,
    pub pinned: bool,
    pub locked: bool,
    pub joined: bool,
    pub unread: bool,
    pub mentions: u32,
}

/// The open forum or media channel: tags, rules and its active posts, pinned
/// first then newest activity first (at most [`MAX_FORUM_POSTS`]; `total`
/// counts all). Archived posts are paged in by the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForumView {
    pub channel: Snowflake,
    pub guild: Snowflake,
    pub media: bool,
    pub tags: Vec<TagView>,
    pub require_tag: bool,
    /// The forum's default sort is by creation date.
    pub sort_creation: bool,
    /// The account may start a post (SEND_MESSAGES in the forum).
    pub can_post: bool,
    pub can_attach: bool,
    /// The account may apply moderated tags.
    pub can_manage: bool,
    pub posts: Vec<PostRow>,
    pub total: usize,
}

/// Most people listed under one voice or Stage row; the rest are one "+N" line.
pub const MAX_VOICE_OCCUPANTS: usize = 8;

/// A line under a voice or Stage row (the row has the same fixed height).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Occupant {
    Member {
        user_id: Snowflake,
        /// Server nickname, else display name, capped.
        name: String,
        avatar: Option<String>,
        muted: bool,
        deafened: bool,
        /// Streaming (Go Live) in this channel.
        streaming: bool,
        /// Camera on.
        video: bool,
        /// Role in a Stage; `None` in an ordinary voice channel.
        seat: Option<StageSeat>,
    },
    /// "Speakers" or "Audience" above that group of a Stage.
    Heading(&'static str),
    More(usize),
}

/// Where a person sits in a Stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageSeat {
    Speaker,
    Audience,
    /// Audience with a raised hand (a request to speak).
    Hand,
}

impl StageSeat {
    pub fn of(state: &fastcord_model::VoiceState) -> Self {
        if !state.suppress {
            Self::Speaker
        } else if state.request_to_speak_timestamp.is_some() {
            Self::Hand
        } else {
            Self::Audience
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelRow {
    pub id: Snowflake,
    pub name: String,
    pub kind: ChannelKind,
    /// Visible category ID, or none for uncategorized/orphaned channels.
    pub parent_id: Option<Snowflake>,
    /// Categories are headers. A voice channel's chat opens with VIEW_CHANNEL
    /// alone; joining its call also needs `permissions.connect`.
    pub enabled: bool,
    pub selected: bool,
    pub permissions: PermissionSummary,
    pub unread: bool,
    pub mentions: u32,
    /// Set for threads, which `parent_id` (a category) does not describe.
    pub thread: Option<ThreadState>,
    /// Set on the lines that follow a voice row; every other field then
    /// describes that voice channel and is not to be acted on.
    pub occupant: Option<Occupant>,
    /// A Stage channel (also `kind == Voice`).
    pub stage: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateChannelRow {
    pub id: Snowflake,
    /// The other person of a one-to-one conversation (for presence).
    pub recipient: Option<Snowflake>,
    pub label: String,
    pub selected: bool,
    pub unread: bool,
    pub mentions: u32,
}

/// A participant of the selected conversation, other than the account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateMember {
    pub id: Snowflake,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateChannelSelection {
    pub channel_id: Snowflake,
    pub name: String,
    /// A group DM (otherwise a one-to-one DM).
    pub group: bool,
    /// The group's own name, empty when it shows its members' names.
    pub custom_name: String,
    pub owner: Option<Snowflake>,
    pub has_icon: bool,
    /// The other participants (at most 64, names capped).
    pub members: Vec<PrivateMember>,
    /// The account's read cursor for the conversation, if it has one.
    pub acked: Option<Snowflake>,
    pub unread: bool,
    pub mentions: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelSelection {
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
    pub name: String,
    pub kind: ChannelKind,
    pub permissions: PermissionSummary,
    /// The account's read cursor for the channel, if it has one.
    pub acked: Option<Snowflake>,
    pub unread: bool,
    pub mentions: u32,
    pub thread: Option<ThreadState>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NavigationSnapshot {
    pub guilds: Window<GuildRow>,
    pub channels: Window<ChannelRow>,
    pub private_channels: Window<PrivateChannelRow>,
    pub private_selection: Option<PrivateChannelSelection>,
    pub selected_guild: Option<Snowflake>,
    /// The selected guild's name, empty when none is selected.
    pub selected_guild_name: String,
    /// The account's own nickname in the selected guild, if it has one.
    pub own_nick: Option<String>,
    /// The account's own user object as the Gateway last reported it.
    pub current_user: Option<fastcord_model::User>,
    /// Held members of the selected guild (at most [`MAX_SNAPSHOT_MEMBERS`]),
    /// for server nicknames and profile roles. Empty outside a guild.
    pub members: std::collections::HashMap<Snowflake, MemberView>,
    /// The selected guild's roles, highest first, without `@everyone`.
    pub guild_roles: Vec<(Snowflake, String)>,
    pub selection: Option<ChannelSelection>,
    /// The selected channel, when it is a forum or media channel.
    pub forum: Option<ForumView>,
    /// Available guild lacks authoritative own-member/role/owner data.
    pub permissions_pending: bool,
    /// The account has not accepted the selected server's rules; sending,
    /// reacting and creating threads are off in it until Discord says so.
    pub own_pending: bool,
    /// The account's member flags in the selected guild (0 when unknown).
    pub own_flags: u32,
    /// Friends, blocks and pending requests (shared, rebuilt per change).
    pub relationships: std::sync::Arc<[fastcord_model::Relationship]>,
    /// Presence of referenced users (shared, copied on write).
    pub presences: std::sync::Arc<std::collections::HashMap<Snowflake, fastcord_model::Presence>>,
    /// Live DM and group-DM calls (shared, rebuilt per change).
    pub calls: std::sync::Arc<[super::CallInfo]>,
    /// Calls that ring this account, with the conversation's name.
    pub incoming_calls: Vec<IncomingCall>,
    /// Unread and mention rows; empty unless the inbox was asked for.
    pub inbox: Inbox,
    /// Quick-switcher matches; empty unless a query was asked for.
    pub quick: Vec<QuickEntry>,
    /// Message requests awaiting an explicit accept or decline.
    pub requests: MessageRequests,
    /// Ignored and blocked users (shared, rebuilt only when it changes).
    pub suppressed: std::sync::Arc<std::collections::BTreeMap<Snowflake, super::Suppression>>,
    /// Privacy settings as Gateway events set them (shared).
    pub privacy: std::sync::Arc<crate::PrivacyLive>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingCall {
    pub channel_id: Snowflake,
    pub label: String,
}

/// Reducer-owned selection. Memory has at most one channel ID per joined guild
/// (including temporarily unavailable guilds), pruned on each reconciliation.
#[derive(Debug, Default)]
pub struct Navigation {
    account: Option<Snowflake>,
    selected_guild: Option<Snowflake>,
    selected_channel: Option<Snowflake>,
    remembered: BTreeMap<Snowflake, Snowflake>,
    selected_private_channel: Option<Snowflake>,
}

impl Navigation {
    pub fn selected_guild(&self) -> Option<Snowflake> {
        self.selected_guild
    }

    pub fn selected_channel(&self) -> Option<Snowflake> {
        self.selected_channel
    }

    /// Preserves valid IDs, clears revoked/deleted channels, and prunes memory.
    /// Outages retain the guild and remembered ID, but never an open channel.
    /// No channel is opened automatically; returning to a guild can restore its
    /// last still-accessible selection. With no retained guild, the first
    /// available READY-order guild becomes selected.
    pub fn reconcile(&mut self, store: &Store) {
        let account = store.current_user().map(|user| user.id);
        if self.account != account {
            self.account = account;
            self.selected_guild = None;
            self.selected_channel = None;
            self.selected_private_channel = None;
            self.remembered = BTreeMap::new();
        }
        self.remembered.retain(|guild_id, channel_id| {
            if let Some(guild) = store.guild(*guild_id) {
                enabled_channel(store, guild, *channel_id).is_some()
            } else {
                store.unavailable.contains(guild_id)
            }
        });
        self.selected_private_channel = self.selected_private_channel.filter(|id| {
            store
                .private_channel(*id)
                .is_some_and(is_private_conversation)
        });
        if self.selected_private_channel.is_none()
            && !self
                .selected_guild
                .is_some_and(|guild| known_guild(store, guild))
        {
            self.selected_guild = store.guild_ids().first().copied();
        } else if self.selected_private_channel.is_some() {
            self.selected_guild = None;
        }
        self.selected_channel = self.selected_guild.and_then(|guild| {
            store.guild(guild)?;
            self.remembered.get(&guild).copied()
        });
    }

    /// Rejects unknown/outage guilds without changing selection.
    pub fn select_guild(&mut self, store: &Store, guild_id: Snowflake) -> bool {
        if store.guild(guild_id).is_none() {
            return false;
        }
        self.reconcile(store);
        self.selected_private_channel = None;
        self.selected_guild = Some(guild_id);
        self.selected_channel = self.remembered.get(&guild_id).copied();
        true
    }

    /// Opens only an enabled channel of the currently selected guild. A stale
    /// UI row or injected ID is never sufficient authorization.
    pub fn select_channel(&mut self, store: &Store, channel_id: Snowflake) -> bool {
        if self.account != store.current_user().map(|user| user.id) {
            return false;
        }
        let Some(guild_id) = self.selected_guild else {
            return false;
        };
        let Some(guild) = store.guild(guild_id) else {
            return false;
        };
        if enabled_channel(store, guild, channel_id).is_none() {
            return false;
        }
        self.selected_private_channel = None;
        self.selected_channel = Some(channel_id);
        self.remembered.insert(guild_id, channel_id);
        true
    }

    /// Selects only a private channel present in reducer-owned state.
    pub fn select_private_channel(&mut self, store: &Store, channel_id: Snowflake) -> bool {
        if self.account != store.current_user().map(|user| user.id)
            || !store
                .private_channel(channel_id)
                .is_some_and(is_private_conversation)
        {
            return false;
        }
        self.selected_private_channel = Some(channel_id);
        self.selected_guild = None;
        self.selected_channel = None;
        true
    }

    pub fn private_window(
        &self,
        store: &Store,
        offset: usize,
        count: usize,
    ) -> Window<PrivateChannelRow> {
        let mut channels: Vec<_> = store
            .private_channels()
            .filter(|channel| is_private_conversation(channel))
            .collect();
        channels.sort_by_key(|channel| (std::cmp::Reverse(channel.last_message_id), channel.id));
        let total = channels.len();
        let offset = offset.min(total);
        let rows = channels
            .into_iter()
            .skip(offset)
            .take(count.min(MAX_WINDOW_ROWS))
            .map(|channel| {
                let unread = store.unread(channel.id, None, channel.last_message_id);
                PrivateChannelRow {
                    id: channel.id,
                    recipient: match channel.recipient_ids[..] {
                        [only] if channel.kind == fastcord_model::ChannelKind::Dm => Some(only),
                        _ => None,
                    },
                    label: private_name(store, channel),
                    selected: self.selected_private_channel == Some(channel.id),
                    unread: unread.unread,
                    mentions: unread.mentions,
                }
            })
            .collect();
        Window {
            offset,
            total,
            rows,
        }
    }

    pub fn private_selection(&self, store: &Store) -> Option<PrivateChannelSelection> {
        if self.account != store.current_user().map(|user| user.id) {
            return None;
        }
        let channel = store.private_channel(self.selected_private_channel?)?;
        if !is_private_conversation(channel) {
            return None;
        }
        Some(PrivateChannelSelection {
            channel_id: channel.id,
            name: private_name(store, channel),
            group: channel.kind == fastcord_model::ChannelKind::GroupDm,
            custom_name: channel
                .name
                .as_deref()
                .map_or_else(String::new, |name| capped_name(Some(name), "")),
            owner: channel.owner_id,
            has_icon: channel.icon.is_some(),
            members: channel
                .recipient_ids
                .iter()
                .take(64)
                .map(|id| PrivateMember {
                    id: *id,
                    name: store.user(*id).map_or_else(
                        || id.to_string(),
                        |user| capped_name(Some(user.display_name()), "Unknown user"),
                    ),
                })
                .collect(),
            acked: store.acked(channel.id),
            unread: unread_of(store, channel).unread,
            mentions: unread_of(store, channel).mentions,
        })
    }

    /// READY/GUILD_CREATE order for available guilds, then outage IDs in numeric
    /// order. Outage names are decimal IDs because no name is authoritative.
    pub fn guild_window(&self, store: &Store, offset: usize, count: usize) -> Window<GuildRow> {
        let ids = || {
            store.guild_ids().iter().copied().chain(
                store
                    .unavailable_guilds()
                    .filter(|id| store.guild(*id).is_none()),
            )
        };
        let total = store.guild_ids().len()
            + store
                .unavailable_guilds()
                .filter(|id| store.guild(*id).is_none())
                .count();
        let offset = offset.min(total);
        let rows = ids()
            .skip(offset)
            .take(count.min(MAX_WINDOW_ROWS))
            .map(|id| {
                let guild = store.guild(id);
                let unread = store.guild_unread(id);
                GuildRow {
                    unread: unread.unread,
                    mentions: unread.mentions,
                    id,
                    icon: guild.and_then(|guild| guild.icon().map(str::to_owned)),
                    name: guild.map_or_else(
                        || id.to_string(),
                        |guild| capped_name(Some(guild.name()), "Unnamed guild"),
                    ),
                    available: guild.is_some(),
                    owned: guild.is_some_and(|guild| {
                        guild.owner_id().is_some()
                            && guild.owner_id() == store.current_user().map(|user| user.id)
                    }),
                    selected: self.account == store.current_user().map(|user| user.id)
                        && self.selected_guild == Some(id),
                }
            })
            .collect();
        Window {
            offset,
            total,
            rows,
        }
    }

    /// Selected guild only. Uncategorized channels come first, then category
    /// groups in `(position, ID)` order, each header followed by children in
    /// `(position, ID)` order. Missing positions are zero. A hidden/missing
    /// category never hides a child with its own VIEW_CHANNEL; that child is
    /// uncategorized and no forbidden category name is copied.
    pub fn channel_window(&self, store: &Store, offset: usize, count: usize) -> Window<ChannelRow> {
        let guild = self
            .selected_guild
            .filter(|_| self.account == store.current_user().map(|user| user.id))
            .and_then(|id| store.guild(id));
        let visible = guild.map_or_else(Vec::new, |guild| {
            visible_channels(store, guild, self.selected_channel)
        });
        let guild = self.selected_guild.and_then(|id| store.guild(id));
        // Voice and Stage rows are followed by their occupants, one fixed-height
        // line each: at most MAX_VOICE_OCCUPANTS and then one "+N" line. A Stage
        // lists its speakers, then its audience (raised hands first), each group
        // under a heading line.
        let lines_of = |row: &VisibleChannel<'_>| -> Vec<Occupant> {
            let Some(guild) = guild.filter(|_| row.kind == ChannelKind::Voice) else {
                return Vec::new();
            };
            let stage = row.channel.kind == ModelChannelKind::GuildStageVoice;
            let mut here: Vec<_> = guild
                .voice_states()
                .filter(|state| state.channel_id == Some(row.channel.id))
                .collect();
            here.sort_by_key(|s| {
                (
                    stage && s.suppress,
                    // Earliest raised hand first; ISO times order as text.
                    s.request_to_speak_timestamp.is_none(),
                    s.request_to_speak_timestamp.clone(),
                    s.user_id,
                )
            });
            let mut lines = Vec::new();
            let mut group = None;
            for state in here.iter().take(MAX_VOICE_OCCUPANTS) {
                if stage && group != Some(state.suppress) {
                    group = Some(state.suppress);
                    lines.push(Occupant::Heading(if state.suppress {
                        "Audience"
                    } else {
                        "Speakers"
                    }));
                }
                let user = store.user(state.user_id);
                let name = guild
                    .member(state.user_id)
                    .and_then(|member| member.nick.as_deref())
                    .filter(|nick| !nick.is_empty())
                    .or_else(|| user.map(|user| user.display_name()));
                lines.push(Occupant::Member {
                    user_id: state.user_id,
                    name: capped_name(name, "Unknown user"),
                    avatar: user.and_then(|user| user.avatar.clone()),
                    muted: state.mute || state.self_mute,
                    deafened: state.deaf || state.self_deaf,
                    streaming: state.self_stream,
                    video: state.self_video,
                    seat: stage.then(|| StageSeat::of(state)),
                });
            }
            if here.len() > MAX_VOICE_OCCUPANTS {
                lines.push(Occupant::More(here.len() - MAX_VOICE_OCCUPANTS));
            }
            lines
        };
        let mut all_lines: Vec<Vec<Occupant>> = visible.iter().map(lines_of).collect();
        let spans: Vec<usize> = all_lines.iter().map(|lines| 1 + lines.len()).collect();
        let total = spans.iter().sum::<usize>();
        let offset = offset.min(total);
        let take = count.min(MAX_WINDOW_ROWS);
        let mut rows = Vec::new();
        let mut start = 0;
        for (at, (row, span)) in visible.iter().zip(spans).enumerate() {
            let end = start + span;
            if end > offset && start < offset + take {
                let channel = ChannelRow {
                    unread: unread_of(store, row.channel).unread,
                    mentions: unread_of(store, row.channel).mentions,
                    id: row.channel.id,
                    name: capped_name(row.channel.name.as_deref(), "Unnamed channel"),
                    kind: row.kind,
                    parent_id: row.parent_id,
                    thread: guild.and_then(|guild| ThreadState::in_guild(guild, row.channel)),
                    enabled: row.permissions.enables(row.kind),
                    selected: self.selected_channel == Some(row.channel.id)
                        && row.permissions.enables(row.kind),
                    permissions: row.permissions,
                    occupant: None,
                    stage: row.channel.kind == ModelChannelKind::GuildStageVoice,
                };
                let lines = std::mem::take(&mut all_lines[at]);
                let all = std::iter::once(channel.clone()).chain(lines.into_iter().map(|line| {
                    ChannelRow {
                        occupant: Some(line),
                        selected: false,
                        ..channel.clone()
                    }
                }));
                for (at, line) in all.enumerate() {
                    if (offset..offset + take).contains(&(start + at)) {
                        rows.push(line);
                    }
                }
            }
            start = end;
        }
        Window {
            offset,
            total,
            rows,
        }
    }

    /// Rechecks authorization before exposing an open channel or its summary.
    pub fn selection(&self, store: &Store) -> Option<ChannelSelection> {
        if self.account != store.current_user().map(|user| user.id) {
            return None;
        }
        let guild_id = self.selected_guild?;
        let guild = store.guild(guild_id)?;
        let channel_id = self.selected_channel?;
        let (channel, kind, permissions) = enabled_channel(store, guild, channel_id)?;
        Some(ChannelSelection {
            acked: store.acked(channel_id),
            unread: unread_of(store, channel).unread,
            mentions: unread_of(store, channel).mentions,
            guild_id,
            channel_id,
            name: capped_name(channel.name.as_deref(), "Unnamed channel"),
            kind,
            permissions,
            thread: ThreadState::in_guild(guild, channel),
        })
    }

    /// The open forum's tags and active posts. Posts are the guild's held
    /// threads under the forum that the account may see.
    pub fn forum_view(&self, store: &Store) -> Option<ForumView> {
        let selection = self.selection(store)?;
        if selection.kind != ChannelKind::Forum {
            return None;
        }
        let guild = store.guild(selection.guild_id)?;
        let forum = guild.channel(selection.channel_id)?;
        let context = PermissionContext::new(store, guild)?;
        let mut posts: Vec<_> = guild
            .channels()
            .filter(|post| post.kind.is_thread() && post.parent_id == Some(forum.id))
            .filter(|post| !post.thread_metadata.as_ref().is_some_and(|m| m.archived))
            .filter(|post| context.permissions(guild, post).view_channel)
            .collect();
        let total = posts.len();
        let pinned = |post: &Channel| post.flags & FLAG_PINNED != 0;
        let activity = |post: &Channel| post.last_message_id.unwrap_or(post.id);
        posts.sort_by_key(|post| {
            (
                std::cmp::Reverse(pinned(post)),
                std::cmp::Reverse(activity(post)),
                std::cmp::Reverse(post.id),
            )
        });
        let permissions = selection.permissions;
        Some(ForumView {
            channel: forum.id,
            guild: guild.id(),
            media: forum.kind == ModelChannelKind::GuildMedia,
            tags: forum
                .available_tags
                .iter()
                .map(|tag| TagView {
                    id: tag.id,
                    name: capped_name(Some(&tag.name), "Tag"),
                    moderated: tag.moderated,
                    emoji: tag.emoji_name.as_deref().map(|e| clip_bytes(e, 32)),
                })
                .collect(),
            require_tag: forum.flags & FLAG_REQUIRE_TAG != 0,
            sort_creation: forum.default_sort_order == Some(1),
            can_post: permissions.send_messages,
            can_attach: permissions.attach_files,
            can_manage: permissions.manage_threads,
            total,
            posts: posts
                .into_iter()
                .take(MAX_FORUM_POSTS)
                .map(|post| {
                    let unread = unread_of(store, post);
                    let metadata = post.thread_metadata.as_ref();
                    PostRow {
                        id: post.id,
                        title: capped_name(post.name.as_deref(), "Untitled post"),
                        tags: post.applied_tags.clone(),
                        replies: post.message_count.unwrap_or(0),
                        owner: post.owner_id,
                        author: post
                            .owner_id
                            .and_then(|id| store.user(id))
                            .map_or_else(String::new, |u| capped_name(Some(u.display_name()), "")),
                        last_activity: activity(post),
                        pinned: pinned(post),
                        locked: metadata.is_some_and(|m| m.locked),
                        joined: post.member.is_some(),
                        unread: unread.unread,
                        mentions: unread.mentions,
                    }
                })
                .collect(),
        })
    }

    pub fn snapshot(
        &self,
        store: &Store,
        guild_offset: usize,
        guild_count: usize,
        channel_offset: usize,
        channel_count: usize,
    ) -> NavigationSnapshot {
        self.snapshot_with_private(
            store,
            guild_offset,
            guild_count,
            channel_offset,
            channel_count,
            0..MAX_WINDOW_ROWS,
        )
    }

    pub fn snapshot_with_private(
        &self,
        store: &Store,
        guild_offset: usize,
        guild_count: usize,
        channel_offset: usize,
        channel_count: usize,
        private_rows: Range<usize>,
    ) -> NavigationSnapshot {
        let selected_guild = self.selected_guild.filter(|id| {
            self.account == store.current_user().map(|user| user.id) && known_guild(store, *id)
        });
        let permissions_pending = selected_guild
            .and_then(|id| store.guild(id))
            .is_some_and(|guild| PermissionContext::new(store, guild).is_none());
        let own_member = selected_guild
            .and_then(|id| store.guild(id))
            .zip(store.current_user())
            .and_then(|(guild, me)| guild.member(me.id));
        NavigationSnapshot {
            guilds: self.guild_window(store, guild_offset, guild_count),
            channels: self.channel_window(store, channel_offset, channel_count),
            private_channels: self.private_window(
                store,
                private_rows.start,
                private_rows.end.saturating_sub(private_rows.start),
            ),
            private_selection: self.private_selection(store),
            selected_guild_name: selected_guild
                .and_then(|id| store.guild(id))
                .map_or_else(String::new, |guild| {
                    capped_name(Some(guild.name()), "Unnamed guild")
                }),
            current_user: store.current_user().cloned(),
            members: selected_guild
                .and_then(|id| store.guild(id))
                .map(|guild| {
                    guild
                        .members()
                        .take(MAX_SNAPSHOT_MEMBERS)
                        .map(|m| {
                            (
                                m.user_id,
                                MemberView {
                                    nick: m
                                        .nick
                                        .as_deref()
                                        .filter(|n| !n.is_empty())
                                        .map(|n| clip_bytes(n, MAX_NICK_BYTES)),
                                    roles: m.roles.clone(),
                                    moderation: store.member_permissions(guild.id(), m.user_id),
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            guild_roles: selected_guild
                .and_then(|id| store.guild(id).map(|g| (id, g)))
                .map(|(id, guild)| {
                    let mut roles: Vec<_> =
                        guild.roles().iter().filter(|role| role.id != id).collect();
                    roles.sort_by_key(|role| std::cmp::Reverse(role.position));
                    roles
                        .into_iter()
                        .take(MAX_SNAPSHOT_ROLES)
                        .map(|role| (role.id, capped_name(Some(&role.name), "Role")))
                        .collect()
                })
                .unwrap_or_default(),
            own_nick: selected_guild
                .and_then(|id| store.guild(id))
                .zip(store.current_user())
                .and_then(|(guild, me)| guild.member(me.id))
                .and_then(|member| member.nick.clone()),
            own_pending: own_member.is_some_and(|m| m.pending == Some(true)),
            own_flags: own_member.and_then(|m| m.flags).unwrap_or(0),
            selected_guild,
            forum: self.forum_view(store),
            selection: self.selection(store),
            permissions_pending,
            relationships: store.relationships(),
            calls: store.calls(),
            incoming_calls: store
                .calls()
                .iter()
                .filter(|call| call.ringing_me)
                .filter_map(|call| {
                    let channel = store.private_channel(call.channel_id)?;
                    if call_hidden(store, channel) {
                        return None;
                    }
                    Some(IncomingCall {
                        channel_id: call.channel_id,
                        label: private_name(store, channel),
                    })
                })
                .collect(),
            presences: store.presences(),
            inbox: Inbox::default(),
            quick: Vec::new(),
            requests: self.message_requests(store),
            suppressed: store.suppressed(),
            privacy: store.privacy(),
        }
    }

    /// Pending message requests, newest first, names capped.
    fn message_requests(&self, store: &Store) -> MessageRequests {
        let mut pending: Vec<_> = store
            .private_channels()
            .filter(|channel| {
                channel.is_pending_request()
                    && matches!(
                        channel.kind,
                        ModelChannelKind::Dm | ModelChannelKind::GroupDm
                    )
            })
            .collect();
        pending.sort_by_key(|channel| (std::cmp::Reverse(channel.last_message_id), channel.id));
        MessageRequests {
            total: pending.len(),
            rows: pending
                .into_iter()
                .take(MAX_REQUEST_ROWS)
                .map(|channel| RequestRow {
                    channel: channel.id,
                    label: private_name(store, channel),
                    spam: channel.is_spam == Some(true),
                })
                .collect(),
        }
    }

    /// Declarative guild/own-member interest only, with no history request and
    /// no member-list range. Outages are never subscribed. Verified owners need
    /// no member fetch to establish access; an omitted/null own roles array does.
    pub fn subscription_target(&self, store: &Store) -> SubscriptionTarget {
        let mut target = SubscriptionTarget::default();
        if self.account != store.current_user().map(|user| user.id) {
            return target;
        }
        let Some(guild_id) = self.selected_guild else {
            return target;
        };
        let Some(guild) = store.guild(guild_id) else {
            return target;
        };
        target.select_guild(guild_id);
        if let Some(channel_id) = self.selected_channel
            && enabled_channel(store, guild, channel_id).is_some()
        {
            target.select_channel(guild_id, channel_id);
            target.clear_member_viewport();
        }
        if let Some(user) = store.current_user()
            && guild.owner_id() != Some(user.id)
            && !guild
                .member(user.id)
                .is_some_and(|member| member.roles_known)
        {
            target.set_member_interest(guild_id, [user.id]);
        }
        target
    }
}

fn known_guild(store: &Store, id: Snowflake) -> bool {
    store.guild(id).is_some() || store.unavailable.contains(&id)
}

/// What a channel row shows.
fn unread_of(store: &Store, channel: &Channel) -> Unread {
    store.unread(channel.id, channel.guild_id, channel.last_message_id)
}

fn clip_bytes(value: &str, limit: usize) -> String {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn capped_name(value: Option<&str>, fallback: &str) -> String {
    let value = value.filter(|name| !name.is_empty()).unwrap_or(fallback);
    let mut end = value.len().min(MAX_NAME_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// An ordinary direct or group conversation. A message request is not one
/// until the account accepts it: it is neither listed nor selectable, so
/// nothing opens, reads or acknowledges it by accident.
fn is_private_conversation(channel: &Channel) -> bool {
    matches!(
        channel.kind,
        ModelChannelKind::Dm | ModelChannelKind::GroupDm
    ) && channel.guild_id.is_none()
        && !channel.is_pending_request()
}

/// Message requests listed at most.
pub const MAX_REQUEST_ROWS: usize = 100;

/// One pending message request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRow {
    pub channel: Snowflake,
    pub label: String,
    /// Discord flagged it as spam.
    pub spam: bool,
}

/// Pending message requests, newest first. `total` counts all of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageRequests {
    pub total: usize,
    pub rows: Vec<RequestRow>,
}

/// A call that must not ring: its one-to-one conversation is with a
/// suppressed user, or it is a message request. A group call keeps ringing,
/// since the ring does not say who started it.
fn call_hidden(store: &Store, channel: &Channel) -> bool {
    channel.is_pending_request()
        || (channel.kind == ModelChannelKind::Dm
            && channel
                .recipient_ids
                .iter()
                .any(|id| store.suppression(*id).is_some()))
}

fn private_name(store: &Store, channel: &Channel) -> String {
    if let Some(name) = channel.name.as_deref().filter(|name| !name.is_empty()) {
        return capped_name(Some(name), "Private conversation");
    }
    let label = channel
        .recipient_ids
        .iter()
        .filter_map(|id| store.user(*id))
        .map(|user| user.display_name())
        .collect::<Vec<_>>()
        .join(", ");
    capped_name(Some(&label), "Private conversation")
}

enum PermissionContext<'a> {
    Owner,
    Member {
        guild: GuildScope<'a>,
        member: MemberScope<'a>,
        /// Membership screening is not accepted: no writing until Discord says so.
        pending: bool,
    },
}

impl<'a> PermissionContext<'a> {
    fn new(store: &'a Store, guild: &'a GuildEntry) -> Option<Self> {
        Self::with_clock(store, guild, OffsetDateTime::now_utc)
    }

    fn with_clock(
        store: &'a Store,
        guild: &'a GuildEntry,
        now: impl FnOnce() -> OffsetDateTime,
    ) -> Option<Self> {
        let user = store.current_user()?.id;
        let owner_id = guild.owner_id()?;
        if user == owner_id {
            return Some(Self::Owner);
        }
        let own = guild.member(user)?;
        if !own.roles_known
            || guild
                .roles()
                .binary_search_by_key(&guild.id(), |role| role.id)
                .is_err()
            || own.roles.iter().any(|id| {
                guild
                    .roles()
                    .binary_search_by_key(id, |role| role.id)
                    .is_err()
            })
        {
            return None;
        }
        Some(Self::Member {
            guild: GuildScope {
                id: guild.id(),
                owner_id,
                roles: guild.roles(),
            },
            member: MemberScope {
                user_id: user,
                role_ids: &own.roles,
                timed_out: own
                    .communication_disabled_until
                    .as_deref()
                    .is_some_and(|until| timeout_active(until, now())),
            },
            pending: own.pending == Some(true),
        })
    }

    fn permissions(&self, entry: &GuildEntry, channel: &Channel) -> PermissionSummary {
        // A thread has no overwrites of its own: it inherits its parent's, and
        // a missing parent leaves it inaccessible.
        let overwrites = if channel.kind.is_thread() {
            match channel.parent_id.and_then(|id| entry.channel(id)) {
                Some(parent) => &parent.permission_overwrites,
                None => return PermissionSummary::default(),
            }
        } else {
            &channel.permission_overwrites
        };
        let permissions = match self {
            Self::Owner => Permissions::ALL,
            Self::Member { guild, member, .. } => {
                channel_permissions(*guild, *member, overwrites, channel.kind.is_text_like())
            }
        };
        let mut summary = PermissionSummary::from(permissions);
        if matches!(self, Self::Member { pending: true, .. }) {
            summary.send_messages = false;
            summary.add_reactions = false;
            summary.create_threads = false;
            summary.attach_files = false;
        }
        if let Some(thread) = ThreadState::of(channel) {
            // Private threads exist only for members and thread managers.
            if channel.kind == ModelChannelKind::PrivateThread
                && !thread.joined
                && !summary.manage_threads
            {
                return PermissionSummary::default();
            }
            summary.send_messages = permissions.contains(Permissions::SEND_MESSAGES_IN_THREADS)
                && (!thread.locked || summary.manage_threads);
        }
        summary
    }
}

// Malformed expiry is unknown, not permission to send/connect. Timeouts are
// evaluated on demand; navigation never starts a periodic expiry poll.
fn timeout_active(until: &str, now: OffsetDateTime) -> bool {
    OffsetDateTime::parse(until, &Rfc3339).map_or(true, |expiry| expiry > now)
}

pub(crate) fn active_timeout(until: &str) -> bool {
    timeout_active(until, OffsetDateTime::now_utc())
}

fn enabled_channel<'a>(
    store: &'a Store,
    guild: &'a GuildEntry,
    id: Snowflake,
) -> Option<(&'a Channel, ChannelKind, PermissionSummary)> {
    let channel = guild.channel(id)?;
    let kind = ChannelKind::from_model(channel.kind)?;
    let permissions = PermissionContext::new(store, guild)?.permissions(guild, channel);
    permissions
        .enables(kind)
        .then_some((channel, kind, permissions))
}

/// Category group, then the channel's slot, then (for a thread) its place
/// after the parent: `(0, 0)` for everything else, `(1, thread)` for threads.
type ChannelOrder = (u8, i32, Snowflake, u8, i32, Snowflake, u8, Snowflake);

struct VisibleChannel<'a> {
    channel: &'a Channel,
    kind: ChannelKind,
    parent_id: Option<Snowflake>,
    permissions: PermissionSummary,
    order: ChannelOrder,
}

fn visible_channels<'a>(
    store: &'a Store,
    guild: &'a GuildEntry,
    open: Option<Snowflake>,
) -> Vec<VisibleChannel<'a>> {
    let Some(context) = PermissionContext::new(store, guild) else {
        return Vec::new();
    };
    // Temporary borrowed metadata, not cloned account state or owned names.
    // The ID-sorted Store order also makes category lookup logarithmic.
    let mut visible: Vec<_> = guild
        .channels()
        .filter_map(|channel| {
            let kind = ChannelKind::from_model(channel.kind)?;
            let permissions = context.permissions(guild, channel);
            if !permissions.view_channel {
                return None;
            }
            // Archived threads are reached through the archive list, and forum
            // posts through the forum's post list; both stay in the sidebar
            // only while open.
            if (ThreadState::of(channel).is_some_and(|t| t.archived)
                || is_forum_post(guild, channel))
                && open != Some(channel.id)
            {
                return None;
            }
            let position = channel.position.unwrap_or(0);
            let order = if kind == ChannelKind::Category {
                (1, position, channel.id, 0, 0, channel.id, 0, Snowflake(0))
            } else {
                (0, 0, Snowflake(0), 1, position, channel.id, 0, Snowflake(0))
            };
            Some(VisibleChannel {
                channel,
                kind,
                parent_id: None,
                permissions,
                order,
            })
        })
        .collect();
    for at in 0..visible.len() {
        if matches!(
            visible[at].kind,
            ChannelKind::Category | ChannelKind::Thread
        ) {
            continue;
        }
        let category = visible[at]
            .channel
            .parent_id
            .and_then(|parent| {
                visible
                    .binary_search_by_key(&parent, |row| row.channel.id)
                    .ok()
            })
            .filter(|parent| visible[*parent].kind == ChannelKind::Category);
        if let Some(category) = category {
            let parent = visible[category].channel;
            visible[at].parent_id = Some(parent.id);
            visible[at].order.0 = 1;
            visible[at].order.1 = parent.position.unwrap_or(0);
            visible[at].order.2 = parent.id;
        }
    }
    // Threads follow their parent, which must itself be visible.
    let mut orphan = vec![false; visible.len()];
    for at in 0..visible.len() {
        if visible[at].kind != ChannelKind::Thread {
            continue;
        }
        let parent = visible[at].channel.parent_id.and_then(|parent| {
            visible
                .binary_search_by_key(&parent, |row| row.channel.id)
                .ok()
                .filter(|p| {
                    !matches!(
                        visible[*p].kind,
                        ChannelKind::Category | ChannelKind::Thread
                    )
                })
        });
        match parent {
            Some(parent) => {
                let mut order = visible[parent].order;
                order.6 = 1;
                order.7 = visible[at].channel.id;
                visible[at].order = order;
            }
            None => orphan[at] = true,
        }
    }
    let mut at = 0;
    visible.retain(|_| {
        at += 1;
        !orphan[at - 1]
    });
    visible.sort_unstable_by_key(|row| row.order);
    visible
}

mod inbox;
pub use inbox::{Inbox, InboxRow, MAX_INBOX_ROWS};
mod quick;
pub use quick::{
    MAX_QUICK_QUERY_CHARS, MAX_QUICK_RESULTS, QuickEntry, QuickKind, Step, StepScope, match_score,
    query_chars,
};

#[cfg(test)]
mod tests;
