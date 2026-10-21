//! The message timeline of the selected channel: the viewport-only read model
//! the account worker publishes, the events the view emits, and the view.
//!
//! A [`Snapshot`] holds only the rows [`Window`] asks to be built, each as a
//! shared `Arc<Message>`, plus the numeric spacers standing in for everything
//! else; the worker owns the history and the [`VariableList`] behind it. The
//! view never fetches, sorts, or caches anything: it lays the rows out, and the
//! widget in [`variable_list`] measures what that layout really produced and
//! reports it as [`Event`]s that carry the channel they were produced for, so a
//! late event for a channel that is no longer open is easy to reject.
//!
//! Every attachment lists its name, size, and type with explicit Download and
//! Open buttons. Decodable images add a "Load preview" button; the thumbnail
//! that replaces it opens a larger in-app view when clicked. The row grows when
//! the thumbnail arrives: the widget publishes the new measured height and the
//! variable list keeps the reader's anchor row where it was.
//!
//! The user's own messages (the worker decides which those are) offer Edit and
//! Delete while hovered; other people's messages offer neither. Deleting asks
//! for confirmation in the row first. Editing happens in the composer; the row
//! only says so. An edit being saved shows its new text at once, marked as
//! saving, and falls back to Discord's copy if it fails.
//!
//! Where the user may send, hovering a message also offers Reply, which hands
//! it to the composer. A reply shows one line above itself naming what it
//! answers (resolved by the worker: loaded, deleted, or still loading);
//! clicking that line jumps to the original, which the worker loads first when
//! it is not retained, and the row jumped to stays tinted while it is near.
//!
//! Reactions are drawn as chips under the message: one per emoji with Discord's
//! count, highlighted when the user reacted. Clicking a chip adds the user's
//! reaction or takes it back; right-clicking it asks to see who reacted, which
//! is loaded only then. Where the user may react, hovering also offers React,
//! which opens the emoji picker for that message. A request the user made is
//! drawn as if Discord had accepted it and is disabled until Discord answers.
//!
//! [`VariableList`]: crate::variable_list::VariableList
use std::collections::HashMap;
use std::sync::Arc;

use fastcord_discord::state::Suppression;
use fastcord_model::{Attachment, Emoji, Message, Snowflake, StickerItem, User};
use iced::alignment::{Horizontal, Vertical};
use iced::font::{Font, Weight};
use iced::widget::{
    Container, button, column, container, hover, image, mouse_area, row, stack, text, tooltip,
};
use iced::{Element, Length, Padding};

use crate::attachments::AttachmentKey;
use crate::changes::{ChangeKind, ChangeState, RowChange};
use crate::emoji::{self, Occurrence, Origin};
use crate::outbox::OutboxItem;
use crate::reactions::{Action, Pending};
use crate::rich;
use crate::style;
use crate::variable_list::{self, Item, Measurement, Report, ScrollRequest, Viewport, Window};

/// Closer to the first retained message than this, the view offers to load older ones.
const TOP_THRESHOLD: f32 = 80.0;
/// Timestamps that are not ISO 8601 are shown truncated to this many characters.
const MAX_TIMESTAMP_CHARS: usize = 32;
const BOLD: Font = Font {
    weight: Weight::Bold,
    ..Font::DEFAULT
};

/// One built row: the message (shared with the worker's store, never copied) and
/// the revision of its body, which tells the height cache when to distrust itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub message: Arc<Message>,
    pub revision: u64,
    /// The user wrote it and may edit and delete it (decided by the worker).
    pub own: bool,
    /// Its unfinished edit or deletion.
    pub change: Option<RowChange>,
    /// What it replies to, for a reply.
    pub reply: Option<ReplyLine>,
    /// The user's reaction requests Discord has not answered yet.
    pub reactions: Vec<Pending>,
    /// A vote or ending of the row's poll Discord has not answered yet.
    pub poll_saving: bool,
    /// The message mentions the account (set by the worker from the store).
    pub mentions_me: bool,
    /// The author is ignored or blocked: `message` is then an empty stand-in
    /// and nothing of the original reached this row.
    pub hidden: Option<Suppression>,
}

#[cfg(test)]
impl Row {
    pub fn new(message: Arc<Message>, revision: u64) -> Self {
        Self {
            message,
            revision,
            own: false,
            change: None,
            reply: None,
            reactions: Vec::new(),
            poll_saving: false,
            mentions_me: false,
            hidden: None,
        }
    }
}

/// Revision bit that tells the height cache a row is a hidden stand-in.
const HIDDEN_REVISION: u64 = 1 << 63;

/// A message with only its place kept: ID, channel, time and author ID.
fn stand_in(message: &Message) -> Message {
    let mut hidden = message.clone();
    hidden.author = User {
        id: message.author.id,
        username: String::new(),
        global_name: None,
        avatar: None,
        bot: false,
    };
    hidden.content.clear();
    hidden.edited_timestamp = None;
    hidden.kind = 0;
    hidden.flags = 0;
    hidden.pinned = false;
    hidden.attachments.clear();
    hidden.reactions.clear();
    hidden.sticker_items.clear();
    hidden.message_snapshots.clear();
    hidden.poll = None;
    hidden.mentions.clear();
    hidden.mention_roles.clear();
    hidden.mention_everyone = false;
    hidden.embeds.clear();
    hidden.message_reference = None;
    hidden.referenced_message = Default::default();
    hidden.nonce = None;
    hidden.call = None;
    hidden
}

/// The one place the suppression policy meets the timeline: rows by ignored or
/// blocked authors lose everything but their position, and replies quoting
/// such an author lose the quote. The account's own messages are never hidden.
pub fn hide_suppressed(
    snapshot: &mut Snapshot,
    suppressed: &std::collections::BTreeMap<Snowflake, Suppression>,
) {
    if suppressed.is_empty() {
        return;
    }
    for row in &mut snapshot.rows {
        if let Some(reply) = &mut row.reply
            && reply.author.is_some_and(|id| suppressed.contains_key(&id))
        {
            reply.state = ReplyState::Hidden;
        }
        if row.own {
            continue;
        }
        if let Some(kind) = suppressed.get(&row.message.author.id) {
            row.message = Arc::new(stand_in(&row.message));
            row.hidden = Some(*kind);
            row.change = None;
            row.reply = None;
            row.reactions.clear();
            row.poll_saving = false;
            row.revision |= HIDDEN_REVISION;
        }
    }
}

/// Source text shown in a row, including an optimistic edit being saved.
pub fn content(row: &Row) -> &str {
    match &row.change {
        Some(RowChange {
            kind: ChangeKind::Edit(text),
            state: ChangeState::Saving,
        }) => text,
        _ => &row.message.content,
    }
}

/// The line a reply shows above itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyLine {
    /// The message it answers: where clicking the line jumps to.
    pub target: Snowflake,
    /// Who wrote it, when known.
    pub author: Option<Snowflake>,
    pub state: ReplyState,
}

/// What is known about the message a reply answers.
#[derive(Clone, PartialEq, Eq)]
pub enum ReplyState {
    /// Its author's name and the start of its text (at most
    /// [`REPLY_PREVIEW_CHARS`](fastcord_model::REPLY_PREVIEW_CHARS) characters).
    Message { author: String, content: String },
    /// It was deleted.
    Deleted,
    /// It is being loaded.
    Loading,
    /// It could not be loaded; a jump tries again.
    Unavailable,
    /// Its author is ignored or blocked.
    Hidden,
}

// The quoted text never reaches a log.
impl std::fmt::Debug for ReplyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Message { .. } => "Message",
            Self::Deleted => "Deleted",
            Self::Loading => "Loading",
            Self::Unavailable => "Unavailable",
            Self::Hidden => "Hidden",
        })
    }
}

/// Who reacted with one emoji of one message, shown under its row. It is
/// loaded only because the user asked to see it (SPEC §9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionUsers {
    pub message: Snowflake,
    pub emoji: Emoji,
    /// The answer a poll's voters are listed for (then `emoji` is unused).
    pub poll_answer: Option<String>,
    pub state: UsersState,
}

#[derive(Clone, PartialEq, Eq)]
pub enum UsersState {
    Loading,
    /// The first display names Discord listed (at most
    /// [`MAX_USERS`](crate::reactions::MAX_USERS)) and whether it has more.
    Loaded {
        names: Vec<String>,
        more: bool,
    },
    /// Why they could not be loaded (a fixed, secret-free sentence).
    Failed(&'static str),
}

// Display names never reach a log.
impl std::fmt::Debug for UsersState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Loading => "Loading",
            Self::Loaded { .. } => "Loaded",
            Self::Failed(_) => "Failed",
        })
    }
}

/// One reaction chip of a row: Discord's count and whether the user reacted,
/// with the user's unanswered request applied on top (drawn as if Discord had
/// accepted it, and disabled until it answers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chip<'a> {
    pub emoji: &'a Emoji,
    pub count: u32,
    pub me: bool,
    pub pending: bool,
}

/// The chips of a row, in Discord's order, then reactions the user is adding
/// that Discord has not reported yet. A chip whose last reaction the user is
/// taking back disappears.
pub fn chips(row: &Row) -> impl Iterator<Item = Chip<'_>> {
    let stored = row.message.reactions.iter().filter_map(move |reaction| {
        let chip = |count, me, pending| Chip {
            emoji: &reaction.emoji,
            count,
            me,
            pending,
        };
        let request = row
            .reactions
            .iter()
            .find(|request| request.emoji.same_reaction(&reaction.emoji))
            .map(|request| request.action);
        match request {
            None => Some(chip(reaction.count, reaction.me, false)),
            Some(Action::Add) => Some(chip(
                reaction.count.saturating_add(u32::from(!reaction.me)),
                true,
                true,
            )),
            Some(Action::Remove) => {
                let count = reaction.count.saturating_sub(u32::from(reaction.me));
                (count > 0).then(|| chip(count, false, true))
            }
        }
    });
    let added = row
        .reactions
        .iter()
        .filter(move |request| {
            request.action == Action::Add
                && !row
                    .message
                    .reactions
                    .iter()
                    .any(|reaction| reaction.emoji.same_reaction(&request.emoji))
        })
        .map(|request| Chip {
            emoji: &request.emoji,
            count: 1,
            me: true,
            pending: true,
        });
    stored.chain(added)
}

/// Everything the timeline view needs, and nothing else. `rows` are exactly
/// the messages of `window.range` (or a prefix-trimmed version of it whose
/// spacers were recomputed with `VariableList::window_for_range`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The channel the rows belong to; events carry it back.
    pub channel_id: Option<Snowflake>,
    pub rows: Vec<Row>,
    pub window: Window,
    /// The list's unacknowledged scroll request; copy it into every snapshot.
    pub scroll: Option<ScrollRequest>,
    /// A page is being fetched.
    pub loading: bool,
    /// Older messages exist beyond the retained rows.
    pub has_older: bool,
    /// Newer messages exist beyond the retained rows (the newest end was
    /// evicted, or arrived while the reader was away from the bottom).
    pub has_newer: bool,
    /// A fixed, secret-free description of why the last page failed.
    pub error: Option<&'static str>,
    /// Unconfirmed sends of every channel, oldest first (at most
    /// [`MAX_OUTBOX`](crate::outbox::MAX_OUTBOX)).
    pub outbox: Vec<OutboxItem>,
    /// The user may send to this channel (validated by the worker).
    pub can_send: bool,
    /// Why the last edit, deletion, or jump could not be carried out, if it could not.
    pub notice: Option<&'static str>,
    /// The row a jump went to, tinted while it is near the viewport.
    pub highlight: Option<Snowflake>,
    /// Messages of this channel known to be deleted (at most
    /// [`MAX_REFERENCES`](crate::history::MAX_REFERENCES)), so the composer
    /// can tell that the message being replied to is gone.
    pub deleted: Vec<Snowflake>,
    /// The user may react here (validated by the worker): rows offer React.
    pub can_react: bool,
    /// Who reacted with one emoji, loaded only because the user asked.
    pub reaction_users: Option<ReactionUsers>,
    /// Names of the roles and channels the rows mention.
    pub names: rich::Names,
    /// What `@` and `#` can complete to in this channel's server.
    pub candidates: Vec<crate::composer::Candidate>,
    /// Display names of the other people typing here, longest-typing first.
    pub typing: Vec<String>,
}

/// A decoded preview: shared RGBA pixels (a clone shares the buffer) and the
/// size they were decoded to.
#[derive(Clone)]
pub struct AttachmentPreview {
    pub handle: image::Handle,
    pub width: u32,
    pub height: u32,
}

/// UI-only state for one message attachment. `Loading` is bounded by the
/// account panel's worker slots; completed thumbnails are shared RGBA buffers.
#[derive(Clone)]
pub enum AttachmentView {
    Loading,
    Ready(AttachmentPreview),
    /// Why the image cannot be shown (a fixed, secret-free message).
    Failed(&'static str),
}
impl std::fmt::Debug for AttachmentView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Loading => "Loading",
            Self::Ready(_) => "Ready",
            Self::Failed(_) => "Failed",
        })
    }
}

/// Display box, in logical pixels, of an inline attachment thumbnail. Previews
/// are decoded to at most this size.
pub const INLINE_PREVIEW: (u32, u32) = (360, 270);

/// Image formats the build decodes; others show metadata with Download/Open.
pub fn is_previewable_image(attachment: &Attachment) -> bool {
    match attachment.content_type.as_deref() {
        Some(content_type) => {
            let mime = content_type.split(';').next().unwrap_or_default().trim();
            [
                "image/png",
                "image/jpeg",
                "image/gif",
                "image/webp",
                "image/bmp",
            ]
            .iter()
            .any(|known| mime.eq_ignore_ascii_case(known))
        }
        None => std::path::Path::new(&attachment.filename)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| {
                ["png", "jpg", "jpeg", "gif", "webp", "bmp"]
                    .iter()
                    .any(|known| extension.eq_ignore_ascii_case(known))
            }),
    }
}
/// What the UI is doing with a row that the snapshot does not know about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interaction {
    /// The message whose text is in the composer for editing.
    pub editing: Option<Snowflake>,
    /// The message the composer is replying to.
    pub replying: Option<Snowflake>,
    /// The message whose deletion waits for confirmation.
    pub confirming: Option<Snowflake>,
    /// The account may pin and unpin messages in the open channel.
    pub can_pin: bool,
    /// The account may start a public thread from a message of the open channel.
    pub can_thread: bool,
    /// The account may hide or restore other people's link previews here
    /// (Manage Messages in a server channel).
    pub can_suppress_embeds: bool,
    /// The message whose secondary actions are shown instead of the usual ones.
    pub more: Option<Snowflake>,
}

/// What the timeline asks of the worker. Each event names its channel.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// An inline asset entered or left the actual clipped viewport.
    EmojiVisibility(Occurrence, bool),
    /// Copy original text/tokens, never rendered asset names or pixels.
    CopyText {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    /// Copy the message's `discord.com/channels/…` link.
    CopyLink {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    /// Copy the message's ID.
    CopyId {
        message_id: Snowflake,
    },
    /// Start reporting another person's message (a review step follows;
    /// nothing is sent by this event).
    Report {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    /// Hide (`true`) or restore the link previews of a message.
    SetEmbeds {
        channel_id: Snowflake,
        message_id: Snowflake,
        suppressed: bool,
    },
    /// Show the secondary actions of a message, or the usual ones again.
    More(Option<Snowflake>),
    /// The widget's latest view of the viewport (coalesce to the newest).
    Viewport {
        channel_id: Snowflake,
        viewport: Viewport,
    },
    /// The height a row actually laid out to (keep the newest per row and width bucket).
    Measured {
        channel_id: Snowflake,
        measurement: Measurement,
    },
    /// Jump to the newest message, reloading it first when it is not retained.
    JumpLatest {
        channel_id: Snowflake,
    },
    LoadOlder {
        channel_id: Snowflake,
    },
    Retry {
        channel_id: Snowflake,
    },
    /// Put one of the user's messages into the composer for editing.
    Edit {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    /// Ask to delete one of the user's messages (confirmation follows).
    Delete {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    ConfirmDelete {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    CancelDelete,
    /// Fetch and show the attachment preview in this message.
    LoadAttachment {
        channel_id: Snowflake,
        message_id: Snowflake,
        attachment_id: Snowflake,
    },
    /// Open the larger in-app view of an attachment image (its thumbnail was clicked).
    ViewAttachment {
        channel_id: Snowflake,
        message_id: Snowflake,
        attachment_id: Snowflake,
    },
    /// Play an Ogg Opus attachment inline.
    PlayAttachment {
        channel_id: Snowflake,
        message_id: Snowflake,
        attachment_id: Snowflake,
    },
    /// Pause, seek, volume or close the attachment being played.
    Media(crate::media_ui::Control),
    /// Save the original attachment bytes using the native save dialog.
    DownloadAttachment {
        channel_id: Snowflake,
        message_id: Snowflake,
        attachment_id: Snowflake,
    },
    /// Open the original attachment with the operating system's file handler.
    OpenAttachment {
        channel_id: Snowflake,
        message_id: Snowflake,
        attachment_id: Snowflake,
    },
    RetryChange {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    DismissChange {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    /// Reply to a message (the composer takes it).
    Reply {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    /// Show a message, loading the history around it first if necessary.
    JumpTo {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    /// A reaction chip was clicked: add the user's reaction with this emoji, or
    /// take it back when it is already theirs. The worker decides which.
    ToggleReaction {
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: Emoji,
    },
    /// A reaction chip was right-clicked: show who reacted with it.
    InspectReaction {
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: Emoji,
    },
    CloseReactionUsers,
    /// A poll answer was clicked: toggle it in the user's votes. The worker
    /// decides what the whole selection becomes from the store.
    PollVote {
        channel_id: Snowflake,
        message_id: Snowflake,
        answer_id: u32,
    },
    /// End the user's own poll now.
    PollEnd {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    /// Show who voted for an answer.
    PollVoters {
        channel_id: Snowflake,
        message_id: Snowflake,
        answer_id: u32,
    },
    /// A link in a message or embed was clicked; the target is http(s) only.
    OpenLink(String),
    /// A spoiler was clicked: uncover it.
    ToggleSpoiler {
        message_id: Snowflake,
        spoiler: u16,
    },
    /// Pin (`true`) or unpin a message.
    SetPinned {
        channel_id: Snowflake,
        message_id: Snowflake,
        pinned: bool,
    },
    /// A message author's name or avatar was clicked: show their profile.
    OpenProfile(fastcord_model::User),
    /// Forward a message to destinations the user picks.
    Forward {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    /// Start a thread from a message.
    StartThread {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
    /// Open the thread a "thread created" row announces.
    OpenThread(Snowflake),
    /// Choose an emoji to react to a message with (opens the emoji picker).
    React {
        channel_id: Snowflake,
        message_id: Snowflake,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Notice {
    Loading,
    Failed(&'static str),
    LoadOlder,
}

/// Which extras the view shows, decided from the snapshot alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Controls {
    /// Centered text instead of rows.
    empty: Option<&'static str>,
    /// A pill at the top.
    notice: Option<Notice>,
    /// The jump-to-latest button.
    jump: bool,
}

impl Controls {
    fn of(snapshot: &Snapshot) -> Self {
        let has_rows = !snapshot.rows.is_empty();
        let notice = match snapshot.error {
            Some(message) => Some(Notice::Failed(message)),
            None if has_rows && snapshot.loading => Some(Notice::Loading),
            None if has_rows && snapshot.has_older && snapshot.window.offset <= TOP_THRESHOLD => {
                Some(Notice::LoadOlder)
            }
            None => None,
        };
        Self {
            empty: (!has_rows && snapshot.error.is_none()).then_some(if snapshot.loading {
                "Loading messages..."
            } else {
                "There are no messages here yet."
            }),
            notice,
            jump: has_rows && (!snapshot.window.at_bottom || snapshot.has_newer),
        }
    }
}

/// Original source for an explicit copy action on a still-loaded message.
pub fn copy_text(snapshot: &Snapshot, channel: Snowflake, message: Snowflake) -> Option<&str> {
    (snapshot.channel_id == Some(channel)).then_some(())?;
    snapshot
        .rows
        .iter()
        .find(|row| row.message.id == message)
        .map(content)
}

/// What a report says about a still-loaded message of someone else.
pub struct ReportSubject {
    pub author: fastcord_model::User,
    pub excerpt: String,
    pub voice: bool,
}

/// Longest excerpt of a reported message shown in the review.
const REPORT_EXCERPT_CHARS: usize = 200;

pub fn report_subject(
    snapshot: &Snapshot,
    channel: Snowflake,
    message: Snowflake,
) -> Option<ReportSubject> {
    (snapshot.channel_id == Some(channel)).then_some(())?;
    let row = snapshot
        .rows
        .iter()
        .find(|row| row.message.id == message && !row.own && row.hidden.is_none())?;
    Some(ReportSubject {
        author: row.message.author.clone(),
        excerpt: content(row)
            .trim()
            .chars()
            .take(REPORT_EXCERPT_CHARS)
            .collect(),
        voice: row.message.is_voice_message(),
    })
}

/// Whether the message is still held in the open channel; `None` when another
/// channel is open (nothing is known then).
pub fn is_retained(snapshot: &Snapshot, channel: Snowflake, message: Snowflake) -> Option<bool> {
    (snapshot.channel_id == Some(channel))
        .then(|| snapshot.rows.iter().any(|row| row.message.id == message))
}

/// What the timeline needs to show account-owned attachment and emoji assets.
#[derive(Clone, Copy)]
pub struct Attachments<'a> {
    pub account: Snowflake,
    pub views: &'a HashMap<AttachmentKey, AttachmentView>,
    /// The attachment being played, if any.
    pub media: Option<&'a crate::media_ui::MediaSession>,
    pub emojis: &'a HashMap<emoji::Asset, emoji::View>,
    pub avatars: &'a HashMap<crate::avatars::AvatarKey, iced::widget::image::Handle>,
    pub revealed: &'a std::collections::HashSet<(Snowflake, u16)>,
    /// Held members of the open server, for nicknames (empty in a DM).
    pub members: &'a HashMap<Snowflake, fastcord_discord::state::navigation::MemberView>,
}

/// The name a message author is shown under: server nickname, else the
/// display name, else the username (the order voice participants use).
fn author_name<'a>(message: &'a Message, assets: Attachments<'a>) -> &'a str {
    assets
        .members
        .get(&message.author.id)
        .and_then(|member| member.nick.as_deref())
        .unwrap_or_else(|| message.author.display_name())
}

pub fn view<'a>(
    snapshot: &'a Snapshot,
    interaction: Interaction,
    assets: Attachments<'a>,
) -> Element<'a, Event> {
    let Some(channel_id) = snapshot.channel_id else {
        return centered("Select a channel to read its messages.");
    };
    let rows = snapshot.rows.iter().enumerate().map(|(at, row)| {
        let continues = at
            .checked_sub(1)
            .and_then(|before| snapshot.rows.get(before))
            .is_some_and(|before| continues_group(before, row));
        (
            Item {
                id: row.message.id,
                revision: row.revision,
            },
            message_view(
                row,
                channel_id,
                interaction,
                snapshot,
                snapshot.highlight == Some(row.message.id)
                    || interaction.replying == Some(row.message.id),
                continues,
                assets,
            ),
        )
    });
    let list = variable_list::view(
        channel_id.0,
        &snapshot.window,
        snapshot.scroll,
        rows,
        move |report| match report {
            Report::Viewport(viewport) => Event::Viewport {
                channel_id,
                viewport,
            },
            Report::Measured(measurement) => Event::Measured {
                channel_id,
                measurement,
            },
        },
    );
    let controls = Controls::of(snapshot);
    let mut layers = vec![list];
    if let Some(message) = controls.empty {
        layers.push(centered(message));
    }
    if let Some(notice) = controls.notice {
        layers.push(notice_layer(notice, channel_id));
    }
    if controls.jump {
        layers.push(jump_layer(channel_id));
    }
    stack(layers)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn centered(message: &'static str) -> Element<'static, Event> {
    container(text(message).size(14).style(text::secondary))
        .center(Length::Fill)
        .into()
}

fn pill_button(label: &'static str, event: Event) -> Element<'static, Event> {
    button(text(label).size(13))
        .on_press(event)
        .padding(Padding {
            top: 4.0,
            right: 12.0,
            bottom: 4.0,
            left: 12.0,
        })
        .style(button::secondary)
        .into()
}

fn notice_layer(notice: Notice, channel_id: Snowflake) -> Element<'static, Event> {
    let content: Element<'static, Event> = match notice {
        Notice::Loading => text("Loading messages...").size(13).into(),
        Notice::Failed(message) => row![
            text(message).size(13).style(text::danger),
            pill_button("Retry", Event::Retry { channel_id }),
        ]
        .spacing(12)
        .align_y(Vertical::Center)
        .into(),
        Notice::LoadOlder => pill_button("Load older messages", Event::LoadOlder { channel_id }),
    };
    container(container(content).padding(8).style(container::rounded_box))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Center)
        .align_y(Vertical::Top)
        .padding(8)
        .into()
}

fn jump_layer(channel_id: Snowflake) -> Element<'static, Event> {
    container(
        button(text("Jump to latest").size(13))
            .on_press(Event::JumpLatest { channel_id })
            .padding(Padding {
                top: 6.0,
                right: 14.0,
                bottom: 6.0,
                left: 14.0,
            })
            .style(button::primary),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Horizontal::Right)
    .align_y(Vertical::Bottom)
    .padding(16)
    .into()
}

/// What a row offers besides the message itself, decided from the row and the
/// UI's interaction state alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowState {
    /// Someone else's message: no Edit or Delete actions.
    Plain,
    /// The user's message: Edit and Delete while hovered.
    Actions,
    /// The composer holds this message's text.
    Editing,
    /// Delete was pressed; the deletion waits for confirmation.
    Confirming,
    /// An edit or deletion is on its way to Discord.
    Saving { delete: bool },
    /// Discord did not accept the change: the reason, Retry, and Dismiss, and
    /// Edit and Delete while hovered.
    Failed { delete: bool, reason: &'static str },
}

fn row_state(row: &Row, interaction: Interaction) -> RowState {
    if !row.own {
        return RowState::Plain;
    }
    let id = Some(row.message.id);
    let change = row
        .change
        .as_ref()
        .map(|change| (change.kind == ChangeKind::Delete, change.state));
    match change {
        Some((delete, ChangeState::Saving)) => RowState::Saving { delete },
        _ if interaction.confirming == id => RowState::Confirming,
        _ if interaction.editing == id => RowState::Editing,
        Some((delete, ChangeState::Failed(reason))) => RowState::Failed { delete, reason },
        None => RowState::Actions,
    }
}

fn row_button(label: &'static str, event: Event) -> button::Button<'static, Event> {
    button(text(label).size(12))
        .on_press(event)
        .padding(Padding {
            top: 2.0,
            right: 8.0,
            bottom: 2.0,
            left: 8.0,
        })
        .style(button::secondary)
}

/// Size of the emoji image on a reaction chip.
const CHIP_ICON: f32 = 18.0;

fn hint(line: &'static str) -> Element<'static, Event> {
    container(text(line).size(12))
        .padding(6)
        .style(container::rounded_box)
        .into()
}

/// One chip. Clicking toggles the user's reaction; right-clicking asks who
/// reacted. Its emoji image is drawn once decoded and visible, text before.
fn chip_view<'a>(
    chip: Chip<'a>,
    index: usize,
    channel_id: Snowflake,
    row: &Row,
    assets: Attachments<'a>,
) -> Element<'a, Event> {
    let message_id = row.message.id;
    let fallback = chip.emoji.name.as_deref().unwrap_or("emoji");
    let icon: Element<'a, Event> = match emoji::reaction_asset(chip.emoji) {
        Some(asset) => emoji::sensed(
            emoji::asset_view(asset, fallback, assets.emojis, CHIP_ICON),
            Occurrence {
                origin: Origin::Reaction {
                    channel: channel_id,
                    message: message_id,
                    revision: row.revision,
                    chip: u8::try_from(index).unwrap_or(u8::MAX),
                },
                asset,
            },
            Event::EmojiVisibility,
        ),
        None => text(fallback).size(15).into(),
    };
    let label = row![icon, text(chip.count.to_string()).size(13)]
        .spacing(4)
        .align_y(Vertical::Center);
    let toggle = Event::ToggleReaction {
        channel_id,
        message_id,
        emoji: chip.emoji.clone(),
    };
    let inspect = Event::InspectReaction {
        channel_id,
        message_id,
        emoji: chip.emoji.clone(),
    };
    let chip_button = button(label)
        .padding([2, 6])
        .on_press_maybe((!chip.pending).then_some(toggle))
        .style(if chip.me {
            button::primary
        } else {
            button::secondary
        });
    mouse_area(tooltip(
        chip_button,
        hint("Click to add or remove your reaction. Right-click to see who reacted."),
        tooltip::Position::Top,
    ))
    .on_right_press(inspect)
    .into()
}

/// The row's chips, wrapped, followed by a button that opens the picker where
/// the user may react. `None` for a message nobody reacted to.
fn reactions_line<'a>(
    row: &'a Row,
    channel_id: Snowflake,
    can_react: bool,
    assets: Attachments<'a>,
) -> Option<Element<'a, Event>> {
    let mut items: Vec<Element<'a, Event>> = chips(row)
        .enumerate()
        .map(|(index, chip)| chip_view(chip, index, channel_id, row, assets))
        .collect();
    if items.is_empty() {
        return None;
    }
    if can_react {
        items.push(
            tooltip(
                button(text("+").size(13))
                    .padding([2, 8])
                    .on_press(Event::React {
                        channel_id,
                        message_id: row.message.id,
                    })
                    .style(button::secondary),
                hint("Add reaction"),
                tooltip::Position::Top,
            )
            .into(),
        );
    }
    Some(
        iced::widget::Row::with_children(items)
            .spacing(4)
            .wrap()
            .vertical_spacing(4)
            .into(),
    )
}

/// What the emoji is called in the sentence that lists who reacted with it.
fn describe_emoji(emoji: &Emoji) -> String {
    match (emoji.id, emoji.name.as_deref()) {
        (Some(_), Some(name)) => format!(":{name}:"),
        (Some(_), None) => "a deleted emoji".to_owned(),
        (None, Some(name)) => name.to_owned(),
        (None, None) => "an unknown emoji".to_owned(),
    }
}

/// The names of those who reacted with one emoji, under the row asked about.
fn users_panel(users: &ReactionUsers) -> Element<'_, Event> {
    let detail: Element<'_, Event> = match &users.state {
        UsersState::Loading if users.poll_answer.is_some() => text("Loading voters…")
            .size(12)
            .style(text::secondary)
            .into(),
        UsersState::Loading => text("Loading who reacted…")
            .size(12)
            .style(text::secondary)
            .into(),
        UsersState::Failed(reason) => text(*reason).size(12).style(text::danger).into(),
        UsersState::Loaded { names, more } => {
            let mut line = if names.is_empty() {
                "No one is listed.".to_owned()
            } else {
                names.join(", ")
            };
            if *more {
                line.push_str(" and others");
            }
            text(line)
                .size(12)
                .width(Length::Fill)
                .wrapping(text::Wrapping::WordOrGlyph)
                .into()
        }
    };
    container(
        column![
            row![
                text(match &users.poll_answer {
                    Some(answer) => format!("Voted for {answer}"),
                    None => format!("Reacted with {}", describe_emoji(&users.emoji)),
                })
                .font(BOLD)
                .size(12),
                row_button("Close", Event::CloseReactionUsers),
            ]
            .spacing(8)
            .align_y(Vertical::Center),
            detail,
        ]
        .spacing(2),
    )
    .padding(6)
    .style(container::rounded_box)
    .into()
}

/// What stands in for a message by an ignored or blocked user: no name, text,
/// time or controls.
fn hidden_view<'a>(kind: Suppression) -> Element<'a, Event> {
    text(match kind {
        Suppression::Ignored => "Message from an ignored user hidden.",
        Suppression::Blocked => "Message from a blocked user hidden.",
    })
    .size(12)
    .style(text::secondary)
    .into()
}

/// Author, time, grapheme-aware wrapped text/emoji runs, attachment lines, and
/// the controls of the user's own messages. Original message text is unchanged.
fn message_view<'a>(
    row: &'a Row,
    channel_id: Snowflake,
    interaction: Interaction,
    snapshot: &'a Snapshot,
    highlighted: bool,
    continues: bool,
    assets: Attachments<'a>,
) -> Element<'a, Event> {
    if let Some(kind) = row.hidden {
        return hidden_view(kind);
    }
    let account_id = assets.account;
    let message = &*row.message;
    let message_id = message.id;
    let state = row_state(row, interaction);
    let mut header = row![
        button(text(author_name(message, assets)).font(BOLD).size(15))
            .padding(0)
            .style(style::plain_button)
            .on_press_with(|| Event::OpenProfile(message.author.clone())),
        text(format_timestamp(
            &message.timestamp,
            crate::localtime::offset()
        ))
        .size(12)
        .style(text::secondary),
    ]
    .spacing(8)
    .align_y(Vertical::Center);
    if highlighted {
        header = header.push(
            text(if interaction.replying == Some(message_id) {
                "Reply target"
            } else {
                "Jumped here"
            })
            .size(12)
            .style(text::secondary),
        );
    }
    if message.edited_timestamp.is_some() {
        header = header.push(text("(edited)").size(12).style(text::secondary));
    }
    // An edit being saved shows its new text right away.
    let content = content(row);
    let mut body = column![].spacing(2);
    if let Some(reply) = &row.reply {
        let label = match &reply.state {
            ReplyState::Message { author, content } => {
                format!("Replying to {author}: {content}")
            }
            ReplyState::Deleted => "Original message was deleted.".to_owned(),
            ReplyState::Loading => "Loading the original message…".to_owned(),
            ReplyState::Unavailable => "Original message unavailable.".to_owned(),
            ReplyState::Hidden => "Replying to a hidden message.".to_owned(),
        };
        let preview = text(label)
            .size(12)
            .width(Length::Fill)
            .wrapping(text::Wrapping::WordOrGlyph);
        body = if matches!(reply.state, ReplyState::Deleted | ReplyState::Hidden) {
            body.push(preview.style(text::secondary))
        } else {
            body.push(
                button(preview)
                    .on_press(Event::JumpTo {
                        channel_id,
                        message_id: reply.target,
                    })
                    .padding([2, 4])
                    .style(button::secondary),
            )
        };
    }
    if !continues {
        body = body.push(header);
    }
    let cx = rich::Cx {
        names: &snapshot.names,
        revealed: assets.revealed,
        emojis: assets.emojis,
        pictures: assets.avatars,
        now: crate::localtime::now(),
    };
    if !content.is_empty() {
        body = body.push(
            rich::body(content, message, channel_id, row.revision, cx).unwrap_or_else(|| {
                emoji::inline(
                    content,
                    assets.emojis,
                    |run| Origin::Timeline {
                        channel: channel_id,
                        message: message_id,
                        revision: row.revision,
                        run,
                    },
                    Event::EmojiVisibility,
                )
            }),
        );
    } else if message.embeds.is_empty()
        && let Some(note) = placeholder(message)
    {
        body = body.push(text(note).size(13).style(text::secondary));
    }
    if let Some(snapshot) = message.forwarded() {
        body = body.push(forwarded_view(
            message,
            snapshot,
            channel_id,
            row.revision,
            cx,
            assets,
        ));
    }
    if let Some(thread) = created_thread(message) {
        body = body.push(row![row_button("Open thread", Event::OpenThread(thread))]);
    }
    for embed in rich::embeds(message, channel_id, row.revision, cx) {
        body = body.push(embed);
    }
    if let Some(poll) = message.poll.as_deref() {
        body = body.push(poll_view(row, poll, channel_id));
    }
    for attachment in &message.attachments {
        let key = AttachmentKey {
            account: account_id,
            channel: channel_id,
            message: message_id,
            attachment: attachment.id,
        };
        body = body.push(attachment_view(
            attachment,
            key,
            assets.views.get(&key),
            assets.media,
            message.is_voice_message(),
        ));
    }
    for sticker in &message.sticker_items {
        body = body.push(sticker_view(sticker, assets.avatars));
    }
    if let Some(line) = reactions_line(row, channel_id, snapshot.can_react, assets) {
        body = body.push(line);
    }
    if let Some(users) = snapshot
        .reaction_users
        .as_ref()
        .filter(|users| users.message == message_id)
    {
        body = body.push(users_panel(users));
    }
    let status = |line: &'static str| text(line).size(12).style(text::secondary);

    let retry = row_button(
        "Retry",
        Event::RetryChange {
            channel_id,
            message_id,
        },
    );
    let dismiss = row_button(
        "Dismiss",
        Event::DismissChange {
            channel_id,
            message_id,
        },
    );
    let edit = Event::Edit {
        channel_id,
        message_id,
    };
    body = match state {
        RowState::Plain | RowState::Actions => body,
        RowState::Saving { delete: false } => body.push(status("Saving your edit…")),
        RowState::Saving { delete: true } => body.push(status("Deleting…")),
        RowState::Editing => body.push(status(
            "Editing in the message box below. Enter saves, Escape cancels.",
        )),
        RowState::Confirming => body.push(
            row![
                text("Delete this message? This cannot be undone.")
                    .size(12)
                    .style(text::danger),
                row_button(
                    "Delete",
                    Event::ConfirmDelete {
                        channel_id,
                        message_id,
                    },
                )
                .style(button::danger),
                row_button("Cancel", Event::CancelDelete),
            ]
            .spacing(8)
            .align_y(Vertical::Center),
        ),
        RowState::Failed { delete, reason } => {
            // The reason names the action and whether it may have taken effect.
            let actions = if delete {
                row![retry, dismiss]
            } else {
                row![retry, row_button("Edit", edit.clone()), dismiss]
            };
            body.push(
                text(reason)
                    .size(12)
                    .width(Length::Fill)
                    .wrapping(text::Wrapping::WordOrGlyph)
                    .style(text::danger),
            )
            .push(actions.spacing(8))
        }
    };
    // The avatar sits in a gutter; a message that continues its author's
    // group has an empty gutter instead and no header.
    let gutter: Element<'a, Event> = if continues {
        container(text("")).width(40).into()
    } else {
        let name = author_name(message, assets);
        let key =
            crate::avatars::AvatarKey::user(message.author.id, message.author.avatar.as_deref());
        button(crate::avatars::view(assets.avatars, &key, 40.0, || {
            container(text(style::initial(name)).size(17).font(BOLD))
                .center(40)
                .style(style::avatar(style::avatar_color(message.author.id.0)))
                .into()
        }))
        .padding(0)
        .style(style::plain_button)
        .on_press_with(|| Event::OpenProfile(message.author.clone()))
        .into()
    };
    if continues && message.edited_timestamp.is_some() {
        body = body.push(text("(edited)").size(11).style(text::secondary));
    }
    let inner = row![gutter, body.width(Length::Fill)].spacing(14);
    let base = if row.mentions_me {
        // Tinted row with a gold bar on its left edge, as the official client.
        container(
            row![
                container(text(""))
                    .width(3)
                    .height(Length::Fill)
                    .style(style::mention_bar),
                inner
            ]
            .spacing(13),
        )
        .style(style::mention_row)
    } else {
        container(inner)
    }
    .width(Length::Fill)
    .padding(Padding {
        top: if continues { 1.0 } else { 14.0 },
        right: 16.0,
        bottom: 2.0,
        left: if row.mentions_me { 0.0 } else { 16.0 },
    });
    let can_reply = snapshot.can_send && interaction.editing.is_none();
    let show_own_actions = matches!(state, RowState::Actions | RowState::Failed { .. });
    let idle = matches!(
        state,
        RowState::Plain | RowState::Actions | RowState::Failed { .. }
    );
    let show_reply = can_reply && idle;
    let show_copy = !content.is_empty() && idle;
    let show_react = snapshot.can_react && idle;
    let show_pin = interaction.can_pin && idle;
    // Ordinary messages and replies; re-forwarding a forward is not offered
    // (not established for user accounts).
    let show_forward = idle && matches!(message.kind, 0 | 19) && message.forwarded().is_none();
    // Only ordinary messages can start a thread.
    let show_thread = interaction.can_thread && idle && matches!(message.kind, 0 | 19);
    // Link previews: the author's own, or anyone's with Manage Messages.
    let show_embeds = idle
        && matches!(message.kind, 0 | 19)
        && (row.own || interaction.can_suppress_embeds)
        && (!message.embeds.is_empty() || message.embeds_suppressed());
    // Only other people's ordinary messages (voice messages included) can be
    // reported.
    let show_report = idle && !row.own && matches!(message.kind, 0 | 19);
    if !idle {
        return base.into();
    }
    // Hover controls are layered over the top right so the measured row height
    // does not depend on whether the pointer is on it.
    let mut actions = row![].spacing(4);
    if interaction.more == Some(message_id) {
        actions = actions
            .push(row_button(
                "Copy link",
                Event::CopyLink {
                    channel_id,
                    message_id,
                },
            ))
            .push(row_button("Copy ID", Event::CopyId { message_id }));
        if show_embeds {
            let suppressed = message.embeds_suppressed();
            actions = actions.push(row_button(
                if suppressed {
                    "Show embeds"
                } else {
                    "Hide embeds"
                },
                Event::SetEmbeds {
                    channel_id,
                    message_id,
                    suppressed: !suppressed,
                },
            ));
        }
        if show_report {
            actions = actions.push(row_button(
                "Report",
                Event::Report {
                    channel_id,
                    message_id,
                },
            ));
        }
        actions = actions.push(row_button("Back", Event::More(None)));
        return hover(base, actions_overlay(actions));
    }
    if show_own_actions {
        actions = actions.push(row_button("Edit", edit)).push(row_button(
            "Delete",
            Event::Delete {
                channel_id,
                message_id,
            },
        ));
    }
    if show_reply {
        actions = actions.push(row_button(
            "Reply",
            Event::Reply {
                channel_id,
                message_id,
            },
        ));
    }
    if show_react {
        actions = actions.push(row_button(
            "React",
            Event::React {
                channel_id,
                message_id,
            },
        ));
    }
    if show_pin {
        actions = actions.push(row_button(
            if message.pinned { "Unpin" } else { "Pin" },
            Event::SetPinned {
                channel_id,
                message_id,
                pinned: !message.pinned,
            },
        ));
    }
    if show_forward {
        actions = actions.push(row_button(
            "Forward",
            Event::Forward {
                channel_id,
                message_id,
            },
        ));
    }
    if show_thread {
        actions = actions.push(row_button(
            "Thread",
            Event::StartThread {
                channel_id,
                message_id,
            },
        ));
    }
    if show_copy {
        actions = actions.push(row_button(
            "Copy text",
            Event::CopyText {
                channel_id,
                message_id,
            },
        ));
    }
    actions = actions.push(row_button("More", Event::More(Some(message_id))));
    hover(base, actions_overlay(actions))
}

fn actions_overlay<'a>(actions: iced::widget::Row<'a, Event>) -> Container<'a, Event> {
    container(actions)
        .width(Length::Fill)
        .align_x(Horizontal::Right)
        .padding(Padding {
            top: 4.0,
            right: 12.0,
            bottom: 0.0,
            left: 0.0,
        })
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// A poll: its question, one button per answer with the votes it has and
/// whether the account chose it, and a line saying whether it is still open.
/// The layout does not depend on the clock: an ended poll only disables its
/// buttons, so a row's measured height stays valid when voting closes.
fn poll_view<'a>(
    row: &'a Row,
    poll: &'a fastcord_model::Poll,
    channel_id: Snowflake,
) -> Element<'a, Event> {
    let message_id = row.message.id;
    let ended = poll.ended(now_ms());
    let can_vote = !ended && !row.poll_saving;
    let total = poll.total_votes();
    let mut body = column![
        text(poll.question.text.as_str())
            .font(BOLD)
            .size(15)
            .wrapping(text::Wrapping::WordOrGlyph),
        text(if ended {
            "Final results"
        } else if poll.allow_multiselect {
            "Select one or more answers"
        } else {
            "Select one answer"
        })
        .size(12)
        .style(text::secondary),
    ]
    .spacing(4);
    for answer in &poll.answers {
        let (count, mine) = poll.count(answer.answer_id);
        let percent = (u64::from(count) * 100).checked_div(total).unwrap_or(0);
        let label = format!(
            "{} {}",
            if mine { "[x]" } else { "[ ]" },
            answer.poll_media.text
        );
        let choose = button(text(label).size(13).wrapping(text::Wrapping::WordOrGlyph))
            .width(Length::Fill)
            .padding([3, 8])
            .on_press_maybe(can_vote.then_some(Event::PollVote {
                channel_id,
                message_id,
                answer_id: answer.answer_id,
            }))
            .style(if mine {
                button::primary
            } else {
                button::secondary
            });
        let mut line = row![choose, text(format!("{count} ({percent}%)")).size(12)]
            .spacing(8)
            .align_y(Vertical::Center);
        if count > 0 {
            line = line.push(
                button(text("Voters").size(12))
                    .on_press(Event::PollVoters {
                        channel_id,
                        message_id,
                        answer_id: answer.answer_id,
                    })
                    .padding([2, 8])
                    .style(button::secondary),
            );
        }
        body = body.push(line);
    }
    let status = if ended {
        "Poll ended".to_owned()
    } else {
        match poll.expiry.as_deref() {
            Some(expiry) => format!(
                "Ends {}",
                format_timestamp(expiry, crate::localtime::offset())
            ),
            None => "No end time".to_owned(),
        }
    };
    let mut footer = row![
        text(format!(
            "{total} {} · {status}",
            if total == 1 { "vote" } else { "votes" }
        ))
        .size(12)
        .style(text::secondary)
    ]
    .spacing(8)
    .align_y(Vertical::Center);
    if row.poll_saving {
        footer = footer.push(text("Saving…").size(12).style(text::secondary));
    } else if row.own && !ended {
        footer = footer.push(row_button(
            "End poll",
            Event::PollEnd {
                channel_id,
                message_id,
            },
        ));
    }
    container(body.push(footer))
        .max_width(480)
        .padding(6)
        .style(container::rounded_box)
        .into()
}

/// A forwarded message: labelled, drawn only from the frozen snapshot (never
/// from the original, so edits or deletion there change nothing here) and
/// without naming where it came from.
fn forwarded_view<'a>(
    message: &'a Message,
    snapshot: &'a fastcord_model::SnapshotMessage,
    channel_id: Snowflake,
    revision: u64,
    cx: rich::Cx<'a>,
    assets: Attachments<'a>,
) -> Element<'a, Event> {
    let mut body = column![text("Forwarded").size(12).style(text::secondary)].spacing(2);
    let parts = rich::Parts::snapshot(message, snapshot);
    if !snapshot.content.is_empty() {
        body = body.push(
            rich::body(&snapshot.content, parts, channel_id, revision, cx)
                .unwrap_or_else(|| text(snapshot.content.as_str()).into()),
        );
    }
    for embed in rich::embeds(parts, channel_id, revision, cx) {
        body = body.push(embed);
    }
    for attachment in &snapshot.attachments {
        let key = AttachmentKey {
            account: assets.account,
            channel: channel_id,
            message: message.id,
            attachment: attachment.id,
        };
        body = body.push(attachment_view(
            attachment,
            key,
            assets.views.get(&key),
            assets.media,
            false,
        ));
    }
    for sticker in &snapshot.sticker_items {
        body = body.push(sticker_view(sticker, assets.avatars));
    }
    if snapshot.content.is_empty()
        && snapshot.embeds.is_empty()
        && snapshot.attachments.is_empty()
        && snapshot.sticker_items.is_empty()
    {
        body = body.push(
            text("The forwarded message has no content to show.")
                .size(13)
                .style(text::secondary),
        );
    }
    container(body)
        .padding(Padding {
            top: 2.0,
            right: 0.0,
            bottom: 2.0,
            left: 10.0,
        })
        .style(container::bordered_box)
        .into()
}

/// A sticker: its first frame at 64 px once loaded; otherwise (loading,
/// failed, deleted, Lottie or an unknown format) its name, said honestly.
fn sticker_view<'a>(
    sticker: &StickerItem,
    views: &'a HashMap<crate::avatars::AvatarKey, iced::widget::image::Handle>,
) -> Element<'a, Event> {
    let key = crate::avatars::AvatarKey::sticker(sticker.id, sticker.format_type);
    if let Some(handle) = key.as_ref().and_then(|key| views.get(key)) {
        return image(handle.clone())
            .width(Length::Fixed(64.0))
            .height(Length::Fixed(64.0))
            .into();
    }
    let note = if key.is_none() {
        format!(
            "Sticker: {} (this sticker format is not shown)",
            sticker.name
        )
    } else {
        format!("Sticker: {}", sticker.name)
    };
    text(note).size(13).style(text::secondary).into()
}

fn attachment_view(
    attachment: &Attachment,
    key: AttachmentKey,
    state: Option<&AttachmentView>,
    media: Option<&crate::media_ui::MediaSession>,
    voice_message: bool,
) -> Element<'static, Event> {
    let ids = (key.channel, key.message, key.attachment);
    let mut content = column![
        text(describe_attachment(attachment))
            .size(13)
            .width(Length::Fill)
            .wrapping(text::Wrapping::WordOrGlyph)
            .style(text::secondary),
    ]
    .spacing(4);
    match state {
        Some(AttachmentView::Ready(preview)) => {
            let thumbnail = image(preview.handle.clone())
                .width(Length::Fixed(preview.width as f32))
                .height(Length::Fixed(preview.height as f32));
            content = content.push(button(thumbnail).padding(0).style(button::text).on_press(
                Event::ViewAttachment {
                    channel_id: ids.0,
                    message_id: ids.1,
                    attachment_id: ids.2,
                },
            ));
        }
        Some(AttachmentView::Loading) => {
            content = content.push(
                text("Loading image preview…")
                    .size(12)
                    .style(text::secondary),
            );
        }
        Some(AttachmentView::Failed(reason)) => {
            content = content.push(text(*reason).size(12).style(text::secondary));
        }
        None => {}
    }
    if crate::media_ui::is_playable(attachment) {
        content = content.push(crate::media_ui::view(media, key, attachment, voice_message));
    }
    let mut actions = row![].spacing(4);
    if is_previewable_image(attachment) {
        let label = match state {
            Some(AttachmentView::Loading) => None,
            Some(AttachmentView::Ready(_)) => Some("View"),
            Some(AttachmentView::Failed(_)) => Some("Retry preview"),
            None => Some("Load preview"),
        };
        let event = match state {
            Some(AttachmentView::Ready(_)) => Event::ViewAttachment {
                channel_id: ids.0,
                message_id: ids.1,
                attachment_id: ids.2,
            },
            _ => Event::LoadAttachment {
                channel_id: ids.0,
                message_id: ids.1,
                attachment_id: ids.2,
            },
        };
        if let Some(label) = label {
            actions = actions.push(row_button(label, event));
        }
    }
    actions = actions.push(row_button(
        "Download",
        Event::DownloadAttachment {
            channel_id: ids.0,
            message_id: ids.1,
            attachment_id: ids.2,
        },
    ));
    // Only passive types are openable; everything else is Download-only.
    if crate::attachment_ui::can_open(attachment) {
        actions = actions.push(row_button(
            "Open",
            Event::OpenAttachment {
                channel_id: ids.0,
                message_id: ids.1,
                attachment_id: ids.2,
            },
        ));
    }
    content = content.push(actions);
    container(content).width(Length::Fill).into()
}

/// Whether `row` continues the group of the message before it: the same
/// author within seven minutes, neither a reply or system message, and
/// nothing about either that needs its own header.
fn continues_group(before: &Row, row: &Row) -> bool {
    const WINDOW_MS: u64 = 7 * 60 * 1000;
    if before.hidden.is_some() || row.hidden.is_some() {
        return false;
    }
    let (a, b) = (&*before.message, &*row.message);
    let plain = |m: &Message| matches!(m.kind, 0 | 19) && m.message_reference.is_none();
    a.author.id == b.author.id
        && plain(a)
        && plain(b)
        && row.reply.is_none()
        && row.change.is_none()
        && before.change.is_none()
        && (b.id.0 >> 22).saturating_sub(a.id.0 >> 22) <= WINDOW_MS
}

/// The thread a "thread created" system message (type 18) announces: its
/// reference names the thread, and the content is the thread's name.
fn created_thread(message: &Message) -> Option<Snowflake> {
    (message.kind == 18)
        .then(|| message.message_reference.as_ref()?.channel_id)
        .flatten()
}

/// What to show for a message without text or attachments.
fn placeholder(message: &Message) -> Option<String> {
    if !message.attachments.is_empty()
        || !message.sticker_items.is_empty()
        || message.forwarded().is_some()
        || message.poll.is_some()
    {
        return None;
    }
    Some(match message.kind {
        3 => call_summary(message),
        0 | 19 | 20 | 23 => "No text content.".to_owned(),
        1 => "A member was added to the conversation.".to_owned(),
        2 => "A member left the conversation.".to_owned(),
        4 => "The conversation name was changed.".to_owned(),
        5 => "The conversation icon was changed.".to_owned(),
        6 => "A message was pinned.".to_owned(),
        7 => "A member joined.".to_owned(),
        8..=11 => "The server was boosted.".to_owned(),
        12 => "A channel was followed.".to_owned(),
        18 => "A thread was created.".to_owned(),
        21 => "The message this thread was started from.".to_owned(),
        22 => "An invite reminder was posted.".to_owned(),
        24 => "AutoMod flagged a message.".to_owned(),
        46 => "A poll has ended.".to_owned(),
        kind => format!("Message type {kind} is not displayed by this client."),
    })
}

/// A call message: still going, missed (nobody but the caller joined), or over.
/// Call data absent from the payload reads as a plain call.
fn call_summary(message: &Message) -> String {
    let name = message.author.display_name();
    match &message.call {
        None => format!("{name} started a call."),
        Some(call) if call.ended_timestamp.is_none() => format!("{name} started a call."),
        Some(call)
            if call
                .participants
                .iter()
                .all(|user| *user == message.author.id) =>
        {
            format!("Missed call from {name}.")
        }
        Some(_) => format!("{name} called. The call has ended."),
    }
}

fn describe_attachment(attachment: &Attachment) -> String {
    let mut details = vec![format_size(attachment.size)];
    if let Some(kind) = attachment.content_type.as_deref() {
        details.push(kind.to_owned());
    }
    if let (Some(width), Some(height)) = (attachment.width, attachment.height) {
        details.push(format!("{width}x{height}"));
    }
    format!(
        "Attachment: {} ({})",
        attachment.filename,
        details.join(", ")
    )
}

pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `2026-10-07T11:32:01+00:00` as `2026-10-07 14:32` at `+03:00`. Without a
/// known local offset (`None`) it reads `2026-10-07 11:32 UTC`.
fn format_timestamp(raw: &str, local: Option<time::UtcOffset>) -> String {
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};
    if let (Some(local), Ok(at)) = (local, OffsetDateTime::parse(raw, &Rfc3339)) {
        let at = at.to_offset(local);
        return format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            at.year(),
            at.month() as u8,
            at.day(),
            at.hour(),
            at.minute()
        );
    }
    format_utc(raw)
}

/// Fallback: offsets other than UTC are shown as written; anything that is not
/// ISO 8601 is shown truncated rather than guessed at.
fn format_utc(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    let iso = bytes.len() >= 16
        && digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10)
        && matches!(bytes[10], b'T' | b' ')
        && digits(11..13)
        && bytes[13] == b':'
        && digits(14..16);
    if !iso {
        return raw.chars().take(MAX_TIMESTAMP_CHARS).collect();
    }
    // The first 16 bytes are ASCII, so these slices are on character boundaries.
    let tail = &raw[16..];
    let zone = tail
        .find(['+', '-', 'Z'])
        .map_or("", |at| &tail[at..])
        .chars()
        .take(6)
        .collect::<String>();
    let zone = match zone.as_str() {
        "" | "Z" | "+00:00" | "-00:00" => "UTC",
        other => other,
    };
    format!("{} {} {zone}", &raw[..10], &raw[11..16])
}

#[cfg(test)]
mod tests {
    use fastcord_model::Reaction;

    use super::*;
    use crate::variable_list::harness::Ui;
    use iced_renderer::core::widget::Tree;

    fn message(n: u64, content: &str) -> Arc<Message> {
        Arc::new(
            serde_json::from_value(serde_json::json!({
                "id": n.to_string(),
                "channel_id": "500",
                "author": {"id": "2", "username": "alt", "global_name": "Alt Account"},
                "content": content,
                "timestamp": "2026-10-07T12:00:00.000000+00:00",
                "attachments": [{
                    "id": "9",
                    "filename": "cat.png",
                    "size": 12345,
                    "url": "https://cdn.example/a/cat.png?ex=SIGNED",
                    "content_type": "image/png",
                    "width": 640,
                    "height": 480
                }],
            }))
            .expect("valid message"),
        )
    }

    fn snapshot(rows: u64, history: usize) -> Snapshot {
        let rows: Vec<Row> = (1..=rows)
            .map(|n| Row::new(message(n, "hello world"), 1))
            .collect();
        let built = rows.len();
        Snapshot {
            channel_id: Some(Snowflake(500)),
            window: Window {
                range: 0..built,
                top: 0.0,
                bottom: (history - built) as f32 * 56.0,
                at_bottom: true,
                ..Window::default()
            },
            rows,
            ..Snapshot::default()
        }
    }

    fn nodes(tree: &Tree) -> usize {
        1 + tree.children.iter().map(nodes).sum::<usize>()
    }

    fn no_views() -> &'static HashMap<AttachmentKey, AttachmentView> {
        static EMPTY: std::sync::LazyLock<HashMap<AttachmentKey, AttachmentView>> =
            std::sync::LazyLock::new(HashMap::new);
        &EMPTY
    }

    fn assets(views: &HashMap<AttachmentKey, AttachmentView>) -> Attachments<'_> {
        static EMOJI: std::sync::LazyLock<HashMap<emoji::Asset, emoji::View>> =
            std::sync::LazyLock::new(HashMap::new);
        static REVEALED: std::sync::LazyLock<std::collections::HashSet<(Snowflake, u16)>> =
            std::sync::LazyLock::new(Default::default);
        static AVATARS: std::sync::LazyLock<
            HashMap<crate::avatars::AvatarKey, iced::widget::image::Handle>,
        > = std::sync::LazyLock::new(HashMap::new);
        static MEMBERS: std::sync::LazyLock<
            HashMap<Snowflake, fastcord_discord::state::navigation::MemberView>,
        > = std::sync::LazyLock::new(HashMap::new);
        Attachments {
            account: Snowflake(1),
            views,
            media: None,
            emojis: &EMOJI,
            avatars: &AVATARS,
            revealed: &REVEALED,
            members: &MEMBERS,
        }
    }

    #[test]
    fn authors_are_named_nickname_then_display_name_then_username() {
        let mut message: Message = serde_json::from_str(
            r#"{"id":"5","channel_id":"1","author":{"id":"2","username":"bee","global_name":"Bee"},"content":"x","timestamp":"2026-10-20T10:00:00.000000+00:00"}"#,
        )
        .unwrap();
        let views = no_views();
        let mut members = HashMap::new();
        fn with<'a>(
            members: &'a HashMap<Snowflake, fastcord_discord::state::navigation::MemberView>,
            views: &'a HashMap<AttachmentKey, AttachmentView>,
        ) -> Attachments<'a> {
            Attachments {
                members,
                ..assets(views)
            }
        }
        assert_eq!(author_name(&message, with(&members, views)), "Bee");
        members.insert(
            Snowflake(2),
            fastcord_discord::state::navigation::MemberView {
                nick: Some("Queen".into()),
                roles: Vec::new(),
                ..Default::default()
            },
        );
        assert_eq!(author_name(&message, with(&members, views)), "Queen");
        // A member without a nickname falls back, and so does a bare username.
        members.get_mut(&Snowflake(2)).unwrap().nick = None;
        message.author.global_name = None;
        assert_eq!(author_name(&message, with(&members, views)), "bee");
    }

    fn attachment_key(message: u64) -> AttachmentKey {
        AttachmentKey {
            account: Snowflake(1),
            channel: CHANNEL,
            message: Snowflake(message),
            attachment: Snowflake(9),
        }
    }

    fn ready(width: u32, height: u32) -> AttachmentView {
        AttachmentView::Ready(AttachmentPreview {
            handle: image::Handle::from_rgba(
                width,
                height,
                vec![0x80; width as usize * height as usize * 4],
            ),
            width,
            height,
        })
    }

    fn built_nodes(snapshot: &Snapshot) -> usize {
        let element = view(snapshot, Interaction::default(), assets(no_views()));
        nodes(&Tree::new(&element))
    }

    #[test]
    fn built_widgets_follow_the_rows_not_the_history() {
        // The same ten rows out of a 100- or a 500-message history cost the same.
        assert_eq!(
            built_nodes(&snapshot(10, 100)),
            built_nodes(&snapshot(10, 500))
        );
        // Every extra row costs the same number of widgets.
        let ten = built_nodes(&snapshot(10, 500));
        let twenty = built_nodes(&snapshot(20, 500));
        let thirty = built_nodes(&snapshot(30, 500));
        assert!(twenty > ten);
        assert_eq!(thirty - twenty, twenty - ten);
        // No channel, or none loaded yet, is a placeholder rather than an error.
        assert!(built_nodes(&Snapshot::default()) < ten);
    }

    #[test]
    fn controls_follow_the_snapshot() {
        let mut state = snapshot(5, 5);
        assert_eq!(
            Controls::of(&state),
            Controls {
                empty: None,
                notice: None,
                jump: false
            }
        );
        // Away from the bottom, or with newer messages out of reach, jumping is offered.
        state.window.at_bottom = false;
        assert!(Controls::of(&state).jump);
        state.window.at_bottom = true;
        state.has_newer = true;
        assert!(Controls::of(&state).jump);
        state.has_newer = false;
        // Older messages are offered only near the top and when idle.
        state.has_older = true;
        state.window.offset = 10.0;
        assert_eq!(Controls::of(&state).notice, Some(Notice::LoadOlder));
        state.window.offset = 5_000.0;
        assert_eq!(Controls::of(&state).notice, None);
        state.window.offset = 10.0;
        state.loading = true;
        assert_eq!(Controls::of(&state).notice, Some(Notice::Loading));
        state.error = Some("history failed");
        assert_eq!(
            Controls::of(&state).notice,
            Some(Notice::Failed("history failed"))
        );
        // Nothing loaded yet.
        let mut empty = Snapshot {
            channel_id: Some(Snowflake(500)),
            loading: true,
            ..Snapshot::default()
        };
        assert_eq!(
            Controls::of(&empty),
            Controls {
                empty: Some("Loading messages..."),
                notice: None,
                jump: false
            }
        );
        empty.loading = false;
        assert_eq!(
            Controls::of(&empty).empty,
            Some("There are no messages here yet.")
        );
        empty.error = Some("history failed");
        assert_eq!(
            Controls::of(&empty),
            Controls {
                empty: None,
                notice: Some(Notice::Failed("history failed")),
                jump: false
            }
        );
    }

    #[test]
    fn debug_output_never_contains_bodies_or_signed_urls() {
        let state = Snapshot {
            rows: vec![Row {
                own: true,
                change: Some(RowChange {
                    kind: ChangeKind::Edit(Arc::from("private edit")),
                    state: ChangeState::Saving,
                }),
                ..Row::new(message(7, "private conversation"), 3)
            }],
            ..snapshot(0, 1)
        };
        let printed = format!("{state:?} {:?}", state.rows[0]);
        assert!(!printed.contains("private"), "{printed}");
        assert!(
            !printed.contains("SIGNED") && !printed.contains("cdn.example"),
            "{printed}"
        );
        assert!(printed.contains('7'), "ids still identify rows: {printed}");
    }

    #[test]
    fn events_name_their_channel() {
        let event = Event::Measured {
            channel_id: Snowflake(500),
            measurement: Measurement {
                id: Snowflake(1),
                revision: 1,
                width_bucket: 20,
                height: 80.0,
            },
        };
        assert_eq!(event, event);
        assert_ne!(
            Event::JumpLatest {
                channel_id: Snowflake(500)
            },
            Event::JumpLatest {
                channel_id: Snowflake(501)
            }
        );
    }

    #[test]
    fn timestamps_read_in_local_time_or_as_utc() {
        assert_eq!(
            format_timestamp("2026-10-07T11:32:01.508000+00:00", None),
            "2026-10-07 11:32 UTC"
        );
        assert_eq!(
            format_timestamp("2026-10-07T11:32:01Z", None),
            "2026-10-07 11:32 UTC"
        );
        assert_eq!(
            format_timestamp("2026-10-07T11:32:01.5+02:00", None),
            "2026-10-07 11:32 +02:00"
        );
        assert_eq!(
            format_timestamp("2026-10-07T11:32", None),
            "2026-10-07 11:32 UTC"
        );
        for odd in ["", "yesterday", "2026-13", "２０２６-10-07T11:32:01+00:00"] {
            assert_eq!(format_timestamp(odd, None), odd);
        }
        let long = "x".repeat(200);
        assert_eq!(
            format_timestamp(&long, None).chars().count(),
            MAX_TIMESTAMP_CHARS
        );
        let plus3 = time::UtcOffset::from_hms(3, 0, 0).ok();
        let local = |raw| format_timestamp(raw, plus3);
        assert_eq!(local("2026-10-21T09:38:01.5+00:00"), "2026-10-21 12:38");
        assert_eq!(local("2026-10-21T22:30:00Z"), "2026-10-22 01:30");
        assert_eq!(local("2026-10-21T11:00:00+02:00"), "2026-10-21 12:00");
        assert_eq!(local("yesterday"), "yesterday");
    }

    #[test]
    fn stickers_show_a_picture_when_loaded_and_say_so_when_not_renderable() {
        use crate::avatars::AvatarKey;
        let png = StickerItem {
            id: Snowflake(3),
            name: "wave".to_owned(),
            format_type: 1,
        };
        let lottie = StickerItem {
            format_type: 3,
            ..png.clone()
        };
        let mut views = HashMap::new();
        let _ = sticker_view(&png, &views);
        views.insert(
            AvatarKey::sticker(Snowflake(3), 1).unwrap(),
            iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
        );
        let _ = sticker_view(&png, &views);
        let _ = sticker_view(&lottie, &views);
    }

    #[test]
    fn sizes_and_attachments_are_described() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert!(format_size(u64::MAX).ends_with(" TB"));
        let message = message(1, "");
        assert_eq!(
            describe_attachment(&message.attachments[0]),
            "Attachment: cat.png (12.1 KB, image/png, 640x480)"
        );
        // Text-less messages say so unless an attachment explains the row.
        assert_eq!(placeholder(&message), None);
        let mut bare = (*message).clone();
        bare.attachments.clear();
        assert_eq!(placeholder(&bare).as_deref(), Some("No text content."));
        bare.sticker_items = vec![StickerItem {
            id: Snowflake(3),
            name: "wave".to_owned(),
            format_type: 3,
        }];
        assert_eq!(placeholder(&bare), None, "a sticker explains the row");
        bare.sticker_items.clear();
        bare.kind = 7;
        assert_eq!(placeholder(&bare).as_deref(), Some("A member joined."));
        bare.kind = 3;
        assert_eq!(
            placeholder(&bare).as_deref(),
            Some("Alt Account started a call.")
        );
        bare.call = Some(fastcord_model::MessageCall {
            participants: vec![bare.author.id],
            ended_timestamp: Some("2026-10-07T12:01:00.000000+00:00".to_owned()),
        });
        assert_eq!(
            placeholder(&bare).as_deref(),
            Some("Missed call from Alt Account.")
        );
        bare.call.as_mut().unwrap().participants.push(Snowflake(77));
        assert_eq!(
            placeholder(&bare).as_deref(),
            Some("Alt Account called. The call has ended.")
        );
        bare.kind = 250;
        assert_eq!(
            placeholder(&bare).as_deref(),
            Some("Message type 250 is not displayed by this client.")
        );
    }

    #[test]
    fn real_text_is_laid_out_at_any_width_and_measured_once() {
        let long = "A long message that must wrap onto several lines when the window is narrow. "
            .repeat(8);
        let rows: Vec<Row> = (1..=12)
            .map(|n| Row::new(message(n, if n % 3 == 0 { "short" } else { &long }), 1))
            .collect();
        let mut totals = Vec::new();
        for width in [320.0, 640.0, 1100.0] {
            let snapshot = Snapshot {
                channel_id: Some(Snowflake(500)),
                window: Window {
                    range: 0..rows.len(),
                    height: 500.0,
                    width_bucket: variable_list::width_bucket(width),
                    at_bottom: true,
                    pinned: true,
                    ..Window::default()
                },
                rows: rows.clone(),
                ..Snapshot::default()
            };
            let mut ui = Ui::new(
                width,
                500.0,
                view(&snapshot, Interaction::default(), assets(no_views())),
            );
            let events = ui.redraw();
            ui.draw();
            let mut viewport = None;
            let mut heights = Vec::new();
            for event in events {
                match event {
                    Event::Viewport {
                        channel_id,
                        viewport: reported,
                    } => {
                        assert_eq!(channel_id, Snowflake(500));
                        viewport = Some(reported);
                    }
                    Event::Measured {
                        channel_id,
                        measurement,
                    } => {
                        assert_eq!(channel_id, Snowflake(500));
                        assert_eq!(measurement.revision, 1);
                        assert_eq!(measurement.width_bucket, variable_list::width_bucket(width));
                        assert!(measurement.height.is_finite() && measurement.height > 0.0);
                        heights.push((measurement.id, measurement.height));
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
            let viewport = viewport.expect("the first redraw reports the viewport");
            assert_eq!((viewport.width, viewport.height), (width, 500.0));
            assert!(
                viewport.at_bottom,
                "a pinned timeline opens at the newest message"
            );
            // One height per built row, in order, reported once; an idle window is silent.
            assert_eq!(
                heights.iter().map(|(id, _)| id.0).collect::<Vec<_>>(),
                (1..=12).collect::<Vec<_>>()
            );
            assert!(ui.redraw().is_empty());
            totals.push(heights.iter().map(|(_, height)| height).sum::<f32>());
        }
        // Narrower rows wrap onto more lines, never fewer.
        assert!(
            totals[0] >= totals[1] && totals[1] >= totals[2],
            "{totals:?}"
        );
    }

    const CHANNEL: Snowflake = Snowflake(500);

    fn own(row: Row, change: Option<RowChange>) -> Row {
        Row {
            own: true,
            change,
            ..row
        }
    }

    fn failed(kind: ChangeKind) -> Option<RowChange> {
        Some(RowChange {
            kind,
            state: ChangeState::Failed("refused"),
        })
    }

    /// Someone else's message 1, the user's message 2, and the user's message
    /// 3 whose edit failed.
    fn mixed() -> Snapshot {
        let rows = vec![
            Row::new(message(1, "theirs"), 1),
            own(Row::new(message(2, "mine"), 1), None),
            own(
                Row::new(message(3, "mine too"), 1),
                failed(ChangeKind::Edit(Arc::from("mine, fixed"))),
            ),
        ];
        Snapshot {
            channel_id: Some(CHANNEL),
            window: Window {
                range: 0..rows.len(),
                height: 400.0,
                at_bottom: true,
                ..Window::default()
            },
            rows,
            ..Snapshot::default()
        }
    }

    /// Everything a user can trigger by pointing at and clicking anywhere in
    /// the timeline, in first-found order, without layout reports.
    fn clickable(snapshot: &Snapshot, interaction: Interaction) -> Vec<Event> {
        clickable_with(snapshot, interaction, no_views())
    }

    fn clickable_with(
        snapshot: &Snapshot,
        interaction: Interaction,
        views: &HashMap<AttachmentKey, AttachmentView>,
    ) -> Vec<Event> {
        let mut ui = Ui::new(640.0, 400.0, view(snapshot, interaction, assets(views)));
        ui.redraw();
        ui.draw();
        let mut found = Vec::new();
        for y in (0..400).step_by(5) {
            for x in (0..640).step_by(5) {
                for event in ui.click(x as f32, y as f32) {
                    if !matches!(event, Event::Viewport { .. } | Event::Measured { .. })
                        && !found.contains(&event)
                    {
                        found.push(event);
                    }
                }
            }
        }
        found
    }

    #[test]
    fn attachment_actions_are_clickable_in_the_timeline() {
        let row = Row::new(message(42, "image"), 1);
        let snapshot = Snapshot {
            channel_id: Some(CHANNEL),
            rows: vec![row],
            window: Window {
                range: 0..1,
                height: 400.0,
                at_bottom: true,
                ..Window::default()
            },
            ..Snapshot::default()
        };
        let found = clickable(&snapshot, Interaction::default());
        for event in [
            Event::LoadAttachment {
                channel_id: CHANNEL,
                message_id: Snowflake(42),
                attachment_id: Snowflake(9),
            },
            Event::DownloadAttachment {
                channel_id: CHANNEL,
                message_id: Snowflake(42),
                attachment_id: Snowflake(9),
            },
            Event::OpenAttachment {
                channel_id: CHANNEL,
                message_id: Snowflake(42),
                attachment_id: Snowflake(9),
            },
        ] {
            assert!(found.contains(&event), "{event:?} in {found:?}");
        }
    }

    fn poll_row(finalized: bool, expiry: &str, own: bool, saving: bool) -> Row {
        let message: Message = serde_json::from_value(serde_json::json!({
            "id": "42",
            "channel_id": "500",
            "author": {"id": "2", "username": "alt"},
            "content": "",
            "timestamp": "2026-10-07T12:00:00.000000+00:00",
            "poll": {
                "question": {"text": "Lunch?"},
                "answers": [
                    {"answer_id": 1, "poll_media": {"text": "Pizza"}},
                    {"answer_id": 2, "poll_media": {"text": "Soup"}}
                ],
                "expiry": expiry,
                "results": {"is_finalized": finalized, "answer_counts": [
                    {"id": 1, "count": 3, "me_voted": true}
                ]}
            }
        }))
        .unwrap();
        let mut row = Row::new(Arc::new(message), 1);
        row.own = own;
        row.poll_saving = saving;
        row
    }

    fn poll_events(row: Row) -> Vec<Event> {
        let mut snapshot = one_row(42);
        snapshot.rows = vec![row];
        snapshot.window.range = 0..1;
        clickable(&snapshot, Interaction::default())
    }

    #[test]
    fn an_open_poll_offers_votes_voters_and_ending_only_to_its_owner() {
        let vote = |answer_id| Event::PollVote {
            channel_id: CHANNEL,
            message_id: Snowflake(42),
            answer_id,
        };
        let end = Event::PollEnd {
            channel_id: CHANNEL,
            message_id: Snowflake(42),
        };
        let voters = |answer_id| Event::PollVoters {
            channel_id: CHANNEL,
            message_id: Snowflake(42),
            answer_id,
        };
        let theirs = poll_events(poll_row(false, "2999-01-01T00:00:00Z", false, false));
        assert!(
            theirs.contains(&vote(1)) && theirs.contains(&vote(2)),
            "{theirs:?}"
        );
        assert!(
            theirs.contains(&voters(1)),
            "an answer with votes lists them"
        );
        assert!(!theirs.contains(&voters(2)), "an empty answer has no list");
        assert!(!theirs.contains(&end));
        let mine = poll_events(poll_row(false, "2999-01-01T00:00:00Z", true, false));
        assert!(mine.contains(&end), "{mine:?}");
        // While a request is unanswered nothing more can be asked of the poll.
        let saving = poll_events(poll_row(false, "2999-01-01T00:00:00Z", true, true));
        assert!(
            !saving.contains(&vote(1)) && !saving.contains(&end),
            "{saving:?}"
        );
        // An ended poll (finalized, or past its time) only shows results.
        for row in [
            poll_row(true, "2999-01-01T00:00:00Z", true, false),
            poll_row(false, "2000-01-01T00:00:00Z", true, false),
        ] {
            let ended = poll_events(row);
            assert!(
                !ended.contains(&vote(1)) && !ended.contains(&end),
                "{ended:?}"
            );
            assert!(ended.contains(&voters(1)));
        }
        // A poll message has no "no text" placeholder.
        assert!(placeholder(&poll_row(false, "", false, false).message).is_none());
    }

    fn forwarded_row(message_id: u64) -> Row {
        let forward: Message = serde_json::from_value(serde_json::json!({
            "id": message_id.to_string(),
            "channel_id": "500",
            "author": {"id": "2", "username": "alt"},
            "content": "",
            "timestamp": "2026-10-07T12:00:00.000000+00:00",
            "message_reference": {"type": 1, "message_id": "77", "channel_id": "88"},
            "message_snapshots": [{"message": {
                "content": "**frozen**",
                "timestamp": "2026-10-06T12:00:00.000000+00:00",
                "attachments": [{
                    "id": "31", "filename": "dog.png", "size": 5,
                    "url": "https://cdn.example/a/dog.png?ex=SIGNED"
                }]
            }}]
        }))
        .unwrap();
        Row::new(Arc::new(forward), 1)
    }

    #[test]
    fn a_forward_shows_its_snapshot_offers_its_media_but_not_a_second_forward() {
        let mut snapshot = one_row(1);
        snapshot.rows = vec![forwarded_row(42), Row::new(message(43, "plain"), 1)];
        snapshot.window.range = 0..2;
        let found = clickable(&snapshot, Interaction::default());
        // The snapshot's attachment is actionable under the forwarding message.
        assert!(
            found.contains(&Event::LoadAttachment {
                channel_id: CHANNEL,
                message_id: Snowflake(42),
                attachment_id: Snowflake(31),
            }),
            "{found:?}"
        );
        let forward = |id: u64| Event::Forward {
            channel_id: CHANNEL,
            message_id: Snowflake(id),
        };
        assert!(found.contains(&forward(43)), "{found:?}");
        assert!(!found.contains(&forward(42)), "{found:?}");
        assert!(placeholder(&forwarded_row(42).message).is_none());
        // The original (77 in channel 88) is never a jump or reply target.
        assert!(!found.iter().any(|e| matches!(e, Event::JumpTo { .. })));
    }

    fn one_row(message_id: u64) -> Snapshot {
        Snapshot {
            channel_id: Some(CHANNEL),
            rows: vec![Row::new(message(message_id, "image"), 1)],
            window: Window {
                range: 0..1,
                height: 400.0,
                at_bottom: true,
                ..Window::default()
            },
            ..Snapshot::default()
        }
    }

    #[test]
    fn a_loaded_thumbnail_opens_the_larger_view_and_keeps_download_and_open() {
        let views = HashMap::from([(attachment_key(42), ready(120, 90))]);
        let found = clickable_with(&one_row(42), Interaction::default(), &views);
        let view_event = Event::ViewAttachment {
            channel_id: CHANNEL,
            message_id: Snowflake(42),
            attachment_id: Snowflake(9),
        };
        assert!(found.contains(&view_event), "{found:?}");
        // The thumbnail (and the View button) replaced Load preview.
        assert!(!found.contains(&Event::LoadAttachment {
            channel_id: CHANNEL,
            message_id: Snowflake(42),
            attachment_id: Snowflake(9),
        }));
        for event in [
            Event::DownloadAttachment {
                channel_id: CHANNEL,
                message_id: Snowflake(42),
                attachment_id: Snowflake(9),
            },
            Event::OpenAttachment {
                channel_id: CHANNEL,
                message_id: Snowflake(42),
                attachment_id: Snowflake(9),
            },
        ] {
            assert!(found.contains(&event), "{event:?} in {found:?}");
        }
    }

    #[test]
    fn only_decodable_images_offer_a_preview_other_files_keep_download_and_open() {
        let mut item = message(1, "").attachments[0].clone();
        assert!(is_previewable_image(&item));
        // Video carries dimensions too, but there is no decoder for it here.
        item.content_type = Some("video/mp4".to_owned());
        item.filename = "clip.mp4".to_owned();
        assert!(!is_previewable_image(&item));
        item.content_type = Some("image/svg+xml".to_owned());
        assert!(!is_previewable_image(&item));
        item.content_type = Some("IMAGE/JPEG; charset=binary".to_owned());
        assert!(is_previewable_image(&item));
        item.content_type = None;
        item.filename = "photo.JPG".to_owned();
        assert!(is_previewable_image(&item));
        item.filename = "notes.pdf".to_owned();
        assert!(!is_previewable_image(&item));

        let mut video = (*message(42, "")).clone();
        video.attachments[0].content_type = Some("video/mp4".to_owned());
        video.attachments[0].filename = "clip.mp4".to_owned();
        let snapshot = Snapshot {
            rows: vec![Row::new(Arc::new(video), 1)],
            ..one_row(42)
        };
        let found = clickable(&snapshot, Interaction::default());
        assert!(!found.iter().any(|event| matches!(
            event,
            Event::LoadAttachment { .. } | Event::ViewAttachment { .. }
        )));
        assert!(
            found
                .iter()
                .any(|event| matches!(event, Event::DownloadAttachment { .. }))
        );
        assert!(
            found
                .iter()
                .any(|event| matches!(event, Event::OpenAttachment { .. }))
        );
    }

    fn clickable_media(
        snapshot: &Snapshot,
        media: Option<&crate::media_ui::MediaSession>,
    ) -> Vec<Event> {
        let assets = Attachments {
            media,
            ..assets(no_views())
        };
        let mut ui = Ui::new(640.0, 400.0, view(snapshot, Interaction::default(), assets));
        ui.redraw();
        ui.draw();
        let mut found = Vec::new();
        for y in (0..400).step_by(5) {
            for x in (0..640).step_by(5) {
                for event in ui.click(x as f32, y as f32) {
                    if !matches!(event, Event::Viewport { .. } | Event::Measured { .. })
                        && !found.contains(&event)
                    {
                        found.push(event);
                    }
                }
            }
        }
        found
    }

    #[test]
    fn playable_attachments_offer_inline_playback_with_controls_that_follow_the_session() {
        use crate::media_ui::{Control, MediaSession, Phase};
        let snapshot_for = |name: &str, kind: &str| {
            let mut item = (*message(42, "")).clone();
            item.attachments[0].content_type = Some(kind.to_owned());
            item.attachments[0].filename = name.to_owned();
            Snapshot {
                rows: vec![Row::new(Arc::new(item), 1)],
                ..one_row(42)
            }
        };
        let play = Event::PlayAttachment {
            channel_id: CHANNEL,
            message_id: Snowflake(42),
            attachment_id: Snowflake(9),
        };
        let voice = snapshot_for("voice-message.ogg", "audio/ogg");
        assert!(clickable_media(&voice, None).contains(&play));
        // MP4 and WebM video get the same Play button.
        for (name, kind) in [("a.mp4", "video/mp4"), ("a.webm", "video/webm")] {
            assert!(
                clickable_media(&snapshot_for(name, kind), None).contains(&play),
                "{name}"
            );
        }
        // Other audio and video formats stay Download/Open-only.
        for (name, kind) in [
            ("a.mp3", "audio/mpeg"),
            ("a.avi", "video/x-msvideo"),
            ("a.m4a", "audio/mp4"),
        ] {
            let found = clickable_media(&snapshot_for(name, kind), None);
            assert!(
                !found
                    .iter()
                    .any(|e| matches!(e, Event::PlayAttachment { .. } | Event::Media(_))),
                "{name}"
            );
        }
        let key = attachment_key(42);
        let mut media = MediaSession::for_test(key);
        let loading = clickable_media(&voice, Some(&media));
        assert!(loading.contains(&Event::Media(Control::Close)));
        assert!(!loading.contains(&play));
        media.phase = Phase::Failed("x");
        assert!(clickable_media(&voice, Some(&media)).contains(&play));
        media.phase = Phase::Active;
        media.snapshot.duration_ms = Some(60_000);
        let active = clickable_media(&voice, Some(&media));
        for control in [Control::TogglePause, Control::ToggleMute, Control::Close] {
            assert!(active.contains(&Event::Media(control)), "{control:?}");
        }
        assert!(!active.contains(&play));
        // A session for another attachment does not leak controls into this one.
        let mut other = key;
        other.attachment = Snowflake(10);
        let elsewhere = MediaSession::for_test(other);
        assert!(clickable_media(&voice, Some(&elsewhere)).contains(&play));
    }

    #[test]
    fn a_blocked_file_type_offers_download_but_no_open() {
        let mut file = (*message(42, "")).clone();
        file.attachments[0].content_type = Some("application/pdf".to_owned());
        file.attachments[0].filename = "report.pdf.exe".to_owned();
        let snapshot = Snapshot {
            rows: vec![Row::new(Arc::new(file), 1)],
            ..one_row(42)
        };
        let found = clickable(&snapshot, Interaction::default());
        assert!(
            found
                .iter()
                .any(|event| matches!(event, Event::DownloadAttachment { .. }))
        );
        assert!(
            !found
                .iter()
                .any(|event| matches!(event, Event::OpenAttachment { .. }))
        );
    }

    #[test]
    fn a_thumbnail_arriving_grows_the_row_and_its_new_height_is_measured() {
        let snapshot = one_row(42);
        let none = HashMap::new();
        let loaded = HashMap::from([(attachment_key(42), ready(300, 220))]);
        let mut ui = Ui::new(
            640.0,
            400.0,
            view(&snapshot, Interaction::default(), assets(&none)),
        );
        let measured = |events: Vec<Event>| -> Vec<(u64, u64, f32)> {
            events
                .into_iter()
                .filter_map(|event| match event {
                    Event::Measured { measurement, .. } => {
                        Some((measurement.id.0, measurement.revision, measurement.height))
                    }
                    _ => None,
                })
                .collect()
        };
        let before = measured(ui.redraw());
        assert_eq!(before.len(), 1);
        assert!(ui.redraw().is_empty(), "an idle row is silent");
        // The same message and revision, now with a decoded image under it.
        ui.show(view(&snapshot, Interaction::default(), assets(&loaded)));
        let after = measured(ui.redraw());
        assert_eq!(after.len(), 1, "the changed height is published once");
        assert_eq!((after[0].0, after[0].1), (before[0].0, before[0].1));
        assert!(
            after[0].2 >= before[0].2 + 220.0,
            "{} -> {}",
            before[0].2,
            after[0].2
        );
        assert!(ui.redraw().is_empty());
    }

    #[test]
    fn copying_inline_assets_returns_exact_original_source_only_for_a_loaded_channel() {
        let source = "👩🏽‍💻 👍🏻 🇺🇦 <:sample:12345> <a:party:45678>";
        let mut snapshot = one_row(42);
        snapshot.rows[0].message = message(42, source);
        assert_eq!(copy_text(&snapshot, CHANNEL, Snowflake(42)), Some(source));
        assert_eq!(copy_text(&snapshot, Snowflake(501), Snowflake(42)), None);
        assert_eq!(copy_text(&snapshot, CHANNEL, Snowflake(99)), None);
        assert!(
            clickable(&snapshot, Interaction::default()).contains(&Event::CopyText {
                channel_id: CHANNEL,
                message_id: Snowflake(42),
            })
        );
    }

    fn names(event: &Event, id: u64) -> bool {
        let message = Snowflake(id);
        matches!(
            *event,
            Event::Edit { message_id, .. }
                | Event::Delete { message_id, .. }
                | Event::ConfirmDelete { message_id, .. }
                | Event::RetryChange { message_id, .. }
                | Event::DismissChange { message_id, .. }
                if message_id == message
        )
    }

    #[test]
    fn only_the_users_own_messages_offer_edit_and_delete() {
        let found = clickable(&mixed(), Interaction::default());
        let at = |message_id: u64| (CHANNEL, Snowflake(message_id));
        let edit = |(channel_id, message_id)| Event::Edit {
            channel_id,
            message_id,
        };
        let delete = |(channel_id, message_id)| Event::Delete {
            channel_id,
            message_id,
        };
        assert!(found.contains(&edit(at(2))), "{found:?}");
        assert!(found.contains(&delete(at(2))), "{found:?}");
        // A failed edit offers Retry and Dismiss, and the row stays editable.
        for event in [
            edit(at(3)),
            delete(at(3)),
            Event::RetryChange {
                channel_id: CHANNEL,
                message_id: Snowflake(3),
            },
            Event::DismissChange {
                channel_id: CHANNEL,
                message_id: Snowflake(3),
            },
        ] {
            assert!(found.contains(&event), "{event:?} in {found:?}");
        }
        // No Edit or Delete acts on someone else's messages, and deletion
        // needs a confirmation first.
        assert!(!found.iter().any(|event| names(event, 1)), "{found:?}");
        assert!(
            !found
                .iter()
                .any(|event| matches!(event, Event::ConfirmDelete { .. }))
        );
    }

    #[test]
    fn replies_are_offered_for_other_and_own_messages_only_when_sendable() {
        let mut snapshot = mixed();
        snapshot.can_send = true;
        let found = clickable(&snapshot, Interaction::default());
        for message_id in [1, 2, 3] {
            assert!(
                found.contains(&Event::Reply {
                    channel_id: CHANNEL,
                    message_id: Snowflake(message_id),
                }),
                "{found:?}"
            );
        }

        snapshot.can_send = false;
        let found = clickable(&snapshot, Interaction::default());
        assert!(
            !found
                .iter()
                .any(|event| matches!(event, Event::Reply { .. }))
        );
    }

    #[test]
    fn reply_previews_jump_to_the_exact_target_except_when_deleted() {
        let line = |state| {
            Some(ReplyLine {
                target: Snowflake(42),
                author: None,
                state,
            })
        };
        let mut snapshot = mixed();
        snapshot.rows = vec![Row {
            reply: line(ReplyState::Message {
                author: "Alice".to_owned(),
                content: "quoted text".to_owned(),
            }),
            ..Row::new(message(1, "answer"), 1)
        }];
        snapshot.window = Window {
            range: 0..1,
            ..snapshot.window
        };
        for state in [
            ReplyState::Message {
                author: "Alice".to_owned(),
                content: "quoted text".to_owned(),
            },
            ReplyState::Loading,
            ReplyState::Unavailable,
        ] {
            snapshot.rows[0].reply = line(state);
            assert!(
                clickable(&snapshot, Interaction::default()).contains(&Event::JumpTo {
                    channel_id: CHANNEL,
                    message_id: Snowflake(42),
                })
            );
        }
        snapshot.rows[0].reply = line(ReplyState::Deleted);
        assert!(
            !clickable(&snapshot, Interaction::default())
                .iter()
                .any(|event| matches!(event, Event::JumpTo { .. }))
        );
    }

    #[test]
    fn pin_actions_need_the_permission() {
        let is_pin = |event: &Event| matches!(event, Event::SetPinned { pinned: true, .. });
        let without = clickable(&mixed(), Interaction::default());
        assert!(!without.iter().any(is_pin));
        let with = clickable(
            &mixed(),
            Interaction {
                can_pin: true,
                ..Interaction::default()
            },
        );
        assert!(with.iter().any(is_pin));
    }

    #[test]
    fn secondary_actions_copy_link_and_id_and_gate_link_previews() {
        let id = |n: u64| Snowflake(n);
        let usual = clickable(&mixed(), Interaction::default());
        assert!(usual.contains(&Event::More(Some(id(1)))));
        assert!(
            !usual
                .iter()
                .any(|e| matches!(e, Event::CopyLink { .. } | Event::CopyId { .. }))
        );

        // A previewed message of theirs (1) and of the account's (2).
        let embed: fastcord_model::Embed =
            serde_json::from_value(serde_json::json!({"title": "t"})).unwrap();
        let mut snapshot = mixed();
        for at in [0, 1] {
            let mut with = (*snapshot.rows[at].message).clone();
            with.embeds.push(embed.clone());
            snapshot.rows[at].message = Arc::new(with);
        }
        let more = |n: u64, can: bool| Interaction {
            more: Some(id(n)),
            can_suppress_embeds: can,
            ..Interaction::default()
        };
        let hide = |n: u64, suppressed: bool| Event::SetEmbeds {
            channel_id: CHANNEL,
            message_id: id(n),
            suppressed,
        };
        let found = clickable(&snapshot, more(1, false));
        assert!(found.contains(&Event::CopyLink {
            channel_id: CHANNEL,
            message_id: id(1)
        }));
        assert!(found.contains(&Event::CopyId { message_id: id(1) }));
        assert!(found.contains(&Event::More(None)));
        assert!(!found.contains(&hide(1, true)), "not theirs to change");
        assert!(clickable(&snapshot, more(1, true)).contains(&hide(1, true)));
        assert!(clickable(&snapshot, more(2, false)).contains(&hide(2, true)));
        // Hidden previews offer the way back, even though none are shown.
        let mut hidden = (*snapshot.rows[1].message).clone();
        hidden.embeds.clear();
        hidden.flags |= fastcord_model::SUPPRESS_EMBEDS_FLAG;
        snapshot.rows[1].message = Arc::new(hidden);
        assert!(clickable(&snapshot, more(2, false)).contains(&hide(2, false)));
        // A message without previews has nothing to hide.
        assert!(
            !clickable(&mixed(), more(2, true))
                .iter()
                .any(|e| matches!(e, Event::SetEmbeds { .. }))
        );
    }

    #[test]
    fn only_other_peoples_messages_offer_a_report_and_it_names_what_is_held() {
        let report = |n: u64| Event::Report {
            channel_id: CHANNEL,
            message_id: Snowflake(n),
        };
        let more = |n: u64| Interaction {
            more: Some(Snowflake(n)),
            ..Interaction::default()
        };
        // Never in the usual row: it is a deliberate step behind More.
        assert!(!clickable(&mixed(), Interaction::default()).contains(&report(1)));
        assert!(clickable(&mixed(), more(1)).contains(&report(1)));
        assert!(!clickable(&mixed(), more(2)).contains(&report(2)), "own");
        let snapshot = mixed();
        let subject = report_subject(&snapshot, CHANNEL, Snowflake(1)).unwrap();
        assert!(!subject.voice && subject.excerpt.chars().count() <= REPORT_EXCERPT_CHARS);
        assert!(
            report_subject(&snapshot, CHANNEL, Snowflake(2)).is_none(),
            "own"
        );
        assert!(report_subject(&snapshot, CHANNEL, Snowflake(99)).is_none());
        // Deleted or evicted: known only for the channel that is open.
        assert_eq!(is_retained(&snapshot, CHANNEL, Snowflake(1)), Some(true));
        assert_eq!(is_retained(&snapshot, CHANNEL, Snowflake(99)), Some(false));
        assert_eq!(is_retained(&snapshot, Snowflake(12345), Snowflake(1)), None);
    }

    #[test]
    fn starting_a_thread_needs_the_permission_and_an_ordinary_message() {
        let is_start = |event: &Event| matches!(event, Event::StartThread { .. });
        assert!(
            !clickable(&mixed(), Interaction::default())
                .iter()
                .any(is_start)
        );
        let allowed = Interaction {
            can_thread: true,
            ..Interaction::default()
        };
        assert!(clickable(&mixed(), allowed).iter().any(is_start));
        // A system message cannot start one.
        let mut system = one_row(7);
        let mut joined = (*system.rows[0].message).clone();
        joined.kind = 7;
        system.rows[0].message = Arc::new(joined);
        assert!(!clickable(&system, allowed).iter().any(is_start));
    }

    #[test]
    fn a_thread_created_row_opens_the_thread_its_reference_names() {
        let mut row = one_row(8);
        let mut created = (*row.rows[0].message).clone();
        created.kind = 18;
        created.content = "release plans".to_owned();
        created.attachments.clear();
        assert_eq!(created_thread(&created), None, "no reference, no thread");
        created.message_reference = Some(fastcord_model::MessageReference {
            kind: 0,
            message_id: None,
            channel_id: Some(Snowflake(900)),
            guild_id: None,
        });
        assert_eq!(created_thread(&created), Some(Snowflake(900)));
        row.rows[0].message = Arc::new(created.clone());
        assert!(
            clickable(&row, Interaction::default()).contains(&Event::OpenThread(Snowflake(900)))
        );
        created.kind = 0;
        assert_eq!(created_thread(&created), None, "only type 18 announces");
    }

    #[test]
    fn deleting_needs_a_confirmation_in_the_row() {
        let found = clickable(
            &mixed(),
            Interaction {
                confirming: Some(Snowflake(2)),
                editing: Some(Snowflake(3)),
                replying: None,
                can_pin: false,
                can_thread: false,
                ..Default::default()
            },
        );
        assert!(found.contains(&Event::ConfirmDelete {
            channel_id: CHANNEL,
            message_id: Snowflake(2),
        }));
        assert!(found.contains(&Event::CancelDelete));
        // While confirming or editing, a row offers nothing else.
        assert!(!found.iter().any(|event| matches!(
            event,
            Event::Edit { .. } | Event::Delete { .. } | Event::RetryChange { .. }
        )));
    }

    #[test]
    fn a_change_being_saved_offers_nothing_and_the_row_says_so() {
        let saving = |kind| {
            own(
                Row::new(message(2, "mine"), 1),
                Some(RowChange {
                    kind,
                    state: ChangeState::Saving,
                }),
            )
        };
        let ui = Interaction {
            confirming: Some(Snowflake(2)),
            editing: Some(Snowflake(2)),
            replying: None,
            can_pin: false,
            can_thread: false,
            ..Default::default()
        };
        let edit = saving(ChangeKind::Edit(Arc::from("new")));
        assert_eq!(row_state(&edit, ui), RowState::Saving { delete: false });
        let delete = saving(ChangeKind::Delete);
        assert_eq!(row_state(&delete, ui), RowState::Saving { delete: true });
        let snapshot = Snapshot {
            rows: vec![delete],
            ..mixed()
        };
        let snapshot = Snapshot {
            window: Window {
                range: 0..1,
                ..snapshot.window.clone()
            },
            ..snapshot
        };
        // Attachment buttons and the author's profile stay (they act on the
        // file or the person, not the message), but nothing that changes or
        // answers the message is offered.
        assert!(
            clickable(&snapshot, Interaction::default())
                .iter()
                .all(|event| matches!(
                    event,
                    Event::OpenProfile(_)
                        | Event::LoadAttachment { .. }
                        | Event::ViewAttachment { .. }
                        | Event::DownloadAttachment { .. }
                        | Event::OpenAttachment { .. }
                ))
        );
        // States for the rest.
        let mine = own(Row::new(message(2, "mine"), 1), None);
        assert_eq!(row_state(&mine, Interaction::default()), RowState::Actions);
        let theirs = Row::new(message(2, "theirs"), 1);
        assert_eq!(row_state(&theirs, ui), RowState::Plain);
        let lost = own(mine, failed(ChangeKind::Delete));
        assert_eq!(
            row_state(&lost, Interaction::default()),
            RowState::Failed {
                delete: true,
                reason: "refused"
            }
        );
        assert_eq!(row_state(&lost, ui), RowState::Confirming);
    }

    #[test]
    fn emoji_sensors_ignore_overscan_and_report_hiding_after_scroll() {
        let mut snapshot = one_row(42);
        snapshot.rows[0].message = message(42, "😀 <:sample:12345> <a:party:45678>");
        snapshot.window.top = 400.0;
        snapshot.window.bottom = 400.0;
        snapshot.window.height = 80.0;
        snapshot.window.pinned = false;
        snapshot.window.at_bottom = false;
        let mut ui = Ui::new(
            640.0,
            80.0,
            view(&snapshot, Interaction::default(), assets(no_views())),
        );
        let visibility = |events: Vec<Event>| {
            events
                .into_iter()
                .filter_map(|event| match event {
                    Event::EmojiVisibility(occurrence, visible) => Some((occurrence, visible)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert!(
            visibility(ui.redraw()).is_empty(),
            "built does not mean visible"
        );
        ui.scroll_to(400.0);
        let shown = visibility(ui.redraw());
        assert_eq!(shown.len(), 3);
        assert!(shown.iter().all(|(_, visible)| *visible));
        ui.scroll_to(0.0);
        let hidden = visibility(ui.redraw());
        assert_eq!(hidden.len(), 3);
        assert!(hidden.iter().all(|(_, visible)| !visible));
        assert!(
            visibility(ui.redraw()).is_empty(),
            "idle sensors do not poll"
        );
        assert_eq!(
            snapshot.rows[0].message.content,
            "😀 <:sample:12345> <a:party:45678>"
        );
    }

    const THUMBS: &str = "\u{1f44d}";
    const PARTY: &str = "\u{1f389}";

    fn unicode(name: &str) -> Emoji {
        Emoji {
            id: None,
            name: Some(name.to_owned()),
            animated: false,
        }
    }

    fn custom(id: u64, name: &str, animated: bool) -> Emoji {
        Emoji {
            id: Some(Snowflake(id)),
            name: Some(name.to_owned()),
            animated,
        }
    }

    /// Message `n` with these (emoji, count, mine) reactions.
    fn reacted(n: u64, reactions: &[(Emoji, u32, bool)]) -> Arc<Message> {
        let mut message = (*message(n, "reactions")).clone();
        message.attachments.clear();
        message.reactions = reactions
            .iter()
            .map(|(emoji, count, me)| Reaction {
                count: *count,
                me: *me,
                emoji: emoji.clone(),
            })
            .collect();
        Arc::new(message)
    }

    fn request(emoji: Emoji, action: Action) -> Pending {
        Pending { emoji, action }
    }

    fn reaction_snapshot(row: Row, can_react: bool) -> Snapshot {
        Snapshot {
            rows: vec![row],
            can_react,
            ..one_row(42)
        }
    }

    fn found_with(
        snapshot: &Snapshot,
        press: impl Fn(&mut Ui<'_, Event>, f32, f32) -> Vec<Event>,
    ) -> Vec<Event> {
        let mut ui = Ui::new(
            640.0,
            400.0,
            view(snapshot, Interaction::default(), assets(no_views())),
        );
        ui.redraw();
        ui.draw();
        let mut found = Vec::new();
        for y in (0..400).step_by(4) {
            for x in (0..640).step_by(4) {
                for event in press(&mut ui, x as f32, y as f32) {
                    if !matches!(
                        event,
                        Event::Viewport { .. }
                            | Event::Measured { .. }
                            | Event::EmojiVisibility(..)
                    ) && !found.contains(&event)
                    {
                        found.push(event);
                    }
                }
            }
        }
        found
    }

    fn right_clickable(snapshot: &Snapshot) -> Vec<Event> {
        found_with(snapshot, |ui, x, y| ui.right_click(x, y))
    }

    fn toggle(emoji: Emoji) -> Event {
        Event::ToggleReaction {
            channel_id: CHANNEL,
            message_id: Snowflake(42),
            emoji,
        }
    }

    fn inspect(emoji: Emoji) -> Event {
        Event::InspectReaction {
            channel_id: CHANNEL,
            message_id: Snowflake(42),
            emoji,
        }
    }

    fn shown(row: &Row) -> Vec<(String, u32, bool, bool)> {
        chips(row)
            .map(|chip| {
                (
                    chip.emoji.name.clone().unwrap_or_default(),
                    chip.count,
                    chip.me,
                    chip.pending,
                )
            })
            .collect()
    }

    #[test]
    fn chips_show_discords_counts_with_unanswered_requests_applied_on_top() {
        let wave = custom(9, "wave", true);
        let stored = [
            (unicode(THUMBS), 2, true),
            (wave.clone(), 1, false),
            (unicode("\u{2764}\u{fe0f}"), 1, true),
        ];
        let row = Row::new(reacted(42, &stored), 1);
        let plain = shown(&row);
        assert_eq!(
            plain,
            [
                (THUMBS.to_owned(), 2, true, false),
                ("wave".to_owned(), 1, false, false),
                ("\u{2764}\u{fe0f}".to_owned(), 1, true, false)
            ]
        );
        let row = Row {
            reactions: vec![
                // Joining a reaction counts the user; one already theirs does not count twice.
                request(wave.clone(), Action::Add),
                request(unicode(THUMBS), Action::Add),
                // Taking back the only reaction of a chip removes the chip.
                request(unicode("\u{2764}\u{fe0f}"), Action::Remove),
                // A reaction nobody made yet appears, counted once.
                request(unicode(PARTY), Action::Add),
                // Taking back something that is not the user's changes no count.
                request(custom(10, "gone", false), Action::Remove),
            ],
            ..Row::new(reacted(42, &stored), 1)
        };
        assert_eq!(
            shown(&row),
            [
                (THUMBS.to_owned(), 2, true, true),
                ("wave".to_owned(), 2, true, true),
                (PARTY.to_owned(), 1, true, true)
            ]
        );
        // Taking back one of several reactions leaves the rest.
        let row = Row {
            reactions: vec![request(unicode(THUMBS), Action::Remove)],
            ..Row::new(reacted(42, &stored), 1)
        };
        assert_eq!(shown(&row)[0], (THUMBS.to_owned(), 1, false, true));
        // A renamed custom emoji is still the same chip.
        let row = Row {
            reactions: vec![request(custom(9, "renamed", true), Action::Add)],
            ..Row::new(reacted(42, &stored), 1)
        };
        assert_eq!(shown(&row)[1], ("wave".to_owned(), 2, true, true));
        assert_eq!(shown(&row).len(), 3);
    }

    #[test]
    fn chips_toggle_on_a_click_and_ask_who_reacted_on_a_right_click() {
        let wave = custom(9, "wave", false);
        let row = Row::new(
            reacted(42, &[(unicode(THUMBS), 3, true), (wave.clone(), 1, false)]),
            1,
        );
        let snapshot = reaction_snapshot(row, true);
        let found = clickable(&snapshot, Interaction::default());
        for emoji in [unicode(THUMBS), wave.clone()] {
            assert!(found.contains(&toggle(emoji.clone())), "{found:?}");
            assert!(
                !found.contains(&inspect(emoji)),
                "a left click only toggles"
            );
        }
        let found = right_clickable(&snapshot);
        for emoji in [unicode(THUMBS), wave.clone()] {
            assert!(found.contains(&inspect(emoji.clone())), "{found:?}");
            assert!(!found.contains(&toggle(emoji)), "a right click only asks");
        }

        // An unanswered request disables its chip; asking who reacted stays possible.
        let pending = Row {
            reactions: vec![request(wave.clone(), Action::Add)],
            ..snapshot.rows[0].clone()
        };
        let snapshot = reaction_snapshot(pending, true);
        let found = clickable(&snapshot, Interaction::default());
        assert!(found.contains(&toggle(unicode(THUMBS))));
        assert!(!found.contains(&toggle(wave.clone())), "{found:?}");
        assert!(right_clickable(&snapshot).contains(&inspect(wave)));
    }

    #[test]
    fn react_is_offered_where_the_user_may_react_and_nowhere_else() {
        let react = Event::React {
            channel_id: CHANNEL,
            message_id: Snowflake(42),
        };
        // With chips: the hover button and the chip row's + button.
        let row = Row::new(reacted(42, &[(unicode(THUMBS), 1, false)]), 1);
        let allowed = clickable(
            &reaction_snapshot(row.clone(), true),
            Interaction::default(),
        );
        assert!(allowed.contains(&react), "{allowed:?}");
        let denied = clickable(&reaction_snapshot(row, false), Interaction::default());
        assert!(!denied.contains(&react), "{denied:?}");
        assert!(
            denied.contains(&toggle(unicode(THUMBS))),
            "chips still work"
        );
        // A message nobody reacted to offers React on hover alone.
        let bare = Row::new(reacted(42, &[]), 1);
        assert!(
            clickable(
                &reaction_snapshot(bare.clone(), true),
                Interaction::default()
            )
            .contains(&react)
        );
        assert!(
            !clickable(&reaction_snapshot(bare, false), Interaction::default()).contains(&react)
        );
        // Not while a change to the message is being saved or confirmed.
        let saving = own(
            Row::new(reacted(42, &[]), 1),
            Some(RowChange {
                kind: ChangeKind::Delete,
                state: ChangeState::Saving,
            }),
        );
        assert!(
            !clickable(&reaction_snapshot(saving, true), Interaction::default()).contains(&react)
        );
    }

    #[test]
    fn chip_widgets_are_per_chip_and_bounded_by_the_most_a_message_keeps() {
        let nodes = |count: u32| {
            let reactions: Vec<_> = (0..count)
                .map(|n| {
                    (
                        unicode(char::from_u32(0x1f600 + n).unwrap().to_string().as_str()),
                        n + 1,
                        false,
                    )
                })
                .collect();
            built_nodes(&reaction_snapshot(
                Row::new(reacted(42, &reactions), 1),
                false,
            ))
        };
        let (none, one, two, three) = (nodes(0), nodes(1), nodes(2), nodes(3));
        assert!(one > none);
        assert_eq!(three - two, two - one, "every chip costs the same");
        let most = nodes(fastcord_model::MAX_REACTIONS as u32);
        assert_eq!(
            most - one,
            (two - one) * (fastcord_model::MAX_REACTIONS - 1),
            "a message's chips are bounded by what it keeps"
        );
        // The tree follows chips, not messages elsewhere in the history.
        assert_eq!(
            built_nodes(&snapshot(10, 100)),
            built_nodes(&snapshot(10, 500))
        );
    }

    #[test]
    fn who_reacted_appears_only_under_its_message_and_can_be_closed() {
        let row = Row::new(reacted(42, &[(unicode(THUMBS), 2, false)]), 1);
        let mut snapshot = reaction_snapshot(row, true);
        assert!(!clickable(&snapshot, Interaction::default()).contains(&Event::CloseReactionUsers));
        let users = |state| {
            Some(ReactionUsers {
                message: Snowflake(42),
                emoji: unicode(THUMBS),
                poll_answer: None,
                state,
            })
        };
        for state in [
            UsersState::Loading,
            UsersState::Failed("Who reacted could not be loaded."),
            UsersState::Loaded {
                names: vec!["Alice".to_owned(), "Bob".to_owned()],
                more: true,
            },
            UsersState::Loaded {
                names: Vec::new(),
                more: false,
            },
        ] {
            snapshot.reaction_users = users(state);
            assert!(
                clickable(&snapshot, Interaction::default()).contains(&Event::CloseReactionUsers)
            );
        }
        // A list asked about another message is not drawn on this one.
        snapshot.reaction_users = Some(ReactionUsers {
            message: Snowflake(43),
            ..snapshot.reaction_users.clone().unwrap()
        });
        assert!(!clickable(&snapshot, Interaction::default()).contains(&Event::CloseReactionUsers));
        // Names never reach a log.
        let printed = format!(
            "{:?}",
            ReactionUsers {
                message: Snowflake(42),
                emoji: unicode(THUMBS),
                poll_answer: None,
                state: UsersState::Loaded {
                    names: vec!["Alice Private".to_owned()],
                    more: false
                }
            }
        );
        assert!(!printed.contains("Alice"), "{printed}");
        assert_eq!(
            describe_emoji(&custom(9, "wave", false)),
            ":wave:",
            "custom emoji are named like the user types them"
        );
        assert_eq!(describe_emoji(&unicode(THUMBS)), THUMBS);
    }

    #[test]
    fn a_request_changing_the_chips_grows_the_row_and_the_new_height_is_measured() {
        let measured = |events: Vec<Event>| -> Vec<(u64, u64, f32)> {
            events
                .into_iter()
                .filter_map(|event| match event {
                    Event::Measured { measurement, .. } => {
                        Some((measurement.id.0, measurement.revision, measurement.height))
                    }
                    _ => None,
                })
                .collect()
        };
        let bare = reaction_snapshot(Row::new(reacted(42, &[]), 1), false);
        // The same message and revision, with the user's request overlaid.
        let asked = reaction_snapshot(
            Row {
                reactions: vec![request(unicode(THUMBS), Action::Add)],
                ..Row::new(reacted(42, &[]), 1)
            },
            false,
        );
        let mut ui = Ui::new(
            640.0,
            400.0,
            view(&bare, Interaction::default(), assets(no_views())),
        );
        let before = measured(ui.redraw());
        assert_eq!(before.len(), 1);
        ui.show(view(&asked, Interaction::default(), assets(no_views())));
        let after = measured(ui.redraw());
        assert_eq!(after.len(), 1, "the changed height is published once");
        assert_eq!((after[0].0, after[0].1), (before[0].0, before[0].1));
        assert!(
            after[0].2 > before[0].2,
            "{} -> {}",
            before[0].2,
            after[0].2
        );
        assert!(ui.redraw().is_empty());
    }

    #[test]
    fn chip_emoji_report_their_visibility_as_reaction_occurrences() {
        let row = Row::new(
            reacted(
                42,
                &[
                    (unicode(THUMBS), 1, false),
                    (custom(12345, "party", true), 2, true),
                    // Text the bundled index does not know stays text.
                    (unicode("\u{e000}"), 1, false),
                ],
            ),
            7,
        );
        let snapshot = reaction_snapshot(row, false);
        let mut ui = Ui::new(
            640.0,
            400.0,
            view(&snapshot, Interaction::default(), assets(no_views())),
        );
        let shown: Vec<_> = ui
            .redraw()
            .into_iter()
            .filter_map(|event| match event {
                Event::EmojiVisibility(occurrence, true) => Some(occurrence),
                _ => None,
            })
            .collect();
        let assets: Vec<_> = shown.iter().map(|occurrence| occurrence.asset).collect();
        assert_eq!(
            assets,
            [
                emoji::reaction_asset(&unicode(THUMBS)).unwrap(),
                emoji::Asset::Custom {
                    id: Snowflake(12345),
                    animated: true
                }
            ]
        );
        for (chip, occurrence) in shown.iter().enumerate() {
            assert_eq!(
                occurrence.origin,
                Origin::Reaction {
                    channel: CHANNEL,
                    message: Snowflake(42),
                    revision: 7,
                    chip: chip as u8
                }
            );
        }
        assert_eq!(emoji::reaction_asset(&unicode("\u{e000}")), None);
    }

    fn suppressed(kind: Suppression) -> std::collections::BTreeMap<Snowflake, Suppression> {
        std::collections::BTreeMap::from([(Snowflake(2), kind)])
    }

    #[test]
    fn rows_by_ignored_or_blocked_authors_keep_only_their_place() {
        let mut quoting = Row::new(message(3, "quotes"), 5);
        quoting.reply = Some(ReplyLine {
            target: Snowflake(1),
            author: Some(Snowflake(2)),
            state: ReplyState::Message {
                author: "Alt Account".into(),
                content: "secret".into(),
            },
        });
        let mut snapshot = Snapshot {
            channel_id: Some(CHANNEL),
            rows: vec![Row::new(message(1, "secret text"), 4), quoting],
            ..Snapshot::default()
        };
        // No suppression: nothing changes.
        hide_suppressed(&mut snapshot, &Default::default());
        assert!(snapshot.rows.iter().all(|row| row.hidden.is_none()));
        assert_eq!(snapshot.rows[0].message.content, "secret text");

        hide_suppressed(&mut snapshot, &suppressed(Suppression::Ignored));
        for row in &snapshot.rows {
            assert_eq!(row.hidden, Some(Suppression::Ignored));
            let message = &*row.message;
            assert!(message.content.is_empty() && message.attachments.is_empty());
            assert!(message.author.username.is_empty() && message.author.global_name.is_none());
            // Its place stays, and the height cache is told it changed.
            assert_eq!(row.revision & HIDDEN_REVISION, HIDDEN_REVISION);
            assert!(row.reply.is_none());
        }
        assert_eq!(snapshot.rows[0].message.id, Snowflake(1));
        // Blocking is reported as blocking.
        let mut blocked = Snapshot {
            rows: vec![Row::new(message(1, "x"), 1)],
            ..Snapshot::default()
        };
        hide_suppressed(&mut blocked, &suppressed(Suppression::Blocked));
        assert_eq!(blocked.rows[0].hidden, Some(Suppression::Blocked));
    }

    #[test]
    fn the_accounts_own_messages_are_never_hidden_and_quotes_of_others_are() {
        let mut own = Row::new(message(1, "mine"), 1);
        own.own = true;
        let mut other = Row::new(message(2, "from someone else"), 1);
        other.message = {
            let mut m = (*other.message).clone();
            m.author.id = Snowflake(9);
            Arc::new(m)
        };
        other.reply = Some(ReplyLine {
            target: Snowflake(1),
            author: Some(Snowflake(2)),
            state: ReplyState::Message {
                author: "x".into(),
                content: "quoted".into(),
            },
        });
        let mut snapshot = Snapshot {
            rows: vec![own, other],
            ..Snapshot::default()
        };
        hide_suppressed(&mut snapshot, &suppressed(Suppression::Ignored));
        assert!(snapshot.rows[0].hidden.is_none());
        assert_eq!(snapshot.rows[0].message.content, "mine");
        // A reply by a visible author to a hidden author loses the quote.
        assert!(snapshot.rows[1].hidden.is_none());
        assert_eq!(
            snapshot.rows[1].reply.as_ref().unwrap().state,
            ReplyState::Hidden
        );
    }

    #[test]
    fn a_hidden_row_offers_no_controls_and_draws_no_original_text() {
        let mut snapshot = Snapshot {
            channel_id: Some(CHANNEL),
            rows: vec![
                Row::new(message(41, "before"), 1),
                Row::new(message(42, "secret"), 1),
            ],
            window: Window {
                range: 0..2,
                height: 400.0,
                at_bottom: true,
                ..Window::default()
            },
            ..Snapshot::default()
        };
        snapshot.rows[0].message = {
            let mut m = (*snapshot.rows[0].message).clone();
            m.author.id = Snowflake(9);
            Arc::new(m)
        };
        hide_suppressed(&mut snapshot, &suppressed(Suppression::Ignored));
        assert!(snapshot.rows[0].hidden.is_none() && snapshot.rows[1].hidden.is_some());
        let found = clickable(&snapshot, Interaction::default());
        // The visible row still offers its attachment; the hidden one nothing.
        assert!(found.iter().any(|event| matches!(event,
            Event::LoadAttachment { message_id, .. } if *message_id == Snowflake(41))));
        assert!(!found.iter().any(|event| match event {
            Event::LoadAttachment { message_id, .. }
            | Event::DownloadAttachment { message_id, .. }
            | Event::OpenAttachment { message_id, .. } => *message_id == Snowflake(42),
            Event::OpenProfile(user) => user.id == Snowflake(2),
            _ => false,
        }));
        // Hidden rows never join a group with their neighbours.
        assert!(!continues_group(&snapshot.rows[0], &snapshot.rows[1]));
    }
}
