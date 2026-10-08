use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::wire::{double_option, lenient_nonce};
use crate::{Snowflake, User};

/// Message type of a reply (`REPLY`).
pub const REPLY_KIND: u8 = 19;

/// Characters of a replied-to message's text a [`ReplyPreview`] keeps: enough
/// for the one line a reply shows above itself, never the whole body.
pub const REPLY_PREVIEW_CHARS: usize = 200;

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

impl fmt::Debug for Attachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attachment")
            .field("id", &self.id)
            .field("size", &self.size)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
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

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// What Discord reported about the message a reply answers.
    #[serde(
        default,
        skip_serializing_if = "Referenced::is_unknown",
        serialize_with = "Referenced::serialize_field"
    )]
    pub referenced_message: Referenced,
    /// The client-chosen nonce echoed by Discord (up to 25 characters), when
    /// there is one. Only used to recognize a message this client sent.
    #[serde(
        default,
        deserialize_with = "lenient_nonce",
        skip_serializing_if = "Option::is_none"
    )]
    pub nonce: Option<String>,
}

/// The part of a replied-to message a reply's preview line shows: who wrote it
/// and the start of its text (at most [`REPLY_PREVIEW_CHARS`] characters). It
/// is presentation data, not a message body, so it is cut rather than refused.
#[derive(Clone, PartialEq, Eq)]
pub struct ReplyPreview {
    pub id: Snowflake,
    pub author: User,
    pub content: String,
}

impl ReplyPreview {
    pub fn of(message: &Message) -> Self {
        Self::new(message.id, message.author.clone(), &message.content)
    }

    pub fn new(id: Snowflake, author: User, content: &str) -> Self {
        Self {
            id,
            author,
            content: content.chars().take(REPLY_PREVIEW_CHARS).collect(),
        }
    }
}

/// The fields of a nested `referenced_message` a preview needs; everything else
/// (including its own nested references) is skipped while decoding.
#[derive(Deserialize, Serialize)]
struct WirePreview<'a> {
    id: Snowflake,
    author: std::borrow::Cow<'a, User>,
    #[serde(default)]
    content: std::borrow::Cow<'a, str>,
}

/// What Discord reported about the message a reply answers
/// (`referenced_message`, SPEC §5.2): absent means Discord did not look it up,
/// `null` means it was deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Referenced {
    /// The field was absent: the replied-to message's state is unknown.
    #[default]
    Unknown,
    /// The field was `null`: the replied-to message was deleted.
    Deleted,
    Message(Box<ReplyPreview>),
}

impl Referenced {
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// The wire form: `null` for a deleted message, a partial message object
    /// otherwise. [`Unknown`](Self::Unknown) is never serialized (the field is
    /// skipped).
    fn serialize_field<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unknown | Self::Deleted => serializer.serialize_none(),
            Self::Message(preview) => WirePreview {
                id: preview.id,
                author: std::borrow::Cow::Borrowed(&preview.author),
                content: std::borrow::Cow::Borrowed(&preview.content),
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Referenced {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(
            match Option::<WirePreview<'_>>::deserialize(deserializer)? {
                None => Self::Deleted,
                Some(wire) => Self::Message(Box::new(ReplyPreview::new(
                    wire.id,
                    wire.author.into_owned(),
                    &wire.content,
                ))),
            },
        )
    }
}

// The replied-to text never reaches a default logging path.
impl fmt::Debug for ReplyPreview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplyPreview")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// A MESSAGE_UPDATE payload. Discord may omit any field except the IDs;
/// an omitted field means "unchanged", never "cleared".
#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
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

impl MessageUpdate {
    /// Whether the update carries any field this client models. An update
    /// without one (an embed unfurl) can never change a message.
    pub fn has_fields(&self) -> bool {
        self.content.is_some()
            || self.edited_timestamp.is_some()
            || self.flags.is_some()
            || self.pinned.is_some()
            || self.attachments.is_some()
    }

    /// Whether [`Message::apply`] would change `message`. An update that only
    /// carries fields this client does not model (an embed unfurl) or repeats
    /// current values changes nothing, so consumers can skip it entirely.
    pub fn alters(&self, message: &Message) -> bool {
        self.content.as_ref().is_some_and(|v| *v != message.content)
            || self
                .edited_timestamp
                .as_ref()
                .is_some_and(|v| *v != message.edited_timestamp)
            || self.flags.is_some_and(|v| v != message.flags)
            || self.pinned.is_some_and(|v| v != message.pinned)
            || self
                .attachments
                .as_ref()
                .is_some_and(|v| *v != message.attachments)
    }
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

    /// The message this one replies to, when it is a reply within its own
    /// channel. Replies always answer a message of the same channel; any other
    /// reference (a crosspost's source, a forward) is not a reply target.
    pub fn replied_to(&self) -> Option<Snowflake> {
        if self.kind != REPLY_KIND {
            return None;
        }
        let reference = self.message_reference.as_ref()?;
        reference
            .channel_id
            .is_none_or(|channel| channel == self.channel_id)
            .then_some(reference.message_id)
            .flatten()
    }
}

// Bodies and signed attachment URLs never reach a default logging path.
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("id", &self.id)
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for MessageUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageUpdate")
            .field("id", &self.id)
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}
