//! Events the Gateway task delivers to its single consumer (the reducer).

use std::fmt;
use std::time::Duration;

use fastcord_model::{
    Channel, Guild, GuildMember, Message, MessageUpdate, Role, Snowflake, User, VoiceState,
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
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct MessageDelete {
    pub id: Snowflake,
    pub channel_id: Snowflake,
    pub guild_id: Option<Snowflake>,
}

/// GUILD_CREATE: a joined or newly available guild, or one that is now known
/// only by ID because of an outage.
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
    GuildCreate(Box<GuildCreate>),
    GuildUpdate(Box<GuildUpdate>),
    GuildDelete(GuildDelete),
    ChannelCreate(Box<Channel>),
    ChannelUpdate(Box<Channel>),
    ChannelDelete(Box<Channel>),
    PassiveUpdate(Box<PassiveUpdate>),
    MemberListUpdate(Box<MemberListUpdate>),
    GuildMemberAdd(Box<GuildMemberEvent>),
    GuildMemberUpdate(Box<GuildMemberEvent>),
    GuildMemberRemove(GuildMemberRemove),
    GuildRoleCreate(Box<GuildRoleEvent>),
    GuildRoleUpdate(Box<GuildRoleEvent>),
    GuildRoleDelete(GuildRoleDelete),
    VoiceStateUpdate(Box<VoiceStateUpdate>),
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
            Self::GuildCreate(_) => "GUILD_CREATE",
            Self::GuildUpdate(_) => "GUILD_UPDATE",
            Self::GuildDelete(_) => "GUILD_DELETE",
            Self::ChannelCreate(_) => "CHANNEL_CREATE",
            Self::ChannelUpdate(_) => "CHANNEL_UPDATE",
            Self::ChannelDelete(_) => "CHANNEL_DELETE",
            Self::PassiveUpdate(_) => "PASSIVE_UPDATE_V2",
            Self::MemberListUpdate(_) => "GUILD_MEMBER_LIST_UPDATE",
            Self::GuildMemberAdd(_) => "GUILD_MEMBER_ADD",
            Self::GuildMemberUpdate(_) => "GUILD_MEMBER_UPDATE",
            Self::GuildMemberRemove(_) => "GUILD_MEMBER_REMOVE",
            Self::GuildRoleCreate(_) => "GUILD_ROLE_CREATE",
            Self::GuildRoleUpdate(_) => "GUILD_ROLE_UPDATE",
            Self::GuildRoleDelete(_) => "GUILD_ROLE_DELETE",
            Self::VoiceStateUpdate(_) => "VOICE_STATE_UPDATE",
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
