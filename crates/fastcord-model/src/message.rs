use crate::wire::double_option;
use crate::{Snowflake, User};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    pub id: Snowflake,
    pub filename: String,
    pub size: u64,
    /// Signed CDN URL. Never log it (SPEC §2.3).
    pub url: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// Unicode emoji have only `name`; custom emoji have `id` and usually `name`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Emoji {
    #[serde(default)]
    pub id: Option<Snowflake>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub animated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reaction {
    pub count: u32,
    #[serde(default)]
    pub me: bool,
    pub emoji: Emoji,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MessageReference {
    #[serde(default)]
    pub message_id: Option<Snowflake>,
    #[serde(default)]
    pub channel_id: Option<Snowflake>,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub id: Snowflake,
    pub channel_id: Snowflake,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    pub author: User,
    pub content: String,
    pub timestamp: String,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: u8,
    #[serde(default)]
    pub flags: u64,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub message_reference: Option<MessageReference>,
}

/// A MESSAGE_UPDATE payload. Discord may omit any field except the IDs;
/// an omitted field means "unchanged", never "cleared".
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct MessageUpdate {
    pub id: Snowflake,
    pub channel_id: Snowflake,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub edited_timestamp: Option<Option<String>>,
    #[serde(default)]
    pub flags: Option<u64>,
    #[serde(default)]
    pub pinned: Option<bool>,
    #[serde(default)]
    pub attachments: Option<Vec<Attachment>>,
}

impl Message {
    /// Merge a partial update. Fields absent from the update are kept.
    pub fn apply(&mut self, update: MessageUpdate) {
        debug_assert_eq!(self.id, update.id);
        if let Some(content) = update.content {
            self.content = content;
        }
        if let Some(edited) = update.edited_timestamp {
            self.edited_timestamp = edited;
        }
        if let Some(flags) = update.flags {
            self.flags = flags;
        }
        if let Some(pinned) = update.pinned {
            self.pinned = pinned;
        }
        if let Some(attachments) = update.attachments {
            self.attachments = attachments;
        }
    }
}
