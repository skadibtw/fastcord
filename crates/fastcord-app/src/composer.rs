//! The message composer: a multi-line editor, per-channel drafts, the explicit
//! `@everyone` choice, and the unconfirmed-message strip above it.
//!
//! Nothing here talks to Discord. Pressing Send turns the draft into a
//! [`Submission`] that the account worker validates and posts; what happens to
//! it afterwards (sending, failed, unconfirmed) comes back in the timeline
//! snapshot's outbox and is only displayed here. Enter sends, Shift+Enter starts
//! a new line, and nothing is ever sent without one of the two.

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use fastcord_discord::{MAX_CONTENT_CHARS, MAX_UPLOAD_FILES, UploadFile};
use fastcord_model::Snowflake;
use iced::keyboard::{self, key};
use iced::widget::text_editor::{Action, Binding, Content, Edit, Motion, Status};
use iced::widget::{
    button, checkbox, column, container, row, scrollable, space, text, text_editor,
};
use iced::{Element, Length, Padding};

use crate::changes::RowChange;
use crate::gateway::NavigationBridge;
use crate::outbox::{MAX_OUTBOX, MAX_REPLY_AUTHOR_CHARS, OpId, OutboxItem, OutboxState, Reply};
use crate::timeline::Snapshot;

/// The editor refuses to grow past this many characters, twice the longest
/// message Discord accepts, so a stray paste cannot build an unbounded buffer.
pub const MAX_EDITOR_CHARS: usize = 2 * MAX_CONTENT_CHARS;
/// Channels whose unsent draft is remembered while the user is elsewhere.
const MAX_DRAFTS: usize = 16;
/// Characters of an unconfirmed message shown in the strip.
const PREVIEW_CHARS: usize = 200;
/// Past this many characters the editor shows a length note.
const NOTE_FROM_CHARS: usize = 1_900;
/// Pixel height up to which the unconfirmed strip grows before it scrolls.
const STRIP_HEIGHT: f32 = 168.0;

pub const EDITOR_ID: &str = "composer";

/// Completions offered at once.
const MAX_COMPLETIONS: usize = 8;
/// Longest `@`/`#` query that still completes.
const MAX_QUERY_CHARS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateKind {
    User,
    Role,
    Channel,
}

/// Something a `@` or `#` can complete to, from the open channel's server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub id: Snowflake,
    pub label: String,
}

impl Candidate {
    /// The reference Discord expects in the message text.
    fn reference(&self) -> String {
        match self.kind {
            CandidateKind::User => format!("<@{}> ", self.id),
            CandidateKind::Role => format!("<@&{}> ", self.id),
            CandidateKind::Channel => format!("<#{}> ", self.id),
        }
    }

    fn shown(&self) -> String {
        match self.kind {
            CandidateKind::Channel => format!("#{}", self.label),
            _ => format!("@{}", self.label),
        }
    }
}

/// The `@`/`#` word the cursor ends: its trigger and the query typed after it.
/// The trigger must start the line or follow whitespace, so addresses and
/// anchors do not complete.
pub fn mention_token(line: &str, column: usize) -> Option<(char, &str)> {
    let end = line.floor_char_boundary(column.min(line.len()));
    let before = &line[..end];
    let query_start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .last()
        .map_or(end, |(at, _)| at);
    let query = &before[query_start..];
    let mut earlier = before[..query_start].chars().rev();
    let trigger = earlier.next().filter(|c| matches!(c, '@' | '#'))?;
    (earlier.next().is_none_or(char::is_whitespace) && query.chars().count() <= MAX_QUERY_CHARS)
        .then_some((trigger, query))
}

/// An editor action. `Debug` names the kind of action, never typed or pasted text.
#[derive(Clone)]
pub struct Act(pub Action);

impl fmt::Debug for Act {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_edit() {
            "Act(edit)"
        } else {
            "Act(motion)"
        })
    }
}

/// What the composer's widgets ask for.
#[derive(Clone, Debug)]
pub enum Event {
    Action(Act),
    Send,
    Everyone(bool),
    ReplyMention(bool),
    CancelReply,
    /// Ask for files to attach. The window opens the system file dialog.
    Attach,
    /// Take the chosen file at this position off the message.
    Detach(usize),
    /// Attach the image on the clipboard (read when asked, never before).
    PasteImage,
    /// Stop an upload that is in progress.
    CancelUpload(OpId),
    /// Replace the `@`/`#` word before the cursor with a reference.
    Complete {
        replaced: usize,
        insertion: String,
    },
    Retry(OpId),
    Discard(OpId),
    /// Move a failed message's text back into the editor and drop the failure.
    Edit(OpId),
    /// Stop editing a sent message and bring the draft back.
    CancelEdit,
}

/// A draft the user chose to send.
#[derive(PartialEq, Eq)]
pub struct Submission {
    pub channel: Snowflake,
    pub content: String,
    pub files: Vec<UploadFile>,
    pub everyone: bool,
    pub reply: Option<Reply>,
}

impl fmt::Debug for Submission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Submission")
            .field("channel", &self.channel)
            .field("everyone", &self.everyone)
            .field("reply", &self.reply.as_ref().map(|reply| reply.message))
            .finish_non_exhaustive()
    }
}
/// New text the user chose to save for one of their messages.
#[derive(PartialEq, Eq)]
pub struct EditSubmission {
    pub channel: Snowflake,
    pub message: Snowflake,
    pub content: String,
}

impl fmt::Debug for EditSubmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EditSubmission")
            .field("channel", &self.channel)
            .field("message", &self.message)
            .finish_non_exhaustive()
    }
}

/// A sent message whose text is in the editor, and the draft it displaced.
struct Editing {
    message: Snowflake,
    /// The message has attachments, so it may be saved without text.
    allow_empty: bool,
    draft: String,
    everyone: bool,
    replying: Option<Reply>,
}

/// Unsent text per channel: bounded in count and, through the editor's cap, in size.
#[derive(Default)]
struct Drafts {
    entries: VecDeque<(Snowflake, String)>,
}

impl Drafts {
    fn take(&mut self, channel: Snowflake) -> Option<String> {
        let at = self.entries.iter().position(|(id, _)| *id == channel)?;
        self.entries.remove(at).map(|(_, text)| text)
    }

    fn put(&mut self, channel: Snowflake, text: String) {
        self.take(channel);
        if text.trim().is_empty() {
            return;
        }
        self.entries.push_front((channel, text));
        self.entries.truncate(MAX_DRAFTS);
    }
}

pub struct Composer {
    content: Content,
    channel: Option<Snowflake>,
    /// Characters in the editor, and in what would be sent (trimmed).
    chars: usize,
    trimmed_chars: usize,
    /// Nothing but whitespace.
    blank: bool,
    /// The user's choice for the message being written; never carried over.
    everyone: bool,
    /// Files chosen for the message being written (at most
    /// [`MAX_UPLOAD_FILES`]); checked again when they are uploaded.
    files: Vec<UploadFile>,
    /// The target and explicit replied-user notification choice.
    replying: Option<Reply>,
    notice: Option<&'static str>,
    drafts: Drafts,
    /// Editing one of the user's messages of `channel` instead of writing one.
    editing: Option<Editing>,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            content: Content::new(),
            channel: None,
            chars: 0,
            trimmed_chars: 0,
            blank: true,
            everyone: false,
            files: Vec::new(),
            replying: None,
            notice: None,
            drafts: Drafts::default(),
            editing: None,
        }
    }
}

impl Composer {
    fn recount(&mut self) {
        let text = self.content.text();
        self.chars = text.chars().count();
        self.trimmed_chars = text.trim().chars().count();
        self.blank = text.trim().is_empty();
    }

    /// The channel to announce typing in after an editor action: only while
    /// writing a new message (never editing one) that has some text.
    pub fn typing_channel(&self, event: &Event) -> Option<Snowflake> {
        let Event::Action(Act(action)) = event else {
            return None;
        };
        (action.is_edit() && self.editing.is_none() && !self.blank)
            .then_some(self.channel)
            .flatten()
    }

    /// Follows the open channel: the draft of the one being left is kept, the
    /// one being entered is restored. The `@everyone` choice never survives a
    /// switch, and neither does editing a message.
    pub fn select(&mut self, channel: Option<Snowflake>) {
        if self.channel == channel {
            return;
        }
        self.cancel_edit();
        if let Some(old) = self.channel {
            self.drafts.put(old, self.content.text());
        }
        self.content = match channel.and_then(|id| self.drafts.take(id)) {
            Some(text) => Content::with_text(&text),
            None => Content::new(),
        };
        self.content.perform(Action::Move(Motion::DocumentEnd));
        self.channel = channel;
        self.everyone = false;
        self.files.clear();
        self.replying = None;
        self.notice = None;
        self.recount();
    }

    /// Applies an editor action, refusing edits that would grow the draft
    /// past [`MAX_EDITOR_CHARS`].
    pub fn perform(&mut self, action: Action) {
        if let Action::Edit(edit) = &action {
            let added = match edit {
                Edit::Insert(_) | Edit::Enter | Edit::Indent => 1,
                Edit::Paste(text) => text.chars().count(),
                Edit::Unindent | Edit::Backspace | Edit::Delete => 0,
            };
            if self.chars + added > MAX_EDITOR_CHARS {
                self.notice = Some("The message editor is full.");
                return;
            }
        }
        let edit = action.is_edit();
        self.content.perform(action);
        if edit {
            self.notice = None;
            self.recount();
        }
    }

    /// Completions for the `@`/`#` word at the cursor: each candidate with the
    /// number of characters (trigger included) it replaces.
    pub fn completions<'s>(&self, snapshot: &'s Snapshot) -> Vec<(&'s Candidate, usize)> {
        if self.editing.is_some() || self.channel.is_none() || snapshot.candidates.is_empty() {
            return Vec::new();
        }
        let cursor = self.content.cursor();
        if cursor.selection.is_some() {
            return Vec::new();
        }
        let Some(line) = self.content.line(cursor.position.line) else {
            return Vec::new();
        };
        let Some((trigger, query)) = mention_token(&line.text, cursor.position.column) else {
            return Vec::new();
        };
        let query_lower = query.to_lowercase();
        let wanted =
            |candidate: &&Candidate| (trigger == '#') == (candidate.kind == CandidateKind::Channel);
        let mut found: Vec<(&Candidate, bool)> = snapshot
            .candidates
            .iter()
            .filter(wanted)
            .filter_map(|candidate| {
                let name = candidate.label.to_lowercase();
                if name.starts_with(&query_lower) {
                    Some((candidate, true))
                } else {
                    name.contains(&query_lower).then_some((candidate, false))
                }
            })
            .collect();
        found.sort_by_key(|(_, prefix)| !prefix);
        let replaced = 1 + query.chars().count();
        found
            .into_iter()
            .take(MAX_COMPLETIONS)
            .map(|(candidate, _)| (candidate, replaced))
            .collect()
    }

    /// Replaces the `replaced` characters before the cursor with `insertion`.
    pub fn complete(&mut self, replaced: usize, insertion: &str) {
        for _ in 0..replaced {
            self.content.perform(Action::Edit(Edit::Backspace));
        }
        self.recount();
        self.insert_emoji(insertion);
    }

    /// Picker insertion is a normal editor paste: it replaces the selection at
    /// the current cursor, keeps source tokens intact, and is one undo step.
    pub fn insert_emoji(&mut self, source: &str) -> bool {
        let selected = self
            .content
            .selection()
            .map_or(0, |text| text.chars().count());
        if self.channel.is_none()
            || self.chars.saturating_sub(selected) + source.chars().count() > MAX_EDITOR_CHARS
        {
            self.notice = Some("The message editor is full.");
            return false;
        }
        self.content
            .perform(Action::Edit(Edit::Paste(Arc::new(source.to_owned()))));
        self.notice = None;
        self.recount();
        true
    }

    pub fn set_everyone(&mut self, allowed: bool) {
        self.everyone = allowed;
    }
    /// Selects a loaded message as the reply target without disturbing the
    /// draft. A new target starts with replied-user notification off.
    pub fn begin_reply(&mut self, snapshot: &Snapshot, message: Snowflake) -> bool {
        let Some(channel) = self
            .channel
            .filter(|&open| snapshot.channel_id == Some(open) && snapshot.can_send)
        else {
            return false;
        };
        if self.editing.is_some() {
            return false;
        }
        let Some(row) = snapshot
            .rows
            .iter()
            .find(|row| row.message.id == message && row.message.channel_id == channel)
        else {
            return false;
        };
        self.replying = Some(Reply {
            message,
            author: Arc::from(
                row.message
                    .author
                    .display_name()
                    .chars()
                    .take(MAX_REPLY_AUTHOR_CHARS)
                    .collect::<String>(),
            ),
            content: Arc::from(reply_preview(&row.message.content)),
            mention_author: false,
        });
        self.notice = None;
        true
    }

    pub fn set_reply_mention(&mut self, allowed: bool) {
        if let Some(reply) = &mut self.replying {
            reply.mention_author = allowed;
        }
    }

    pub fn cancel_reply(&mut self) {
        self.replying = None;
        self.notice = None;
    }

    fn reply_deleted(&self, snapshot: &Snapshot) -> bool {
        self.replying
            .as_ref()
            .is_some_and(|reply| snapshot.deleted.contains(&reply.message))
    }

    /// Whether Send is available: there is an open channel the account may
    /// send to, something to send within Discord's limit, and room in the
    /// outbox. Reads only cached counts, so the view can ask every frame.
    fn ready(&self, can_send: bool, outbox_len: usize) -> bool {
        self.channel.is_some()
            && self.editing.is_none()
            && can_send
            && outbox_len < MAX_OUTBOX
            && (!self.blank || !self.files.is_empty())
            && self.trimmed_chars <= MAX_CONTENT_CHARS
    }

    /// Adds chosen files to the message being written. Each is checked now and
    /// again when it is uploaded. The first problem is said; files before it
    /// stay attached.
    pub fn attach(&mut self, paths: Vec<PathBuf>) {
        if self.channel.is_none() || self.editing.is_some() {
            return;
        }
        for path in paths {
            if self.files.iter().any(|file| file.path() == path) {
                continue;
            }
            if self.files.len() >= MAX_UPLOAD_FILES {
                self.notice = Some(fastcord_discord::UploadError::TooMany.reason());
                return;
            }
            match UploadFile::inspect(&path) {
                Ok(file) => {
                    self.files.push(file);
                    self.notice = None;
                }
                Err(error) => self.notice = Some(error.reason()),
            }
        }
    }

    /// Adds an image the program saved from the clipboard; it is deleted when
    /// it is no longer attached to anything.
    pub fn attach_pasted(&mut self, path: &std::path::Path) {
        if self.channel.is_none() || self.editing.is_some() {
            let _ = std::fs::remove_file(path);
            return;
        }
        if self.files.len() >= MAX_UPLOAD_FILES {
            self.notice = Some(fastcord_discord::UploadError::TooMany.reason());
            let _ = std::fs::remove_file(path);
            return;
        }
        match UploadFile::inspect_temporary(path) {
            Ok(file) => {
                self.files.push(file);
                self.notice = None;
            }
            Err(error) => self.notice = Some(error.reason()),
        }
    }

    /// Says why a paste did not produce a file.
    pub fn paste_failed(&mut self, reason: &'static str) {
        self.notice = Some(reason);
    }

    /// Takes one chosen file off the message being written.
    pub fn detach(&mut self, index: usize) {
        if index < self.files.len() {
            self.files.remove(index);
        }
    }

    /// The draft as a send request, if it may be sent now (see `ready`). Does
    /// not clear the draft; [`sent`](Self::sent) does, once the worker has
    /// accepted the request.
    pub fn submission(&self, can_send: bool, outbox_len: usize) -> Option<Submission> {
        let channel = self.channel.filter(|_| self.ready(can_send, outbox_len))?;
        Some(Submission {
            channel,
            content: self.content.text(),
            files: self.files.clone(),
            everyone: self.everyone,
            reply: self.replying.clone(),
        })
    }

    /// The message whose text is being edited, if any.
    pub fn editing(&self) -> Option<Snowflake> {
        self.editing.as_ref().map(|editing| editing.message)
    }

    /// The message the current draft replies to, if any.
    pub fn replying(&self) -> Option<Snowflake> {
        self.replying.as_ref().map(|reply| reply.message)
    }

    /// Puts one of the user's messages of the open channel into the editor:
    /// the text of its failed edit if it has one, otherwise its current text.
    /// The draft is set aside and comes back when editing ends. Only rows the
    /// worker marked as the user's own qualify, and none with a change still
    /// being saved. Returns whether editing started.
    pub fn begin_edit(&mut self, snapshot: &Snapshot, message: Snowflake) -> bool {
        let Some(channel) = self
            .channel
            .filter(|&open| snapshot.channel_id == Some(open))
        else {
            return false;
        };
        let Some(row) = snapshot
            .rows
            .iter()
            .find(|row| row.message.id == message && row.message.channel_id == channel && row.own)
        else {
            return false;
        };
        if row.change.as_ref().is_some_and(RowChange::saving) {
            return false;
        }
        let text = row
            .change
            .as_ref()
            .and_then(RowChange::failed_edit)
            .unwrap_or(&row.message.content);
        if text.chars().count() > MAX_EDITOR_CHARS {
            self.notice = Some("That message is too long to edit here.");
            return false;
        }
        let (draft, everyone, replying) = match self.editing.take() {
            Some(editing) => (editing.draft, editing.everyone, editing.replying),
            None => (self.content.text(), self.everyone, self.replying.take()),
        };
        self.editing = Some(Editing {
            message,
            allow_empty: !row.message.attachments.is_empty(),
            draft,
            everyone,
            replying,
        });
        self.content = Content::with_text(text);
        self.content.perform(Action::Move(Motion::DocumentEnd));
        self.everyone = false;
        self.replying = None;
        self.notice = None;
        self.recount();
        true
    }

    /// Ends editing and brings the set-aside draft back. Nothing is saved.
    pub fn cancel_edit(&mut self) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        self.content = Content::with_text(&editing.draft);
        self.content.perform(Action::Move(Motion::DocumentEnd));
        self.everyone = editing.everyone;
        self.replying = editing.replying;
        self.notice = None;
        self.recount();
    }

    /// Whether Save is available: within Discord's limit, and not empty
    /// unless the message keeps attachments (an empty message is a delete).
    fn edit_ready(&self) -> bool {
        self.editing.as_ref().is_some_and(|editing| {
            (!self.blank || editing.allow_empty) && self.trimmed_chars <= MAX_CONTENT_CHARS
        })
    }

    /// The edited text as a save request, if it may be saved. Does not end
    /// editing; [`cancel_edit`](Self::cancel_edit) does, once the worker has
    /// accepted the request.
    pub fn edit_submission(&self) -> Option<EditSubmission> {
        let editing = self.editing.as_ref().filter(|_| self.edit_ready())?;
        Some(EditSubmission {
            channel: self.channel?,
            message: editing.message,
            content: self.content.text(),
        })
    }

    /// An edit or delete action the worker's queue did not accept.
    pub fn refused(&mut self) {
        self.notice = Some(BUSY);
    }

    /// The worker accepted the draft: empty the editor.
    pub fn sent(&mut self) {
        self.content = Content::new();
        self.everyone = false;
        self.files.clear();
        self.replying = None;
        self.notice = None;
        self.recount();
    }

    /// Puts a failed message's text back for editing: replacing a blank
    /// editor, or after the current text. Returns `false`, changing nothing,
    /// when it does not fit.
    pub fn restore(&mut self, text: &str) -> bool {
        let separator = if self.blank { "" } else { "\n\n" };
        let added = separator.chars().count() + text.chars().count();
        let base = if self.blank { 0 } else { self.chars };
        if base + added > MAX_EDITOR_CHARS {
            self.notice = Some("There is no room in the editor for that message.");
            return false;
        }
        if self.blank {
            self.content = Content::with_text(text);
        } else {
            self.content.perform(Action::Move(Motion::DocumentEnd));
            self.content
                .perform(Action::Edit(Edit::Paste(Arc::new(format!(
                    "{separator}{text}"
                )))));
        }
        self.content.perform(Action::Move(Motion::DocumentEnd));
        self.notice = None;
        self.recount();
        true
    }
    /// Restores a failed send with its original reply target and notification
    /// choice. A nonblank draft can only merge when its reply metadata matches.
    pub fn restore_send(&mut self, text: &str, files: &[UploadFile], reply: Option<Reply>) -> bool {
        let replacing = self.blank && self.files.is_empty();
        if !replacing && self.replying != reply {
            self.notice = Some("Finish the current draft before editing this failed send.");
            return false;
        }
        if !text.is_empty() && !self.restore(text) {
            return false;
        }
        for file in files {
            if self.files.len() < MAX_UPLOAD_FILES
                && !self.files.iter().any(|known| known.path() == file.path())
            {
                self.files.push(file.clone());
            }
        }
        if replacing {
            self.replying = reply;
        }
        true
    }
}

/// Applies one composer event. Typing stays local; a send, retry, or discard
/// is handed to the account worker, and the draft is cleared only once the
/// worker's queue has accepted it, so a refused send never loses the text.
pub fn update(
    composer: &mut Composer,
    event: Event,
    snapshot: &Snapshot,
    bridge: &NavigationBridge,
) {
    match event {
        Event::Action(Act(action)) => composer.perform(action),
        Event::Everyone(allowed) => composer.set_everyone(allowed),
        Event::ReplyMention(allowed) => composer.set_reply_mention(allowed),
        Event::CancelReply => composer.cancel_reply(),
        // The dialog is opened by the window; the files come back as `attach`.
        Event::Attach | Event::PasteImage => {}
        Event::Detach(index) => composer.detach(index),
        Event::CancelUpload(id) => {
            if !bridge.cancel_upload(id) {
                composer.notice = Some(BUSY);
            }
        }
        Event::Complete {
            replaced,
            insertion,
        } => composer.complete(replaced, &insertion),
        Event::Send if composer.editing.is_some() => {
            if let Some(edit) = composer.edit_submission() {
                if bridge.edit_message(edit.channel, edit.message, edit.content) {
                    composer.cancel_edit();
                } else {
                    composer.notice = Some(BUSY);
                }
            } else if composer.blank {
                composer.notice = Some(EMPTY_EDIT);
            }
        }
        Event::Send => {
            if composer.reply_deleted(snapshot) {
                composer.notice = Some(REPLY_DELETED);
                return;
            }
            let open = snapshot.can_send && snapshot.channel_id == composer.channel;
            let Some(submission) = composer.submission(open, snapshot.outbox.len()) else {
                return;
            };
            if bridge.send_message_with_files(
                submission.channel,
                submission.content,
                submission.files,
                submission.everyone,
                submission.reply,
            ) {
                composer.sent();
            } else {
                composer.notice = Some(NOT_QUEUED);
            }
        }
        Event::CancelEdit => composer.cancel_edit(),
        Event::Retry(id) => {
            if !bridge.retry_send(id) {
                composer.notice = Some(BUSY);
            }
        }
        Event::Discard(id) => {
            if !bridge.discard_send(id) {
                composer.notice = Some(BUSY);
            }
        }
        Event::Edit(id) => {
            // Moving a failed send's text in ends editing a sent message first.
            composer.cancel_edit();
            let failed = snapshot.outbox.iter().find(|item| {
                item.id == id
                    && Some(item.channel_id) == composer.channel
                    && matches!(item.state, OutboxState::Failed(_))
            });
            // The text moves only if it fits; the failed entry goes once it has.
            if let Some(item) = failed
                && composer.restore_send(&item.content, &item.files, item.reply.clone())
                && !bridge.discard_send(id)
            {
                composer.notice = Some(BUSY);
            }
        }
    }
}

/// "X is typing…", "X and Y are typing…", or "Several people are typing…".
fn typing_line(names: &[String]) -> Option<String> {
    match names {
        [] => None,
        [one] => Some(format!("{one} is typing…")),
        [a, b] => Some(format!("{a} and {b} are typing…")),
        _ => Some("Several people are typing…".to_owned()),
    }
}

const BUSY: &str = "That is not possible right now (busy or disconnected); try again.";
const EMPTY_EDIT: &str = "A message needs some text. Use Delete to remove it instead.";
const NOT_QUEUED: &str =
    "Not sent: too many messages are waiting, or the connection has stopped. Your text is kept.";
const REPLY_DELETED: &str = "That message was deleted. Choose another message to reply to.";

fn preview(content: &str) -> String {
    let mut shown: String = content
        .chars()
        .take(PREVIEW_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if content.chars().nth(PREVIEW_CHARS).is_some() {
        shown.push('…');
    }
    shown
}

fn reply_preview(content: &str) -> String {
    let mut chars = content
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c });
    let mut shown: String = chars.by_ref().take(PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        shown.pop();
        shown.push('…');
    }
    shown
}

fn small_button(label: &'static str, event: Event) -> Element<'static, Event> {
    button(text(label).size(12))
        .on_press(event)
        .padding(Padding {
            top: 2.0,
            right: 8.0,
            bottom: 2.0,
            left: 8.0,
        })
        .style(button::secondary)
        .into()
}

/// One unconfirmed message. `here`: it belongs to the open channel, where the
/// account may send if `can_send`; only then can it be retried or edited.
fn outbox_row(item: &OutboxItem, here: bool, can_send: bool) -> Element<'_, Event> {
    let actionable = here && can_send;
    let body = text(preview(&item.content))
        .size(13)
        .width(Length::Fill)
        .wrapping(text::Wrapping::WordOrGlyph);
    let mut content = column![].spacing(4);
    if !here {
        content = content.push(text("In another channel").size(11).style(text::secondary));
    }
    if let Some(reply) = &item.reply {
        content = content.push(
            text(format!("Replying to {}: {}", reply.author, reply.content))
                .size(11)
                .width(Length::Fill)
                .wrapping(text::Wrapping::WordOrGlyph)
                .style(text::secondary),
        );
    }
    for file in &item.files {
        content = content.push(
            text(format!(
                "Attachment: {} ({})",
                file.filename(),
                crate::timeline::format_size(file.size())
            ))
            .size(12)
            .style(text::secondary),
        );
    }
    content = match item.state {
        OutboxState::Sending => {
            let status = match item.upload {
                Some((sent, total)) if total > 0 => format!(
                    "Uploading {} of {}\u{2026}",
                    crate::timeline::format_size(sent.min(total)),
                    crate::timeline::format_size(total)
                ),
                _ => "Sending\u{2026}".to_owned(),
            };
            let mut line = row![text(status).size(12).style(text::secondary)]
                .spacing(8)
                .align_y(iced::alignment::Vertical::Center);
            if item.upload.is_some() {
                line = line.push(small_button("Cancel upload", Event::CancelUpload(item.id)));
            }
            content.push(body.style(text::secondary)).push(line)
        }
        OutboxState::Failed(reason) => {
            let mut actions = row![].spacing(8);
            if actionable {
                actions = actions
                    .push(small_button("Retry", Event::Retry(item.id)))
                    .push(small_button("Edit", Event::Edit(item.id)));
            }
            actions = actions.push(small_button("Discard", Event::Discard(item.id)));
            content
                .push(body)
                .push(
                    text(format!("Not sent. {reason}"))
                        .size(12)
                        .style(text::danger),
                )
                .push(actions)
        }
        OutboxState::Uncertain(reason) => {
            let mut actions = row![].spacing(8);
            if actionable {
                actions = actions.push(small_button("Retry send", Event::Retry(item.id)));
            }
            actions = actions.push(small_button("Discard", Event::Discard(item.id)));
            content
                .push(body)
                .push(
                    text(format!(
                        "Not confirmed. {reason} If it was, it appears in its channel by itself. A retry within a few minutes cannot post it twice: Discord returns the message it already has."
                    ))
                    .size(12)
                    .style(text::danger),
                )
                .push(actions)
        }
    };
    container(content)
        .width(Length::Fill)
        .padding(8)
        .style(container::rounded_box)
        .into()
}

/// The unconfirmed-message strip, then the editor with its controls. The
/// editor and Send are present only where the account may send; editing one
/// of the user's messages brings the editor wherever the message is shown.
pub fn view<'a>(composer: &'a Composer, snapshot: &'a Snapshot) -> Element<'a, Event> {
    let mut layout = column![].spacing(6);
    if !snapshot.outbox.is_empty() {
        let strip = column(snapshot.outbox.iter().map(|item| {
            let here = snapshot.channel_id == Some(item.channel_id);
            outbox_row(item, here, snapshot.can_send)
        }))
        .spacing(6);
        layout = layout.push(container(scrollable(strip)).max_height(STRIP_HEIGHT));
    }
    if let Some(notice) = snapshot.notice {
        layout = layout.push(text(notice).size(12).style(text::danger));
    }
    if let Some(line) = typing_line(&snapshot.typing) {
        layout = layout.push(text(line).size(12).style(text::secondary));
    }
    let editing = composer.editing.is_some();
    if let Some(reply) = &composer.replying {
        let deleted = snapshot.deleted.contains(&reply.message);
        let label = if deleted {
            "That message was deleted; choose another reply target.".to_owned()
        } else {
            format!("Replying to {}: {}", reply.author, reply.content)
        };
        let mut summary = row![
            text(label)
                .size(12)
                .width(Length::Fill)
                .wrapping(text::Wrapping::WordOrGlyph),
        ]
        .spacing(8)
        .align_y(iced::alignment::Vertical::Center);
        if deleted {
            summary = summary.push(text("Deleted").size(12).style(text::danger));
        } else if !editing {
            summary = summary.push(
                checkbox(reply.mention_author)
                    .label("Notify replied user")
                    .on_toggle(Event::ReplyMention)
                    .size(14)
                    .text_size(12),
            );
        }
        summary = summary.push(small_button("Cancel reply", Event::CancelReply));
        layout = layout.push(summary);
    }
    if !snapshot.can_send && !editing {
        // Without the editor's controls row, a refused action is said here.
        if let Some(notice) = composer.notice {
            layout = layout.push(text(notice).size(12).style(text::secondary));
        }
        return layout.into();
    }
    if editing {
        layout = layout.push(
            row![
                text("Editing your message").size(12),
                text("Enter saves, Escape cancels.")
                    .size(12)
                    .style(text::secondary),
                space::horizontal(),
                small_button("Cancel", Event::CancelEdit),
            ]
            .spacing(8)
            .align_y(iced::alignment::Vertical::Center),
        );
    }
    let editor = text_editor(&composer.content)
        .id(EDITOR_ID)
        .placeholder(if editing {
            "Edited message (Enter to save, Escape to cancel)"
        } else {
            "Message (Enter to send, Shift+Enter for a new line)"
        })
        .on_action(|action| Event::Action(Act(action)))
        .key_binding(move |press| {
            let focused = matches!(press.status, Status::Focused { .. });
            match press.key.as_ref() {
                keyboard::Key::Named(key::Named::Enter) if focused && !press.modifiers.shift() => {
                    Some(Binding::Custom(Event::Send))
                }
                keyboard::Key::Named(key::Named::Escape) if focused && editing => {
                    Some(Binding::Custom(Event::CancelEdit))
                }
                _ => Binding::from_key_press(press),
            }
        })
        .size(14)
        .padding(8)
        .min_height(38)
        .max_height(150);
    if !composer.files.is_empty() {
        layout = layout.push(column(composer.files.iter().enumerate().map(
            |(index, file)| {
                row![
                    text(format!(
                        "{} ({})",
                        file.filename(),
                        crate::timeline::format_size(file.size())
                    ))
                    .size(12)
                    .width(Length::Fill)
                    .wrapping(text::Wrapping::WordOrGlyph),
                    small_button("Remove", Event::Detach(index)),
                ]
                .spacing(8)
                .align_y(iced::alignment::Vertical::Center)
                .into()
            },
        )));
    }
    let picks = composer.completions(snapshot);
    if !picks.is_empty() {
        layout = layout.push(
            row(picks.into_iter().map(|(candidate, replaced)| {
                button(text(candidate.shown()).size(12))
                    .on_press(Event::Complete {
                        replaced,
                        insertion: candidate.reference(),
                    })
                    .padding([2, 6])
                    .style(button::secondary)
                    .into()
            }))
            .spacing(4)
            .wrap(),
        );
    }
    layout = layout.push(editor);

    let mut controls = row![]
        .spacing(12)
        .align_y(iced::alignment::Vertical::Center);
    let (label, submittable) = if editing {
        ("Save", composer.edit_ready())
    } else {
        controls = controls.push(
            checkbox(composer.everyone)
                .label("Allow @everyone and @here to notify")
                .on_toggle(Event::Everyone)
                .size(14)
                .text_size(12),
        );
        (
            "Send",
            composer.ready(snapshot.can_send, snapshot.outbox.len())
                && !composer.reply_deleted(snapshot),
        )
    };
    if !editing {
        controls = controls
            .push(small_button("Attach files", Event::Attach))
            .push(small_button("Paste image", Event::PasteImage));
    }
    controls = controls.push(space::horizontal());
    if let Some(note) = length_note(composer, snapshot.outbox.len()) {
        controls = controls.push(text(note).size(12).style(text::secondary));
    }
    controls = controls.push(
        button(text(label).size(13))
            .on_press_maybe(submittable.then_some(Event::Send))
            .padding([4, 14]),
    );
    layout.push(controls).into()
}

/// A short line about why Send is unavailable or what to watch, if any.
fn length_note(composer: &Composer, outbox_len: usize) -> Option<String> {
    if let Some(notice) = composer.notice {
        return Some(notice.to_owned());
    }
    if outbox_len >= MAX_OUTBOX && composer.editing.is_none() {
        return Some("Retry or discard unsent messages first.".to_owned());
    }
    if composer.trimmed_chars > MAX_CONTENT_CHARS {
        return Some(format!(
            "{} / {MAX_CONTENT_CHARS} characters: too long for Discord.",
            composer.trimmed_chars
        ));
    }
    (composer.trimmed_chars >= NOTE_FROM_CHARS).then(|| {
        format!(
            "{} characters (Discord allows 2,000, or 4,000 with Nitro).",
            composer.trimmed_chars
        )
    })
}

#[cfg(test)]
mod tests {
    use iced::widget::text_editor::Edit;

    use super::*;

    const A: Snowflake = Snowflake(500);
    const B: Snowflake = Snowflake(501);

    fn type_text(composer: &mut Composer, text: &str) {
        for c in text.chars() {
            composer.perform(Action::Edit(if c == '\n' {
                Edit::Enter
            } else {
                Edit::Insert(c)
            }));
        }
    }

    fn paste(composer: &mut Composer, text: &str) {
        composer.perform(Action::Edit(Edit::Paste(Arc::new(text.to_owned()))));
    }

    fn composer_in(channel: Snowflake) -> Composer {
        let mut composer = Composer::default();
        composer.select(Some(channel));
        composer
    }

    #[test]
    fn typing_builds_a_submission_that_is_only_made_when_sending_is_allowed() {
        let mut composer = composer_in(A);
        assert_eq!(composer.submission(true, 0), None, "empty");
        type_text(&mut composer, "  \n  ");
        assert_eq!(composer.submission(true, 0), None, "whitespace only");
        type_text(&mut composer, "hello\nworld");
        let submission = composer.submission(true, 0).unwrap();
        assert_eq!(submission.channel, A);
        assert_eq!(submission.content.trim(), "hello\nworld");
        assert!(!submission.everyone);
        // The channel, the outbox limit, and the permission gate it.
        assert_eq!(composer.submission(false, 0), None);
        assert_eq!(composer.submission(true, MAX_OUTBOX), None);
        assert!(composer.submission(true, MAX_OUTBOX - 1).is_some());
        // Building a submission changes nothing: only `sent` clears the draft.
        assert!(composer.submission(true, 0).is_some());
        composer.sent();
        assert_eq!(composer.submission(true, 0), None);
    }

    #[test]
    fn typing_is_announced_only_for_text_edits_of_a_new_message() {
        let mut composer = composer_in(A);
        let edit = Event::Action(Act(Action::Edit(Edit::Insert('x'))));
        let motion = Event::Action(Act(Action::Move(Motion::Left)));
        assert_eq!(composer.typing_channel(&edit), None, "nothing written yet");
        type_text(&mut composer, "hi");
        assert_eq!(composer.typing_channel(&edit), Some(A));
        assert_eq!(
            composer.typing_channel(&motion),
            None,
            "cursor moves are silent"
        );
        assert_eq!(composer.typing_channel(&Event::Send), None);
    }

    #[test]
    fn the_typing_line_names_one_two_or_several() {
        let n = |names: &[&str]| {
            typing_line(&names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
        };
        assert_eq!(n(&[]), None);
        assert_eq!(n(&["Ann"]).unwrap(), "Ann is typing…");
        assert_eq!(n(&["Ann", "Bo"]).unwrap(), "Ann and Bo are typing…");
        assert_eq!(n(&["A", "B", "C"]).unwrap(), "Several people are typing…");
    }

    #[test]
    fn everyone_is_an_explicit_per_message_choice() {
        let mut composer = composer_in(A);
        type_text(&mut composer, "@everyone hi");
        assert!(
            !composer.submission(true, 0).unwrap().everyone,
            "off by default"
        );
        composer.set_everyone(true);
        assert!(composer.submission(true, 0).unwrap().everyone);
        composer.sent();
        type_text(&mut composer, "again");
        assert!(
            !composer.submission(true, 0).unwrap().everyone,
            "not carried over"
        );
        composer.set_everyone(true);
        composer.select(Some(B));
        type_text(&mut composer, "elsewhere");
        assert!(
            !composer.submission(true, 0).unwrap().everyone,
            "not carried across channels"
        );
    }

    #[test]
    fn replying_preserves_the_draft_and_sends_the_explicit_author_choice() {
        use crate::gateway::Command;

        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let mut snapshot = open(A, Vec::new());
        snapshot.rows = vec![sent(7, "quoted message", false, None)];
        let mut composer = composer_in(A);
        type_text(&mut composer, "my answer");

        assert!(composer.begin_reply(&snapshot, Snowflake(7)));
        let initial = composer.submission(true, 0).unwrap();
        assert_eq!(initial.content, "my answer");
        let target = initial.reply.as_ref().unwrap();
        assert_eq!(target.message, Snowflake(7));
        assert_eq!(&*target.author, "fixture");
        assert_eq!(&*target.content, "quoted message");
        assert!(
            !target.mention_author,
            "replied-user mention is off by default"
        );
        assert!(!format!("{initial:?}").contains("quoted message"));

        update(&mut composer, Event::ReplyMention(true), &snapshot, &bridge);
        update(&mut composer, Event::Send, &snapshot, &bridge);
        match commands.try_recv().unwrap() {
            Command::Send {
                channel,
                content,
                files: _,
                everyone,
                reply: Some(reply),
            } => {
                assert_eq!((channel, content.trim(), everyone), (A, "my answer", false));
                assert_eq!(reply.message, Snowflake(7));
                assert!(reply.mention_author);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(composer.blank && composer.replying.is_none());
    }

    #[test]
    fn a_deleted_reply_target_keeps_the_draft_and_never_sends_plain_text() {
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let mut snapshot = open(A, Vec::new());
        snapshot.rows = vec![sent(7, "original", false, None)];
        let mut composer = composer_in(A);
        assert!(composer.begin_reply(&snapshot, Snowflake(7)));
        type_text(&mut composer, "answer");
        snapshot.deleted.push(Snowflake(7));

        update(&mut composer, Event::Send, &snapshot, &bridge);
        assert!(commands.try_recv().is_err());
        assert_eq!(composer.content.text(), "answer");
        assert!(composer.replying.is_some());
        assert_eq!(composer.notice, Some(REPLY_DELETED));
    }
    #[test]
    fn editing_a_failed_reply_restores_its_reference_and_blocks_a_deleted_target() {
        use crate::gateway::Command;

        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let reply = Reply {
            message: Snowflake(7),
            author: Arc::from("fixture"),
            content: Arc::from("original"),
            mention_author: true,
        };
        let mut snapshot = open(
            A,
            vec![OutboxItem {
                id: OpId(9),
                channel_id: A,
                content: Arc::from("answer"),
                reply: Some(reply.clone()),
                state: OutboxState::Failed("Discord rejected the reply."),
                files: Vec::new(),
                upload: None,
            }],
        );
        snapshot.deleted.push(Snowflake(7));
        let mut composer = composer_in(A);

        update(&mut composer, Event::Edit(OpId(9)), &snapshot, &bridge);
        assert!(matches!(commands.try_recv(), Ok(Command::Discard(OpId(9)))));
        assert_eq!(composer.content.text().trim(), "answer");
        assert_eq!(composer.replying(), Some(Snowflake(7)));
        assert_eq!(
            composer.submission(true, 0).unwrap().reply,
            Some(reply.clone())
        );

        update(&mut composer, Event::Send, &snapshot, &bridge);
        assert!(
            commands.try_recv().is_err(),
            "no plain message may be queued"
        );
        assert_eq!(composer.content.text().trim(), "answer");
        assert_eq!(composer.replying(), Some(Snowflake(7)));
        assert_eq!(composer.notice, Some(REPLY_DELETED));
    }

    #[test]
    fn restoring_a_failed_send_does_not_replace_incompatible_draft_metadata() {
        let mut composer = composer_in(A);
        type_text(&mut composer, "current draft");
        let current = Reply {
            message: Snowflake(1),
            author: Arc::from("current"),
            content: Arc::from("current target"),
            mention_author: false,
        };
        let failed = Reply {
            message: Snowflake(2),
            author: Arc::from("failed"),
            content: Arc::from("failed target"),
            mention_author: true,
        };
        composer.replying = Some(current.clone());
        assert!(!composer.restore_send("failed text", &[], Some(failed)));
        assert_eq!(composer.content.text(), "current draft");
        assert_eq!(composer.replying, Some(current));
        assert!(composer.notice.is_some());
    }

    #[test]
    fn a_draft_over_discords_limit_cannot_be_sent_and_the_editor_is_capped() {
        let mut composer = composer_in(A);
        paste(&mut composer, &"x".repeat(MAX_CONTENT_CHARS));
        assert!(
            composer.submission(true, 0).is_some(),
            "exactly at the limit"
        );
        type_text(&mut composer, "y");
        assert_eq!(composer.submission(true, 0), None, "one over");
        assert!(length_note(&composer, 0).unwrap().contains("too long"));
        // Whitespace around the text is not counted, as it is not sent.
        let mut padded = composer_in(A);
        paste(
            &mut padded,
            &format!("  {}  ", "x".repeat(MAX_CONTENT_CHARS)),
        );
        assert!(padded.submission(true, 0).is_some());
        // The editor stops growing at its hard cap, however the text arrives.
        let mut flood = composer_in(A);
        paste(&mut flood, &"z".repeat(MAX_EDITOR_CHARS));
        assert_eq!(flood.chars, MAX_EDITOR_CHARS);
        paste(&mut flood, "more");
        type_text(&mut flood, "more");
        flood.perform(Action::Edit(Edit::Enter));
        assert_eq!(flood.chars, MAX_EDITOR_CHARS, "refused");
        assert!(flood.notice.is_some());
        // Deleting is still possible, and clears the notice.
        flood.perform(Action::Edit(Edit::Backspace));
        assert_eq!(flood.chars, MAX_EDITOR_CHARS - 1);
        assert!(flood.notice.is_none());
    }

    #[test]
    fn drafts_follow_the_channel_and_stay_bounded() {
        let mut composer = composer_in(A);
        type_text(&mut composer, "draft for A");
        composer.select(Some(B));
        assert!(composer.blank, "B starts empty");
        type_text(&mut composer, "draft for B");
        composer.select(None);
        composer.select(Some(A));
        assert_eq!(composer.content.text().trim(), "draft for A");
        composer.select(Some(B));
        assert_eq!(composer.content.text().trim(), "draft for B");
        // Selecting what is already selected keeps everything.
        composer.select(Some(B));
        assert_eq!(composer.content.text().trim(), "draft for B");
        // A submitted message leaves no draft behind.
        composer.sent();
        composer.select(Some(A));
        composer.select(Some(B));
        assert!(composer.blank);
        // Only the most recent few channels keep a draft.
        let mut composer = Composer::default();
        for id in 0..(MAX_DRAFTS as u64 + 10) {
            composer.select(Some(Snowflake(1_000 + id)));
            type_text(&mut composer, "keep me");
        }
        composer.select(None);
        assert_eq!(composer.drafts.entries.len(), MAX_DRAFTS);
        composer.select(Some(Snowflake(1_000)));
        assert!(composer.blank, "the oldest draft was dropped");
        composer.select(Some(Snowflake(1_000 + MAX_DRAFTS as u64 + 9)));
        assert_eq!(composer.content.text().trim(), "keep me");
    }

    #[test]
    fn a_failed_message_can_be_moved_back_into_the_editor() {
        let mut composer = composer_in(A);
        assert!(composer.restore("first\nline two"));
        assert_eq!(composer.content.text().trim_end(), "first\nline two");
        // With text already there, it is appended after a blank line.
        assert!(composer.restore("second"));
        assert_eq!(
            composer.content.text().trim_end(),
            "first\nline two\n\nsecond"
        );
        // It must fit; otherwise nothing changes.
        let before = composer.content.text();
        assert!(!composer.restore(&"q".repeat(MAX_EDITOR_CHARS)));
        assert_eq!(composer.content.text(), before);
        assert!(composer.notice.is_some());
        // Whitespace-only text counts as blank and is replaced.
        let mut blank = composer_in(B);
        type_text(&mut blank, "   ");
        assert!(blank.restore("fresh"));
        assert_eq!(blank.content.text().trim_end(), "fresh");
    }

    #[test]
    fn debug_output_never_contains_typed_text() {
        let event = Event::Action(Act(Action::Edit(Edit::Paste(Arc::new(
            "private words".to_owned(),
        )))));
        assert!(!format!("{event:?}").contains("private"));
        assert!(!format!("{:?}", Act(Action::Edit(Edit::Insert('p')))).contains('p'));
    }

    #[test]
    fn previews_are_single_line_and_bounded() {
        assert_eq!(preview("a\nb\tc"), "a b c");
        let long = "x".repeat(PREVIEW_CHARS + 50);
        let shown = preview(&long);
        assert_eq!(shown.chars().count(), PREVIEW_CHARS + 1);
        assert!(shown.ends_with('…'));
        let reply = reply_preview(&long);
        assert_eq!(reply.chars().count(), PREVIEW_CHARS);
        assert!(reply.ends_with('…'));
        assert_eq!(
            preview(&"y".repeat(PREVIEW_CHARS)).chars().count(),
            PREVIEW_CHARS
        );
    }

    #[test]
    fn length_notes_warn_before_the_limit_and_explain_a_full_outbox() {
        let mut composer = composer_in(A);
        assert_eq!(length_note(&composer, 0), None);
        paste(&mut composer, &"x".repeat(NOTE_FROM_CHARS));
        assert!(length_note(&composer, 0).unwrap().contains("2,000"));
        assert!(
            length_note(&composer, MAX_OUTBOX)
                .unwrap()
                .contains("unsent")
        );
    }

    #[test]
    fn mention_words_need_a_leading_boundary_and_a_short_query() {
        assert_eq!(mention_token("hi @al", 6), Some(('@', "al")));
        assert_eq!(mention_token("@", 1), Some(('@', "")));
        assert_eq!(mention_token("see #gen", 8), Some(('#', "gen")));
        assert_eq!(mention_token("mail me@example", 15), None);
        assert_eq!(mention_token("hi @al b", 8), None);
        assert_eq!(mention_token("hi @al b", 6), Some(('@', "al")));
        assert_eq!(mention_token("@\u{e9}", 3), Some(('@', "\u{e9}")));
        assert_eq!(mention_token("@\u{e9}", 1), Some(('@', "")));
        assert_eq!(mention_token(&format!("@{}", "a".repeat(40)), 41), None);
    }

    fn candidate(kind: CandidateKind, id: u64, label: &str) -> Candidate {
        Candidate {
            kind,
            id: Snowflake(id),
            label: label.to_owned(),
        }
    }

    #[test]
    fn completion_inserts_the_chosen_reference_and_keeps_the_rest() {
        let mut composer = composer_in(A);
        let mut snapshot = open(A, Vec::new());
        snapshot.candidates = vec![
            candidate(CandidateKind::User, 11, "Malice"),
            candidate(CandidateKind::User, 12, "Alice"),
            candidate(CandidateKind::Role, 13, "mods"),
            candidate(CandidateKind::Channel, 14, "general"),
        ];
        type_text(&mut composer, "hi @ali");
        let picks = composer.completions(&snapshot);
        // Prefix matches come before substring matches; channels are not offered.
        assert_eq!(
            picks.iter().map(|(c, _)| c.id.0).collect::<Vec<_>>(),
            [12, 11]
        );
        let (pick, replaced) = picks[0];
        assert_eq!(replaced, 4);
        let insertion = pick.reference();
        composer.complete(replaced, &insertion);
        assert_eq!(composer.content.text().trim_end(), "hi <@12>");
        assert!(composer.completions(&snapshot).is_empty());

        type_text(&mut composer, "#gen");
        let picks = composer.completions(&snapshot);
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0].0.reference(), "<#14> ");
        composer.complete(picks[0].1, &picks[0].0.reference());
        assert_eq!(composer.content.text().trim_end(), "hi <@12> <#14>");

        type_text(&mut composer, "@mo");
        assert_eq!(composer.completions(&snapshot)[0].0.reference(), "<@&13> ");
    }

    fn real_file(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fastcord-composer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn attached_files_make_a_text_free_send_and_clear_when_it_is_queued() {
        let mut composer = composer_in(A);
        let snapshot = open(A, Vec::new());
        assert!(
            composer.submission(true, 0).is_none(),
            "nothing to send yet"
        );
        let photo = real_file("send-a.png", b"png");
        composer.attach(vec![photo.clone(), photo.clone()]);
        assert_eq!(composer.files.len(), 1, "the same file is not added twice");
        let submission = composer.submission(true, 0).unwrap();
        assert_eq!(
            (submission.content.as_str(), submission.files.len()),
            ("", 1)
        );
        // Missing, empty and folder choices are said and not attached.
        composer.attach(vec![photo.with_file_name("gone.png")]);
        assert!(composer.notice.unwrap().contains("missing"));
        composer.attach(vec![real_file("empty-a.png", b"")]);
        assert!(composer.notice.unwrap().contains("empty"));
        assert_eq!(composer.files.len(), 1);
        // The bridge queues it and the draft clears only then.
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        update(&mut composer, Event::Send, &snapshot, &bridge);
        assert!(composer.files.is_empty());
        assert!(matches!(
            commands.try_recv(),
            Ok(crate::gateway::Command::Send { files, .. }) if files.len() == 1
        ));
    }

    #[test]
    fn at_most_ten_files_and_none_survive_a_channel_switch_or_removal() {
        let mut composer = composer_in(A);
        let paths: Vec<PathBuf> = (0..11)
            .map(|n| real_file(&format!("many-{n}.png"), b"x"))
            .collect();
        composer.attach(paths);
        assert_eq!(composer.files.len(), MAX_UPLOAD_FILES);
        assert!(composer.notice.unwrap().contains("10"));
        composer.detach(0);
        composer.detach(99);
        assert_eq!(composer.files.len(), MAX_UPLOAD_FILES - 1);
        composer.select(Some(B));
        assert!(composer.files.is_empty());
    }

    #[test]
    fn a_failed_send_brings_its_files_back_with_its_text() {
        let mut composer = composer_in(A);
        let file = UploadFile::inspect(&real_file("restore-a.png", b"x")).unwrap();
        assert!(composer.restore_send("caption", std::slice::from_ref(&file), None));
        assert_eq!(composer.files.len(), 1);
        assert!(!composer.blank);
    }

    #[test]
    fn a_pasted_image_is_deleted_once_nothing_holds_it() {
        let mut composer = composer_in(A);
        let path = real_file("pasted-a.png", b"png");
        composer.attach_pasted(&path);
        assert!(path.exists());
        let held = composer.submission(true, 0).unwrap();
        composer.detach(0);
        assert!(path.exists(), "the queued send still needs it");
        drop(held);
        assert!(!path.exists());
        // Refused pastes (no open channel) leave no file behind either.
        let other = real_file("pasted-b.png", b"png");
        Composer::default().attach_pasted(&other);
        assert!(!other.exists());
    }

    fn open(channel: Snowflake, outbox: Vec<OutboxItem>) -> Snapshot {
        Snapshot {
            channel_id: Some(channel),
            can_send: true,
            outbox,
            ..Snapshot::default()
        }
    }

    #[test]
    fn send_hands_the_draft_to_the_worker_and_clears_it_only_when_queued() {
        use crate::gateway::Command;
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let mut composer = composer_in(A);
        type_text(&mut composer, "hello @everyone");
        composer.set_everyone(true);
        // A snapshot of another channel (a switch in progress) sends nothing.
        update(&mut composer, Event::Send, &open(B, Vec::new()), &bridge);
        assert!(commands.try_recv().is_err());
        assert!(!composer.blank);
        update(&mut composer, Event::Send, &open(A, Vec::new()), &bridge);
        match commands.try_recv().unwrap() {
            Command::Send {
                channel,
                content,
                everyone,
                ..
            } => {
                assert_eq!(
                    (channel, content.trim(), everyone),
                    (A, "hello @everyone", true)
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(composer.blank && !composer.everyone, "cleared once queued");
        // Without a worker, the text stays and the user is told.
        drop(commands);
        type_text(&mut composer, "kept");
        update(&mut composer, Event::Send, &open(A, Vec::new()), &bridge);
        assert_eq!(composer.content.text().trim(), "kept");
        assert_eq!(composer.notice, Some(NOT_QUEUED));
    }

    #[test]
    fn edit_moves_only_a_failed_message_back_and_then_discards_it() {
        use crate::gateway::Command;
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let item = |id, state| OutboxItem {
            id: OpId(id),
            channel_id: A,
            content: Arc::from(format!("text {id}")),
            reply: None,
            state,
            files: Vec::new(),
            upload: None,
        };
        let snapshot = open(
            A,
            vec![
                item(1, OutboxState::Failed("refused")),
                item(2, OutboxState::Uncertain("lost")),
                item(3, OutboxState::Sending),
                OutboxItem {
                    channel_id: B,
                    ..item(4, OutboxState::Failed("refused"))
                },
            ],
        );
        let mut composer = composer_in(A);
        for id in [2, 3, 4, 99] {
            update(&mut composer, Event::Edit(OpId(id)), &snapshot, &bridge);
        }
        assert!(
            composer.blank,
            "uncertain, sending, other channels', or unknown messages stay put"
        );
        assert!(commands.try_recv().is_err());
        update(&mut composer, Event::Edit(OpId(1)), &snapshot, &bridge);
        assert_eq!(composer.content.text().trim(), "text 1");
        assert!(matches!(commands.try_recv(), Ok(Command::Discard(OpId(1)))));
        // Retry and discard go to the worker unchanged.
        update(&mut composer, Event::Retry(OpId(2)), &snapshot, &bridge);
        update(&mut composer, Event::Discard(OpId(2)), &snapshot, &bridge);
        assert!(matches!(commands.try_recv(), Ok(Command::Retry(OpId(2)))));
        assert!(matches!(commands.try_recv(), Ok(Command::Discard(OpId(2)))));
    }

    fn sent(id: u64, content: &str, own: bool, change: Option<RowChange>) -> crate::timeline::Row {
        use fastcord_model::{Message, User};
        crate::timeline::Row {
            message: Arc::new(Message {
                id: Snowflake(id),
                channel_id: A,
                guild_id: None,
                author: User {
                    id: Snowflake(if own { 42 } else { 7 }),
                    username: "fixture".to_owned(),
                    global_name: None,
                    avatar: None,
                    bot: false,
                },
                content: content.to_owned(),
                timestamp: "2026-10-08T09:00:00.000000+00:00".to_owned(),
                edited_timestamp: None,
                kind: 0,
                flags: 0,
                pinned: false,
                attachments: Vec::new(),
                reactions: Vec::new(),
                sticker_items: Vec::new(),
                message_snapshots: Vec::new(),
                mentions: Vec::new(),
                mention_roles: Vec::new(),
                mention_everyone: false,
                embeds: Vec::new(),
                message_reference: None,
                referenced_message: fastcord_model::Referenced::Unknown,
                nonce: None,
                call: None,
            }),
            revision: 1,
            own,
            change,
            reply: None,
            reactions: Vec::new(),
        }
    }

    fn showing(rows: Vec<crate::timeline::Row>) -> Snapshot {
        Snapshot {
            rows,
            ..open(A, Vec::new())
        }
    }

    #[test]
    fn editing_sets_the_draft_aside_and_saving_or_cancelling_brings_it_back() {
        use crate::gateway::Command;
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let snapshot = showing(vec![sent(7, "original", true, None)]);
        let mut composer = composer_in(A);
        type_text(&mut composer, "half-written");
        composer.set_everyone(true);
        assert!(composer.begin_edit(&snapshot, Snowflake(7)));
        assert_eq!(composer.editing(), Some(Snowflake(7)));
        assert_eq!(composer.content.text(), "original");
        assert!(!composer.everyone, "@everyone belongs to the draft");
        type_text(&mut composer, " text");
        assert_eq!(
            composer.submission(true, 0),
            None,
            "Enter saves, never sends"
        );
        update(&mut composer, Event::Send, &snapshot, &bridge);
        match commands.try_recv().unwrap() {
            Command::Edit {
                channel,
                message,
                content,
            } => assert_eq!(
                (channel, message, content.as_str()),
                (A, Snowflake(7), "original text")
            ),
            other => panic!("unexpected {other:?}"),
        }
        assert!(commands.try_recv().is_err(), "nothing was sent");
        assert_eq!(composer.editing(), None);
        assert_eq!(composer.content.text(), "half-written");
        assert!(composer.everyone, "the draft's choice comes back with it");
        // Escape (CancelEdit) saves nothing and restores the draft too.
        assert!(composer.begin_edit(&snapshot, Snowflake(7)));
        type_text(&mut composer, " never saved");
        update(&mut composer, Event::CancelEdit, &snapshot, &bridge);
        assert!(commands.try_recv().is_err());
        assert_eq!(composer.content.text(), "half-written");
        // Without a worker the edited text stays in the editor.
        assert!(composer.begin_edit(&snapshot, Snowflake(7)));
        drop(commands);
        update(&mut composer, Event::Send, &snapshot, &bridge);
        assert_eq!(composer.editing(), Some(Snowflake(7)));
        assert_eq!(composer.notice, Some(BUSY));
    }

    #[test]
    fn only_the_users_rows_without_a_change_in_progress_can_be_edited() {
        use crate::changes::{ChangeKind, ChangeState};
        let change = |state| {
            Some(RowChange {
                kind: ChangeKind::Edit(Arc::from("my fix")),
                state,
            })
        };
        let snapshot = showing(vec![
            sent(1, "theirs", false, None),
            sent(2, "mine", true, change(ChangeState::Saving)),
            sent(3, "mine too", true, change(ChangeState::Failed("refused"))),
            sent(4, "mine as well", true, None),
        ]);
        let mut composer = composer_in(A);
        assert!(
            !composer.begin_edit(&snapshot, Snowflake(1)),
            "someone else's"
        );
        assert!(!composer.begin_edit(&snapshot, Snowflake(2)), "being saved");
        assert!(!composer.begin_edit(&snapshot, Snowflake(99)), "not shown");
        let elsewhere = Snapshot {
            channel_id: Some(B),
            ..snapshot.clone()
        };
        assert!(
            !composer.begin_edit(&elsewhere, Snowflake(4)),
            "another channel"
        );
        assert_eq!(composer.editing(), None);
        // A failed edit comes back as the user wrote it, not as Discord has it.
        type_text(&mut composer, "draft");
        assert!(composer.begin_edit(&snapshot, Snowflake(3)));
        assert_eq!(composer.content.text(), "my fix");
        // Switching to another message keeps the original draft aside.
        assert!(composer.begin_edit(&snapshot, Snowflake(4)));
        assert_eq!(composer.content.text(), "mine as well");
        composer.cancel_edit();
        assert_eq!(composer.content.text(), "draft");
    }

    #[test]
    fn an_empty_edit_is_refused_unless_attachments_remain() {
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let mut with_file = sent(8, "caption", true, None);
        Arc::make_mut(&mut with_file.message)
            .attachments
            .push(fastcord_model::Attachment {
                id: Snowflake(80),
                filename: "file.txt".to_owned(),
                size: 3,
                url: "https://cdn.example/file.txt".to_owned(),
                proxy_url: None,
                content_type: None,
                width: None,
                height: None,
            });
        let snapshot = showing(vec![sent(7, "text", true, None), with_file]);
        let mut composer = composer_in(A);
        assert!(composer.begin_edit(&snapshot, Snowflake(7)));
        composer.content = Content::new();
        composer.recount();
        assert!(composer.edit_submission().is_none());
        update(&mut composer, Event::Send, &snapshot, &bridge);
        assert!(commands.try_recv().is_err());
        assert_eq!(composer.notice, Some(EMPTY_EDIT));
        assert!(composer.begin_edit(&snapshot, Snowflake(8)));
        composer.content = Content::new();
        composer.recount();
        update(&mut composer, Event::Send, &snapshot, &bridge);
        assert!(commands.try_recv().is_ok(), "the file remains the message");
        // Over Discord's limit, Save is unavailable as Send is.
        assert!(composer.begin_edit(&snapshot, Snowflake(7)));
        paste(&mut composer, &"x".repeat(MAX_CONTENT_CHARS));
        assert!(composer.edit_submission().is_none());
    }

    #[test]
    fn switching_channels_ends_editing_and_keeps_the_draft() {
        let snapshot = showing(vec![sent(7, "original", true, None)]);
        let mut composer = composer_in(A);
        type_text(&mut composer, "draft for A");
        assert!(composer.begin_edit(&snapshot, Snowflake(7)));
        composer.select(Some(B));
        assert_eq!(composer.editing(), None);
        assert!(composer.blank);
        composer.select(Some(A));
        assert_eq!(composer.content.text(), "draft for A");
        // Moving a failed send back into the editor also ends editing first.
        assert!(composer.begin_edit(&snapshot, Snowflake(7)));
        let failed = OutboxItem {
            id: OpId(1),
            channel_id: A,
            content: Arc::from("unsent"),
            reply: None,
            state: OutboxState::Failed("refused"),
            files: Vec::new(),
            upload: None,
        };
        let bridge = NavigationBridge::new();
        let _commands = bridge.take_commands().unwrap();
        update(
            &mut composer,
            Event::Edit(OpId(1)),
            &Snapshot {
                outbox: vec![failed],
                ..snapshot
            },
            &bridge,
        );
        assert_eq!(composer.editing(), None);
        assert_eq!(composer.content.text(), "draft for A\n\nunsent");
    }

    #[test]
    fn picker_inserts_at_cursor_and_replaces_selection_without_changing_source() {
        let mut composer = composer_in(A);
        paste(&mut composer, "ab\ncd");
        composer.perform(Action::Move(Motion::DocumentStart));
        composer.perform(Action::Move(Motion::Right));
        let sequence = "👩🏽‍💻👍🏻🇺🇦";
        assert!(composer.insert_emoji(sequence));
        assert_eq!(composer.content.text(), format!("a{sequence}b\ncd"));
        composer.perform(Action::SelectAll);
        assert!(composer.insert_emoji("<:sample:12345> <a:party:45678>"));
        assert_eq!(
            composer.submission(true, 0).unwrap().content,
            "<:sample:12345> <a:party:45678>"
        );
        composer.select(Some(B));
        composer.select(Some(A));
        assert_eq!(composer.content.text(), "<:sample:12345> <a:party:45678>");
    }

    #[test]
    fn a_full_editor_allows_selection_replacement_but_never_grows_for_a_picker() {
        let mut composer = composer_in(A);
        paste(&mut composer, &"x".repeat(MAX_EDITOR_CHARS));
        assert!(!composer.insert_emoji("😀"));
        assert_eq!(composer.chars, MAX_EDITOR_CHARS);
        composer.perform(Action::SelectAll);
        assert!(composer.insert_emoji("👍🏿"));
        assert_eq!(composer.content.text(), "👍🏿");
    }
}
