//! Account-worker history coordination: which page of the selected channel to
//! fetch, how its messages reach the layout index, and the bounded snapshot the
//! timeline view receives. Everything here runs in the account worker; `view`
//! only reads the snapshot. Nothing is fetched until a readable channel is
//! selected, and a page is requested only for a reason: opening the channel,
//! scrolling near the end of what is retained, an explicit button, or retry.
//! Sends, and edits and deletions of the user's own messages, start only from
//! explicit commands and are checked here against the store, never against
//! what a possibly stale UI showed.
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;

use fastcord_discord::gateway::{Dispatch, MessageDelete, MessageReaction};
use fastcord_discord::history::HistoryCursor;
use fastcord_discord::message_store::{
    MAX_MESSAGE_BYTES, MessageStore, PageMerge, PageToken, message_bytes,
};
use fastcord_discord::state::navigation::{ChannelKind, NavigationSnapshot};
use fastcord_discord::{
    ComposeError, EditedMessage, MentionPolicy, NetworkFailure, NonceGenerator, ReactionEmoji,
    ReactionUser, RestClient, RestError, UploadError, UploadFile, Uploader, edit_response_update,
    is_own_message,
};
use fastcord_model::{Emoji, Message, Referenced, Snowflake};
use iced::futures::StreamExt;
use iced::futures::future::{AbortHandle, Abortable};
use iced::futures::stream::FuturesUnordered;

use crate::changes::{self, ChangeKind, ChangeRequest, Changes};
use crate::outbox::{Finished, MAX_REPLY_AUTHOR_CHARS, OpId, Outbox, Reply, SendFailure, Slots};
use crate::reactions::{self, Action, Reactions};
use crate::timeline::{ReactionUsers, ReplyLine, ReplyState, Row, Snapshot, UsersState};
use crate::variable_list::{Item, Measurement, VariableList, Viewport};

/// Message bodies one snapshot may hold. Together with the latest slot and the
/// copy the UI displays this stays inside the 2 MiB UI-delta budget (SPEC §4.5).
pub const SNAPSHOT_BODY_BUDGET: usize = MAX_MESSAGE_BYTES;
/// Measured heights waiting for the worker; the widget reports at most one per
/// built row, so more than this are stale duplicates.
const MAX_MEASUREMENTS: usize = 128;
/// Preview resolutions and deletion markers retained for the open channel.
pub const MAX_REFERENCES: usize = 128;

const OVERSIZE: &str = "A message is too large to display and was left out.";
const NOT_OPEN: &str = "That message's channel is no longer open.";
const NOT_LOADED: &str = "That message is no longer loaded; it may have been deleted.";
const NOT_OWN: &str = "Only your own messages can be edited or deleted.";
const EMPTY_EDIT: &str = "A message needs some text. Delete it instead.";
const LONG_EDIT: &str = "The message is too long for Discord.";
const DELETED_WHILE_EDITING: &str = "That message was deleted before your edit was saved.";
const UNSHOWN_FAILURE: &str = "A change to a message that is no longer shown failed.";
const DELETED_JUMP: &str = "That message was deleted and cannot be opened.";
const UNAVAILABLE_JUMP: &str = "That message is unavailable and cannot be opened.";
const NO_UNREAD: &str = "There are no newer messages.";
const NO_REACTIONS: &str = "You cannot add reactions in this channel.";
const BAD_EMOJI: &str = "That emoji cannot be used as a reaction.";

type PageFuture = Pin<Box<dyn Future<Output = Result<Vec<Message>, RestError>> + Send>>;
type ReferenceFuture =
    Pin<Box<dyn Future<Output = (Snowflake, Result<Option<Message>, RestError>)> + Send>>;
type SendFuture = Pin<Box<dyn Future<Output = (OpId, Result<Box<Message>, SendFailure>)> + Send>>;
type ChangeFuture = Pin<Box<dyn Future<Output = ChangeDone> + Send>>;
type AckFuture =
    Pin<Box<dyn Future<Output = (Snowflake, Snowflake, Result<(), RestError>)> + Send>>;
type ReactionFuture = Pin<Box<dyn Future<Output = ReactionDone> + Send>>;
type UsersFuture =
    Pin<Box<dyn Future<Output = (u64, Result<Vec<ReactionUser>, RestError>)> + Send>>;

/// An acknowledgement to send: the channel's cursor to this message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ack {
    channel: Snowflake,
    message: Snowflake,
    manual: bool,
}

enum ReferenceState {
    Preview { author: String, content: String },
    Deleted,
    Unavailable,
}

fn reply_author(author: &str) -> String {
    author.chars().take(MAX_REPLY_AUTHOR_CHARS).collect()
}

fn reply_content(content: &str) -> String {
    content
        .chars()
        .take(fastcord_model::REPLY_PREVIEW_CHARS)
        .collect()
}

/// Discord's answer to an edit or deletion of one of the user's messages.
pub struct ChangeDone {
    pub channel: Snowflake,
    pub message: Snowflake,
    pub result: ChangeResult,
}

pub enum ChangeResult {
    Edited(Result<Box<Message>, RestError>),
    Deleted(Result<(), RestError>),
}

/// Discord's answer to a reaction request of the user.
pub struct ReactionDone {
    pub channel: Snowflake,
    pub message: Snowflake,
    pub emoji: Emoji,
    pub action: Action,
    pub result: Result<(), RestError>,
}

/// The "who reacted" list the user asked for, as the worker keeps it.
struct Inspection {
    /// Identifies the request that answers it; a replaced list's late answer
    /// finds a different serial and is dropped.
    serial: u64,
    channel: Snowflake,
    message: Snowflake,
    emoji: Emoji,
    state: UsersState,
}

/// A finished request of the worker: a history page, a reply preview, a send,
/// a change, a reaction, or the list of who reacted.
pub enum Completed {
    Page(Result<Vec<Message>, RestError>),
    Reference(Snowflake, Box<Result<Option<Message>, RestError>>),
    Send(OpId, Result<Box<Message>, SendFailure>),
    /// An acknowledgement of (channel, message) was answered.
    Ack(Snowflake, Snowflake, Result<(), RestError>),
    Change(ChangeDone),
    Reaction(ReactionDone),
    ReactionUsers(u64, Result<Vec<ReactionUser>, RestError>),
}

/// An explicit user action from the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    Older(Snowflake),
    Latest(Snowflake),
    Retry(Snowflake),
    JumpTo {
        channel: Snowflake,
        message: Snowflake,
    },
    /// Show the first message after the account's read cursor `acked`.
    FirstUnread {
        channel: Snowflake,
        acked: Snowflake,
    },
}

/// The timeline's one-shot inputs to the worker, coalesced.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// The window is active and the timeline is on screen: what is shown
    /// counts as read.
    pub viewing: bool,
    pub revision: u64,
    pub intent: Option<Intent>,
    pub viewport: Option<(Snowflake, Viewport)>,
    pub measurements: Vec<(Snowflake, Measurement)>,
}

impl Request {
    /// Keeps the newest height per row and width bucket.
    pub fn measure(&mut self, channel: Snowflake, measurement: Measurement) {
        if let Some(slot) = self.measurements.iter_mut().find(|(id, old)| {
            *id == channel
                && old.id == measurement.id
                && old.width_bucket == measurement.width_bucket
        }) {
            *slot = (channel, measurement);
            return;
        }
        if self.measurements.len() == MAX_MEASUREMENTS {
            self.measurements.remove(0);
        }
        self.measurements.push((channel, measurement));
    }
}

#[derive(Default)]
pub struct History {
    store: MessageStore,
    list: VariableList,
    channel: Option<Snowflake>,
    /// Revision of the last explicit intent handled.
    revision: u64,
    error: Option<&'static str>,
    /// The page whose failure the Retry action repeats.
    failed: Option<HistoryCursor>,
    pending: Option<(PageToken, PageFuture)>,
    /// One speculative fetch for a visible reply preview at a time.
    reference_pending: Option<(Snowflake, ReferenceFuture)>,
    /// Bounded previews and known-deleted targets for the open channel.
    references: VecDeque<(Snowflake, ReferenceState)>,
    /// The row reached by the last Jump to a reply target.
    highlight: Option<Snowflake>,
    /// A page just landed: the list may still be short of the viewport.
    check_paging: bool,
    dirty: bool,
    /// The account's own ID (from READY): only its messages can be ours.
    user: Option<Snowflake>,
    /// The open channel when the user may send to it.
    sendable: Option<Snowflake>,
    outbox: Outbox,
    nonces: NonceGenerator,
    sends: FuturesUnordered<SendFuture>,
    /// Sends still uploading files, so the user can stop them. Posting the
    /// message is never abortable: that outcome would be unknowable.
    uploads: HashMap<OpId, AbortHandle>,
    /// Read acknowledgements: one request at a time, the newest wish kept.
    acks: FuturesUnordered<AckFuture>,
    ack_inflight: Option<Ack>,
    ack_wanted: Option<Ack>,
    /// The last automatic acknowledgement tried, so one that fails is not
    /// repeated until a newer message is on screen.
    ack_last: Option<(Snowflake, Snowflake)>,
    /// Acknowledgements Discord accepted, for the reducer to apply locally.
    acked_done: Vec<(Snowflake, Snowflake)>,
    viewing: bool,
    /// (channel, read cursor) whose first unread message the next page around
    /// the cursor finds.
    first_unread: Option<(Snowflake, Snowflake)>,
    /// The credential-free client for file bytes, built on first use.
    uploader: Option<Uploader>,
    /// Edits and deletions of the user's own messages.
    changes: Changes,
    change_requests: FuturesUnordered<ChangeFuture>,
    /// Why the last edit or delete command could not be carried out, or the
    /// outcome of one that has no row to show it on.
    notice: Option<&'static str>,
    /// The open channel when the user may add reactions to it.
    reactable: Option<Snowflake>,
    /// Reactions of the user that Discord has not answered yet.
    reactions: Reactions,
    reaction_requests: FuturesUnordered<ReactionFuture>,
    /// The "who reacted" list the user asked for, and the one request
    /// loading it.
    inspection: Option<Inspection>,
    users_pending: Option<UsersFuture>,
    inspections: u64,
}

impl History {
    /// A history whose outbox frees the UI's reserved `slots`.
    pub fn new(slots: Slots) -> Self {
        Self {
            outbox: Outbox::new(slots),
            ..Self::default()
        }
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    fn cancel(&mut self) {
        if let Some((token, _)) = self.pending.take() {
            self.store.cancel_page(token);
        }
    }

    /// Releases everything: a new session (READY) or a closed channel.
    fn reset(&mut self) {
        self.cancel();
        self.reference_pending = None;
        self.references.clear();
        self.highlight = None;
        self.inspection = None;
        self.users_pending = None;
        self.store.clear();
        self.list = VariableList::default();
        self.channel = None;
        self.error = None;
        self.failed = None;
        self.notice = None;
        self.check_paging = false;
        self.ack_wanted = None;
        self.ack_last = None;
        self.first_unread = None;
        self.prune_changes();
        self.dirty = true;
    }

    /// Failed changes are shown on their message's row; without the row they go.
    fn prune_changes(&mut self) {
        let store = &self.store;
        if self
            .changes
            .retain_held(|channel, message| store.revision(channel, message).is_some())
        {
            self.dirty = true;
        }
    }

    fn reference_state(&self, target: Snowflake) -> Option<&ReferenceState> {
        self.references
            .iter()
            .find(|(id, _)| *id == target)
            .map(|(_, state)| state)
    }

    fn remember_reference(&mut self, target: Snowflake, state: ReferenceState) {
        if let Some(at) = self.references.iter().position(|(id, _)| *id == target) {
            self.references.remove(at);
        }
        self.references.push_back((target, state));
        while self.references.len() > MAX_REFERENCES {
            self.references.pop_front();
        }
    }

    fn mark_reference_deleted(&mut self, channel: Snowflake, target: Snowflake) {
        if self.channel != Some(channel) {
            return;
        }
        if self
            .reference_pending
            .as_ref()
            .is_some_and(|(id, _)| *id == target)
        {
            self.reference_pending = None;
        }
        self.remember_reference(target, ReferenceState::Deleted);
        self.dirty = true;
    }

    /// Applies an ordered Gateway event before the metadata store sees it.
    /// A `MESSAGE_CREATE` of the user's own message that carries an unconfirmed
    /// send's nonce confirms that send, whether or not the REST answer came
    /// first; the store ignores the second copy of a message it already holds.
    /// An update carrying a failed edit's text, or a deletion, settles that
    /// message's change.
    pub fn apply(&mut self, event: &Dispatch) {
        match event {
            Dispatch::Ready(ready) => {
                self.user = Some(ready.user.id);
                self.reset();
                self.store.set_user(ready.user.id);
                return;
            }
            Dispatch::MessageCreate(message) => {
                let ours = self.user == Some(message.author.id);
                if ours
                    && let Some(nonce) = message.nonce.as_deref()
                    && self.outbox.confirm(message.channel_id, nonce)
                {
                    self.dirty = true;
                }
            }
            Dispatch::MessageUpdate(update) => {
                if let Some(content) = update.content.as_deref()
                    && self
                        .changes
                        .observed_text(update.channel_id, update.id, content)
                {
                    self.dirty = true;
                }
            }
            Dispatch::MessageDelete(deleted) => {
                self.dirty |= self
                    .changes
                    .observed_delete(deleted.channel_id, std::slice::from_ref(&deleted.id));
            }
            Dispatch::MessageDeleteBulk(deleted) => {
                self.dirty |= self
                    .changes
                    .observed_delete(deleted.channel_id, &deleted.ids);
            }
            _ => {}
        }
        self.apply_store(event);
    }

    /// Applies a message event, from the Gateway or from a REST answer, to the
    /// store: the one path every message change takes.
    fn apply_store(&mut self, event: &Dispatch) {
        match event {
            Dispatch::MessageDelete(deleted) => {
                self.mark_reference_deleted(deleted.channel_id, deleted.id);
            }
            Dispatch::MessageDeleteBulk(deleted) => {
                for id in &deleted.ids {
                    self.mark_reference_deleted(deleted.channel_id, *id);
                }
            }
            _ => {}
        }
        let change = self.store.apply_dispatch(event);
        if change.changed {
            self.dirty = true;
        }
        if change.rejected > 0 {
            self.error = Some(OVERSIZE);
            self.dirty = true;
        }
    }

    /// Records a message to send and starts posting it. `channel` must be the
    /// open channel and sendable, or the text is kept as a failed operation.
    /// Returns the operation's identity.
    #[cfg(test)]
    pub fn send(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        content: &str,
        everyone: bool,
        reply: Option<Reply>,
    ) -> OpId {
        self.send_with_files(rest, channel, content, Vec::new(), everyone, reply)
    }

    /// [`send`](Self::send) for a message that also uploads `files`.
    pub fn send_with_files(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        content: &str,
        files: Vec<UploadFile>,
        everyone: bool,
        reply: Option<Reply>,
    ) -> OpId {
        let allowed = self.sendable == Some(channel);
        let nonce = self.nonces.next_now();
        let mentions = MentionPolicy {
            everyone,
            ..MentionPolicy::default()
        };
        let id = self
            .outbox
            .enqueue_with_files(channel, content, files, mentions, reply, nonce, allowed);
        // client does; away from the live edge nothing moves.
        let live = self
            .store
            .window(channel)
            .is_some_and(|window| window.newer.is_none());
        if allowed && live && self.channel == Some(channel) {
            self.list.jump_latest();
        }
        self.start_sends(rest);
        self.dirty = true;
        id
    }

    /// The user's explicit retry of a failed or unconfirmed send.
    pub fn retry_send(&mut self, rest: &RestClient, id: OpId) {
        let allowed = self
            .outbox
            .channel_of(id)
            .is_some_and(|channel| self.sendable == Some(channel));
        if allowed && self.outbox.retry(id) {
            self.start_sends(rest);
        }
        self.dirty = true;
    }

    /// Stops a send that is still uploading its files. Nothing was created, so
    /// it stays in the outbox as failed for an explicit retry or discard.
    pub fn cancel_upload(&mut self, id: OpId) {
        if let Some(handle) = self.uploads.get(&id) {
            handle.abort();
        }
    }

    /// The user's explicit discard of a failed or unconfirmed send.
    pub fn discard_send(&mut self, id: OpId) {
        if self.outbox.discard(id) {
            self.dirty = true;
        }
    }

    /// The held message `message` of the open `channel`, if the user may
    /// change it: the store's copy decides, not what the UI displayed.
    fn changeable(
        &self,
        channel: Snowflake,
        message: Snowflake,
    ) -> Result<Arc<Message>, &'static str> {
        if self.channel != Some(channel) {
            return Err(NOT_OPEN);
        }
        let held = self.store.get(channel, message).ok_or(NOT_LOADED)?;
        match self.user {
            Some(user) if is_own_message(&held, user) => Ok(held),
            _ => Err(NOT_OWN),
        }
    }

    /// The user's explicit edit of one of their messages. Only `content` is
    /// sent, so the message keeps its attachments. Text equal to the current
    /// text needs no request (and settles a failed edit of the message).
    pub fn edit_message(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        content: &str,
    ) {
        self.dirty = true;
        self.notice = None;
        let held = match self.changeable(channel, message) {
            Ok(held) => held,
            Err(notice) => {
                self.notice = Some(notice);
                return;
            }
        };
        let allow_empty = !held.attachments.is_empty();
        let edit = match EditedMessage::new(content, allow_empty) {
            Ok(edit) => edit,
            Err(ComposeError::Empty) => {
                self.notice = Some(EMPTY_EDIT);
                return;
            }
            Err(ComposeError::TooLong) => {
                self.notice = Some(LONG_EDIT);
                return;
            }
        };
        if edit.content() == held.content {
            // Back to Discord's text: a failed edit of it is moot.
            let failed_edit = self
                .changes
                .of(channel, message)
                .is_some_and(|change| change.failed_edit().is_some());
            if failed_edit {
                self.changes.dismiss(channel, message);
            }
            return;
        }
        let kind = ChangeKind::Edit(Arc::from(edit.content()));
        match self.changes.begin(channel, message, kind, allow_empty) {
            Ok(()) => self.start_change(rest, channel, message, ChangeRequest::Edit(edit)),
            Err(notice) => self.notice = Some(notice),
        }
    }

    /// The user's explicit, confirmed deletion of one of their messages.
    pub fn delete_message(&mut self, rest: &RestClient, channel: Snowflake, message: Snowflake) {
        self.dirty = true;
        self.notice = None;
        if let Err(notice) = self.changeable(channel, message) {
            self.notice = Some(notice);
            return;
        }
        match self
            .changes
            .begin(channel, message, ChangeKind::Delete, false)
        {
            Ok(()) => self.start_change(rest, channel, message, ChangeRequest::Delete),
            Err(notice) => self.notice = Some(notice),
        }
    }

    /// The user's explicit retry of a failed edit or deletion, re-checked
    /// against the store like a new one.
    pub fn retry_change(&mut self, rest: &RestClient, channel: Snowflake, message: Snowflake) {
        self.dirty = true;
        self.notice = None;
        if let Err(notice) = self.changeable(channel, message) {
            self.notice = Some(notice);
            return;
        }
        if let Some(request) = self.changes.retry(channel, message) {
            self.start_change(rest, channel, message, request);
        }
    }

    /// The user's dismissal of a failed edit or deletion.
    pub fn dismiss_change(&mut self, channel: Snowflake, message: Snowflake) {
        if self.changes.dismiss(channel, message) {
            self.dirty = true;
        }
    }

    /// The user's click on a reaction chip: take their reaction back when it
    /// is theirs, add it otherwise.
    pub fn toggle_reaction(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        emoji: Emoji,
    ) {
        self.react(rest, channel, message, emoji, true);
    }

    /// The user's pick of an emoji in the picker: add their reaction unless
    /// it already is theirs (nothing to ask Discord then).
    pub fn add_reaction(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        emoji: Emoji,
    ) {
        self.react(rest, channel, message, emoji, false);
    }

    /// Checks the request against the store (never against what a possibly
    /// stale UI showed) and makes it once. A new reaction needs the
    /// permission to add reactions; joining one that others already made needs
    /// only the history the user is reading, and taking one's own back needs
    /// nothing. The server stays authoritative.
    fn react(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        emoji: Emoji,
        toggle: bool,
    ) {
        self.dirty = true;
        self.notice = None;
        if self.channel != Some(channel) {
            self.notice = Some(NOT_OPEN);
            return;
        }
        let Some(held) = self.store.get(channel, message) else {
            self.notice = Some(NOT_LOADED);
            return;
        };
        let existing = held
            .reactions
            .iter()
            .find(|reaction| reaction.emoji.same_reaction(&emoji));
        let action = if existing.is_some_and(|reaction| reaction.me) {
            if !toggle {
                return;
            }
            Action::Remove
        } else {
            Action::Add
        };
        if action == Action::Add && existing.is_none() && self.reactable != Some(channel) {
            self.notice = Some(NO_REACTIONS);
            return;
        }
        let Ok(target) = ReactionEmoji::new(&emoji) else {
            self.notice = Some(BAD_EMOJI);
            return;
        };
        if let Err(notice) = self.reactions.begin(channel, message, &emoji, action) {
            self.notice = Some(notice);
            return;
        }
        let rest = rest.clone();
        self.reaction_requests.push(Box::pin(async move {
            let result = match action {
                Action::Add => rest.add_reaction(channel, message, &target).await,
                Action::Remove => rest.remove_own_reaction(channel, message, &target).await,
            };
            ReactionDone {
                channel,
                message,
                emoji,
                action,
                result,
            }
        }));
    }

    /// Applies Discord's answer to a reaction request. Returns whether Discord
    /// rejected the login. A success goes into the store through the same path
    /// as the Gateway's report of the user's own reaction, which makes
    /// whichever arrives second a no-op; a failure changes nothing and is said
    /// once. A removal of a reaction that is already gone is what it asked for.
    fn finish_reaction(&mut self, done: ReactionDone) -> bool {
        let ReactionDone {
            channel,
            message,
            emoji,
            action,
            result,
        } = done;
        self.dirty = true;
        self.reactions.finish(channel, message, &emoji);
        match result {
            Ok(()) => self.apply_own_reaction(channel, message, emoji, action == Action::Add),
            Err(RestError::ResourceGone) if action == Action::Remove => {
                self.apply_own_reaction(channel, message, emoji, false);
            }
            Err(error) => {
                self.notice = Some(reactions::failure(action, error));
                return error == RestError::AuthenticationRequired;
            }
        }
        false
    }

    fn apply_own_reaction(
        &mut self,
        channel: Snowflake,
        message: Snowflake,
        emoji: Emoji,
        added: bool,
    ) {
        let Some(user) = self.user else {
            return;
        };
        let event = Box::new(MessageReaction {
            channel_id: channel,
            message_id: message,
            guild_id: None,
            user_id: user,
            emoji,
            burst: false,
        });
        self.apply_store(&if added {
            Dispatch::MessageReactionAdd(event)
        } else {
            Dispatch::MessageReactionRemove(event)
        });
    }

    /// The user's explicit request to see who reacted with an emoji. Nothing
    /// is listed unless it is asked for; one list is kept at a time.
    pub fn reaction_users(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        emoji: Emoji,
    ) {
        self.dirty = true;
        self.notice = None;
        if self.channel != Some(channel) {
            self.notice = Some(NOT_OPEN);
            return;
        }
        if self.store.get(channel, message).is_none() {
            self.notice = Some(NOT_LOADED);
            return;
        }
        let Ok(target) = ReactionEmoji::new(&emoji) else {
            self.notice = Some(BAD_EMOJI);
            return;
        };
        self.inspections = self.inspections.wrapping_add(1);
        let serial = self.inspections;
        self.inspection = Some(Inspection {
            serial,
            channel,
            message,
            emoji,
            state: UsersState::Loading,
        });
        let rest = rest.clone();
        // One more than is shown says whether Discord has more.
        let limit = u8::try_from(reactions::MAX_USERS + 1).unwrap_or(u8::MAX);
        self.users_pending = Some(Box::pin(async move {
            (
                serial,
                rest.reaction_users(channel, message, &target, limit).await,
            )
        }));
    }

    /// The user closed the list of who reacted; its request is dropped.
    pub fn close_reaction_users(&mut self) {
        if self.inspection.take().is_some() {
            self.dirty = true;
        }
        self.users_pending = None;
    }

    /// Applies the list of who reacted. Returns whether Discord rejected the
    /// login. An answer for a list the user replaced or closed is dropped.
    fn finish_reaction_users(
        &mut self,
        serial: u64,
        result: Result<Vec<ReactionUser>, RestError>,
    ) -> bool {
        self.users_pending = None;
        let Some(inspection) = self
            .inspection
            .as_mut()
            .filter(|inspection| inspection.serial == serial)
        else {
            return false;
        };
        self.dirty = true;
        match result {
            Ok(mut users) => {
                let more = users.len() > reactions::MAX_USERS;
                users.truncate(reactions::MAX_USERS);
                inspection.state = UsersState::Loaded {
                    names: users.into_iter().map(|user| user.name).collect(),
                    more,
                };
                false
            }
            Err(error) => {
                inspection.state = UsersState::Failed(reactions::users_failure(error));
                error == RestError::AuthenticationRequired
            }
        }
    }

    /// A list of who reacted belongs to a row; without the row it goes.
    fn prune_inspection(&mut self) {
        let store = &self.store;
        if self.inspection.as_ref().is_some_and(|inspection| {
            store
                .revision(inspection.channel, inspection.message)
                .is_none()
        }) {
            self.inspection = None;
            self.users_pending = None;
            self.dirty = true;
        }
    }

    /// Makes the request once; its answer comes back through [`Completed`].
    fn start_change(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        request: ChangeRequest,
    ) {
        let rest = rest.clone();
        self.change_requests.push(Box::pin(async move {
            let result = match request {
                ChangeRequest::Edit(edit) => ChangeResult::Edited(
                    rest.edit_message(channel, message, &edit)
                        .await
                        .map(Box::new),
                ),
                ChangeRequest::Delete => {
                    ChangeResult::Deleted(rest.delete_message(channel, message).await)
                }
            };
            ChangeDone {
                channel,
                message,
                result,
            }
        }));
    }

    /// Applies Discord's answer to a change. Returns whether Discord rejected
    /// the login. The answer goes into the store through the same path as the
    /// Gateway's report of it, so whichever arrives second changes nothing; an
    /// edit's answer older than an edit the Gateway already reported is not
    /// applied, and neither can recreate a message that is gone.
    fn finish_change(&mut self, done: ChangeDone) -> bool {
        let ChangeDone {
            channel,
            message,
            result,
        } = done;
        self.dirty = true;
        let (error, edit) = match result {
            ChangeResult::Edited(Ok(answer)) => {
                self.changes.finish(channel, message, Ok(()));
                let held = self.store.get(channel, message);
                if let Some(update) = edit_response_update(held.as_deref(), *answer) {
                    self.apply_store(&Dispatch::MessageUpdate(Box::new(update)));
                }
                return false;
            }
            ChangeResult::Deleted(Ok(())) => (None, false),
            ChangeResult::Edited(Err(error)) => (Some(error), true),
            ChangeResult::Deleted(Err(error)) => (Some(error), false),
        };
        match error {
            // Deleted, or (404) already gone: either way the message no longer
            // exists, and an edit of it cannot be saved.
            None | Some(RestError::ResourceGone) => {
                self.changes.finish(channel, message, Ok(()));
                self.apply_store(&Dispatch::MessageDelete(MessageDelete {
                    id: message,
                    channel_id: channel,
                    guild_id: None,
                }));
                if edit {
                    self.notice = Some(DELETED_WHILE_EDITING);
                }
                false
            }
            Some(error) => {
                let reason = if edit {
                    changes::edit_failure(error)
                } else {
                    changes::delete_failure(error)
                };
                if self.changes.finish(channel, message, Err(reason))
                    && self.store.revision(channel, message).is_none()
                {
                    // No row can show this failure; say so once and let it go.
                    self.changes.dismiss(channel, message);
                    self.notice = Some(UNSHOWN_FAILURE);
                }
                error == RestError::AuthenticationRequired
            }
        }
    }

    /// Posts what is ready: one request per channel at a time, each exactly once.
    fn start_sends(&mut self, rest: &RestClient) {
        for (id, channel, message) in self.outbox.take_ready() {
            let rest = rest.clone();
            self.sends.push(Box::pin(async move {
                (
                    id,
                    rest.create_message(channel, &message)
                        .await
                        .map(Box::new)
                        .map_err(SendFailure::from),
                )
            }));
        }
        for (id, channel, post) in self.outbox.take_ready_files() {
            let Some(uploader) = self.uploader.clone().or_else(|| {
                self.uploader = Uploader::new().ok();
                self.uploader.clone()
            }) else {
                self.outbox.fail(
                    id,
                    SendFailure::Upload(UploadError::Network(NetworkFailure::Other)),
                );
                continue;
            };
            let rest = rest.clone();
            let (handle, registration) = AbortHandle::new_pair();
            self.uploads.insert(id, handle);
            self.sends.push(Box::pin(async move {
                let upload = rest.upload_files(&uploader, channel, post.files(), post.progress());
                let uploaded = match Abortable::new(upload, registration).await {
                    Ok(Ok(uploaded)) => uploaded,
                    Ok(Err(error)) => return (id, Err(SendFailure::Upload(error))),
                    Err(_) => return (id, Err(SendFailure::Cancelled)),
                };
                // From here the outcome of the post can be unknowable, so it
                // is no longer abortable.
                let Ok(message) = post.message(uploaded) else {
                    return (id, Err(SendFailure::Upload(UploadError::BadSlots)));
                };
                (
                    id,
                    rest.create_message(channel, &message)
                        .await
                        .map(Box::new)
                        .map_err(SendFailure::from),
                )
            }));
        }
    }

    /// Applies a send's answer. Returns whether Discord rejected the login.
    /// Discord's answer is confirmation: the created message goes into the
    /// store through the same path as the Gateway's copy, which makes whichever
    /// arrives second a no-op. An answer for an operation the Gateway already
    /// confirmed is dropped, so a message deleted in between is not resurrected.
    fn finish_send(
        &mut self,
        rest: &RestClient,
        id: OpId,
        result: Result<Box<Message>, SendFailure>,
    ) -> bool {
        self.dirty = true;
        self.uploads.remove(&id);
        let rejected = match result {
            Ok(message) => {
                if self.outbox.finish(id, Ok(())) == Finished::Confirmed {
                    self.apply_created(message);
                }
                false
            }
            Err(failure) => {
                self.outbox.fail(id, failure);
                matches!(
                    failure,
                    SendFailure::Send(error) if error.cause() == RestError::AuthenticationRequired
                ) || matches!(
                    failure,
                    SendFailure::Upload(UploadError::Rest(RestError::AuthenticationRequired))
                )
            }
        };
        self.start_sends(rest);
        rejected
    }

    fn apply_created(&mut self, message: Box<Message>) {
        let change = self.store.apply_dispatch(&Dispatch::MessageCreate(message));
        if change.rejected > 0 {
            self.error = Some(OVERSIZE);
        }
    }

    /// Mirrors the retained rows into the layout index and aims the store's
    /// eviction at what the reader is looking at.
    fn synchronize(&mut self) {
        let Some(channel) = self.channel else {
            return;
        };
        let items: Vec<Item> = self
            .store
            .items(channel)
            .map(|(id, revision)| Item { id, revision })
            .collect();
        self.dirty |= self.list.set_items(&items);
        let live = self
            .store
            .window(channel)
            .is_none_or(|window| window.newer.is_none());
        self.list.set_live(live);
        self.focus();
        self.prune_changes();
        self.prune_inspection();
    }

    /// `None` is the live edge: the newest rows are the ones to keep.
    fn focus(&mut self) {
        let Some(channel) = self.channel else {
            return;
        };
        let focus = if self.list.window().pinned {
            None
        } else {
            self.list.anchor().map(|anchor| anchor.id)
        };
        self.store.set_focus(channel, focus);
    }

    fn start(&mut self, rest: &RestClient, cursor: HistoryCursor) {
        let Some(channel) = self.channel else {
            return;
        };
        if self.pending.is_some() {
            return;
        }
        self.focus();
        let token = self.store.begin_page(channel, cursor);
        let rest = rest.clone();
        self.pending = Some((
            token,
            Box::pin(async move { rest.channel_messages(channel, cursor).await }),
        ));
        self.error = None;
        self.failed = None;
        self.dirty = true;
    }

    fn start_reference(&mut self, rest: &RestClient, target: Snowflake) {
        let Some(channel) = self.channel else {
            return;
        };
        if self.reference_pending.is_some()
            || self.reference_state(target).is_some()
            || self.store.get(channel, target).is_some()
            || self.store.covers(channel, target)
        {
            return;
        }
        let rest = rest.clone();
        self.reference_pending = Some((
            target,
            Box::pin(async move { (target, rest.channel_message(channel, target).await) }),
        ));
        self.dirty = true;
    }

    /// Resolve only replies in the rows the variable list is building. One
    /// speculative request at a time keeps both traffic and pending state small.
    fn resolve_visible_references(&mut self, rest: &RestClient) {
        let Some(channel) = self.channel else {
            return;
        };
        if self.reference_pending.is_some() {
            return;
        }
        let window = self.list.window();
        let target = self
            .store
            .ids(channel)
            .skip(window.range.start)
            .take(window.range.len())
            .find_map(|id| {
                let message = self.store.get(channel, id)?;
                let target = message.replied_to()?;
                match &message.referenced_message {
                    Referenced::Deleted => None,
                    Referenced::Message(preview) if preview.id == target => None,
                    _ if self.store.get(channel, target).is_some()
                        || self.store.covers(channel, target)
                        || self.reference_state(target).is_some() =>
                    {
                        None
                    }
                    _ => Some(target),
                }
            });
        if let Some(target) = target {
            self.start_reference(rest, target);
        }
    }

    /// Brings history in line with the validated selection and applies the
    /// timeline's one-shot inputs. Permission loss or deselection closes it.
    pub fn refresh(
        &mut self,
        rest: &RestClient,
        navigation: &NavigationSnapshot,
        request: &Request,
    ) {
        let guild_selection = navigation.selection.as_ref();
        let private_selection = navigation.private_selection.as_ref();
        let selected = guild_selection
            .filter(|selection| {
                matches!(
                    selection.kind,
                    ChannelKind::Text | ChannelKind::Announcement
                ) && selection.permissions.read_history
            })
            .map(|selection| selection.channel_id)
            .or_else(|| private_selection.map(|selection| selection.channel_id));
        self.sendable = guild_selection
            .filter(|selection| {
                Some(selection.channel_id) == selected && selection.permissions.send_messages
            })
            .map(|selection| selection.channel_id)
            .or_else(|| {
                private_selection
                    .filter(|selection| Some(selection.channel_id) == selected)
                    .map(|selection| selection.channel_id)
            });
        self.reactable = guild_selection
            .filter(|selection| {
                Some(selection.channel_id) == selected && selection.permissions.add_reactions
            })
            .map(|selection| selection.channel_id)
            .or_else(|| {
                private_selection
                    .filter(|selection| Some(selection.channel_id) == selected)
                    .map(|selection| selection.channel_id)
            });
        if selected != self.channel {
            // Revisits fetch a fresh page instead of keeping hidden histories.
            self.reset();
            self.channel = selected;
            if selected.is_some() {
                self.start(rest, HistoryCursor::Latest);
            }
        }
        self.synchronize();
        if let Some((channel, viewport)) = request.viewport
            && Some(channel) == self.channel
        {
            self.dirty |= self.list.viewport(viewport);
            self.focus();
        }
        for (channel, measurement) in &request.measurements {
            if Some(*channel) == self.channel {
                self.dirty |= self.list.measure(*measurement);
            }
        }
        if self
            .highlight
            .is_some_and(|message| !self.list.in_window(message))
        {
            self.highlight = None;
            self.dirty = true;
        }
        if request.revision != self.revision {
            self.revision = request.revision;
            self.intent(rest, request.intent);
        }
        if request.viewport.is_some() || self.check_paging {
            self.page_for_position(rest);
        }
        self.resolve_visible_references(rest);
        self.viewing = request.viewing;
        let acked = guild_selection
            .filter(|selection| Some(selection.channel_id) == selected)
            .and_then(|selection| selection.acked)
            .or_else(|| {
                private_selection
                    .filter(|selection| Some(selection.channel_id) == selected)
                    .and_then(|selection| selection.acked)
            });
        self.auto_ack(rest, acked);
    }

    /// Acknowledges the newest message when the reader is looking at it: the
    /// window is active, the list follows the live edge and sits at the bottom.
    /// Reading older history, or an inactive window, never moves the cursor.
    fn auto_ack(&mut self, rest: &RestClient, acked: Option<Snowflake>) {
        let (Some(channel), Some(acked)) = (self.channel, acked) else {
            return;
        };
        let live = self
            .store
            .window(channel)
            .is_some_and(|window| window.newer.is_none());
        if !self.viewing || !live || !self.list.window().pinned {
            return;
        }
        let Some(newest) = self.store.ids(channel).last() else {
            return;
        };
        if newest <= acked || self.ack_last == Some((channel, newest)) {
            return;
        }
        self.ack_last = Some((channel, newest));
        self.want_ack(rest, channel, newest, false);
    }

    fn want_ack(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        manual: bool,
    ) {
        self.ack_wanted = Some(Ack {
            channel,
            message,
            manual,
        });
        self.start_ack(rest);
    }

    fn start_ack(&mut self, rest: &RestClient) {
        if self.ack_inflight.is_some() {
            return;
        }
        let Some(ack) = self.ack_wanted.take() else {
            return;
        };
        self.ack_inflight = Some(ack);
        let rest = rest.clone();
        self.acks.push(Box::pin(async move {
            let result = rest.ack_message(ack.channel, ack.message, ack.manual).await;
            (ack.channel, ack.message, result)
        }));
    }

    /// The user marked a conversation read. `message` is its newest.
    pub fn mark_read(&mut self, rest: &RestClient, channel: Snowflake, message: Snowflake) {
        self.want_ack(rest, channel, message, true);
    }

    /// Acknowledgements Discord accepted since the last call.
    pub fn take_acked(&mut self) -> Vec<(Snowflake, Snowflake)> {
        std::mem::take(&mut self.acked_done)
    }

    /// Applies an acknowledgement's answer. Returns whether Discord rejected
    /// the login. A failure changes nothing here: the cursor on Discord did
    /// not move, and the markers stay.
    fn finish_ack(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        result: Result<(), RestError>,
    ) -> bool {
        self.ack_inflight = None;
        let rejected = result == Err(RestError::AuthenticationRequired);
        if result.is_ok() {
            self.acked_done.push((channel, message));
        }
        self.start_ack(rest);
        rejected
    }

    fn intent(&mut self, rest: &RestClient, intent: Option<Intent>) {
        let Some(channel) = self.channel else {
            return;
        };
        match intent {
            Some(Intent::Older(target)) if target == channel => {
                if let Some(cursor) = self.store.window(channel).and_then(|w| w.older) {
                    self.start(rest, cursor);
                }
            }
            Some(Intent::Latest(target)) if target == channel => {
                let live = self
                    .store
                    .window(channel)
                    .is_some_and(|window| window.newer.is_none());
                if live {
                    self.list.jump_latest();
                    self.dirty = true;
                } else {
                    self.cancel();
                    self.start(rest, HistoryCursor::Latest);
                }
            }
            Some(Intent::Retry(target)) if target == channel => {
                self.error = None;
                self.dirty = true;
                if let Some(cursor) = self.failed.take() {
                    self.start(rest, cursor);
                }
            }
            Some(Intent::FirstUnread {
                channel: target,
                acked,
            }) if target == channel => {
                self.cancel();
                self.failed = None;
                self.error = None;
                self.notice = None;
                self.highlight = None;
                let first = self.store.ids(channel).find(|id| *id > acked);
                match first {
                    Some(first) if self.store.covers(channel, acked) => {
                        if self.list.jump_to(first) {
                            self.highlight = Some(first);
                        } else {
                            self.notice = Some(UNAVAILABLE_JUMP);
                        }
                        self.dirty = true;
                    }
                    _ => {
                        self.first_unread = Some((channel, acked));
                        self.start(rest, HistoryCursor::Around(acked));
                    }
                }
            }
            Some(Intent::JumpTo {
                channel: target,
                message,
            }) if target == channel => {
                self.cancel();
                self.failed = None;
                self.error = None;
                self.notice = None;
                self.highlight = None;
                if self.store.get(channel, message).is_some() {
                    if self.list.jump_to(message) {
                        self.highlight = Some(message);
                    } else {
                        self.notice = Some(UNAVAILABLE_JUMP);
                    }
                    self.dirty = true;
                } else if matches!(self.reference_state(message), Some(ReferenceState::Deleted)) {
                    self.notice = Some(DELETED_JUMP);
                    self.dirty = true;
                } else if self.store.covers(channel, message) {
                    self.notice = Some(UNAVAILABLE_JUMP);
                    self.dirty = true;
                } else {
                    self.start(rest, HistoryCursor::Around(message));
                }
            }
            _ => {}
        }
    }

    /// Scrolling toward the end of the retained rows asks for the adjacent
    /// page. Opening a server or channel list never reaches here.
    fn page_for_position(&mut self, rest: &RestClient) {
        self.check_paging = false;
        let Some(channel) = self.channel else {
            return;
        };
        if self.pending.is_some() || self.failed.is_some() {
            return;
        }
        let Some(window) = self.store.window(channel) else {
            return;
        };
        let position = self.list.window();
        if position.offset <= position.height
            && let Some(cursor) = window.older
        {
            self.start(rest, cursor);
        } else if position.at_bottom
            && let Some(cursor) = window.newer
        {
            self.start(rest, cursor);
        }
    }

    /// Resolves when an in-flight page, reply preview, send, change, reaction
    /// request, or list of who reacted does.
    pub async fn next_completed(&mut self) -> Completed {
        let page = async {
            match &mut self.pending {
                Some((_, future)) => future.await,
                None => std::future::pending().await,
            }
        };
        let ack = async {
            match self.acks.next().await {
                Some(done) => done,
                None => std::future::pending().await,
            }
        };
        let reference = async {
            match &mut self.reference_pending {
                Some((_, future)) => future.await,
                None => std::future::pending().await,
            }
        };
        let send = async {
            match self.sends.next().await {
                Some((id, result)) => (id, result),
                None => std::future::pending().await,
            }
        };
        let change = async {
            match self.change_requests.next().await {
                Some(done) => done,
                None => std::future::pending().await,
            }
        };
        let reaction = async {
            match self.reaction_requests.next().await {
                Some(done) => done,
                None => std::future::pending().await,
            }
        };
        let users = async {
            match &mut self.users_pending {
                Some(future) => future.await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            page = page => Completed::Page(page),
            (target, result) = reference => Completed::Reference(target, Box::new(result)),
            (id, result) = send => Completed::Send(id, result),
            (channel, message, result) = ack => Completed::Ack(channel, message, result),
            done = change => Completed::Change(done),
            done = reaction => Completed::Reaction(done),
            (serial, result) = users => Completed::ReactionUsers(serial, result),
        }
    }

    /// Applies whatever finished. Returns whether Discord rejected the login.
    pub fn complete(&mut self, rest: &RestClient, completed: Completed) -> bool {
        match completed {
            Completed::Page(result) => self.finish(result),
            Completed::Reference(target, result) => self.finish_reference(target, *result),
            Completed::Send(id, result) => self.finish_send(rest, id, result),
            Completed::Ack(channel, message, result) => {
                self.finish_ack(rest, channel, message, result)
            }
            Completed::Change(done) => self.finish_change(done),
            Completed::Reaction(done) => self.finish_reaction(done),
            Completed::ReactionUsers(serial, result) => self.finish_reaction_users(serial, result),
        }
    }

    /// Applies a finished history page. Returns whether Discord rejected the login.
    pub fn finish(&mut self, result: Result<Vec<Message>, RestError>) -> bool {
        let Some((token, _)) = self.pending.take() else {
            return false;
        };
        let cursor = token.cursor();
        // A page around the read cursor looks for the first unread message,
        // not for the cursor's own message, which may be long gone.
        let first_unread_page = match (cursor, self.first_unread) {
            (HistoryCursor::Around(target), Some((channel, acked))) => {
                (Some(channel) == self.channel && target == acked).then_some(acked)
            }
            _ => None,
        };
        let jump_target = match cursor {
            HistoryCursor::Around(target) if first_unread_page.is_none() => Some(target),
            _ => None,
        };
        self.dirty = true;
        match result {
            Ok(page) => {
                if let Some(target) = jump_target
                    && (matches!(self.reference_state(target), Some(ReferenceState::Deleted))
                        || !page.iter().any(|message| message.id == target))
                {
                    self.store.cancel_page(token);
                    self.remember_reference(target, ReferenceState::Deleted);
                    self.notice = Some(DELETED_JUMP);
                    return false;
                }
                let jump = jump_target
                    .map(|target| (target, page.iter().any(|message| message.id == target)));
                let first_unread = first_unread_page.map(|acked| {
                    self.first_unread = None;
                    page.iter()
                        .map(|message| message.id)
                        .filter(|id| *id > acked)
                        .min()
                });
                match self.store.merge_page(token, page) {
                    PageMerge::Applied { rejected, .. } => {
                        if rejected > 0 {
                            self.error = Some(OVERSIZE);
                        }
                        self.synchronize();
                        if cursor == HistoryCursor::Latest {
                            self.list.jump_latest();
                        }
                        if let Some((target, appeared)) = jump {
                            if self
                                .channel
                                .is_some_and(|channel| self.store.get(channel, target).is_some())
                            {
                                if self.list.jump_to(target) {
                                    self.highlight = Some(target);
                                } else {
                                    self.notice = Some(UNAVAILABLE_JUMP);
                                }
                            } else if matches!(
                                self.reference_state(target),
                                Some(ReferenceState::Deleted)
                            ) || !appeared
                            {
                                self.remember_reference(target, ReferenceState::Deleted);
                                self.notice = Some(DELETED_JUMP);
                            } else {
                                self.remember_reference(target, ReferenceState::Unavailable);
                                self.notice = Some(UNAVAILABLE_JUMP);
                            }
                        }
                        match first_unread {
                            Some(Some(first)) if self.list.jump_to(first) => {
                                self.highlight = Some(first);
                            }
                            Some(_) => self.notice = Some(NO_UNREAD),
                            None => {}
                        }
                        self.check_paging = true;
                        false
                    }
                    PageMerge::Stale => {
                        // Nothing changed; the same request is still available.
                        self.failed = Some(cursor);
                        self.error = Some(
                            "The channel changed while loading. Retry to load a current page.",
                        );
                        false
                    }
                }
            }
            Err(error) => {
                self.store.cancel_page(token);
                self.failed = Some(cursor);
                self.error = Some(match error {
                    RestError::PermissionDenied => {
                        "Discord denied access to this history. Check your permissions here."
                    }
                    RestError::ResourceGone => "This channel is no longer available.",
                    RestError::AuthenticationRequired => "Discord no longer accepts this login.",
                    RestError::ResponseBodyTooLarge | RestError::InvalidJson => {
                        "Discord sent a history page fastcord could not accept."
                    }
                    _ => "Could not load messages. Retry when the connection is back.",
                });
                error == RestError::AuthenticationRequired
            }
        }
    }

    fn finish_reference(
        &mut self,
        target: Snowflake,
        result: Result<Option<Message>, RestError>,
    ) -> bool {
        let Some((pending, _)) = self.reference_pending.take() else {
            return false;
        };
        if pending != target {
            return false;
        }
        if matches!(self.reference_state(target), Some(ReferenceState::Deleted)) {
            return false;
        }
        let channel = self.channel;
        let authentication_required = match result {
            Ok(Some(message)) if Some(message.channel_id) == channel && message.id == target => {
                self.remember_reference(
                    target,
                    ReferenceState::Preview {
                        author: reply_author(message.author.display_name()),
                        content: reply_content(&message.content),
                    },
                );
                false
            }
            Ok(None) => {
                self.remember_reference(target, ReferenceState::Deleted);
                false
            }
            Ok(Some(_)) => {
                self.remember_reference(target, ReferenceState::Unavailable);
                false
            }
            Err(error) => {
                self.remember_reference(target, ReferenceState::Unavailable);
                error == RestError::AuthenticationRequired
            }
        };
        self.dirty = true;
        authentication_required
    }

    fn reply_line(&self, channel: Snowflake, message: &Message) -> Option<ReplyLine> {
        let target = message.replied_to()?;
        let state = if matches!(self.reference_state(target), Some(ReferenceState::Deleted)) {
            ReplyState::Deleted
        } else {
            match &message.referenced_message {
                Referenced::Deleted => ReplyState::Deleted,
                Referenced::Message(preview) if preview.id == target => ReplyState::Message {
                    author: reply_author(preview.author.display_name()),
                    content: reply_content(&preview.content),
                },
                _ => {
                    if let Some(referenced) = self.store.get(channel, target) {
                        ReplyState::Message {
                            author: reply_author(referenced.author.display_name()),
                            content: reply_content(&referenced.content),
                        }
                    } else {
                        match self.reference_state(target) {
                            Some(ReferenceState::Preview { author, content }) => {
                                ReplyState::Message {
                                    author: author.clone(),
                                    content: content.clone(),
                                }
                            }
                            Some(ReferenceState::Deleted) => ReplyState::Deleted,
                            Some(ReferenceState::Unavailable) => ReplyState::Unavailable,
                            None if self.store.covers(channel, target) => ReplyState::Unavailable,
                            None if self
                                .reference_pending
                                .as_ref()
                                .is_some_and(|(pending, _)| *pending == target) =>
                            {
                                ReplyState::Loading
                            }
                            None => ReplyState::Loading,
                        }
                    }
                }
            }
        };
        Some(ReplyLine { target, state })
    }

    /// The bounded read model: only the built window's messages, trimmed to
    /// the byte budget, with spacers recomputed for whatever was trimmed.
    pub fn snapshot(&mut self) -> Snapshot {
        self.dirty = false;
        let Some(channel) = self.channel else {
            return Snapshot::default();
        };
        let mut window = self.list.window();
        let ids: Vec<Snowflake> = self
            .store
            .ids(channel)
            .skip(window.range.start)
            .take(window.range.len())
            .collect();
        let mut messages: Vec<(Arc<Message>, usize)> = ids
            .iter()
            .filter_map(|id| self.store.get(channel, *id))
            .map(|message| {
                let cost = message_bytes(&message);
                (message, cost)
            })
            .collect();
        if messages.len() != ids.len() {
            // The index and the store disagree for an instant (an event not
            // yet synchronized); show nothing rather than misplace rows.
            messages.clear();
            window = self.list.window_for_range(0..0);
        }
        let total: usize = messages.iter().map(|(_, cost)| cost).sum();
        if total > SNAPSHOT_BODY_BUDGET {
            // Keep the visible rows, then what fits after them.
            let first = self
                .list
                .index_at(window.offset)
                .clamp(window.range.start, window.range.end.saturating_sub(1));
            messages.drain(..first - window.range.start);
            let mut used = 0;
            let mut keep = 0;
            for (_, cost) in &messages {
                if keep > 0 && used + cost > SNAPSHOT_BODY_BUDGET {
                    break;
                }
                used += cost;
                keep += 1;
            }
            messages.truncate(keep);
            window = self.list.window_for_range(first..first + keep);
        }
        let rows = messages
            .into_iter()
            .map(|(message, _)| {
                let revision = self.store.revision(channel, message.id).unwrap_or(0);
                let own = self.user.is_some_and(|user| is_own_message(&message, user));
                let change = self.changes.of(channel, message.id);
                let reply = self.reply_line(channel, &message);
                let reactions = self.reactions.of(channel, message.id);
                Row {
                    message,
                    revision,
                    own,
                    change,
                    reply,
                    reactions,
                }
            })
            .collect();
        let deleted: Vec<Snowflake> = self
            .references
            .iter()
            .filter_map(|(id, state)| matches!(state, ReferenceState::Deleted).then_some(*id))
            .collect();
        let bounds = self.store.window(channel);
        Snapshot {
            channel_id: Some(channel),
            rows,
            window,
            scroll: self.list.scroll_request(),
            loading: self.pending.is_some(),
            has_older: bounds.is_some_and(|window| window.older.is_some()),
            has_newer: bounds.is_some_and(|window| window.newer.is_some()),
            error: self.error,
            outbox: self.outbox.items(),
            can_send: self.sendable == Some(channel),
            highlight: self.highlight,
            deleted,
            notice: self.notice,
            can_react: self.reactable == Some(channel),
            names: Default::default(),
            candidates: Vec::new(),
            reaction_users: self.inspection.as_ref().map(|inspection| ReactionUsers {
                message: inspection.message,
                emoji: inspection.emoji.clone(),
                state: inspection.state.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use fastcord_discord::SendError;
    use fastcord_discord::gateway::{ClientProperties, HostOs};
    use fastcord_discord::gateway::{MessageDelete, MessageDeleteBulk};
    use fastcord_discord::message_store::{MAX_MESSAGES_PER_CHANNEL, MESSAGE_BUDGET};
    use fastcord_discord::state::navigation::{ChannelSelection, PermissionSummary};
    use fastcord_discord::{NetworkFailure, RetryableFailure, StatusCode, UserToken};
    use fastcord_model::{Attachment, MessageReference, MessageUpdate, Reaction, User};

    use super::*;
    use crate::changes::{ChangeState, RowChange};
    use crate::outbox::OutboxState;
    use crate::variable_list::{Anchor, ScrollTarget};

    const CHANNEL: Snowflake = Snowflake(777);
    /// Message `n` is the `n`-th message of the fixture channel; IDs grow with time.
    const NEWEST: u64 = 10_000;

    fn rest() -> RestClient {
        let properties = ClientProperties::web(HostOs::Linux, "en-US", 1);
        RestClient::new(
            UserToken::new("offline-fixture-credential".to_owned()),
            properties,
        )
        .unwrap()
    }

    fn message_with(n: u64, content: String) -> Message {
        Message {
            id: Snowflake(n),
            channel_id: CHANNEL,
            guild_id: None,
            author: User {
                id: Snowflake(1 + n % 7),
                username: format!("author{}", n % 7),
                global_name: None,
                avatar: None,
                bot: false,
            },
            content,
            timestamp: "2026-10-07T12:00:00.000000+00:00".to_owned(),
            edited_timestamp: None,
            kind: 0,
            flags: 0,
            pinned: false,
            attachments: Vec::new(),
            reactions: Vec::new(),
            mentions: Vec::new(),
            mention_roles: Vec::new(),
            mention_everyone: false,
            embeds: Vec::new(),
            message_reference: None,
            referenced_message: Referenced::Unknown,
            nonce: None,
        }
    }

    fn message(n: u64) -> Message {
        message_with(n, format!("fixture message {n}"))
    }

    fn reply_message(n: u64, target: Snowflake) -> Message {
        let mut reply = message(n);
        reply.kind = 19;
        reply.message_reference = Some(MessageReference {
            message_id: Some(target),
            channel_id: Some(CHANNEL),
            guild_id: None,
        });
        reply
    }

    /// What Discord answers for a cursor over messages `1..=newest`, newest first.
    fn serve(cursor: HistoryCursor, newest: u64) -> Vec<Message> {
        let (low, high) = match cursor {
            HistoryCursor::Latest => (newest.saturating_sub(49).max(1), newest),
            HistoryCursor::Before(id) => (id.0.saturating_sub(50).max(1), id.0.saturating_sub(1)),
            HistoryCursor::After(id) => (id.0 + 1, (id.0 + 50).min(newest)),
            HistoryCursor::Around(id) => (
                id.0.saturating_sub(24).max(1),
                id.0.saturating_add(24).min(newest),
            ),
        };
        (low..=high).rev().map(message).collect()
    }

    fn navigation(readable: bool) -> NavigationSnapshot {
        NavigationSnapshot {
            selection: Some(ChannelSelection {
                acked: None,
                unread: false,
                mentions: 0,
                guild_id: Snowflake(5),
                channel_id: CHANNEL,
                name: "general".to_owned(),
                kind: ChannelKind::Text,
                permissions: PermissionSummary {
                    view_channel: true,
                    read_history: readable,
                    send_messages: true,
                    ..PermissionSummary::default()
                },
            }),
            ..NavigationSnapshot::default()
        }
    }

    /// The widget reporting that the reader is at `anchor`, `at_bottom` or not.
    fn viewport(anchor: Snowflake, in_row: f32, at_bottom: bool) -> Request {
        Request {
            viewport: Some((
                CHANNEL,
                Viewport {
                    offset: 0.0,
                    height: 600.0,
                    width: 640.0,
                    anchor: Some(Anchor { id: anchor, in_row }),
                    at_bottom,
                    applied: u64::MAX,
                },
            )),
            ..Request::default()
        }
    }

    fn cursor(history: &History) -> Option<HistoryCursor> {
        history.pending.as_ref().map(|(token, _)| token.cursor())
    }

    fn assert_bounded(history: &mut History) -> Snapshot {
        assert!(history.store.messages_held() <= MAX_MESSAGES_PER_CHANNEL);
        assert!(history.store.tracked_bytes() <= MESSAGE_BUDGET);
        let snapshot = history.snapshot();
        let bytes: usize = snapshot
            .rows
            .iter()
            .map(|row| message_bytes(&row.message))
            .sum();
        assert!(bytes <= SNAPSHOT_BODY_BUDGET);
        assert_eq!(snapshot.rows.len(), snapshot.window.range.len());
        assert!(snapshot.rows.len() <= 128);
        snapshot
    }

    fn navigation_acked(acked: u64) -> NavigationSnapshot {
        let mut navigation = navigation(true);
        navigation.selection.as_mut().unwrap().acked = Some(Snowflake(acked));
        navigation
    }

    fn watching() -> Request {
        Request {
            viewing: true,
            ..Request::default()
        }
    }

    #[test]
    fn the_newest_message_is_acknowledged_only_when_read_live_and_once() {
        let rest = rest();
        let mut history = sending_history();
        let navigation = navigation_acked(NEWEST - 5);

        // An inactive window reads nothing.
        history.refresh(&rest, &navigation, &Request::default());
        assert_eq!(history.acks.len(), 0);
        // Looking at the live edge acknowledges the newest message, once.
        history.refresh(&rest, &navigation, &watching());
        assert_eq!(history.acks.len(), 1);
        assert_eq!(
            history.ack_inflight.map(|ack| (ack.message, ack.manual)),
            Some((Snowflake(NEWEST), false))
        );
        history.refresh(&rest, &navigation, &watching());
        assert_eq!(history.acks.len(), 1, "no duplicate while one is in flight");

        // Discord accepted it: the reducer is told, and nothing is repeated.
        assert!(!history.complete(&rest, Completed::Ack(CHANNEL, Snowflake(NEWEST), Ok(()))));
        assert_eq!(history.take_acked(), [(CHANNEL, Snowflake(NEWEST))]);
        history.refresh(&rest, &navigation, &watching());
        assert_eq!(
            history.acks.len(),
            1,
            "the same newest message is not re-sent"
        );
    }

    #[test]
    fn browsing_older_history_or_a_conversation_without_a_cursor_never_acknowledges() {
        let rest = rest();
        let mut history = sending_history();
        // No read cursor: nothing to move.
        history.refresh(&rest, &navigation(true), &watching());
        assert_eq!(history.acks.len(), 0);
        // The reader scrolled up: what arrives below stays unread.
        let navigation = navigation_acked(NEWEST - 5);
        history.refresh(
            &rest,
            &navigation,
            &Request {
                viewing: true,
                ..viewport(Snowflake(NEWEST - 100), 10.0, false)
            },
        );
        history.apply(&Dispatch::MessageCreate(Box::new(message(NEWEST + 1))));
        history.refresh(&rest, &navigation, &watching());
        assert_eq!(history.acks.len(), 0);
    }

    #[test]
    fn a_failed_acknowledgement_changes_nothing_and_a_login_rejection_is_reported() {
        let rest = rest();
        let mut history = sending_history();
        let navigation = navigation_acked(NEWEST - 5);
        history.refresh(&rest, &navigation, &watching());
        assert!(!history.complete(
            &rest,
            Completed::Ack(CHANNEL, Snowflake(NEWEST), Err(RestError::PermissionDenied))
        ));
        assert!(history.take_acked().is_empty());
        // Not retried in a loop, but marking read on purpose goes through.
        history.refresh(&rest, &navigation, &watching());
        assert_eq!(history.acks.len(), 1);
        history.mark_read(&rest, CHANNEL, Snowflake(NEWEST));
        assert_eq!(history.acks.len(), 2);
        assert!(history.complete(
            &rest,
            Completed::Ack(
                CHANNEL,
                Snowflake(NEWEST),
                Err(RestError::AuthenticationRequired)
            )
        ));
    }

    #[test]
    fn first_unread_shows_the_message_after_the_cursor_held_or_fetched() {
        let rest = rest();
        let mut history = sending_history();
        let navigation = navigation_acked(NEWEST - 5);
        // The cursor is inside the retained run: no request is needed.
        history.refresh(
            &rest,
            &navigation,
            &Request {
                revision: 1,
                intent: Some(Intent::FirstUnread {
                    channel: CHANNEL,
                    acked: Snowflake(NEWEST - 5),
                }),
                ..Request::default()
            },
        );
        assert_eq!(cursor(&history), None);
        assert_eq!(history.highlight, Some(Snowflake(NEWEST - 4)));

        // Far behind the retained run: the messages right after it are fetched
        // and the oldest of them is shown.
        history.refresh(
            &rest,
            &navigation,
            &Request {
                revision: 2,
                intent: Some(Intent::FirstUnread {
                    channel: CHANNEL,
                    acked: Snowflake(100),
                }),
                ..Request::default()
            },
        );
        assert_eq!(
            cursor(&history),
            Some(HistoryCursor::Around(Snowflake(100)))
        );
        assert!(!history.finish(Ok(serve(HistoryCursor::Around(Snowflake(100)), NEWEST))));
        assert_eq!(history.highlight, Some(Snowflake(101)));
    }

    fn open_latest(history: &mut History) {
        history.refresh(&rest(), &navigation(true), &Request::default());
        assert_eq!(cursor(history), Some(HistoryCursor::Latest));
        assert!(!history.finish(Ok(serve(HistoryCursor::Latest, NEWEST))));
    }

    #[test]
    fn visible_replies_load_uncached_targets_and_follow_deletions() {
        let rest = rest();
        let mut history = History::default();
        open_latest(&mut history);
        let target = Snowflake(9_000);
        let deleted = Snowflake(8_999);
        let first_reply = Snowflake(NEWEST + 1);
        let second_reply = Snowflake(NEWEST + 2);
        history.apply(&Dispatch::MessageCreate(Box::new(reply_message(
            first_reply.0,
            target,
        ))));
        history.apply(&Dispatch::MessageCreate(Box::new(reply_message(
            second_reply.0,
            deleted,
        ))));

        history.refresh(&rest, &navigation(true), &viewport(second_reply, 0.0, true));
        assert_eq!(
            history.reference_pending.as_ref().map(|(id, _)| *id),
            Some(target)
        );
        assert!(!history.finish_reference(target, Ok(Some(message(target.0)))));

        history.refresh(&rest, &navigation(true), &Request::default());
        assert_eq!(
            history.reference_pending.as_ref().map(|(id, _)| *id),
            Some(deleted)
        );
        assert!(!history.finish_reference(deleted, Ok(None)));
        let snapshot = history.snapshot();
        let row = |id| {
            snapshot
                .rows
                .iter()
                .find(|row| row.message.id == id)
                .unwrap()
        };
        assert!(matches!(
            &row(first_reply).reply.as_ref().unwrap().state,
            ReplyState::Message { author, content }
                if author == "author5" && content == "fixture message 9000"
        ));
        assert_eq!(
            row(second_reply).reply.as_ref().unwrap().state,
            ReplyState::Deleted
        );
        assert!(snapshot.deleted.contains(&deleted));

        history.apply(&Dispatch::MessageDelete(MessageDelete {
            id: target,
            channel_id: CHANNEL,
            guild_id: None,
        }));
        let snapshot = history.snapshot();
        let row = snapshot
            .rows
            .iter()
            .find(|row| row.message.id == first_reply)
            .unwrap();
        assert_eq!(row.reply.as_ref().unwrap().state, ReplyState::Deleted);
        assert!(snapshot.deleted.contains(&target));
    }

    #[test]
    fn reply_jump_loads_around_missing_targets_and_does_not_jump_to_neighbors() {
        let rest = rest();
        let mut history = History::default();
        open_latest(&mut history);
        let cached = Snowflake(NEWEST - 1);
        history.refresh(
            &rest,
            &navigation(true),
            &Request {
                revision: 1,
                intent: Some(Intent::JumpTo {
                    channel: CHANNEL,
                    message: cached,
                }),
                ..Request::default()
            },
        );
        assert_eq!(cursor(&history), None);
        assert_eq!(history.highlight, Some(cached));
        assert!(matches!(
            history.list.scroll_request().unwrap().target,
            ScrollTarget::Anchor(anchor) if anchor.id == cached
        ));

        let uncached = Snowflake(9_000);
        history.refresh(
            &rest,
            &navigation(true),
            &Request {
                revision: 2,
                intent: Some(Intent::JumpTo {
                    channel: CHANNEL,
                    message: uncached,
                }),
                ..Request::default()
            },
        );
        assert_eq!(cursor(&history), Some(HistoryCursor::Around(uncached)));
        assert!(!history.finish(Ok(serve(HistoryCursor::Around(uncached), NEWEST,))));
        assert!(history.store.get(CHANNEL, uncached).is_some());
        assert_eq!(history.snapshot().highlight, Some(uncached));
        let retained: Vec<_> = history.store.ids(CHANNEL).collect();
        let anchor = history.list.anchor();
        let scroll = history.list.scroll_request();
        assert!(
            anchor.is_some(),
            "the successful jump established a reader anchor"
        );

        let deleted = Snowflake(8_000);
        history.refresh(
            &rest,
            &navigation(true),
            &Request {
                revision: 3,
                intent: Some(Intent::JumpTo {
                    channel: CHANNEL,
                    message: deleted,
                }),
                ..Request::default()
            },
        );
        let mut neighbors = serve(HistoryCursor::Around(deleted), NEWEST);
        neighbors.retain(|message| message.id != deleted);
        assert!(!history.finish(Ok(neighbors)));
        let snapshot = history.snapshot();
        assert!(!snapshot.rows.iter().any(|row| row.message.id == deleted));
        assert!(snapshot.deleted.contains(&deleted));
        assert_eq!(snapshot.notice, Some(DELETED_JUMP));
        assert_ne!(snapshot.highlight, Some(deleted));
        assert_eq!(history.store.ids(CHANNEL).collect::<Vec<_>>(), retained);
        assert_eq!(history.list.anchor(), anchor);
        assert_eq!(history.list.scroll_request(), scroll);

        let raced = Snowflake(8_100);
        history.refresh(
            &rest,
            &navigation(true),
            &Request {
                revision: 4,
                intent: Some(Intent::JumpTo {
                    channel: CHANNEL,
                    message: raced,
                }),
                ..Request::default()
            },
        );
        assert_eq!(cursor(&history), Some(HistoryCursor::Around(raced)));
        history.apply(&Dispatch::MessageDelete(MessageDelete {
            id: raced,
            channel_id: CHANNEL,
            guild_id: None,
        }));
        assert!(!history.finish(Ok(serve(HistoryCursor::Around(raced), NEWEST))));
        let snapshot = history.snapshot();
        assert!(snapshot.deleted.contains(&raced));
        assert_eq!(snapshot.notice, Some(DELETED_JUMP));
        assert_eq!(history.store.ids(CHANNEL).collect::<Vec<_>>(), retained);
        assert_eq!(history.list.anchor(), anchor);
        assert_eq!(history.list.scroll_request(), scroll);
    }

    #[test]
    fn ten_thousand_messages_page_through_without_being_retained() {
        let rest = rest();
        let navigation = navigation(true);
        let mut history = History::default();
        open_latest(&mut history);
        let mut seen = std::collections::BTreeSet::new();
        seen.extend(history.store.ids(CHANNEL));
        let mut pages = 1;
        // The reader keeps scrolling to the oldest retained message.
        loop {
            let first = history.store.ids(CHANNEL).next().unwrap();
            history.refresh(&rest, &navigation, &viewport(first, 0.0, false));
            let Some(next) = cursor(&history) else { break };
            assert_eq!(next, HistoryCursor::Before(first), "pages walk backward");
            assert!(!history.finish(Ok(serve(next, NEWEST))));
            seen.extend(history.store.ids(CHANNEL));
            pages += 1;
            assert_bounded(&mut history);
            assert!(pages <= 202, "never more than one request per page");
        }
        // The newest page, 199 full older pages, and one empty page: a full
        // page cannot prove the start of history, an empty one does.
        assert_eq!(pages, 201);
        assert_eq!(history.store.ids(CHANNEL).next(), Some(Snowflake(1)));
        assert!(history.store.window(CHANNEL).unwrap().older.is_none());
        // Every message was shown at some point, yet only a bounded window of
        // them is held now, and the newest end was the part let go.
        assert!(seen.len() >= 9_500, "{} distinct messages", seen.len());
        assert!(history.store.messages_held() <= MAX_MESSAGES_PER_CHANNEL);
        assert!(history.store.window(CHANNEL).unwrap().newer.is_some());
        let snapshot = assert_bounded(&mut history);
        assert!(snapshot.has_newer && !snapshot.has_older);
    }

    #[test]
    fn prepending_a_page_keeps_the_readers_anchor() {
        let rest = rest();
        let navigation = navigation(true);
        let mut history = History::default();
        open_latest(&mut history);
        // Reading message 9_952, 12 px into its row, near the top of the page.
        history.refresh(&rest, &navigation, &viewport(Snowflake(9_952), 12.0, false));
        let before = history.list.anchor().unwrap();
        assert_eq!(before.id, Snowflake(9_952));
        let older = cursor(&history).expect("near the oldest retained row");
        assert_eq!(older, HistoryCursor::Before(Snowflake(9_951)));
        history.finish(Ok(serve(older, NEWEST)));
        let snapshot = history.snapshot();
        let anchor = history.list.anchor().unwrap();
        assert_eq!(anchor.id, before.id);
        assert!((anchor.in_row - before.in_row).abs() < 0.01);
        // Fifty rows were added above it. The widget keeps the reader in place
        // itself in the same frame; the worker asks for no correction that could
        // snap back a reader who has already scrolled on.
        assert_eq!(history.store.channel_len(CHANNEL), 100);
        assert!(snapshot.scroll.is_none());
        assert!(
            snapshot
                .rows
                .iter()
                .any(|row| row.message.id == Snowflake(9_952))
        );
    }

    #[test]
    fn gateway_events_update_and_delete_rows_while_paging_state_is_kept() {
        let mut history = History::default();
        open_latest(&mut history);
        let revision_of =
            |history: &History, n: u64| history.store.revision(CHANNEL, Snowflake(n)).unwrap();
        let before = revision_of(&history, 9_999);
        history.apply(&Dispatch::MessageUpdate(Box::new(MessageUpdate {
            id: Snowflake(9_999),
            channel_id: CHANNEL,
            content: Some("edited".to_owned()),
            edited_timestamp: Some(Some("2026-10-07T12:01:00.000000+00:00".to_owned())),
            flags: None,
            pinned: None,
            attachments: None,
            mentions: None,
            mention_roles: None,
            mention_everyone: None,
            embeds: None,
        })));
        assert!(history.dirty());
        let snapshot = history.snapshot();
        let row = snapshot
            .rows
            .iter()
            .find(|row| row.message.id == Snowflake(9_999))
            .unwrap();
        assert_eq!(row.message.content, "edited");
        assert_ne!(
            row.revision, before,
            "a changed body invalidates its layout"
        );
        history.apply(&Dispatch::MessageDelete(MessageDelete {
            id: Snowflake(9_998),
            channel_id: CHANNEL,
            guild_id: None,
        }));
        history.apply(&Dispatch::MessageDeleteBulk(MessageDeleteBulk {
            ids: vec![Snowflake(9_997), Snowflake(9_996)],
            channel_id: CHANNEL,
            guild_id: None,
        }));
        history.apply(&Dispatch::MessageCreate(Box::new(message(10_001))));
        let snapshot = history.snapshot();
        let shown: Vec<u64> = snapshot.rows.iter().map(|row| row.message.id.0).collect();
        assert!(!shown.contains(&9_998) && !shown.contains(&9_997) && !shown.contains(&9_996));
        assert!(shown.contains(&10_001) && shown.contains(&9_999));
        assert_eq!(history.store.channel_len(CHANNEL), 50 - 3 + 1);
    }

    #[test]
    fn only_a_readable_selected_channel_is_fetched_and_leaving_it_releases_everything() {
        let rest = rest();
        let mut history = History::default();
        history.refresh(&rest, &navigation(false), &Request::default());
        assert!(cursor(&history).is_none(), "no read-history permission");
        assert_eq!(history.snapshot(), Snapshot::default());
        history.refresh(&rest, &NavigationSnapshot::default(), &Request::default());
        assert!(cursor(&history).is_none(), "nothing selected");

        open_latest(&mut history);
        assert_eq!(history.store.channel_len(CHANNEL), 50);
        // Permission loss closes the channel and drops its bodies at once.
        history.refresh(&rest, &navigation(false), &Request::default());
        assert_eq!(history.store.messages_held(), 0);
        assert!(history.snapshot().rows.is_empty());
        // A page still in flight for a closed channel is not applied.
        history.refresh(&rest, &navigation(true), &Request::default());
        assert_eq!(cursor(&history), Some(HistoryCursor::Latest));
        history.refresh(&rest, &NavigationSnapshot::default(), &Request::default());
        assert!(history.pending.is_none());
        assert!(!history.finish(Ok(serve(HistoryCursor::Latest, NEWEST))));
        assert_eq!(history.store.messages_held(), 0);
    }

    #[test]
    fn failures_are_visible_retried_only_on_request_and_auth_loss_is_reported() {
        let rest = rest();
        let navigation = navigation(true);
        let mut history = History::default();
        history.refresh(&rest, &navigation, &Request::default());
        assert!(!history.finish(Err(RestError::PermissionDenied)));
        let snapshot = history.snapshot();
        assert!(snapshot.rows.is_empty() && !snapshot.loading);
        assert!(snapshot.error.is_some_and(|text| text.contains("denied")));
        // Scrolling or measuring never repeats a failed page by itself.
        history.refresh(&rest, &navigation, &viewport(Snowflake(1), 0.0, true));
        assert!(cursor(&history).is_none());
        // Retry repeats exactly the failed request.
        let retry = Request {
            revision: 1,
            intent: Some(Intent::Retry(CHANNEL)),
            ..Request::default()
        };
        history.refresh(&rest, &navigation, &retry);
        assert_eq!(cursor(&history), Some(HistoryCursor::Latest));
        assert!(history.snapshot().error.is_none());
        assert!(history.finish(Err(RestError::AuthenticationRequired)));
    }

    #[test]
    fn jump_to_latest_reloads_the_newest_page_when_it_was_not_retained() {
        let rest = rest();
        let navigation = navigation(true);
        let mut history = History::default();
        open_latest(&mut history);
        // Page back until the newest rows are no longer retained.
        for _ in 0..12 {
            let first = history.store.ids(CHANNEL).next().unwrap();
            history.refresh(&rest, &navigation, &viewport(first, 0.0, false));
            let next = cursor(&history).unwrap();
            history.finish(Ok(serve(next, NEWEST)));
        }
        assert!(history.snapshot().has_newer);
        let jump = Request {
            revision: 1,
            intent: Some(Intent::Latest(CHANNEL)),
            ..Request::default()
        };
        history.refresh(&rest, &navigation, &jump);
        assert_eq!(cursor(&history), Some(HistoryCursor::Latest));
        history.finish(Ok(serve(HistoryCursor::Latest, NEWEST)));
        let snapshot = assert_bounded(&mut history);
        assert!(!snapshot.has_newer);
        assert_eq!(history.store.ids(CHANNEL).last(), Some(Snowflake(NEWEST)));
        assert!(matches!(
            snapshot.scroll.map(|request| request.target),
            Some(ScrollTarget::Bottom)
        ));
    }

    #[test]
    fn oversized_messages_are_reported_and_a_snapshot_stays_within_its_budget() {
        let rest = rest();
        let navigation = navigation(true);
        let mut history = History::default();
        history.refresh(&rest, &navigation, &Request::default());
        let big = "x".repeat(200 * 1024);
        let page: Vec<Message> = (1..=50)
            .rev()
            .map(|n| match n {
                // Larger than the largest body the store accepts.
                7 => message_with(n, "y".repeat(MAX_MESSAGE_BYTES)),
                _ => message_with(n, big.clone()),
            })
            .collect();
        history.finish(Ok(page));
        let snapshot = assert_bounded(&mut history);
        assert!(
            snapshot
                .error
                .is_some_and(|text| text.contains("too large"))
        );
        assert!(history.store.get(CHANNEL, Snowflake(7)).is_none());
        assert!(!snapshot.rows.is_empty());
        assert!(snapshot.rows.len() < 49, "bodies bound the rows built");
    }

    #[test]
    fn measurements_coalesce_per_row_and_stay_capped() {
        use crate::variable_list::Measurement;
        let mut request = Request::default();
        let measure = |id, bucket, height| Measurement {
            id: Snowflake(id),
            revision: 1,
            width_bucket: bucket,
            height,
        };
        for height in 0..1_000 {
            request.measure(CHANNEL, measure(1, 3, height as f32));
        }
        assert_eq!(request.measurements.len(), 1);
        assert_eq!(request.measurements[0].1.height, 999.0);
        request.measure(CHANNEL, measure(1, 4, 5.0));
        assert_eq!(request.measurements.len(), 2, "another width bucket");
        for id in 0..1_000 {
            request.measure(CHANNEL, measure(id, 3, 10.0));
        }
        assert!(request.measurements.len() <= MAX_MEASUREMENTS);
    }

    const SELF: Snowflake = Snowflake(42);

    /// The account's own message `n`, as Discord echoes it.
    fn own(n: u64, content: &str, nonce: Option<String>) -> Box<Message> {
        let mut message = message_with(n, content.to_owned());
        message.author.id = SELF;
        message.nonce = nonce;
        Box::new(message)
    }

    /// An open, sendable channel at its live edge, as the account `SELF`.
    fn sending_history() -> History {
        let mut history = History::new(Slots::default());
        history.user = Some(SELF);
        open_latest(&mut history);
        history
    }

    fn wire_nonce(history: &History, id: OpId) -> String {
        history.outbox.nonce_of(id).unwrap().to_string()
    }

    fn copies(history: &History, id: u64) -> usize {
        history
            .store
            .ids(CHANNEL)
            .filter(|held| *held == Snowflake(id))
            .count()
    }

    /// Drops the request futures as if their posts had been made; tests never
    /// poll them, so nothing reaches the network.
    fn take_posts(history: &mut History) -> usize {
        std::mem::take(&mut history.sends).len()
    }

    #[test]
    fn either_arrival_order_of_rest_and_gateway_shows_the_message_once() {
        let rest = rest();
        let mut history = sending_history();
        let retained = history.store.ids(CHANNEL).count();

        // REST answer first, then the Gateway's copy.
        let first = history.send(&rest, CHANNEL, "first", false, None);
        assert_eq!(take_posts(&mut history), 1);
        assert_eq!(history.snapshot().outbox.len(), 1, "shown as sending");
        let nonce = wire_nonce(&history, first);
        let created = own(NEWEST + 1, "first", Some(nonce));
        assert!(!history.complete(&rest, Completed::Send(first, Ok(created.clone()))));
        assert!(history.snapshot().outbox.is_empty());
        assert_eq!(copies(&history, NEWEST + 1), 1);
        history.apply(&Dispatch::MessageCreate(created));
        assert_eq!(
            copies(&history, NEWEST + 1),
            1,
            "the Gateway copy is not a second row"
        );

        // Gateway copy first, then the REST answer.
        let second = history.send(&rest, CHANNEL, "second", false, None);
        assert_eq!(take_posts(&mut history), 1);
        let nonce = wire_nonce(&history, second);
        let created = own(NEWEST + 2, "second", Some(nonce));
        history.apply(&Dispatch::MessageCreate(created.clone()));
        assert!(
            history.snapshot().outbox.is_empty(),
            "confirmed by the Gateway"
        );
        assert!(!history.complete(&rest, Completed::Send(second, Ok(created))));
        assert_eq!(copies(&history, NEWEST + 2), 1);
        assert_eq!(history.store.ids(CHANNEL).count(), retained + 2);
        assert_eq!(take_posts(&mut history), 0, "nothing was posted again");
    }

    #[test]
    fn an_ambiguous_failure_waits_for_the_user_and_resolves_from_the_gateway() {
        let rest = rest();
        let mut history = sending_history();
        let id = history.send(&rest, CHANNEL, "maybe", false, None);
        assert_eq!(take_posts(&mut history), 1);
        let timeout = SendError::Ambiguous(RestError::Retryable(RetryableFailure::Network(
            NetworkFailure::Timeout,
        )));
        assert!(!history.complete(&rest, Completed::Send(id, Err(timeout.into()))));
        let shown = history.snapshot().outbox;
        assert!(matches!(shown[0].state, OutboxState::Uncertain(_)));

        // Nothing the worker does by itself posts it again.
        history.refresh(&rest, &navigation(true), &Request::default());
        history.apply(&Dispatch::MessageCreate(Box::new(message(NEWEST + 1))));
        let other = history.send(&rest, CHANNEL, "unrelated", false, None);
        let other_nonce = wire_nonce(&history, other);
        history.apply(&Dispatch::MessageCreate(own(
            NEWEST + 2,
            "unrelated",
            Some(other_nonce),
        )));
        assert_eq!(
            take_posts(&mut history),
            1,
            "only the new message was posted"
        );
        assert!(matches!(
            history.snapshot().outbox[0].state,
            OutboxState::Uncertain(_)
        ));

        // Someone else's message with the same nonce text is not ours.
        let nonce = wire_nonce(&history, id);
        let mut foreign = own(NEWEST + 3, "maybe", Some(nonce.clone()));
        foreign.author.id = Snowflake(7);
        history.apply(&Dispatch::MessageCreate(foreign));
        assert_eq!(history.snapshot().outbox.len(), 1);

        // Discord did create it: the Gateway copy resolves it without a post.
        history.apply(&Dispatch::MessageCreate(own(
            NEWEST + 4,
            "maybe",
            Some(nonce),
        )));
        assert!(history.snapshot().outbox.is_empty());
        assert_eq!(copies(&history, NEWEST + 4), 1);
        assert_eq!(take_posts(&mut history), 0);
    }

    #[test]
    fn an_explicit_retry_posts_once_more_with_the_same_nonce_while_permitted() {
        let rest = rest();
        let mut history = sending_history();
        let id = history.send(&rest, CHANNEL, "again", true, None);
        take_posts(&mut history);
        let nonce = wire_nonce(&history, id);
        let refused = SendError::NotSent(RestError::Http(StatusCode::BAD_REQUEST));
        history.complete(&rest, Completed::Send(id, Err(refused.into())));
        assert!(matches!(
            history.snapshot().outbox[0].state,
            OutboxState::Failed(_)
        ));

        // Without send permission the retry does nothing.
        let mut read_only = navigation(true);
        read_only
            .selection
            .as_mut()
            .unwrap()
            .permissions
            .send_messages = false;
        history.refresh(&rest, &read_only, &Request::default());
        assert!(!history.snapshot().can_send);
        history.retry_send(&rest, id);
        assert_eq!(take_posts(&mut history), 0);

        history.refresh(&rest, &navigation(true), &Request::default());
        history.retry_send(&rest, id);
        assert_eq!(take_posts(&mut history), 1);
        assert_eq!(
            wire_nonce(&history, id),
            nonce,
            "Discord dedupes by this nonce"
        );
        // A second retry while it is in flight is ignored.
        history.retry_send(&rest, id);
        assert_eq!(take_posts(&mut history), 0);
        // The answer to the retry confirms it.
        let created = own(NEWEST + 1, "again", Some(nonce));
        history.complete(&rest, Completed::Send(id, Ok(created)));
        assert!(history.snapshot().outbox.is_empty());
        assert_eq!(copies(&history, NEWEST + 1), 1);
    }

    #[test]
    fn sends_to_a_channel_that_cannot_be_sent_to_are_kept_but_never_posted() {
        let rest = rest();
        let mut history = sending_history();
        let id = history.send(&rest, Snowflake(999), "elsewhere", false, None);
        assert_eq!(take_posts(&mut history), 0);
        assert!(history.outbox.nonce_of(id).is_some(), "the text is kept");
        // A login rejected while sending is reported to the worker.
        let id = history.send(&rest, CHANNEL, "x", false, None);
        take_posts(&mut history);
        assert!(history.complete(
            &rest,
            Completed::Send(
                id,
                Err(SendError::NotSent(RestError::AuthenticationRequired).into())
            )
        ));
    }

    /// Drops the change request futures as if they had been sent; tests never
    /// poll them, so nothing reaches the network.
    fn take_changes(history: &mut History) -> usize {
        std::mem::take(&mut history.change_requests).len()
    }

    fn attachment() -> Attachment {
        Attachment {
            id: Snowflake(77),
            filename: "cat.png".to_owned(),
            size: 1_024,
            url: "https://cdn.example/attachments/cat.png".to_owned(),
            proxy_url: None,
            content_type: Some("image/png".to_owned()),
            width: Some(64),
            height: Some(64),
        }
    }

    /// A sending history that also holds the account's own message
    /// `NEWEST + 1`, which has an attachment.
    fn with_own_message() -> (History, Snowflake) {
        let mut history = sending_history();
        let mut message = own(NEWEST + 1, "tpyo", None);
        message.attachments.push(attachment());
        history.apply(&Dispatch::MessageCreate(message));
        (history, Snowflake(NEWEST + 1))
    }

    fn held(history: &History, id: Snowflake) -> Arc<Message> {
        history.store.get(CHANNEL, id).unwrap()
    }

    /// Discord's full answer to an edit: the held message with new text and time.
    fn edited(history: &History, id: Snowflake, content: &str, at: &str) -> Box<Message> {
        let mut message = (*held(history, id)).clone();
        message.content = content.to_owned();
        message.edited_timestamp = Some(at.to_owned());
        Box::new(message)
    }

    /// The Gateway's MESSAGE_UPDATE for an edit: text and time only.
    fn gateway_edit(id: Snowflake, content: &str, at: &str) -> Dispatch {
        Dispatch::MessageUpdate(Box::new(MessageUpdate {
            id,
            channel_id: CHANNEL,
            content: Some(content.to_owned()),
            edited_timestamp: Some(Some(at.to_owned())),
            flags: None,
            pinned: None,
            attachments: None,
            mentions: None,
            mention_roles: None,
            mention_everyone: None,
            embeds: None,
        }))
    }

    /// The row as the timeline would show it now (the worker synchronizes the
    /// layout index on every refresh before it publishes).
    fn row_of(history: &mut History, id: Snowflake) -> Row {
        history.refresh(&rest(), &navigation(true), &Request::default());
        history
            .snapshot()
            .rows
            .into_iter()
            .find(|row| row.message.id == id)
            .unwrap()
    }

    fn answered(id: Snowflake, result: ChangeResult) -> Completed {
        Completed::Change(ChangeDone {
            channel: CHANNEL,
            message: id,
            result,
        })
    }

    const T1: &str = "2026-10-08T09:01:00.000000+00:00";
    const T2: &str = "2026-10-08T09:02:00.000000+00:00";
    const T3: &str = "2026-10-08T09:03:00.000000+00:00";
    const T4: &str = "2026-10-08T09:04:00.000000+00:00";

    #[test]
    fn an_edit_changes_only_the_text_and_either_arrival_order_shows_it_once() {
        let rest = rest();
        let (mut history, id) = with_own_message();
        let row = row_of(&mut history, id);
        assert!(row.own && row.change.is_none());

        // REST answer first, then the Gateway's echo.
        history.edit_message(&rest, CHANNEL, id, "  typo fixed \n");
        assert_eq!(take_changes(&mut history), 1);
        let row = row_of(&mut history, id);
        assert_eq!(
            row.change,
            Some(RowChange {
                kind: ChangeKind::Edit(Arc::from("typo fixed")),
                state: ChangeState::Saving,
            }),
            "shown as being saved, trimmed"
        );
        assert_eq!(
            row.message.content, "tpyo",
            "the store keeps Discord's copy"
        );
        let answer = edited(&history, id, "typo fixed", T1);
        assert!(!history.complete(&rest, answered(id, ChangeResult::Edited(Ok(answer)))));
        let row = row_of(&mut history, id);
        assert!(row.change.is_none());
        assert_eq!(row.message.content, "typo fixed");
        assert_eq!(row.message.attachments, vec![attachment()]);
        let revision = row.revision;
        history.apply(&gateway_edit(id, "typo fixed", T1));
        assert_eq!(
            history.store.revision(CHANNEL, id),
            Some(revision),
            "the echo changes nothing"
        );

        // The Gateway first (it omits attachments), then the REST answer.
        history.edit_message(&rest, CHANNEL, id, "second");
        assert_eq!(take_changes(&mut history), 1);
        history.apply(&gateway_edit(id, "second", T2));
        assert!(
            row_of(&mut history, id).change.is_some_and(|c| c.saving()),
            "the change waits for its own answer"
        );
        assert_eq!(held(&history, id).attachments, vec![attachment()]);
        let answer = edited(&history, id, "second", T2);
        history.complete(&rest, answered(id, ChangeResult::Edited(Ok(answer))));
        let row = row_of(&mut history, id);
        assert!(row.change.is_none());
        assert_eq!(row.message.content, "second");
        assert_eq!(row.message.attachments, vec![attachment()]);

        // Another client edits again before this answer arrives: the older
        // answer does not overwrite the newer text.
        history.edit_message(&rest, CHANNEL, id, "third");
        assert_eq!(take_changes(&mut history), 1);
        let stale = edited(&history, id, "third", T3);
        history.apply(&gateway_edit(id, "fourth", T4));
        history.complete(&rest, answered(id, ChangeResult::Edited(Ok(stale))));
        assert_eq!(held(&history, id).content, "fourth");
        assert_eq!(held(&history, id).attachments, vec![attachment()]);
        assert_eq!(
            history
                .store
                .ids(CHANNEL)
                .filter(|held| *held == id)
                .count(),
            1
        );
        assert_eq!(take_changes(&mut history), 0, "nothing was sent again");
    }

    #[test]
    fn only_the_users_own_loaded_messages_of_the_open_channel_are_changed() {
        let rest = rest();
        let (mut history, id) = with_own_message();
        // Someone else's message (author 1 + n % 7, never SELF).
        let theirs = Snowflake(NEWEST);
        assert!(!row_of(&mut history, theirs).own);
        history.edit_message(&rest, CHANNEL, theirs, "hijack");
        assert_eq!(history.snapshot().notice, Some(NOT_OWN));
        history.delete_message(&rest, CHANNEL, theirs);
        assert_eq!(history.snapshot().notice, Some(NOT_OWN));
        // A system message naming the user as its author.
        let mut joined = own(NEWEST + 2, "", None);
        joined.kind = 7;
        history.apply(&Dispatch::MessageCreate(joined));
        assert!(!row_of(&mut history, Snowflake(NEWEST + 2)).own);
        history.delete_message(&rest, CHANNEL, Snowflake(NEWEST + 2));
        assert_eq!(history.snapshot().notice, Some(NOT_OWN));
        // Not loaded, or not in the open channel.
        history.delete_message(&rest, CHANNEL, Snowflake(5));
        assert_eq!(history.snapshot().notice, Some(NOT_LOADED));
        history.edit_message(&rest, Snowflake(778), id, "elsewhere");
        assert_eq!(history.snapshot().notice, Some(NOT_OPEN));
        assert_eq!(take_changes(&mut history), 0);
        // Before READY nothing is anyone's own.
        let mut anonymous = History::default();
        open_latest(&mut anonymous);
        anonymous.apply(&Dispatch::MessageCreate(own(NEWEST + 1, "x", None)));
        anonymous.delete_message(&rest, CHANNEL, Snowflake(NEWEST + 1));
        assert_eq!(anonymous.snapshot().notice, Some(NOT_OWN));
        assert_eq!(take_changes(&mut anonymous), 0);
        // The user's own message is accepted, and a valid command clears the notice.
        history.delete_message(&rest, CHANNEL, id);
        assert_eq!(take_changes(&mut history), 1);
        assert_eq!(history.snapshot().notice, None);
    }

    #[test]
    fn edits_are_validated_and_unchanged_text_needs_no_request() {
        let rest = rest();
        let (mut history, id) = with_own_message();
        history.apply(&Dispatch::MessageCreate(own(NEWEST + 2, "plain", None)));
        let plain = Snowflake(NEWEST + 2);
        history.edit_message(&rest, CHANNEL, plain, " \n ");
        assert_eq!(history.snapshot().notice, Some(EMPTY_EDIT));
        history.edit_message(&rest, CHANNEL, plain, &"x".repeat(4_001));
        assert_eq!(history.snapshot().notice, Some(LONG_EDIT));
        history.edit_message(&rest, CHANNEL, plain, "  plain ");
        assert_eq!(history.snapshot().notice, None);
        assert_eq!(take_changes(&mut history), 0, "nothing changed");
        // With an attachment the text may go; the attachment stays the message.
        history.edit_message(&rest, CHANNEL, id, "");
        assert_eq!(take_changes(&mut history), 1);
        // One change per message at a time.
        history.delete_message(&rest, CHANNEL, id);
        assert_eq!(history.snapshot().notice, Some(changes::BUSY));
        assert_eq!(take_changes(&mut history), 0);
    }

    #[test]
    fn a_deletion_removes_the_row_once_and_a_404_counts_as_deleted() {
        let rest = rest();
        let (mut history, id) = with_own_message();
        let reply_id = Snowflake(NEWEST + 5);
        history.apply(&Dispatch::MessageCreate(Box::new(reply_message(
            reply_id.0, id,
        ))));
        history.refresh(&rest, &navigation(true), &Request::default());
        let before_delete = history.snapshot();
        let mut composer = crate::composer::Composer::default();
        composer.select(Some(CHANNEL));
        assert!(composer.begin_reply(&before_delete, id));
        composer.perform(iced::widget::text_editor::Action::Edit(
            iced::widget::text_editor::Edit::Paste(std::sync::Arc::new("answer".to_owned())),
        ));
        history.delete_message(&rest, CHANNEL, id);
        assert_eq!(take_changes(&mut history), 1);
        assert_eq!(
            row_of(&mut history, id).change.map(|change| change.kind),
            Some(ChangeKind::Delete)
        );
        history.complete(&rest, answered(id, ChangeResult::Deleted(Ok(()))));
        assert!(history.store.get(CHANNEL, id).is_none());
        let after_delete = history.snapshot();
        assert!(after_delete.deleted.contains(&id));
        let reply_row = after_delete
            .rows
            .iter()
            .find(|row| row.message.id == reply_id)
            .unwrap();
        assert_eq!(
            reply_row.reply.as_ref().unwrap().state,
            ReplyState::Deleted,
            "REST confirmation invalidates embedded/cached reply previews immediately"
        );
        let bridge = crate::gateway::NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        crate::composer::update(
            &mut composer,
            crate::composer::Event::Send,
            &after_delete,
            &bridge,
        );
        assert!(
            commands.try_recv().is_err(),
            "the REST-known deletion disables reply send"
        );
        assert_eq!(composer.replying(), Some(id));
        assert_eq!(
            composer.submission(true, 0).unwrap().content.trim(),
            "answer"
        );
        history.apply(&Dispatch::MessageDelete(MessageDelete {
            id,
            channel_id: CHANNEL,
            guild_id: None,
        }));
        assert_eq!(history.changes.len(), 0);

        // The Gateway first: the change ends, the late answer changes nothing.
        history.apply(&Dispatch::MessageCreate(own(NEWEST + 2, "two", None)));
        let two = Snowflake(NEWEST + 2);
        history.delete_message(&rest, CHANNEL, two);
        history.apply(&Dispatch::MessageDelete(MessageDelete {
            id: two,
            channel_id: CHANNEL,
            guild_id: None,
        }));
        assert_eq!(history.changes.len(), 0);
        history.complete(&rest, answered(two, ChangeResult::Deleted(Ok(()))));
        assert!(history.store.get(CHANNEL, two).is_none());

        // Already gone (404): deleted, not a failure.
        history.apply(&Dispatch::MessageCreate(own(NEWEST + 3, "three", None)));
        let three = Snowflake(NEWEST + 3);
        history.delete_message(&rest, CHANNEL, three);
        history.complete(
            &rest,
            answered(three, ChangeResult::Deleted(Err(RestError::ResourceGone))),
        );
        assert!(history.store.get(CHANNEL, three).is_none());
        assert_eq!(history.changes.len(), 0);
        assert_eq!(history.snapshot().notice, None);

        // An edit answered 404: the message is gone, and the user is told.
        history.apply(&Dispatch::MessageCreate(own(NEWEST + 4, "four", None)));
        let four = Snowflake(NEWEST + 4);
        history.edit_message(&rest, CHANNEL, four, "four!");
        history.complete(
            &rest,
            answered(four, ChangeResult::Edited(Err(RestError::ResourceGone))),
        );
        assert!(history.store.get(CHANNEL, four).is_none());
        assert_eq!(history.snapshot().notice, Some(DELETED_WHILE_EDITING));
        assert_eq!(take_changes(&mut history), 3, "one request per action");
    }

    #[test]
    fn failed_changes_wait_for_the_user_and_resolve_from_the_gateway() {
        let rest = rest();
        let (mut history, id) = with_own_message();
        let timeout = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout));
        history.edit_message(&rest, CHANNEL, id, "maybe saved");
        take_changes(&mut history);
        assert!(!history.complete(&rest, answered(id, ChangeResult::Edited(Err(timeout)))));
        let change = row_of(&mut history, id).change.unwrap();
        assert!(
            matches!(change.state, ChangeState::Failed(reason) if reason.contains("may have been saved"))
        );
        assert_eq!(change.failed_edit(), Some("maybe saved"));
        // Nothing the worker does by itself asks again.
        history.refresh(&rest, &navigation(true), &Request::default());
        history.apply(&Dispatch::MessageCreate(Box::new(message(NEWEST + 5))));
        history.apply(&gateway_edit(id, "another client's text", T1));
        assert_eq!(take_changes(&mut history), 0);
        assert!(row_of(&mut history, id).change.is_some());
        // It was saved after all: the Gateway reports exactly that text.
        history.apply(&gateway_edit(id, "maybe saved", T2));
        assert!(row_of(&mut history, id).change.is_none());
        assert_eq!(held(&history, id).content, "maybe saved");

        // A refused deletion is retried only on request, once, then dismissed.
        history.delete_message(&rest, CHANNEL, id);
        take_changes(&mut history);
        history.complete(
            &rest,
            answered(id, ChangeResult::Deleted(Err(RestError::PermissionDenied))),
        );
        assert!(matches!(
            row_of(&mut history, id).change.unwrap().state,
            ChangeState::Failed(reason) if reason.contains("not allow")
        ));
        history.retry_change(&rest, CHANNEL, id);
        assert_eq!(take_changes(&mut history), 1);
        history.retry_change(&rest, CHANNEL, id);
        assert_eq!(take_changes(&mut history), 0, "already being saved");
        history.complete(
            &rest,
            answered(id, ChangeResult::Deleted(Err(RestError::PermissionDenied))),
        );
        history.dismiss_change(CHANNEL, id);
        assert!(row_of(&mut history, id).change.is_none());
        assert_eq!(held(&history, id).content, "maybe saved", "still there");

        // A login rejected while saving is reported to the worker.
        history.edit_message(&rest, CHANNEL, id, "x");
        assert_eq!(take_changes(&mut history), 1);
        assert!(history.complete(
            &rest,
            answered(
                id,
                ChangeResult::Edited(Err(RestError::AuthenticationRequired))
            )
        ));

        // A failure for a message that left with its channel keeps nothing.
        history.retry_change(&rest, CHANNEL, id);
        assert_eq!(take_changes(&mut history), 1);
        history.refresh(&rest, &NavigationSnapshot::default(), &Request::default());
        history.complete(&rest, answered(id, ChangeResult::Edited(Err(timeout))));
        assert_eq!(history.changes.len(), 0);
    }

    #[test]
    fn unfinished_changes_are_bounded() {
        use crate::changes::MAX_CHANGES;
        let rest = rest();
        let mut history = sending_history();
        for n in 1..=MAX_CHANGES as u64 + 1 {
            history.apply(&Dispatch::MessageCreate(own(NEWEST + n, "mine", None)));
        }
        for n in 1..=MAX_CHANGES as u64 {
            history.delete_message(&rest, CHANNEL, Snowflake(NEWEST + n));
        }
        assert_eq!(take_changes(&mut history), MAX_CHANGES);
        let last = Snowflake(NEWEST + MAX_CHANGES as u64 + 1);
        history.edit_message(&rest, CHANNEL, last, "one too many");
        assert_eq!(history.snapshot().notice, Some(changes::FULL));
        assert_eq!(take_changes(&mut history), 0);
        assert!(row_of(&mut history, last).change.is_none());
    }
    #[test]
    fn selected_private_conversation_uses_the_existing_history_and_send_path() {
        let rest = rest();
        let mut history = History::new(Slots::default());
        let channel = Snowflake(300_000_000_000_000_001);
        let navigation = NavigationSnapshot {
            private_selection: Some(
                fastcord_discord::state::navigation::PrivateChannelSelection {
                    channel_id: channel,
                    name: "Nelly".to_owned(),
                    acked: None,
                    unread: false,
                    mentions: 0,
                },
            ),
            ..NavigationSnapshot::default()
        };
        history.refresh(&rest, &navigation, &Request::default());
        assert_eq!(history.snapshot().channel_id, Some(channel));
        assert!(history.snapshot().can_send);

        history.refresh(&rest, &NavigationSnapshot::default(), &Request::default());
        assert_eq!(history.snapshot().channel_id, None);
        assert!(!history.snapshot().can_send);
    }

    fn emoji(name: &str) -> Emoji {
        Emoji {
            id: None,
            name: Some(name.to_owned()),
            animated: false,
        }
    }

    fn custom_emoji(id: u64, name: &str) -> Emoji {
        Emoji {
            id: Some(Snowflake(id)),
            name: Some(name.to_owned()),
            animated: false,
        }
    }

    fn tally(emoji: Emoji, count: u32, me: bool) -> Reaction {
        Reaction { count, me, emoji }
    }

    const THUMBS: &str = "\u{1f44d}";
    const PARTY: &str = "\u{1f389}";

    fn navigation_reacting(add_reactions: bool) -> NavigationSnapshot {
        let mut navigation = navigation(true);
        navigation
            .selection
            .as_mut()
            .unwrap()
            .permissions
            .add_reactions = add_reactions;
        navigation
    }

    /// An open channel where the account `SELF` may react, holding somebody
    /// else's message that has 👍 from two others and `wave` from the user.
    fn reacting_history() -> (History, Snowflake) {
        let mut history = sending_history();
        history.store.set_user(SELF);
        let mut message = message_with(NEWEST + 1, "react to this".to_owned());
        message.reactions = vec![
            tally(emoji(THUMBS), 2, false),
            tally(custom_emoji(9, "wave"), 1, true),
        ];
        history.apply(&Dispatch::MessageCreate(Box::new(message)));
        history.refresh(&rest(), &navigation_reacting(true), &Request::default());
        (history, Snowflake(NEWEST + 1))
    }

    fn reaction_row(history: &mut History, id: Snowflake) -> Row {
        history.refresh(&rest(), &navigation_reacting(true), &Request::default());
        history
            .snapshot()
            .rows
            .into_iter()
            .find(|row| row.message.id == id)
            .unwrap()
    }

    fn chip_of(row: &Row, name: &str) -> Option<(u32, bool, bool)> {
        crate::timeline::chips(row)
            .find(|chip| chip.emoji.name.as_deref() == Some(name))
            .map(|chip| (chip.count, chip.me, chip.pending))
    }

    fn reacted(history: &History, id: Snowflake) -> Vec<(String, u32, bool)> {
        held(history, id)
            .reactions
            .iter()
            .map(|reaction| {
                (
                    reaction.emoji.name.clone().unwrap_or_default(),
                    reaction.count,
                    reaction.me,
                )
            })
            .collect()
    }

    fn reaction_answered(
        id: Snowflake,
        emoji: Emoji,
        action: Action,
        result: Result<(), RestError>,
    ) -> Completed {
        Completed::Reaction(ReactionDone {
            channel: CHANNEL,
            message: id,
            emoji,
            action,
            result,
        })
    }

    fn take_reactions(history: &mut History) -> usize {
        std::mem::take(&mut history.reaction_requests).len()
    }

    fn gateway_reaction(
        id: Snowflake,
        user: Snowflake,
        emoji: Emoji,
        added: bool,
        burst: bool,
    ) -> Dispatch {
        let event = Box::new(MessageReaction {
            channel_id: CHANNEL,
            message_id: id,
            guild_id: None,
            user_id: user,
            emoji,
            burst,
        });
        if added {
            Dispatch::MessageReactionAdd(event)
        } else {
            Dispatch::MessageReactionRemove(event)
        }
    }

    #[test]
    fn a_chip_click_asks_once_overlays_the_row_and_the_answer_updates_the_store() {
        let rest = rest();
        let (mut history, id) = reacting_history();
        let before = history.store.revision(CHANNEL, id).unwrap();

        // Add: the row shows it at once, disabled; the store keeps Discord's copy.
        history.toggle_reaction(&rest, CHANNEL, id, emoji(THUMBS));
        assert_eq!(history.reaction_requests.len(), 1);
        assert_eq!(
            history.reactions.of(CHANNEL, id),
            [reactions::Pending {
                emoji: emoji(THUMBS),
                action: Action::Add
            }]
        );
        let row = reaction_row(&mut history, id);
        assert_eq!(chip_of(&row, THUMBS), Some((3, true, true)));
        assert_eq!(reacted(&history, id)[0], (THUMBS.to_owned(), 2, false));
        assert_eq!(history.store.revision(CHANNEL, id), Some(before));

        // Discord's answer goes into the store and clears the overlay.
        assert!(!history.complete(
            &rest,
            reaction_answered(id, emoji(THUMBS), Action::Add, Ok(()))
        ));
        assert_eq!(history.reactions.len(), 0);
        assert_eq!(reacted(&history, id)[0], (THUMBS.to_owned(), 3, true));
        let row = reaction_row(&mut history, id);
        assert!(row.reactions.is_empty());
        assert_eq!(chip_of(&row, THUMBS), Some((3, true, false)));

        // The Gateway's echo of the same reaction changes nothing at all.
        let after = history.store.revision(CHANNEL, id).unwrap();
        assert!(after > before);
        history.apply(&gateway_reaction(id, SELF, emoji(THUMBS), true, false));
        assert_eq!(history.store.revision(CHANNEL, id), Some(after));
        assert!(!history.dirty(), "an echo is not a change");
        assert_eq!(history.store.tracked_bytes(), history.store.recount());

        // Remove: clicking one's own reaction takes it back.
        history.toggle_reaction(&rest, CHANNEL, id, emoji(THUMBS));
        assert_eq!(history.reactions.of(CHANNEL, id)[0].action, Action::Remove);
        let row = reaction_row(&mut history, id);
        assert_eq!(chip_of(&row, THUMBS), Some((2, false, true)));
        assert!(!history.complete(
            &rest,
            reaction_answered(id, emoji(THUMBS), Action::Remove, Ok(()))
        ));
        assert_eq!(reacted(&history, id)[0], (THUMBS.to_owned(), 2, false));
        let settled = history.store.revision(CHANNEL, id).unwrap();
        history.apply(&gateway_reaction(id, SELF, emoji(THUMBS), false, false));
        assert_eq!(history.store.revision(CHANNEL, id), Some(settled));
        assert_eq!(take_reactions(&mut history), 2, "one request per click");
    }

    #[test]
    fn the_gateway_first_or_the_answer_first_shows_the_reaction_once() {
        let rest = rest();
        let (mut history, id) = reacting_history();
        history.toggle_reaction(&rest, CHANNEL, id, emoji(PARTY));
        // The Gateway's report arrives before Discord's answer to the request.
        history.apply(&gateway_reaction(id, SELF, emoji(PARTY), true, false));
        assert_eq!(reacted(&history, id)[2], (PARTY.to_owned(), 1, true));
        let row = reaction_row(&mut history, id);
        assert_eq!(
            chip_of(&row, PARTY),
            Some((1, true, true)),
            "still disabled"
        );
        let revision = history.store.revision(CHANNEL, id);
        assert!(!history.complete(
            &rest,
            reaction_answered(id, emoji(PARTY), Action::Add, Ok(()))
        ));
        assert_eq!(history.store.revision(CHANNEL, id), revision);
        assert_eq!(reacted(&history, id)[2], (PARTY.to_owned(), 1, true));
        assert_eq!(history.reactions.len(), 0);
    }

    #[test]
    fn a_failed_request_changes_nothing_is_said_once_and_is_never_repeated() {
        let rest = rest();
        let (mut history, id) = reacting_history();
        let before = reacted(&history, id);
        history.toggle_reaction(&rest, CHANNEL, id, emoji(THUMBS));
        assert_eq!(take_reactions(&mut history), 1);
        assert!(!history.complete(
            &rest,
            reaction_answered(
                id,
                emoji(THUMBS),
                Action::Add,
                Err(RestError::PermissionDenied)
            )
        ));
        assert_eq!(
            history.snapshot().notice,
            Some(reactions::failure(Action::Add, RestError::PermissionDenied))
        );
        assert_eq!(reacted(&history, id), before);
        assert_eq!(history.reactions.len(), 0);
        assert_eq!(take_reactions(&mut history), 0, "nothing was repeated");
        let row = reaction_row(&mut history, id);
        assert!(row.reactions.is_empty());

        // An unconfirmed answer says the request may have been applied; the
        // chip stays at Discord's count until the Gateway says otherwise.
        history.toggle_reaction(&rest, CHANNEL, id, emoji(THUMBS));
        let unconfirmed = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout));
        assert!(!history.complete(
            &rest,
            reaction_answered(id, emoji(THUMBS), Action::Add, Err(unconfirmed))
        ));
        assert!(
            history
                .snapshot()
                .notice
                .is_some_and(|notice| notice.contains("may have been applied"))
        );
        assert_eq!(reacted(&history, id), before);
        // A refused login is reported to the caller, which stops the account.
        history.toggle_reaction(&rest, CHANNEL, id, emoji(THUMBS));
        assert!(history.complete(
            &rest,
            reaction_answered(
                id,
                emoji(THUMBS),
                Action::Add,
                Err(RestError::AuthenticationRequired)
            )
        ));
    }

    #[test]
    fn removing_a_reaction_that_is_already_gone_is_what_the_user_asked_for() {
        let rest = rest();
        let (mut history, id) = reacting_history();
        history.toggle_reaction(&rest, CHANNEL, id, custom_emoji(9, "wave"));
        assert_eq!(history.reactions.of(CHANNEL, id)[0].action, Action::Remove);
        assert!(!history.complete(
            &rest,
            reaction_answered(
                id,
                custom_emoji(9, "wave"),
                Action::Remove,
                Err(RestError::ResourceGone)
            )
        ));
        assert_eq!(history.snapshot().notice, None);
        // The last reaction left: the chip is gone.
        assert_eq!(reacted(&history, id), [(THUMBS.to_owned(), 2, false)]);
        // Adding to a message that is gone is an error, though.
        history.toggle_reaction(&rest, CHANNEL, id, emoji(THUMBS));
        assert!(!history.complete(
            &rest,
            reaction_answered(id, emoji(THUMBS), Action::Add, Err(RestError::ResourceGone))
        ));
        assert_eq!(
            history.snapshot().notice,
            Some(reactions::failure(Action::Add, RestError::ResourceGone))
        );
    }

    #[test]
    fn only_one_request_per_reaction_is_in_flight_and_the_ledger_is_bounded() {
        let rest = rest();
        let (mut history, id) = reacting_history();
        history.toggle_reaction(&rest, CHANNEL, id, emoji(THUMBS));
        history.toggle_reaction(&rest, CHANNEL, id, emoji(THUMBS));
        assert_eq!(history.snapshot().notice, Some(reactions::BUSY));
        assert_eq!(
            take_reactions(&mut history),
            1,
            "the second click asked nothing"
        );
        // A renamed custom emoji is the same reaction.
        history.toggle_reaction(&rest, CHANNEL, id, custom_emoji(9, "renamed"));
        history.toggle_reaction(&rest, CHANNEL, id, custom_emoji(9, "wave"));
        assert_eq!(history.snapshot().notice, Some(reactions::BUSY));
        take_reactions(&mut history);

        // Sixteen different emoji fill the ledger; the seventeenth is refused.
        let mut history = sending_history();
        history.store.set_user(SELF);
        let message = Box::new(message_with(NEWEST + 1, "popular".to_owned()));
        history.apply(&Dispatch::MessageCreate(message));
        let id = Snowflake(NEWEST + 1);
        history.refresh(&rest, &navigation_reacting(true), &Request::default());
        for n in 0..reactions::MAX_PENDING as u32 {
            let name = char::from_u32(0x1f600 + n).unwrap().to_string();
            history.toggle_reaction(&rest, CHANNEL, id, emoji(&name));
        }
        assert_eq!(history.reactions.len(), reactions::MAX_PENDING);
        history.toggle_reaction(&rest, CHANNEL, id, emoji(PARTY));
        assert_eq!(history.snapshot().notice, Some(reactions::FULL));
        assert_eq!(history.reactions.len(), reactions::MAX_PENDING);
        assert_eq!(take_reactions(&mut history), reactions::MAX_PENDING);
    }

    #[test]
    fn a_new_emoji_needs_the_permission_but_joining_an_existing_reaction_does_not() {
        let rest = rest();
        let (mut history, id) = reacting_history();
        history.refresh(&rest, &navigation_reacting(false), &Request::default());
        assert!(!history.snapshot().can_react);
        // Nobody reacted with 🎉 yet: adding it needs ADD_REACTIONS.
        history.toggle_reaction(&rest, CHANNEL, id, emoji(PARTY));
        assert_eq!(history.snapshot().notice, Some(NO_REACTIONS));
        history.add_reaction(&rest, CHANNEL, id, emoji(PARTY));
        assert_eq!(history.snapshot().notice, Some(NO_REACTIONS));
        assert_eq!(take_reactions(&mut history), 0);
        // 👍 already has reactions; adding to it needs only the history, and
        // taking one's own back needs nothing.
        history.toggle_reaction(&rest, CHANNEL, id, emoji(THUMBS));
        history.toggle_reaction(&rest, CHANNEL, id, custom_emoji(9, "wave"));
        assert_eq!(history.snapshot().notice, None);
        assert_eq!(take_reactions(&mut history), 2);
        history.refresh(&rest, &navigation_reacting(true), &Request::default());
        assert!(history.snapshot().can_react);
    }

    #[test]
    fn picking_an_emoji_that_is_already_the_users_asks_nothing() {
        let rest = rest();
        let (mut history, id) = reacting_history();
        history.add_reaction(&rest, CHANNEL, id, custom_emoji(9, "wave"));
        assert_eq!(take_reactions(&mut history), 0);
        assert_eq!(history.snapshot().notice, None);
        // A pick adds; it never takes a reaction back.
        history.add_reaction(&rest, CHANNEL, id, emoji(THUMBS));
        assert_eq!(history.reactions.of(CHANNEL, id)[0].action, Action::Add);
        assert_eq!(take_reactions(&mut history), 1);
    }

    #[test]
    fn requests_are_checked_against_the_store_not_against_the_ui() {
        let rest = rest();
        let (mut history, id) = reacting_history();
        history.toggle_reaction(&rest, Snowflake(1), id, emoji(THUMBS));
        assert_eq!(history.snapshot().notice, Some(NOT_OPEN));
        history.toggle_reaction(&rest, CHANNEL, Snowflake(1), emoji(THUMBS));
        assert_eq!(history.snapshot().notice, Some(NOT_LOADED));
        // An emoji that cannot be named in a request is refused locally.
        let nameless = Emoji {
            id: Some(Snowflake(77)),
            name: None,
            animated: false,
        };
        history.toggle_reaction(&rest, CHANNEL, id, nameless.clone());
        assert_eq!(history.snapshot().notice, Some(BAD_EMOJI));
        history.reaction_users(&rest, CHANNEL, id, nameless);
        assert_eq!(history.snapshot().notice, Some(BAD_EMOJI));
        assert_eq!(take_reactions(&mut history), 0);
        assert!(history.users_pending.is_none());
    }

    #[test]
    fn gateway_reactions_keep_held_rows_current_and_leave_everything_else_alone() {
        let (mut history, id) = reacting_history();
        history.snapshot();
        let other = Snowflake(7);
        history.apply(&gateway_reaction(id, other, emoji(THUMBS), true, false));
        history.apply(&gateway_reaction(id, other, emoji(PARTY), true, false));
        assert!(history.dirty());
        assert_eq!(
            reacted(&history, id),
            [
                (THUMBS.to_owned(), 3, false),
                ("wave".to_owned(), 1, true),
                (PARTY.to_owned(), 1, false)
            ]
        );
        // A super reaction counts but is not the user's normal reaction.
        history.apply(&gateway_reaction(id, SELF, emoji(THUMBS), true, true));
        assert_eq!(reacted(&history, id)[0], (THUMBS.to_owned(), 4, false));
        history.apply(&gateway_reaction(id, other, emoji(PARTY), false, false));
        history.apply(&Dispatch::MessageReactionRemoveEmoji(Box::new(
            fastcord_discord::gateway::MessageReactionRemoveEmoji {
                channel_id: CHANNEL,
                message_id: id,
                guild_id: None,
                emoji: emoji(THUMBS),
            },
        )));
        assert_eq!(reacted(&history, id), [("wave".to_owned(), 1, true)]);
        history.snapshot();

        // Messages that are not held, and other channels, cost nothing.
        let bytes = history.store.tracked_bytes();
        for n in 0..1_000 {
            let missing = Snowflake(n);
            history.apply(&gateway_reaction(missing, other, emoji(PARTY), true, false));
            let mut elsewhere = gateway_reaction(id, other, emoji(PARTY), true, false);
            if let Dispatch::MessageReactionAdd(event) = &mut elsewhere {
                event.channel_id = Snowflake(1);
            }
            history.apply(&elsewhere);
        }
        assert_eq!(history.store.tracked_bytes(), bytes);
        assert!(!history.dirty(), "nothing displayed changed");

        history.apply(&Dispatch::MessageReactionRemoveAll(
            fastcord_discord::gateway::MessageReactionRemoveAll {
                channel_id: CHANNEL,
                message_id: id,
                guild_id: None,
            },
        ));
        assert!(reacted(&history, id).is_empty());
        assert!(history.dirty());
        assert_eq!(history.store.tracked_bytes(), history.store.recount());
        let row = reaction_row(&mut history, id);
        assert_eq!(crate::timeline::chips(&row).count(), 0);
    }

    #[test]
    fn ready_names_the_account_whose_reactions_are_the_users_own() {
        let mut history = History::new(Slots::default());
        history.apply(&Dispatch::Ready(Box::new(
            fastcord_discord::gateway::Ready {
                session_id: fastcord_discord::gateway::SessionId::new("fixture-session".to_owned()),
                read: Default::default(),
                user: User {
                    id: SELF,
                    username: "alt".to_owned(),
                    global_name: None,
                    avatar: None,
                    bot: false,
                },
                users: Vec::new(),
                guilds: Vec::new(),
                unavailable_guilds: Vec::new(),
                private_channels: Vec::new(),
            },
        )));
        open_latest(&mut history);
        let id = Snowflake(NEWEST + 1);
        history.apply(&Dispatch::MessageCreate(Box::new(message(id.0))));
        history.apply(&gateway_reaction(
            id,
            Snowflake(7),
            emoji(THUMBS),
            true,
            false,
        ));
        history.apply(&gateway_reaction(id, SELF, emoji(PARTY), true, false));
        assert_eq!(
            reacted(&history, id),
            [(THUMBS.to_owned(), 1, false), (PARTY.to_owned(), 1, true)]
        );
        // A new session forgets the previous one's messages, not who the user is.
        history.apply(&Dispatch::MessageReactionRemove(Box::new(
            MessageReaction {
                channel_id: CHANNEL,
                message_id: id,
                guild_id: None,
                user_id: SELF,
                emoji: emoji(PARTY),
                burst: false,
            },
        )));
        assert_eq!(reacted(&history, id), [(THUMBS.to_owned(), 1, false)]);
    }

    fn people(n: usize) -> Vec<ReactionUser> {
        (0..n)
            .map(|n| ReactionUser {
                id: Snowflake(1_000 + n as u64),
                name: format!("person {n}"),
            })
            .collect()
    }

    #[test]
    fn who_reacted_is_loaded_only_when_asked_and_stays_bounded() {
        let rest = rest();
        let (mut history, id) = reacting_history();
        let snapshot = history.snapshot();
        assert!(snapshot.reaction_users.is_none() && history.users_pending.is_none());

        history.reaction_users(&rest, CHANNEL, id, emoji(THUMBS));
        assert!(history.users_pending.is_some());
        let shown = history.snapshot().reaction_users.unwrap();
        assert_eq!((shown.message, &shown.emoji), (id, &emoji(THUMBS)));
        assert_eq!(shown.state, UsersState::Loading);
        let serial = history.inspection.as_ref().unwrap().serial;

        // More than are shown: the first 25 and a note that there are others.
        assert!(!history.complete(
            &rest,
            Completed::ReactionUsers(serial, Ok(people(reactions::MAX_USERS + 1)))
        ));
        assert!(history.users_pending.is_none());
        let UsersState::Loaded { names, more } = history.snapshot().reaction_users.unwrap().state
        else {
            panic!("loaded");
        };
        assert_eq!((names.len(), more), (reactions::MAX_USERS, true));
        assert_eq!(names[0], "person 0");

        // Asking about another emoji replaces the list; the replaced one's
        // late answer is dropped.
        history.reaction_users(&rest, CHANNEL, id, custom_emoji(9, "wave"));
        let newer = history.inspection.as_ref().unwrap().serial;
        assert_ne!(newer, serial);
        history.complete(&rest, Completed::ReactionUsers(serial, Ok(people(2))));
        assert_eq!(
            history.snapshot().reaction_users.unwrap().state,
            UsersState::Loading
        );
        history.complete(&rest, Completed::ReactionUsers(newer, Ok(people(2))));
        assert_eq!(
            history.snapshot().reaction_users.unwrap().state,
            UsersState::Loaded {
                names: vec!["person 0".to_owned(), "person 1".to_owned()],
                more: false
            }
        );

        // A failure is a fixed sentence; closing drops list and request.
        history.reaction_users(&rest, CHANNEL, id, emoji(THUMBS));
        let failed = history.inspection.as_ref().unwrap().serial;
        assert!(history.complete(
            &rest,
            Completed::ReactionUsers(failed, Err(RestError::AuthenticationRequired))
        ));
        assert!(matches!(
            history.snapshot().reaction_users.unwrap().state,
            UsersState::Failed(_)
        ));
        history.reaction_users(&rest, CHANNEL, id, emoji(THUMBS));
        history.close_reaction_users();
        assert!(history.snapshot().reaction_users.is_none());
        assert!(history.users_pending.is_none());

        // A list belongs to its row: without the message it goes.
        history.reaction_users(&rest, CHANNEL, id, emoji(THUMBS));
        history.apply(&Dispatch::MessageDelete(MessageDelete {
            id,
            channel_id: CHANNEL,
            guild_id: None,
        }));
        history.refresh(&rest, &navigation_reacting(true), &Request::default());
        assert!(history.snapshot().reaction_users.is_none());
        assert!(history.users_pending.is_none());
    }

    #[test]
    fn a_new_session_or_channel_drops_the_list_but_not_requests_in_flight() {
        let rest = rest();
        let (mut history, id) = reacting_history();
        history.toggle_reaction(&rest, CHANNEL, id, emoji(THUMBS));
        history.reaction_users(&rest, CHANNEL, id, emoji(THUMBS));
        history.refresh(&rest, &NavigationSnapshot::default(), &Request::default());
        assert!(history.inspection.is_none() && history.users_pending.is_none());
        // The request is still answered; its answer meets an empty store.
        assert_eq!(history.reaction_requests.len(), 1);
        assert!(!history.complete(
            &rest,
            reaction_answered(id, emoji(THUMBS), Action::Add, Ok(()))
        ));
        assert_eq!(history.reactions.len(), 0);
        assert_eq!(history.store.messages_held(), 0);
        assert_eq!(history.store.tracked_bytes(), history.store.recount());
    }
}
