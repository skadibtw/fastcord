//! Discord domain types shared by every fastcord crate.
//!
//! This crate has no runtime, GUI, or I/O dependencies.

mod channel;
mod embed;
mod guild;
pub mod markup;
mod message;
mod permissions;
mod snowflake;
mod user;
mod voice;
mod wire;

pub use channel::{Channel, ChannelKind};
pub use embed::{
    Embed, EmbedField, EmbedFooter, EmbedMedia, EmbedText, MAX_EMBED_FIELDS, MAX_EMBEDS,
};
pub use guild::{Guild, GuildEmoji, GuildMember, Role};
pub use message::{
    Attachment, Emoji, MAX_REACTION_EMOJI_NAME_BYTES, MAX_REACTIONS, Message, MessageReference,
    MessageUpdate, REPLY_KIND, REPLY_PREVIEW_CHARS, Reaction, ReactionChange, Referenced,
    ReplyPreview,
};
pub use permissions::{
    GuildScope, MemberScope, OverwriteKind, PermissionOverwrite, Permissions, channel_permissions,
    guild_permissions,
};
pub use snowflake::{DISCORD_EPOCH_MS, ParseSnowflakeError, Snowflake};
pub use user::User;
pub use voice::{
    StreamCreate, StreamCreateRequest, StreamDelete, StreamDeleteReason, StreamKey,
    StreamServerUpdate, StreamType, StreamUpdate, VoiceServerUpdate, VoiceState, VoiceStateRequest,
    VoiceToken,
};
