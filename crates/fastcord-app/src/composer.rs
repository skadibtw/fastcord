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
use std::sync::Arc;

use fastcord_discord::MAX_CONTENT_CHARS;
use fastcord_model::Snowflake;
use iced::keyboard::{self, key};
use iced::widget::text_editor::{Action, Binding, Content, Edit, Motion, Status};
use iced::widget::{
    button, checkbox, column, container, row, scrollable, space, text, text_editor,
};
use iced::{Element, Length, Padding};

use crate::gateway::NavigationBridge;
use crate::outbox::{MAX_OUTBOX, OpId, OutboxItem, OutboxState};
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
    Retry(OpId),
    Discard(OpId),
    /// Move a failed message's text back into the editor and drop the failure.
    Edit(OpId),
}

/// A draft the user chose to send.
#[derive(Debug, PartialEq, Eq)]
pub struct Submission {
    pub channel: Snowflake,
    pub content: String,
    pub everyone: bool,
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
    notice: Option<&'static str>,
    drafts: Drafts,
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
            notice: None,
            drafts: Drafts::default(),
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

    /// Follows the open channel: the draft of the one being left is kept, the
    /// one being entered is restored. The `@everyone` choice never survives a switch.
    pub fn select(&mut self, channel: Option<Snowflake>) {
        if self.channel == channel {
            return;
        }
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

    pub fn set_everyone(&mut self, allowed: bool) {
        self.everyone = allowed;
    }

    /// Whether Send is available: there is an open channel the account may
    /// send to, something to send within Discord's limit, and room in the
    /// outbox. Reads only cached counts, so the view can ask every frame.
    fn ready(&self, can_send: bool, outbox_len: usize) -> bool {
        self.channel.is_some()
            && can_send
            && outbox_len < MAX_OUTBOX
            && !self.blank
            && self.trimmed_chars <= MAX_CONTENT_CHARS
    }

    /// The draft as a send request, if it may be sent now (see `ready`). Does
    /// not clear the draft; [`sent`](Self::sent) does, once the worker has
    /// accepted the request.
    pub fn submission(&self, can_send: bool, outbox_len: usize) -> Option<Submission> {
        let channel = self.channel.filter(|_| self.ready(can_send, outbox_len))?;
        Some(Submission {
            channel,
            content: self.content.text(),
            everyone: self.everyone,
        })
    }

    /// The worker accepted the draft: empty the editor.
    pub fn sent(&mut self) {
        self.content = Content::new();
        self.everyone = false;
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
        Event::Send => {
            let open = snapshot.can_send && snapshot.channel_id == composer.channel;
            let Some(submission) = composer.submission(open, snapshot.outbox.len()) else {
                return;
            };
            if bridge.send_message(submission.channel, submission.content, submission.everyone) {
                composer.sent();
            } else {
                composer.notice = Some(NOT_QUEUED);
            }
        }
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
            let failed = snapshot.outbox.iter().find(|item| {
                item.id == id
                    && Some(item.channel_id) == composer.channel
                    && matches!(item.state, OutboxState::Failed(_))
            });
            // The text moves only if it fits; the failed entry goes once it has.
            if let Some(item) = failed
                && composer.restore(&item.content)
                && !bridge.discard_send(id)
            {
                composer.notice = Some(BUSY);
            }
        }
    }
}

const BUSY: &str = "That is not possible right now (busy or disconnected); try again.";
const NOT_QUEUED: &str =
    "Not sent: too many messages are waiting, or the connection has stopped. Your text is kept.";

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
    content = match item.state {
        OutboxState::Sending => content
            .push(body.style(text::secondary))
            .push(text("Sending…").size(12).style(text::secondary)),
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
/// editor and Send are present only where the account may send.
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
    if !snapshot.can_send {
        return layout.into();
    }
    let editor = text_editor(&composer.content)
        .id(EDITOR_ID)
        .placeholder("Message (Enter to send, Shift+Enter for a new line)")
        .on_action(|action| Event::Action(Act(action)))
        .key_binding(|press| {
            let enter = matches!(press.key.as_ref(), keyboard::Key::Named(key::Named::Enter));
            if enter && !press.modifiers.shift() && matches!(press.status, Status::Focused { .. }) {
                Some(Binding::Custom(Event::Send))
            } else {
                Binding::from_key_press(press)
            }
        })
        .size(14)
        .padding(8)
        .min_height(38)
        .max_height(150);
    layout = layout.push(editor);

    let submittable = composer.ready(snapshot.can_send, snapshot.outbox.len());
    let mut controls = row![
        checkbox(composer.everyone)
            .label("Allow @everyone and @here to notify")
            .on_toggle(Event::Everyone)
            .size(14)
            .text_size(12),
        space::horizontal(),
    ]
    .spacing(12)
    .align_y(iced::alignment::Vertical::Center);
    if let Some(note) = length_note(composer, snapshot.outbox.len()) {
        controls = controls.push(text(note).size(12).style(text::secondary));
    }
    controls = controls.push(
        button(text("Send").size(13))
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
    if outbox_len >= MAX_OUTBOX {
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
            state,
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
}
