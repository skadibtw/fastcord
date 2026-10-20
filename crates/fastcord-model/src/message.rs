use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Embed;
use crate::Poll;
use crate::embed::lenient_embeds;
use crate::wire::{double_option, lenient_nonce};
use crate::{Snowflake, User};

/// Message type of a reply (`REPLY`).
pub const REPLY_KIND: u8 = 19;

/// Characters of a replied-to message's text a [`ReplyPreview`] keeps: enough
/// for the one line a reply shows above itself, never the whole body.
pub const REPLY_PREVIEW_CHARS: usize = 200;

/// Most distinct reaction emoji one message keeps. Discord allows 20 normal and
/// 5 burst emoji per message; the headroom covers their union.
pub const MAX_REACTIONS: usize = 32;

/// Longest reaction emoji `name` in bytes accepted from an event or put in a
/// request path: far above any Unicode sequence or custom emoji name.
pub const MAX_REACTION_EMOJI_NAME_BYTES: usize = 64;

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// Voice message length in seconds, as the sender reported it.
    #[serde(default, deserialize_with = "bounded_duration")]
    pub duration_secs: Option<f32>,
    /// Voice message loudness envelope: base64 of at most [`MAX_WAVEFORM_CHARS`] characters.
    /// Dropped when longer, so a hostile value cannot grow the message cache.
    #[serde(default, deserialize_with = "bounded_waveform")]
    pub waveform: Option<String>,
}

/// Longest base64 `waveform` kept (Discord sends 256 samples, about 344 characters).
pub const MAX_WAVEFORM_CHARS: usize = 512;

/// `IS_VOICE_MESSAGE` in a message's `flags`.
pub const VOICE_MESSAGE_FLAG: u64 = 1 << 13;

/// Never NaN or infinite, so comparing attachments stays reflexive (`Eq` below).
fn bounded_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
    let value = Option::<f64>::deserialize(d)?;
    Ok(value
        .filter(|secs| secs.is_finite() && (0.0..=86_400.0).contains(secs))
        .map(|secs| secs as f32))
}

// `duration_secs` is finite whenever it comes from the wire (see `bounded_duration`).
impl Eq for Attachment {}

fn bounded_waveform<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(d)?;
    Ok(value.filter(|text| text.len() <= MAX_WAVEFORM_CHARS))
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

impl Emoji {
    /// Whether both name the same reaction target. A custom emoji is identified
    /// by its ID alone, so a rename (or the missing name of a deleted emoji)
    /// never splits a reaction; a Unicode emoji (no ID) by its exact, non-empty
    /// name. An emoji with neither is no reaction's target, not even its own.
    pub fn same_reaction(&self, other: &Emoji) -> bool {
        match (self.id, other.id) {
            (Some(ours), Some(theirs)) => ours == theirs,
            (None, None) => match (&self.name, &other.name) {
                (Some(ours), Some(theirs)) => !ours.is_empty() && ours == theirs,
                _ => false,
            },
            _ => false,
        }
    }
}

/// One emoji's reactions on a message, kept as an aggregate: how many users
/// reacted with it and whether the current user's own reaction is among them.
/// The reacting users themselves are never stored (SPEC §9).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reaction {
    /// Every reaction with this emoji, normal and burst together.
    pub count: u32,
    /// The current user reacted normally with this emoji. A burst reaction
    /// never sets it.
    #[serde(default)]
    pub me: bool,
    pub emoji: Emoji,
}

/// One change to a message's reaction aggregate, as the Gateway reports it. It
/// borrows the emoji, so applying an event allocates nothing.
///
/// `by_me` is whether the reacting user is the current account and `burst`
/// whether the reaction is a burst (super) reaction. Both kinds count toward
/// [`Reaction::count`]; only the account's own normal reaction sets
/// [`Reaction::me`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReactionChange<'a> {
    /// A user reacted.
    Add {
        emoji: &'a Emoji,
        by_me: bool,
        burst: bool,
    },
    /// A user took a reaction back.
    Remove {
        emoji: &'a Emoji,
        by_me: bool,
        burst: bool,
    },
    /// Every reaction with this emoji was removed.
    RemoveEmoji(&'a Emoji),
    /// Every reaction was removed.
    RemoveAll,
}

/// What a [`ReactionChange`] does to a message's reactions, decided without
/// changing anything, so that [`Message::alters_reactions`] and
/// [`Message::apply_reaction`] cannot disagree.
enum ReactionEdit<'a> {
    Keep,
    Push { emoji: &'a Emoji, me: bool },
    Update { at: usize, count: u32, me: bool },
    Remove { at: usize },
    Clear,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MessageReference {
    /// Discord's reference `type`: 0 reply/crosspost, 1 forward.
    #[serde(rename = "type", default, skip_serializing_if = "is_default_reference")]
    pub kind: u8,
    #[serde(default)]
    pub message_id: Option<Snowflake>,
    #[serde(default)]
    pub channel_id: Option<Snowflake>,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
}

fn is_default_reference(kind: &u8) -> bool {
    *kind == 0
}

/// `message_reference.type` of a forward.
pub const FORWARD_REFERENCE: u8 = 1;
/// Most snapshots one message keeps (a forward carries exactly one).
pub const MAX_MESSAGE_SNAPSHOTS: usize = 1;

/// The frozen copy of a message a forward carries (`message_snapshots[].message`).
/// It is Discord's own copy at the moment of forwarding: later edits or
/// deletion of the original never reach it, and it names neither the source
/// channel nor its author.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MessageSnapshot {
    pub message: SnapshotMessage,
}

#[derive(Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SnapshotMessage {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: u8,
    #[serde(default)]
    pub flags: u64,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default, deserialize_with = "lenient_embeds")]
    pub embeds: Vec<Embed>,
    #[serde(default, deserialize_with = "lenient_stickers")]
    pub sticker_items: Vec<StickerItem>,
    #[serde(default)]
    pub mentions: Vec<User>,
    #[serde(default)]
    pub mention_roles: Vec<Snowflake>,
}

impl fmt::Debug for SnapshotMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotMessage")
            .field("content_chars", &self.content.chars().count())
            .field("attachments", &self.attachments.len())
            .finish_non_exhaustive()
    }
}

impl SnapshotMessage {
    /// This snapshot dressed as a message of `parent` (same ID, author and
    /// channel) so the ordinary body, mention and embed renderers can draw it.
    pub fn as_message(&self, parent: &Message) -> Message {
        Message {
            content: self.content.clone(),
            timestamp: self.timestamp.clone(),
            edited_timestamp: self.edited_timestamp.clone(),
            kind: 0,
            flags: self.flags,
            attachments: self.attachments.clone(),
            embeds: self.embeds.clone(),
            sticker_items: self.sticker_items.clone(),
            mentions: self.mentions.clone(),
            mention_roles: self.mention_roles.clone(),
            reactions: Vec::new(),
            message_reference: None,
            message_snapshots: Vec::new(),
            poll: None,
            referenced_message: Referenced::Unknown,
            nonce: None,
            call: None,
            mention_everyone: false,
            pinned: false,
            ..parent.clone()
        }
    }
}

fn lenient_snapshots<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<MessageSnapshot>, D::Error> {
    // One malformed snapshot must not discard the whole message.
    let values = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .take(MAX_MESSAGE_SNAPSHOTS)
        .collect())
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
    /// Stickers sent with the message (at most [`MAX_STICKER_ITEMS`]).
    #[serde(
        default,
        deserialize_with = "lenient_stickers",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub sticker_items: Vec<StickerItem>,
    /// The frozen content of a forwarded message (see [`Message::forwarded`]).
    #[serde(
        default,
        deserialize_with = "lenient_snapshots",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub message_snapshots: Vec<MessageSnapshot>,
    /// The poll this message carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll: Option<Box<Poll>>,
    /// Users this message mentions (their names resolve `<@id>`).
    #[serde(default)]
    pub mentions: Vec<User>,
    #[serde(default)]
    pub mention_roles: Vec<Snowflake>,
    #[serde(default)]
    pub mention_everyone: bool,
    #[serde(default, deserialize_with = "lenient_embeds")]
    pub embeds: Vec<Embed>,
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
    /// How a call message (type 3) ended; absent on every other type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<MessageCall>,
}

/// Longest sticker name kept; Discord allows 30 characters.
pub const MAX_STICKER_NAME_CHARS: usize = 64;
/// Most stickers one message keeps (Discord sends at most 3).
pub const MAX_STICKER_ITEMS: usize = 3;

/// A sticker a message carries (`sticker_items`). `format_type` is Discord's:
/// 1 PNG, 2 APNG, 3 Lottie, 4 GIF; anything else is unknown and shown as text.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StickerItem {
    pub id: Snowflake,
    #[serde(default, deserialize_with = "lenient_sticker_name")]
    pub name: String,
    #[serde(default)]
    pub format_type: u8,
}

fn lenient_sticker_name<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let name = Option::<String>::deserialize(d)?.unwrap_or_default();
    Ok(name.chars().take(MAX_STICKER_NAME_CHARS).collect())
}

fn lenient_stickers<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<StickerItem>, D::Error> {
    let mut items = Vec::<StickerItem>::deserialize(d)?;
    items.truncate(MAX_STICKER_ITEMS);
    Ok(items)
}

/// The `call` object of a call message: who joined and when it ended
/// (`None` while it is still going).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MessageCall {
    #[serde(default)]
    pub participants: Vec<Snowflake>,
    #[serde(default)]
    pub ended_timestamp: Option<String>,
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
    #[serde(default)]
    pub mentions: Option<Vec<User>>,
    #[serde(default)]
    pub mention_roles: Option<Vec<Snowflake>>,
    #[serde(default)]
    pub mention_everyone: Option<bool>,
    #[serde(default, deserialize_with = "lenient_some_embeds")]
    pub embeds: Option<Vec<Embed>>,
    /// A poll's new state (votes, or its end).
    #[serde(default)]
    pub poll: Option<Box<Poll>>,
}

fn lenient_some_embeds<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<Embed>>, D::Error> {
    lenient_embeds(d).map(Some)
}

impl MessageUpdate {
    /// An update that carries only a poll's new state.
    pub fn poll_only(id: Snowflake, channel_id: Snowflake, poll: Poll) -> Self {
        Self {
            id,
            channel_id,
            content: None,
            edited_timestamp: None,
            flags: None,
            pinned: None,
            attachments: None,
            mentions: None,
            mention_roles: None,
            mention_everyone: None,
            embeds: None,
            poll: Some(Box::new(poll)),
        }
    }

    /// Whether the update carries any field this client models. An update
    /// without one (an embed unfurl) can never change a message.
    pub fn has_fields(&self) -> bool {
        self.content.is_some()
            || self.edited_timestamp.is_some()
            || self.flags.is_some()
            || self.pinned.is_some()
            || self.attachments.is_some()
            || self.mentions.is_some()
            || self.mention_roles.is_some()
            || self.mention_everyone.is_some()
            // An empty list is an unfurl that shows nothing or a removal; the
            // latter still applies to a cached message through `alters`.
            || self.embeds.as_ref().is_some_and(|e| !e.is_empty())
            || self.poll.is_some()
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
            || self
                .mentions
                .as_ref()
                .is_some_and(|v| *v != message.mentions)
            || self
                .mention_roles
                .as_ref()
                .is_some_and(|v| *v != message.mention_roles)
            || self
                .mention_everyone
                .is_some_and(|v| v != message.mention_everyone)
            || self.embeds.as_ref().is_some_and(|v| *v != message.embeds)
            || self.poll.as_deref().is_some_and(|newer| {
                message
                    .poll
                    .as_deref()
                    .is_some_and(|held| !(held.finalized() && !newer.finalized()) && held != newer)
            })
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
        if let Some(mentions) = update.mentions {
            self.mentions = mentions;
        }
        if let Some(roles) = update.mention_roles {
            self.mention_roles = roles;
        }
        if let Some(everyone) = update.mention_everyone {
            self.mention_everyone = everyone;
        }
        if let Some(embeds) = update.embeds {
            self.embeds = embeds;
        }
        if let (Some(newer), Some(held)) = (update.poll, self.poll.as_mut()) {
            held.replace(*newer);
        }
    }

    /// The snapshot of a forwarded message: only a type-1 reference with a
    /// snapshot counts, and the original is never looked up.
    pub fn forwarded(&self) -> Option<&SnapshotMessage> {
        (self.message_reference.as_ref()?.kind == FORWARD_REFERENCE)
            .then(|| self.message_snapshots.first().map(|s| &s.message))
            .flatten()
    }

    /// Whether Discord marked this as a voice message.
    pub fn is_voice_message(&self) -> bool {
        self.flags & VOICE_MESSAGE_FLAG != 0
    }

    /// An attachment of this message or of the snapshot it forwards.
    pub fn attachment(&self, id: Snowflake) -> Option<&Attachment> {
        let own = self.attachments.iter();
        let forwarded = self.forwarded().into_iter().flat_map(|s| &s.attachments);
        own.chain(forwarded).find(|attachment| attachment.id == id)
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

    /// Whether [`apply_reaction`](Self::apply_reaction) would change this
    /// message. Pure, so a caller can skip the copy-on-write for an event that
    /// repeats what the message already shows: the Gateway's echo of a reaction
    /// a REST answer applied first, and the other way around.
    pub fn alters_reactions(&self, change: ReactionChange<'_>) -> bool {
        !matches!(self.reaction_edit(change), ReactionEdit::Keep)
    }

    /// Applies one change to the reaction aggregate and reports whether the
    /// message changed. Entries are found with [`Emoji::same_reaction`] and
    /// keep their first-seen order.
    ///
    /// * Add: the account's own normal reaction sets `me` and counts once;
    ///   repeating it changes nothing. A burst reaction, or one by anyone else,
    ///   only counts. A new emoji gets an entry unless [`MAX_REACTIONS`] are
    ///   already held.
    /// * Remove: the account's own normal reaction clears `me` and lowers the
    ///   count (nothing happens when `me` is not set); any other removal only
    ///   lowers the count. An entry whose count reaches zero is dropped, and an
    ///   emoji that has no entry changes nothing.
    /// * Removing an emoji drops its entry; removing all drops every entry.
    ///
    /// Counts saturate. A structural change compacts the stored vector, since
    /// the message store accounts for allocations by capacity.
    pub fn apply_reaction(&mut self, change: ReactionChange<'_>) -> bool {
        match self.reaction_edit(change) {
            ReactionEdit::Keep => return false,
            ReactionEdit::Update { at, count, me } => {
                let reaction = &mut self.reactions[at];
                reaction.count = count;
                reaction.me = me;
                return true;
            }
            ReactionEdit::Push { emoji, me } => self.reactions.push(Reaction {
                count: 1,
                me,
                emoji: emoji.clone(),
            }),
            ReactionEdit::Remove { at } => {
                self.reactions.remove(at);
            }
            ReactionEdit::Clear => self.reactions.clear(),
        }
        self.reactions.shrink_to_fit();
        true
    }

    fn reaction_edit<'a>(&self, change: ReactionChange<'a>) -> ReactionEdit<'a> {
        let find = |emoji: &Emoji| {
            self.reactions
                .iter()
                .position(|held| held.emoji.same_reaction(emoji))
        };
        match change {
            ReactionChange::Add {
                emoji,
                by_me,
                burst,
            } => {
                let own = by_me && !burst;
                let Some(at) = find(emoji) else {
                    return if self.reactions.len() < MAX_REACTIONS {
                        ReactionEdit::Push { emoji, me: own }
                    } else {
                        ReactionEdit::Keep
                    };
                };
                let held = &self.reactions[at];
                let (count, me) = (held.count.saturating_add(1), held.me || own);
                if (own && held.me) || (count, me) == (held.count, held.me) {
                    ReactionEdit::Keep
                } else {
                    ReactionEdit::Update { at, count, me }
                }
            }
            ReactionChange::Remove {
                emoji,
                by_me,
                burst,
            } => {
                let own = by_me && !burst;
                let Some(at) = find(emoji) else {
                    return ReactionEdit::Keep;
                };
                let held = &self.reactions[at];
                if own && !held.me {
                    return ReactionEdit::Keep;
                }
                match held.count.saturating_sub(1) {
                    0 => ReactionEdit::Remove { at },
                    count => ReactionEdit::Update {
                        at,
                        count,
                        me: held.me && !own,
                    },
                }
            }
            ReactionChange::RemoveEmoji(emoji) => {
                find(emoji).map_or(ReactionEdit::Keep, |at| ReactionEdit::Remove { at })
            }
            ReactionChange::RemoveAll if self.reactions.is_empty() => ReactionEdit::Keep,
            ReactionChange::RemoveAll => ReactionEdit::Clear,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticker_items_are_bounded_and_tolerate_missing_fields() {
        let message: Message = serde_json::from_value(serde_json::json!({
            "id": "1", "channel_id": "2", "content": "",
            "timestamp": "2026-10-07T12:00:00.000000+00:00",
            "author": {"id": "3", "username": "a"},
            "sticker_items": [
                {"id": "10", "name": "ok", "format_type": 4},
                {"id": "11", "name": null},
                {"id": "12", "name": "x".repeat(500), "format_type": 1},
                {"id": "13", "name": "over", "format_type": 1}
            ]
        }))
        .unwrap();
        assert_eq!(message.sticker_items.len(), MAX_STICKER_ITEMS);
        assert_eq!(message.sticker_items[0].format_type, 4);
        assert_eq!(message.sticker_items[1].name, "");
        assert_eq!(message.sticker_items[1].format_type, 0);
        assert_eq!(
            message.sticker_items[2].name.chars().count(),
            MAX_STICKER_NAME_CHARS
        );
    }

    fn forward_json(extra: serde_json::Value) -> Message {
        let mut value = serde_json::json!({
            "id": "1", "channel_id": "2", "content": "",
            "timestamp": "2026-10-07T12:00:00.000000+00:00",
            "author": {"id": "3", "username": "a"},
            "message_reference": {"type": 1, "message_id": "10", "channel_id": "11", "guild_id": "12"},
            "message_snapshots": [{"message": {
                "content": "frozen", "timestamp": "2026-10-01T00:00:00.000000+00:00",
                "type": 0, "flags": 0,
                "attachments": [{"id": "20", "filename": "a.png", "size": 1, "url": "https://cdn.example/a.png?ex=SECRET"}],
                "mentions": [{"id": "5", "username": "m"}]
            }}]
        });
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_forward_decodes_its_snapshot_and_a_plain_reference_is_not_one() {
        let message = forward_json(serde_json::json!({}));
        let snapshot = message.forwarded().expect("forward");
        assert_eq!(snapshot.content, "frozen");
        assert_eq!(snapshot.attachments.len(), 1);
        assert_eq!(message.replied_to(), None, "a forward is never a reply");
        assert!(message.attachment(Snowflake(20)).is_some());
        assert!(message.attachment(Snowflake(21)).is_none());
        let dressed = snapshot.as_message(&message);
        assert_eq!(
            (dressed.id, dressed.content.as_str()),
            (message.id, "frozen")
        );
        assert!(dressed.message_snapshots.is_empty() && dressed.message_reference.is_none());
        // The same snapshot under a reply reference (type 0) is not shown.
        let reply = forward_json(serde_json::json!({
            "message_reference": {"type": 0, "message_id": "10"}
        }));
        assert!(reply.forwarded().is_none());
        assert!(!format!("{snapshot:?}").contains("SECRET"));
    }

    #[test]
    fn snapshots_are_bounded_and_one_malformed_snapshot_does_not_drop_the_message() {
        let message = forward_json(serde_json::json!({
            "message_snapshots": [
                {"message": "garbage"},
                {"message": {"content": "second"}},
                {"message": {"content": "third"}}
            ]
        }));
        assert_eq!(message.message_snapshots.len(), MAX_MESSAGE_SNAPSHOTS);
        assert_eq!(message.forwarded().unwrap().content, "second");
        let none = forward_json(serde_json::json!({"message_snapshots": []}));
        assert!(none.forwarded().is_none());
    }

    fn unicode(name: &str) -> Emoji {
        Emoji {
            id: None,
            name: Some(name.to_owned()),
            animated: false,
        }
    }

    fn custom(id: u64, name: Option<&str>) -> Emoji {
        Emoji {
            id: Some(Snowflake(id)),
            name: name.map(str::to_owned),
            animated: false,
        }
    }

    fn reacted(reactions: &[(&Emoji, u32, bool)]) -> Message {
        Message {
            sticker_items: Vec::new(),
            message_snapshots: Vec::new(),
            poll: None,
            id: Snowflake(1),
            channel_id: Snowflake(2),
            guild_id: None,
            author: User {
                id: Snowflake(3),
                username: "author".to_owned(),
                global_name: None,
                avatar: None,
                bot: false,
            },
            content: "text".to_owned(),
            timestamp: "2026-10-07T12:00:00.000000+00:00".to_owned(),
            edited_timestamp: None,
            kind: 0,
            flags: 0,
            pinned: false,
            attachments: Vec::new(),
            reactions: reactions
                .iter()
                .map(|&(emoji, count, me)| Reaction {
                    count,
                    me,
                    emoji: emoji.clone(),
                })
                .collect(),
            mentions: Vec::new(),
            mention_roles: Vec::new(),
            mention_everyone: false,
            embeds: Vec::new(),
            message_reference: None,
            referenced_message: Referenced::Unknown,
            nonce: None,
            call: None,
        }
    }

    /// `(who, count, me)` per entry, in order; `who` is `#id` for a custom
    /// emoji and the name for a Unicode one.
    fn shown(message: &Message) -> Vec<(String, u32, bool)> {
        message
            .reactions
            .iter()
            .map(|reaction| {
                let who = match (reaction.emoji.id, &reaction.emoji.name) {
                    (Some(id), _) => format!("#{id}"),
                    (None, name) => name.clone().unwrap_or_default(),
                };
                (who, reaction.count, reaction.me)
            })
            .collect()
    }

    fn expected(entries: &[(&str, u32, bool)]) -> Vec<(String, u32, bool)> {
        entries
            .iter()
            .map(|&(who, count, me)| (who.to_owned(), count, me))
            .collect()
    }

    fn add(emoji: &Emoji, by_me: bool) -> ReactionChange<'_> {
        ReactionChange::Add {
            emoji,
            by_me,
            burst: false,
        }
    }

    fn burst_add(emoji: &Emoji, by_me: bool) -> ReactionChange<'_> {
        ReactionChange::Add {
            emoji,
            by_me,
            burst: true,
        }
    }

    fn remove(emoji: &Emoji, by_me: bool) -> ReactionChange<'_> {
        ReactionChange::Remove {
            emoji,
            by_me,
            burst: false,
        }
    }

    fn burst_remove(emoji: &Emoji, by_me: bool) -> ReactionChange<'_> {
        ReactionChange::Remove {
            emoji,
            by_me,
            burst: true,
        }
    }

    const UP: &str = "\u{1f44d}";
    const DOWN: &str = "\u{1f44e}";

    #[test]
    fn unicode_emoji_are_the_same_reaction_by_exact_non_empty_name() {
        let up = unicode(UP);
        let animated = Emoji {
            animated: true,
            ..up.clone()
        };
        assert!(up.same_reaction(&up.clone()));
        assert!(
            up.same_reaction(&animated),
            "the animated flag is no identity"
        );
        assert!(!up.same_reaction(&unicode(DOWN)));
        // A variation selector or a skin tone is a different emoji.
        assert!(!unicode("\u{2764}").same_reaction(&unicode("\u{2764}\u{fe0f}")));
        assert!(!up.same_reaction(&unicode("\u{1f44d}\u{1f3fd}")));
        assert!(!unicode("a").same_reaction(&unicode("A")));
        // Without a name there is no identity, not even equal to itself.
        for nameless in [
            unicode(""),
            Emoji {
                id: None,
                name: None,
                animated: false,
            },
        ] {
            assert!(!nameless.same_reaction(&nameless.clone()));
            assert!(!nameless.same_reaction(&up));
            assert!(!up.same_reaction(&nameless));
        }
    }

    #[test]
    fn custom_emoji_are_the_same_reaction_by_id_alone() {
        let wave = custom(777, Some("wave"));
        assert!(
            wave.same_reaction(&custom(777, Some("renamed"))),
            "a rename does not split a reaction"
        );
        assert!(
            wave.same_reaction(&custom(777, None)),
            "nor does the missing name of a deleted emoji"
        );
        assert!(custom(777, None).same_reaction(&custom(777, None)));
        assert!(wave.same_reaction(&Emoji {
            animated: true,
            ..wave.clone()
        }));
        assert!(
            !wave.same_reaction(&custom(778, Some("wave"))),
            "another emoji that happens to share the name"
        );
    }

    #[test]
    fn unicode_and_custom_emoji_never_share_a_reaction() {
        assert!(!unicode("wave").same_reaction(&custom(777, Some("wave"))));
        assert!(!custom(777, Some("wave")).same_reaction(&unicode("wave")));
        assert!(!custom(777, None).same_reaction(&unicode("")));

        let mut message = reacted(&[]);
        assert!(message.apply_reaction(add(&unicode("wave"), false)));
        assert!(message.apply_reaction(add(&custom(777, Some("wave")), false)));
        assert_eq!(
            shown(&message),
            expected(&[("wave", 1, false), ("#777", 1, false)])
        );
    }

    #[test]
    fn adding_gives_each_emoji_an_entry_and_counts_every_reaction() {
        let (up, down) = (unicode(UP), unicode(DOWN));
        let mut message = reacted(&[]);
        assert!(message.alters_reactions(add(&up, false)));
        assert!(message.apply_reaction(add(&up, false)));
        assert_eq!(shown(&message), expected(&[(UP, 1, false)]));
        // Another user with the same emoji only counts.
        assert!(message.apply_reaction(add(&up, false)));
        assert_eq!(shown(&message), expected(&[(UP, 2, false)]));
        // The account itself counts and is marked.
        assert!(message.apply_reaction(add(&up, true)));
        assert_eq!(shown(&message), expected(&[(UP, 3, true)]));
        // A new emoji by the account starts marked, after the first-seen one.
        assert!(message.apply_reaction(add(&down, true)));
        assert_eq!(shown(&message), expected(&[(UP, 3, true), (DOWN, 1, true)]));
        // Someone else's reaction never clears or sets the account's.
        assert!(message.apply_reaction(add(&down, false)));
        assert_eq!(shown(&message), expected(&[(UP, 3, true), (DOWN, 2, true)]));
    }

    #[test]
    fn the_accounts_own_reaction_applies_once_whichever_of_rest_and_gateway_comes_first() {
        let up = unicode(UP);
        let mut message = reacted(&[(&up, 1, false)]);
        // Added: the REST answer and the Gateway echo are the same change.
        assert!(message.alters_reactions(add(&up, true)));
        assert!(message.apply_reaction(add(&up, true)));
        let once = message.clone();
        assert!(!message.alters_reactions(add(&up, true)));
        assert!(!message.apply_reaction(add(&up, true)));
        assert_eq!(message, once);
        assert_eq!(shown(&message), expected(&[(UP, 2, true)]));
        // Removed: likewise, and the other user's reaction stays.
        assert!(message.alters_reactions(remove(&up, true)));
        assert!(message.apply_reaction(remove(&up, true)));
        let once = message.clone();
        assert!(!message.alters_reactions(remove(&up, true)));
        assert!(!message.apply_reaction(remove(&up, true)));
        assert_eq!(message, once);
        assert_eq!(shown(&message), expected(&[(UP, 1, false)]));
        // Nothing held at all: nothing to take back.
        let mut empty = reacted(&[]);
        assert!(!empty.apply_reaction(remove(&up, true)));
        assert!(empty.reactions.is_empty());
    }

    #[test]
    fn burst_reactions_count_but_never_touch_the_accounts_own_state() {
        let up = unicode(UP);
        let mut message = reacted(&[]);
        // The account's first reaction is a burst: counted, not marked.
        assert!(message.apply_reaction(burst_add(&up, true)));
        assert_eq!(shown(&message), expected(&[(UP, 1, false)]));
        // A burst is not idempotent: each one counts.
        assert!(message.apply_reaction(burst_add(&up, true)));
        assert_eq!(shown(&message), expected(&[(UP, 2, false)]));
        assert!(message.apply_reaction(add(&up, true)));
        assert_eq!(shown(&message), expected(&[(UP, 3, true)]));
        assert!(message.apply_reaction(burst_add(&up, true)));
        assert!(message.apply_reaction(burst_add(&up, false)));
        assert_eq!(shown(&message), expected(&[(UP, 5, true)]));
        // Taking a burst back lowers the count and leaves the normal reaction.
        assert!(message.apply_reaction(burst_remove(&up, true)));
        assert_eq!(shown(&message), expected(&[(UP, 4, true)]));
        let mut unmarked = reacted(&[(&up, 2, false)]);
        assert!(unmarked.apply_reaction(burst_remove(&up, true)));
        assert_eq!(shown(&unmarked), expected(&[(UP, 1, false)]));
        assert!(unmarked.apply_reaction(burst_remove(&up, false)));
        assert!(unmarked.reactions.is_empty());
    }

    #[test]
    fn removing_lowers_the_count_and_drops_an_entry_that_reaches_zero_keeping_the_order() {
        let (a, b) = (unicode("a"), unicode("b"));
        let wave = custom(7, Some("wave"));
        let mut message = reacted(&[(&a, 1, false), (&b, 2, true), (&wave, 1, false)]);
        // An emoji with no entry changes nothing.
        assert!(!message.alters_reactions(remove(&unicode("zzz"), false)));
        assert!(!message.apply_reaction(remove(&unicode("zzz"), false)));
        // Someone else's removal leaves the account's own state alone.
        assert!(message.apply_reaction(remove(&b, false)));
        assert_eq!(
            shown(&message),
            expected(&[("a", 1, false), ("b", 1, true), ("#7", 1, false)])
        );
        // The last reaction goes with the entry, wherever it is in the list.
        assert!(message.apply_reaction(remove(&b, true)));
        assert_eq!(
            shown(&message),
            expected(&[("a", 1, false), ("#7", 1, false)])
        );
        // A renamed custom emoji is still found by its ID.
        assert!(message.apply_reaction(remove(&custom(7, Some("renamed")), false)));
        assert_eq!(shown(&message), expected(&[("a", 1, false)]));
        assert!(message.apply_reaction(remove(&a, false)));
        assert!(message.reactions.is_empty());

        // The account's own removal clears `me` and counts down.
        let mut message = reacted(&[(&a, 3, true)]);
        assert!(message.apply_reaction(remove(&a, true)));
        assert_eq!(shown(&message), expected(&[("a", 2, false)]));
    }

    #[test]
    fn an_entry_stored_with_a_zero_count_goes_with_the_next_removal() {
        // Discord should never report one; a removal cleans it up instead of
        // underflowing.
        let a = unicode("a");
        let mut message = reacted(&[(&a, 0, false)]);
        assert!(message.apply_reaction(remove(&a, false)));
        assert!(message.reactions.is_empty());
    }

    #[test]
    fn removing_an_emoji_or_everything_ignores_counts_and_self_state() {
        let a = unicode("a");
        let wave = custom(7, Some("wave"));
        let mut message = reacted(&[(&a, 5, true), (&wave, 2, false)]);
        assert!(!message.alters_reactions(ReactionChange::RemoveEmoji(&unicode("zzz"))));
        assert!(!message.apply_reaction(ReactionChange::RemoveEmoji(&unicode("zzz"))));
        assert!(message.apply_reaction(ReactionChange::RemoveEmoji(&custom(7, None))));
        assert_eq!(shown(&message), expected(&[("a", 5, true)]));
        assert!(message.alters_reactions(ReactionChange::RemoveAll));
        assert!(message.apply_reaction(ReactionChange::RemoveAll));
        assert!(message.reactions.is_empty());
        assert_eq!(
            message.reactions.capacity(),
            0,
            "the allocation is released too"
        );
        assert!(!message.alters_reactions(ReactionChange::RemoveAll));
        assert!(!message.apply_reaction(ReactionChange::RemoveAll));
    }

    #[test]
    fn a_message_never_keeps_more_than_the_maximum_number_of_distinct_emoji() {
        let emoji: Vec<Emoji> = (0..MAX_REACTIONS + 5)
            .map(|n| unicode(&format!("e{n}")))
            .collect();
        let mut message = reacted(&[]);
        for (n, emoji) in emoji.iter().enumerate() {
            let kept = n < MAX_REACTIONS;
            assert_eq!(
                message.alters_reactions(add(emoji, n % 2 == 0)),
                kept,
                "{n}"
            );
            assert_eq!(message.apply_reaction(add(emoji, n % 2 == 0)), kept, "{n}");
        }
        assert_eq!(message.reactions.len(), MAX_REACTIONS);
        assert_eq!(
            message
                .reactions
                .iter()
                .map(|reaction| &reaction.emoji)
                .collect::<Vec<_>>(),
            emoji[..MAX_REACTIONS].iter().collect::<Vec<_>>(),
            "the first-seen emoji are the ones kept, in order"
        );

        // A full message still counts what it holds...
        assert!(message.apply_reaction(add(&emoji[3], false)));
        assert_eq!(message.reactions[3].count, 2);
        assert!(!message.alters_reactions(add(&emoji[MAX_REACTIONS], false)));
        // ...and takes a new emoji again once an entry goes, at the end.
        assert!(message.apply_reaction(ReactionChange::RemoveEmoji(&emoji[0])));
        assert!(message.apply_reaction(add(&emoji[MAX_REACTIONS], false)));
        assert_eq!(message.reactions.len(), MAX_REACTIONS);
        assert_eq!(
            message.reactions.last().map(|reaction| &reaction.emoji),
            Some(&emoji[MAX_REACTIONS])
        );
        assert_eq!(message.reactions[0].emoji, emoji[1]);
    }

    #[test]
    fn counts_saturate_instead_of_wrapping() {
        let up = unicode(UP);
        let mut message = reacted(&[(&up, u32::MAX, false)]);
        // Nothing can be counted past the maximum, so there is no change at all.
        for change in [add(&up, false), burst_add(&up, false), burst_add(&up, true)] {
            assert!(!message.alters_reactions(change), "{change:?}");
            assert!(!message.apply_reaction(change), "{change:?}");
        }
        assert_eq!(shown(&message), expected(&[(UP, u32::MAX, false)]));
        // The account's own state still changes, whatever the count does.
        assert!(message.alters_reactions(add(&up, true)));
        assert!(message.apply_reaction(add(&up, true)));
        assert_eq!(shown(&message), expected(&[(UP, u32::MAX, true)]));
        assert!(message.apply_reaction(remove(&up, true)));
        assert_eq!(shown(&message), expected(&[(UP, u32::MAX - 1, false)]));
    }

    #[test]
    fn alters_reactions_agrees_with_apply_reaction_for_every_change() {
        let (up, down) = (unicode(UP), unicode(DOWN));
        let wave = custom(7, Some("wave"));
        let renamed = custom(7, Some("renamed"));
        let nameless = unicode("");
        let full: Vec<Emoji> = (0..MAX_REACTIONS)
            .map(|n| unicode(&format!("e{n}")))
            .collect();
        let full_entries: Vec<(&Emoji, u32, bool)> =
            full.iter().map(|emoji| (emoji, 1, false)).collect();
        let bases = [
            reacted(&[]),
            reacted(&[(&up, 1, true)]),
            reacted(&[(&up, 2, false), (&wave, 1, true)]),
            reacted(&[(&up, u32::MAX, true), (&wave, u32::MAX, false)]),
            reacted(&[(&up, 0, false)]),
            reacted(&full_entries),
        ];
        let mut changes = vec![ReactionChange::RemoveAll];
        for emoji in [&up, &down, &wave, &renamed, &nameless, &full[0]] {
            changes.push(ReactionChange::RemoveEmoji(emoji));
            for (by_me, burst) in [(false, false), (false, true), (true, false), (true, true)] {
                changes.push(ReactionChange::Add {
                    emoji,
                    by_me,
                    burst,
                });
                changes.push(ReactionChange::Remove {
                    emoji,
                    by_me,
                    burst,
                });
            }
        }
        let (mut changing, mut keeping) = (0, 0);
        for base in &bases {
            for &change in &changes {
                let alters = base.alters_reactions(change);
                let mut applied = base.clone();
                assert_eq!(
                    applied.apply_reaction(change),
                    alters,
                    "{change:?} on {:?}",
                    shown(base)
                );
                assert_eq!(applied != *base, alters, "{change:?} on {:?}", shown(base));
                assert!(applied.reactions.len() <= MAX_REACTIONS.max(base.reactions.len()));
                if alters {
                    changing += 1;
                } else {
                    keeping += 1;
                }
            }
        }
        assert!(changing > 50 && keeping > 50, "{changing} and {keeping}");
    }

    #[test]
    fn structural_changes_compact_the_stored_reactions() {
        let emoji: Vec<Emoji> = (0..9).map(|n| unicode(&format!("e{n}"))).collect();
        let mut message = reacted(&[]);
        let compact = |message: &Message| message.reactions.capacity() == message.reactions.len();
        for emoji in &emoji {
            assert!(message.apply_reaction(add(emoji, false)));
            assert!(compact(&message), "after adding {emoji:?}");
        }
        assert!(message.apply_reaction(ReactionChange::RemoveEmoji(&emoji[4])));
        assert!(compact(&message));
        assert!(message.apply_reaction(remove(&emoji[0], false)));
        assert_eq!(message.reactions.len(), 7);
        assert!(compact(&message));
        // Changing a count in place does not reallocate.
        let allocation = message.reactions.as_ptr();
        assert!(message.apply_reaction(add(&emoji[1], false)));
        assert_eq!(message.reactions.as_ptr(), allocation);
        assert!(message.apply_reaction(ReactionChange::RemoveAll));
        assert_eq!(message.reactions.capacity(), 0);
    }

    #[test]
    fn reaction_changes_touch_nothing_but_the_reactions() {
        let up = unicode(UP);
        let before = reacted(&[]);
        let mut after = before.clone();
        assert!(after.apply_reaction(add(&up, true)));
        assert_ne!(after, before);
        after.reactions.clear();
        assert_eq!(after, before);
    }

    #[test]
    fn voice_message_metadata_is_read_and_hostile_values_are_dropped() {
        let wire = |extra: serde_json::Value| -> Attachment {
            let mut value = serde_json::json!({
                "id": "1", "filename": "voice-message.ogg", "size": 10, "url": "https://x"
            });
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value(value).unwrap()
        };
        let ok = wire(serde_json::json!({"duration_secs": 3.25, "waveform": "AAEC"}));
        assert_eq!(
            (ok.duration_secs, ok.waveform.as_deref()),
            (Some(3.25), Some("AAEC"))
        );
        let long = "A".repeat(MAX_WAVEFORM_CHARS + 1);
        let hostile = wire(serde_json::json!({"duration_secs": -1.0, "waveform": long}));
        assert_eq!((hostile.duration_secs, hostile.waveform), (None, None));
        let huge = wire(serde_json::json!({"duration_secs": 1e30}));
        assert_eq!(huge.duration_secs, None);
        assert_eq!(wire(serde_json::json!({})).waveform, None);
    }
}
