//! Events the Gateway task delivers to its single consumer (the reducer).

use std::fmt;
use std::time::Duration;

use fastcord_model::{
    ActivityUpdate, CallCreate, CallDelete, CallUpdate, Channel, Emoji, Guild, GuildMember,
    Message, MessageUpdate, Relationship, RelationshipKind, Role, ScheduledEvent,
    ScheduledEventKey, ScheduledEventUser, Snowflake, StageInstance, User, VoiceServerUpdate,
    VoiceState,
};

/// The Gateway session ID. Not a credential on its own (resuming also needs the
/// token), but redacted in `Debug` like everything session-related.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionId(pub(crate) String);

impl SessionId {
    pub fn new(id: String) -> Self {
        Self(id)
    }

    /// Voice signaling correlates its join attempts with this session ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionId([REDACTED])")
    }
}

/// Why the connection is being re-established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconnectReason {
    /// Opcode 7: the server is migrating the session.
    ServerRequested,
    /// No heartbeat ACK arrived within an interval; the connection is dead.
    HeartbeatTimeout,
    /// HELLO never arrived.
    HelloTimeout,
    /// A frame or the compression stream could not be decoded; the zlib
    /// dictionary cannot be trusted any more, so the connection is replaced.
    Protocol,
    /// Opcode 9.
    InvalidSession {
        resumable: bool,
    },
    /// The server closed the socket (with an optional close code).
    Closed(Option<u16>),
    /// The socket failed mid-stream.
    Network,
    ConnectFailed,
    /// `GET /gateway` failed.
    DiscoveryFailed,
}

/// A terminal condition that is neither a login problem nor recoverable by
/// reconnecting. Automatic attempts have stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// One event exceeded the 16 MiB client safety ceiling, so account state
    /// would otherwise be silently truncated.
    EventTooLarge,
    /// READY or READY_SUPPLEMENTAL could not be decoded (payload drift).
    MalformedReady,
    /// The server closed with a code that reconnecting cannot fix (rejected
    /// payload, invalid version, invalid intents, and similar).
    Rejected(u16),
    /// More Gateway sessions are open than the account may have.
    TooManySessions,
    /// Discord requires the account holder to act first (a challenge, terms,
    /// verification). fastcord never completes or bypasses these.
    ActionRequired(String),
    /// Repeated Invalid Session answers to Identify: the profile or account is
    /// being refused without an explicit close code.
    IdentifyRejected,
    /// Discovery returned a URL that is not a Discord Gateway.
    InvalidGatewayUrl,
    /// Locally planned guild subscriptions exceeded the outbound payload ceiling.
    SubscriptionTooLarge,
    /// A Go Live signal exceeded the outbound payload safety ceiling.
    StreamSignalTooLarge,
    /// Required normalized state could not fit the metadata safety ceiling.
    StateTooLarge,
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventTooLarge => f.write_str("Discord sent an event larger than fastcord's 16 MiB safety limit, so the account state could not be loaded safely."),
            Self::MalformedReady => f.write_str("Discord's startup data (READY) could not be decoded. Discord may have changed its protocol; update fastcord."),
            Self::Rejected(code) => write!(f, "Discord closed the Gateway connection with code {code}, which reconnecting cannot fix."),
            Self::TooManySessions => f.write_str("This account has too many open Discord sessions. Close one and log in again."),
            Self::ActionRequired(action) => write!(f, "Discord requires an action on this account ({action}). Complete it in the official Discord client; fastcord does not complete or bypass it."),
            Self::IdentifyRejected => f.write_str("Discord repeatedly refused this connection without a reason. Check the account in the official client; fastcord stops instead of retrying."),
            Self::InvalidGatewayUrl => f.write_str("Discord returned a Gateway address that is not a Discord Gateway. fastcord refused to send the login token there."),
            Self::SubscriptionTooLarge => f.write_str("A guild subscription could not be encoded within Discord's 15 KiB Gateway payload limit. Automatic attempts have stopped."),
            Self::StreamSignalTooLarge => f.write_str("A Go Live signal could not be encoded within Discord's 15 KiB Gateway payload limit. Automatic attempts have stopped."),
            Self::StateTooLarge => f.write_str("Required account state exceeds fastcord's 12 MiB metadata safety limit. The connection has stopped rather than silently dropping guild identity or permission data."),
        }
    }
}

/// The explicit connection state machine (SPEC §4.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Opening a connection. `attempt` counts consecutive failed attempts.
    Connecting {
        attempt: u32,
    },
    AwaitHello,
    Identifying,
    Resuming,
    Ready,
    /// Waiting `delay` before the next attempt.
    Reconnecting {
        attempt: u32,
        delay: Duration,
        reason: ReconnectReason,
    },
    /// Terminal: the token is no longer accepted. Return to login.
    AuthenticationRequired,
    /// Terminal: see [`StopReason`].
    Stopped(StopReason),
}

impl ConnectionState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::AuthenticationRequired | Self::Stopped(_))
    }
}

/// Normalized READY. User objects are stored once in `users`; guilds, private
/// channels, and members reference them by ID. The current user's member is
/// the only member of each guild until READY_SUPPLEMENTAL.
///
/// A READY always replaces everything derived from a previous session.
#[derive(Clone, PartialEq, Eq)]
pub struct Ready {
    pub session_id: SessionId,
    pub user: User,
    pub users: Vec<User>,
    pub guilds: Vec<Guild>,
    /// Guilds in an outage or geo-restricted: known by ID only.
    pub unavailable_guilds: Vec<Snowflake>,
    pub private_channels: Vec<Channel>,
    /// Friends, blocks and pending requests (at most [`MAX_RELATIONSHIPS`]).
    pub relationships: Vec<Relationship>,
    /// Users flagged `user_ignored` whose relationship type is none of the
    /// four kept (at most [`MAX_RELATIONSHIPS`]).
    pub ignored: Vec<Snowflake>,
    /// Presences READY carried (at most [`MAX_PRESENCES`]); the store keeps
    /// only those of referenced users.
    pub presences: Vec<PresenceUpdate>,
    /// The account's read cursors and mutes.
    pub read: ReadyRead,
}

/// Most relationships kept from READY or events; the rest are dropped.
pub const MAX_RELATIONSHIPS: usize = 2000;

/// Most presences held or taken from one READY.
pub const MAX_PRESENCES: usize = 2000;

/// One user's presence, whole (READY) or partial (PRESENCE_UPDATE): a field
/// the payload omitted is `None` and leaves what is held as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceUpdate {
    pub user: Snowflake,
    pub status: Option<fastcord_model::Status>,
    /// `Some(None)`: activities were sent and none is a custom status.
    pub custom: Option<Option<String>>,
}

/// RELATIONSHIP_UPDATE: the type of a known relationship changed. A payload
/// without a recognised type (a nickname change) leaves it as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelationshipUpdate {
    pub id: Snowflake,
    pub kind: Option<RelationshipKind>,
    /// `user_ignored` when the payload carried it; omitted leaves it as it is.
    pub ignored: Option<bool>,
}

/// One channel's read cursor from READY: the last message the account
/// acknowledged (not the channel's newest) and its unread mentions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadStateEntry {
    pub channel_id: Snowflake,
    pub last_acked: Snowflake,
    pub mention_count: u32,
}

/// Muted guilds and channels (including DMs) at READY.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MuteSettings {
    pub guilds: Vec<Snowflake>,
    pub channels: Vec<Snowflake>,
    /// Every settings entry, for the notification levels and suppressions.
    pub entries: Vec<GuildMuteUpdate>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadyRead {
    pub entries: Vec<ReadStateEntry>,
    pub mutes: MuteSettings,
    /// Server folders from the user settings; `None` when READY had none.
    pub folders: Option<crate::user_settings::GuildFolders>,
}

/// What a server or channel notifies about (`message_notifications`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotifyLevel {
    #[default]
    All,
    Mentions,
    Nothing,
}

impl NotifyLevel {
    /// 0 to 2; 3 (use the server's setting) and anything else is `None`.
    pub fn from_wire(value: u64) -> Option<Self> {
        match value {
            0 => Some(Self::All),
            1 => Some(Self::Mentions),
            2 => Some(Self::Nothing),
            _ => None,
        }
    }

    pub fn wire(self) -> u8 {
        self as u8
    }
}

/// MESSAGE_ACK: some client of the account moved a channel's read cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageAck {
    pub channel_id: Snowflake,
    pub message_id: Snowflake,
    pub mention_count: u32,
}

/// TYPING_START: someone began typing in a channel. The server's timestamp is
/// not kept; expiry is local.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypingStart {
    pub channel_id: Snowflake,
    pub user_id: Snowflake,
}

/// VOICE_CHANNEL_EFFECT_SEND of a soundboard sound: someone played a sound in a voice
/// channel. Emoji effects are not surfaced. Consumed by the voice layer, not the reducer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoundboardPlay {
    pub channel_id: Snowflake,
    pub guild_id: Option<Snowflake>,
    pub user_id: Snowflake,
    pub sound_id: Snowflake,
    /// The sound's own volume, 0 to 100.
    pub volume_percent: u8,
}

/// USER_GUILD_SETTINGS_UPDATE, reduced to what unread markers need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuildMuteUpdate {
    /// `None` for the account's direct messages.
    pub guild_id: Option<Snowflake>,
    pub guild_muted: bool,
    pub channels: Vec<(Snowflake, bool)>,
    /// The server's level; `None` follows the server's own default.
    pub level: Option<NotifyLevel>,
    pub suppress_everyone: bool,
    pub suppress_roles: bool,
    /// Channel overrides; `None` inherits.
    pub channel_levels: Vec<(Snowflake, Option<NotifyLevel>)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupplementalGuild {
    pub id: Snowflake,
    pub voice_states: Vec<VoiceState>,
    pub members: Vec<GuildMember>,
}

/// Normalized READY_SUPPLEMENTAL: the rest of what PRIORITIZED_READY_PAYLOAD
/// held back from READY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadySupplemental {
    pub guilds: Vec<SupplementalGuild>,
    /// Users newly seen in this event; READY's `users` table still applies.
    pub users: Vec<User>,
    /// Private channels omitted from READY.
    pub lazy_private_channels: Vec<Channel>,
    /// Friends' presences (`merged_presences.friends`).
    pub presences: Vec<PresenceUpdate>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct MessageDelete {
    pub id: Snowflake,
    pub channel_id: Snowflake,
    pub guild_id: Option<Snowflake>,
}

/// MESSAGE_DELETE_BULK. The ordered event removes all supplied IDs atomically
/// from the consumer's current history, without retaining cold tombstones.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct MessageDeleteBulk {
    pub ids: Vec<Snowflake>,
    pub channel_id: Snowflake,
    pub guild_id: Option<Snowflake>,
}

/// MESSAGE_REACTION_ADD or MESSAGE_REACTION_REMOVE: one user's reaction to a
/// message. Only what the reaction aggregate needs is kept; the decoder has
/// already checked that the emoji can identify a reaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageReaction {
    pub channel_id: Snowflake,
    pub message_id: Snowflake,
    pub guild_id: Option<Snowflake>,
    /// Who reacted, or took the reaction back.
    pub user_id: Snowflake,
    pub emoji: Emoji,
    /// A burst (super) reaction rather than a normal one.
    pub burst: bool,
}

/// MESSAGE_REACTION_REMOVE_EMOJI: every reaction with this emoji was removed
/// from a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageReactionRemoveEmoji {
    pub channel_id: Snowflake,
    pub message_id: Snowflake,
    pub guild_id: Option<Snowflake>,
    pub emoji: Emoji,
}

/// MESSAGE_REACTION_REMOVE_ALL: every reaction was removed from a message.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct MessageReactionRemoveAll {
    pub channel_id: Snowflake,
    pub message_id: Snowflake,
    pub guild_id: Option<Snowflake>,
}

/// MESSAGE_POLL_VOTE_ADD or MESSAGE_POLL_VOTE_REMOVE: one user's vote for one
/// answer of a poll was added or taken back.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct MessagePollVote {
    pub user_id: Snowflake,
    pub channel_id: Snowflake,
    pub message_id: Snowflake,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    pub answer_id: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct GuildEmojisUpdate {
    pub guild_id: Snowflake,
    pub emojis: Vec<fastcord_model::GuildEmoji>,
}

/// GUILD_CREATE: a joined or newly available guild, or one that is now known
/// only by ID because of an outage.
// One event is held at a time, so the size gap costs nothing worth a Box.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq, Eq)]
pub enum GuildCreate {
    Available { guild: Guild, users: Vec<User> },
    Unavailable(Snowflake),
}

/// Partial GUILD_UPDATE; presence bits preserve omitted metadata, while an
/// explicit null owner cannot leave a stale owner permission bypass behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuildUpdate {
    pub id: Snowflake,
    pub name: Option<String>,
    pub name_present: bool,
    pub icon: Option<String>,
    pub icon_present: bool,
    pub owner_id: Option<Snowflake>,
    pub owner_present: bool,
    pub roles: Vec<Role>,
    pub roles_present: bool,
    pub member_count: Option<u32>,
    /// Present when the update carries the guild's settings (`features`).
    pub settings: Option<fastcord_model::GuildSettings>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuildDelete {
    pub id: Snowflake,
    /// `true`: an outage, the guild is still joined. `false`: removed.
    pub unavailable: bool,
}

/// PASSIVE_UPDATE_V2 for a guild this connection is not subscribed to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassiveUpdate {
    pub guild_id: Snowflake,
    pub channels: Vec<ChannelUnread>,
    pub updated_voice_states: Vec<VoiceState>,
    pub removed_voice_states: Vec<Snowflake>,
    pub members: Vec<GuildMember>,
    pub users: Vec<User>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ChannelUnread {
    pub id: Snowflake,
    #[serde(default)]
    pub last_message_id: Option<Snowflake>,
}

/// GUILD_MEMBER_ADD or GUILD_MEMBER_UPDATE: one member, normalized like READY's
/// (the user object is reported once in `users`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuildMemberEvent {
    pub guild_id: Snowflake,
    pub member: GuildMember,
    /// Presence bits distinguish omitted update fields from explicit null/empty.
    pub nick_present: bool,
    pub roles_present: bool,
    pub timeout_present: bool,
    pub username_present: bool,
    pub global_name_present: bool,
    pub avatar_present: bool,
    pub bot_present: bool,
    pub users: Vec<User>,
}

/// THREAD_LIST_SYNC: the active threads of some parent channels (all of the
/// guild's when `parent_ids` is empty), replacing what was held for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadListSync {
    pub guild_id: Snowflake,
    pub parent_ids: Vec<Snowflake>,
    /// Always carry their `guild_id`.
    pub threads: Vec<Channel>,
    /// Threads the current user has joined.
    pub joined: Vec<Snowflake>,
}

/// THREAD_MEMBER_UPDATE: the current user's own thread membership changed
/// (they joined or were added).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadMemberUpdate {
    pub guild_id: Snowflake,
    pub thread_id: Snowflake,
}

/// THREAD_MEMBERS_UPDATE: other members joined or left. Only the current
/// user's entries change what the client shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadMembersUpdate {
    pub guild_id: Snowflake,
    pub thread_id: Snowflake,
    pub added: Vec<Snowflake>,
    pub removed: Vec<Snowflake>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuildMemberRemove {
    pub guild_id: Snowflake,
    pub user_id: Snowflake,
}

/// GUILD_ROLE_CREATE or GUILD_ROLE_UPDATE: complete role permission metadata.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct GuildRoleEvent {
    pub guild_id: Snowflake,
    pub role: Role,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct GuildRoleDelete {
    pub guild_id: Snowflake,
    pub role_id: Snowflake,
}

/// VOICE_STATE_UPDATE for a guild voice channel, with the member it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceStateUpdate {
    pub state: VoiceState,
    pub member: Option<GuildMember>,
    pub users: Vec<User>,
}

/// Which member list of a guild an update belongs to: `everyone`, or a hash of
/// the channel's permission overwrites. The event does not name the channel.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemberListId(pub String);

/// A member-list section header.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GroupId {
    Online,
    Offline,
    /// A hoisted role.
    Role(Snowflake),
    /// A group this client does not know.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListGroup {
    pub id: GroupId,
    pub count: u32,
}

/// One row of a member list. Members are referenced by user ID; the member
/// objects themselves arrive in [`MemberListUpdate::members`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListRow {
    Group(ListGroup),
    Member(Snowflake),
    /// A row the payload did not let us read. It still occupies its index, so
    /// the indices of every later row stay right.
    Unreadable,
}

/// A member-list operation. Indices are positions in the whole flattened list
/// (headers and members), not within a requested range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberListOp {
    /// The rows of `start..=end` (fewer rows than the span mean the list ends).
    Sync {
        start: u32,
        end: u32,
        rows: Vec<ListRow>,
    },
    Insert {
        index: u32,
        row: ListRow,
    },
    Update {
        index: u32,
        row: ListRow,
    },
    Delete {
        index: u32,
    },
    /// The server no longer maintains `start..=end`.
    Invalidate {
        start: u32,
        end: u32,
    },
    /// An operation this client does not know: the list can no longer be
    /// trusted and is emptied until it is synchronized again.
    Unknown,
}

/// GUILD_MEMBER_LIST_UPDATE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberListUpdate {
    pub guild_id: Snowflake,
    pub list_id: MemberListId,
    pub member_count: Option<u32>,
    pub online_count: Option<u32>,
    pub groups: Option<Vec<ListGroup>>,
    pub ops: Vec<MemberListOp>,
    /// Every member the operations reference.
    pub members: Vec<GuildMember>,
    pub users: Vec<User>,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct ChannelRecipientAdd {
    pub channel_id: Snowflake,
    pub user: User,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelRecipientRemove {
    pub channel_id: Snowflake,
    pub user_id: Snowflake,
}

/// Typed dispatch events. Events the client has no handler for are consumed
/// (their sequence still counts) and never surfaced. Live events keep their
/// wire shape; only READY-family payloads are normalized.
#[derive(Clone, PartialEq, Eq)]
pub enum Dispatch {
    Ready(Box<Ready>),
    ReadySupplemental(Box<ReadySupplemental>),
    /// Replay after a successful Resume is complete. State is kept.
    Resumed,
    MessageCreate(Box<Message>),
    MessageUpdate(Box<MessageUpdate>),
    MessageDelete(MessageDelete),
    MessageDeleteBulk(MessageDeleteBulk),
    /// The account's read cursor moved.
    MessageAck(MessageAck),
    TypingStart(TypingStart),
    PresenceUpdate(Box<PresenceUpdate>),
    RelationshipAdd(Box<Relationship>),
    RelationshipUpdate(RelationshipUpdate),
    RelationshipRemove(Snowflake),
    /// `USER_SETTINGS_UPDATE`, reduced to the privacy fields this client knows.
    UserSettingsUpdate(Box<crate::PrivacySettings>),
    /// Mute settings changed.
    UserGuildSettingsUpdate(Box<GuildMuteUpdate>),
    /// Server folders changed (`USER_SETTINGS_PROTO_UPDATE`, or
    /// `USER_SETTINGS_UPDATE` carrying `guild_folders`).
    UserFoldersUpdate(Box<crate::user_settings::GuildFolders>),
    /// A user reacted to a message.
    MessageReactionAdd(Box<MessageReaction>),
    /// A user took a reaction back.
    MessageReactionRemove(Box<MessageReaction>),
    MessageReactionRemoveAll(MessageReactionRemoveAll),
    MessageReactionRemoveEmoji(Box<MessageReactionRemoveEmoji>),
    /// Discord settled an interaction the account started.
    InteractionSuccess(crate::interaction::InteractionOutcome),
    /// Discord refused or lost an interaction the account started.
    InteractionFailure(crate::interaction::InteractionOutcome),
    /// An app asks the account to fill in a form.
    InteractionModalCreate(Box<crate::interaction::InteractionModal>),
    /// A user voted for a poll answer.
    MessagePollVoteAdd(MessagePollVote),
    /// A user withdrew a vote for a poll answer.
    MessagePollVoteRemove(MessagePollVote),
    GuildCreate(Box<GuildCreate>),
    GuildUpdate(Box<GuildUpdate>),
    GuildEmojisUpdate(Box<GuildEmojisUpdate>),
    GuildDelete(GuildDelete),
    ChannelCreate(Box<Channel>),
    ChannelUpdate(Box<Channel>),
    ChannelDelete(Box<Channel>),
    ThreadListSync(Box<ThreadListSync>),
    ThreadMemberUpdate(ThreadMemberUpdate),
    ThreadMembersUpdate(Box<ThreadMembersUpdate>),
    ChannelRecipientAdd(Box<ChannelRecipientAdd>),
    ChannelRecipientRemove(Box<ChannelRecipientRemove>),
    PassiveUpdate(Box<PassiveUpdate>),
    MemberListUpdate(Box<MemberListUpdate>),
    GuildMemberAdd(Box<GuildMemberEvent>),
    GuildMemberUpdate(Box<GuildMemberEvent>),
    GuildMemberRemove(GuildMemberRemove),
    /// The account's own user object changed (display name, avatar, ...).
    UserUpdate(Box<User>),
    GuildRoleCreate(Box<GuildRoleEvent>),
    GuildRoleUpdate(Box<GuildRoleEvent>),
    GuildRoleDelete(GuildRoleDelete),
    VoiceStateUpdate(Box<VoiceStateUpdate>),
    /// A soundboard sound was played in a voice channel.
    VoiceChannelSound(SoundboardPlay),
    /// An embedded Activity started, changed or ended in a voice channel.
    EmbeddedActivity(ActivityUpdate),
    /// A Stage session started or its topic changed.
    StageInstanceUpsert(StageInstance),
    /// A Stage session ended.
    StageInstanceDelete(StageInstance),
    /// A scheduled event was created or changed.
    ScheduledEventUpsert(Box<ScheduledEvent>),
    ScheduledEventDelete(ScheduledEventKey),
    /// A member became (`added`) or stopped being interested in an event.
    ScheduledEventUser {
        user: ScheduledEventUser,
        added: bool,
    },
    /// An AutoMod rule was created, changed or deleted (issue #96).
    AutoModeration(Box<crate::AutoModChange>),
    /// A server's soundboard sounds were created, edited, deleted or replaced.
    SoundboardChanged(Snowflake),
    /// A server's stickers were created, edited or deleted (`GUILD_STICKERS_UPDATE`).
    StickersChanged(Snowflake),
    /// A DM or group-DM call started, with who is ringing and connected.
    CallCreate(Box<CallCreate>),
    CallUpdate(CallUpdate),
    CallDelete(CallDelete),
    /// The voice server for the account's voice connection; for the voice
    /// layer, not the reducer. A `None` endpoint means it is being reallocated.
    VoiceServerUpdate(Box<VoiceServerUpdate>),
    /// The stream RTC server for the account's stream connection; for the
    /// media layer, not the reducer. A `None` endpoint means it is being
    /// reallocated.
    StreamCreate(Box<fastcord_model::StreamCreate>),
    StreamServerUpdate(Box<fastcord_model::StreamServerUpdate>),
    StreamUpdate(Box<fastcord_model::StreamUpdate>),
    StreamDelete(Box<fastcord_model::StreamDelete>),
}

impl Dispatch {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ready(_) => "READY",
            Self::ReadySupplemental(_) => "READY_SUPPLEMENTAL",
            Self::Resumed => "RESUMED",
            Self::MessageCreate(_) => "MESSAGE_CREATE",
            Self::MessageUpdate(_) => "MESSAGE_UPDATE",
            Self::MessageDelete(_) => "MESSAGE_DELETE",
            Self::MessageDeleteBulk(_) => "MESSAGE_DELETE_BULK",
            Self::MessageAck(_) => "MESSAGE_ACK",
            Self::TypingStart(_) => "TYPING_START",
            Self::PresenceUpdate(_) => "PRESENCE_UPDATE",
            Self::RelationshipAdd(_) => "RELATIONSHIP_ADD",
            Self::RelationshipUpdate(_) => "RELATIONSHIP_UPDATE",
            Self::RelationshipRemove(_) => "RELATIONSHIP_REMOVE",
            Self::UserSettingsUpdate(_) => "USER_SETTINGS_UPDATE",
            Self::UserGuildSettingsUpdate(_) => "USER_GUILD_SETTINGS_UPDATE",
            Self::UserFoldersUpdate(_) => "USER_SETTINGS_PROTO_UPDATE",
            Self::MessageReactionAdd(_) => "MESSAGE_REACTION_ADD",
            Self::MessageReactionRemove(_) => "MESSAGE_REACTION_REMOVE",
            Self::MessageReactionRemoveAll(_) => "MESSAGE_REACTION_REMOVE_ALL",
            Self::MessageReactionRemoveEmoji(_) => "MESSAGE_REACTION_REMOVE_EMOJI",
            Self::InteractionSuccess(_) => "INTERACTION_SUCCESS",
            Self::InteractionFailure(_) => "INTERACTION_FAILURE",
            Self::InteractionModalCreate(_) => "INTERACTION_MODAL_CREATE",
            Self::MessagePollVoteAdd(_) => "MESSAGE_POLL_VOTE_ADD",
            Self::MessagePollVoteRemove(_) => "MESSAGE_POLL_VOTE_REMOVE",
            Self::GuildCreate(_) => "GUILD_CREATE",
            Self::GuildUpdate(_) => "GUILD_UPDATE",
            Self::GuildEmojisUpdate(_) => "GUILD_EMOJIS_UPDATE",
            Self::GuildDelete(_) => "GUILD_DELETE",
            Self::ChannelCreate(_) => "CHANNEL_CREATE",
            Self::ChannelUpdate(_) => "CHANNEL_UPDATE",
            Self::ChannelDelete(_) => "CHANNEL_DELETE",
            Self::ThreadListSync(_) => "THREAD_LIST_SYNC",
            Self::ThreadMemberUpdate(_) => "THREAD_MEMBER_UPDATE",
            Self::ThreadMembersUpdate(_) => "THREAD_MEMBERS_UPDATE",
            Self::ChannelRecipientAdd(_) => "CHANNEL_RECIPIENT_ADD",
            Self::ChannelRecipientRemove(_) => "CHANNEL_RECIPIENT_REMOVE",
            Self::PassiveUpdate(_) => "PASSIVE_UPDATE_V2",
            Self::MemberListUpdate(_) => "GUILD_MEMBER_LIST_UPDATE",
            Self::GuildMemberAdd(_) => "GUILD_MEMBER_ADD",
            Self::GuildMemberUpdate(_) => "GUILD_MEMBER_UPDATE",
            Self::GuildMemberRemove(_) => "GUILD_MEMBER_REMOVE",
            Self::UserUpdate(_) => "USER_UPDATE",
            Self::GuildRoleCreate(_) => "GUILD_ROLE_CREATE",
            Self::GuildRoleUpdate(_) => "GUILD_ROLE_UPDATE",
            Self::GuildRoleDelete(_) => "GUILD_ROLE_DELETE",
            Self::VoiceStateUpdate(_) => "VOICE_STATE_UPDATE",
            Self::VoiceChannelSound(_) => "VOICE_CHANNEL_EFFECT_SEND",
            Self::EmbeddedActivity(_) => "EMBEDDED_ACTIVITY_UPDATE",
            Self::StageInstanceUpsert(_) => "STAGE_INSTANCE_UPDATE",
            Self::StageInstanceDelete(_) => "STAGE_INSTANCE_DELETE",
            Self::ScheduledEventUpsert(_) => "GUILD_SCHEDULED_EVENT_UPDATE",
            Self::ScheduledEventDelete(_) => "GUILD_SCHEDULED_EVENT_DELETE",
            Self::ScheduledEventUser { added: true, .. } => "GUILD_SCHEDULED_EVENT_USER_ADD",
            Self::ScheduledEventUser { .. } => "GUILD_SCHEDULED_EVENT_USER_REMOVE",
            Self::SoundboardChanged(_) => "GUILD_SOUNDBOARD_SOUNDS_UPDATE",
            Self::StickersChanged(_) => "GUILD_STICKERS_UPDATE",
            Self::AutoModeration(_) => "AUTO_MODERATION_RULE_UPDATE",
            Self::CallCreate(_) => "CALL_CREATE",
            Self::CallUpdate(_) => "CALL_UPDATE",
            Self::CallDelete(_) => "CALL_DELETE",
            Self::VoiceServerUpdate(_) => "VOICE_SERVER_UPDATE",
            Self::StreamCreate(_) => "STREAM_CREATE",
            Self::StreamServerUpdate(_) => "STREAM_SERVER_UPDATE",
            Self::StreamUpdate(_) => "STREAM_UPDATE",
            Self::StreamDelete(_) => "STREAM_DELETE",
        }
    }
}

// Debug names the event only: payloads carry message content, signed
// attachment URLs, and account data that must stay out of logs.
impl fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Dispatch::{}", self.name())
    }
}

impl fmt::Debug for Ready {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ready")
            .field("guilds", &self.guilds.len())
            .field("unavailable_guilds", &self.unavailable_guilds.len())
            .field("private_channels", &self.private_channels.len())
            .field("users", &self.users.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for GuildCreate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GuildCreate")
    }
}

/// What the Gateway task hands the reducer, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayEvent {
    State(ConnectionState),
    /// A dispatch with its sequence number. Delivered in sequence order; a
    /// sequence already delivered (replay overlap after Resume) is dropped
    /// before it gets here.
    Dispatch {
        sequence: u64,
        event: Dispatch,
    },
}
