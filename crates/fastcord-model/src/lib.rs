//! Discord domain types shared by every fastcord crate.
//!
//! This crate has no runtime, GUI, or I/O dependencies.

mod channel;
mod guild;
mod message;
mod permissions;
mod snowflake;
mod user;
mod wire;

pub use channel::{Channel, ChannelKind};
pub use guild::Role;
pub use message::{Attachment, Emoji, Message, MessageReference, MessageUpdate, Reaction};
pub use permissions::{
    GuildScope, MemberScope, OverwriteKind, PermissionOverwrite, Permissions, channel_permissions,
    guild_permissions,
};
pub use snowflake::{ParseSnowflakeError, Snowflake};
pub use user::User;
