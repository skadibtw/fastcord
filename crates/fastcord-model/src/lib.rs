//! Discord domain types shared by every fastcord crate.
//!
//! This crate has no runtime, GUI, or I/O dependencies.

mod call;
mod channel;
mod component;
mod embed;
mod guild;
pub mod markup;
mod message;
mod permissions;
mod poll;
mod presence;
mod relationship;
mod scheduled_event;
mod snowflake;
mod user;
mod voice;
mod wire;

pub use call::{CallCreate, CallDelete, CallUpdate};
pub use channel::{
    Channel, ChannelKind, FLAG_PINNED, FLAG_REQUIRE_TAG, ForumTag, MAX_APPLIED_TAGS,
    MAX_FORUM_TAGS, ThreadMember, ThreadMetadata,
};
pub use component::{
    BUTTON_LINK, BUTTON_PREMIUM, Button, Component, EPHEMERAL_FLAG, MAX_COMPONENT_DEPTH,
    MAX_COMPONENTS, MAX_SELECT_OPTIONS, Select, SelectOption, compact_components,
    components_heap_bytes, parse_components,
};
pub use embed::{
    Embed, EmbedField, EmbedFooter, EmbedMedia, EmbedText, MAX_EMBED_FIELDS, MAX_EMBEDS,
};
pub use guild::{
    Guild, GuildEmoji, GuildMember, GuildSettings, MAX_DESCRIPTION_BYTES, MAX_FEATURE_BYTES,
    MAX_FEATURES, MAX_GUILD_VOICE_STATES, Role, creator_id, null_default_string,
};
pub use message::{
    Attachment, CROSSPOSTED_FLAG, Emoji, FORWARD_REFERENCE, IS_CROSSPOST_FLAG,
    MAX_MESSAGE_SNAPSHOTS, MAX_REACTION_EMOJI_NAME_BYTES, MAX_REACTIONS, MAX_STICKER_ITEMS,
    MAX_STICKER_NAME_CHARS, MAX_WAVEFORM_CHARS, Message, MessageCall, MessageReference,
    MessageSnapshot, MessageUpdate, REPLY_KIND, REPLY_PREVIEW_CHARS, Reaction, ReactionChange,
    Referenced, ReplyPreview, SUPPRESS_EMBEDS_FLAG, SnapshotMessage, StickerItem,
    VOICE_MESSAGE_FLAG,
};
pub use permissions::{
    GuildScope, MemberScope, OverwriteKind, PERMISSION_GROUPS, PermissionInfo, PermissionOverwrite,
    Permissions, channel_permissions, guild_permissions, scope as permission_scope,
};
pub use poll::{
    AnswerCount, MAX_POLL_ANSWER_CHARS, MAX_POLL_ANSWERS, MAX_POLL_QUESTION_CHARS, Poll,
    PollAnswer, PollMedia, PollResults, parse_unix_ms,
};
pub use presence::{MAX_CUSTOM_STATUS_CHARS, Presence, Status};
pub use relationship::{Relationship, RelationshipKind};
pub use scheduled_event::{
    EventKind, EventStatus, MAX_DESCRIPTION_CHARS, MAX_LOCATION_CHARS, MAX_NAME_CHARS,
    ScheduledEvent, ScheduledEventKey, ScheduledEventUser,
};
pub use snowflake::{DISCORD_EPOCH_MS, ParseSnowflakeError, Snowflake};
pub use user::User;
pub use voice::{
    ActivityUpdate, MAX_ACTIVITY_NAME_CHARS, StageInstance, StreamCreate, StreamCreateRequest,
    StreamDelete, StreamDeleteReason, StreamKey, StreamServerUpdate, StreamType, StreamUpdate,
    VoiceServerUpdate, VoiceState, VoiceStateRequest, VoiceToken,
};
