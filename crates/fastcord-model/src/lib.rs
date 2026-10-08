//! Discord domain types shared by every fastcord crate.
//!
//! This crate has no runtime, GUI, or I/O dependencies.

mod channel;
mod guild;
mod message;
mod permissions;
mod snowflake;
mod user;
mod voice;
mod wire;

pub use channel::{Channel, ChannelKind};
pub use guild::{Guild, GuildMember, Role};
pub use message::{
    Attachment, Emoji, Message, MessageReference, MessageUpdate, REPLY_KIND, REPLY_PREVIEW_CHARS,
    Reaction, Referenced, ReplyPreview,
};
pub use permissions::{
    GuildScope, MemberScope, OverwriteKind, PermissionOverwrite, Permissions, channel_permissions,
    guild_permissions,
};
pub use snowflake::{DISCORD_EPOCH_MS, ParseSnowflakeError, Snowflake};
pub use user::User;
pub use voice::{VoiceServerUpdate, VoiceState, VoiceStateRequest, VoiceToken};
