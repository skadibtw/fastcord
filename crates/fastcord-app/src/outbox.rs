//! Messages the user has sent that Discord has not confirmed (SPEC §5.1).
//!
//! Each send is one operation with a stable [`Nonce`]. An operation is removed
//! only by a confirmation (the REST response, or `MESSAGE_CREATE` carrying its
//! nonce and the user's own ID, whichever comes first) or by the user. It is
//! never re-posted by the program: a failure leaves it [`OutboxState::Failed`]
//! (Discord definitely did not create it) or [`OutboxState::Uncertain`] (it may
//! have), and only an explicit retry posts it again, with the same nonce so that
//! Discord returns the message it already made rather than creating another.
//!
//! One post per channel is in flight at a time, so messages sent in a row arrive
//! in the order they were written. At most [`MAX_OUTBOX`] unconfirmed operations
//! exist, each holding at most `MAX_CONTENT_CHARS` characters, so the outbox is
//! bounded however long the connection is down: the UI reserves one of the
//! shared [`Slots`] before it queues a send, and the outbox frees it when the
//! operation leaves.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fastcord_discord::{
    ComposeError, MAX_CONTENT_CHARS, MentionPolicy, NetworkFailure, Nonce, OutgoingMessage,
    Refusal, ReplyTo, RestError, RetryableFailure, SendError, UploadError, UploadFile,
    UploadProgress, UploadedFile,
};
use fastcord_model::{REPLY_PREVIEW_CHARS, Snowflake};

/// Unconfirmed messages, across all channels, the composer accepts before the
/// user has to retry or discard some.
pub const MAX_OUTBOX: usize = 16;
/// Display names retained with unfinished replies are clipped to this many
/// Unicode scalar values.
pub const MAX_REPLY_AUTHOR_CHARS: usize = 128;

/// The outbox's capacity, shared by the UI and the account worker. A send is
/// queued only after [`reserve`](Self::reserve) succeeds; the slot is freed
/// when its operation leaves the outbox (or when the queue refuses it), so
/// unconfirmed operations plus queued sends never exceed [`MAX_OUTBOX`], however
/// stale the UI's snapshot is.
#[derive(Clone, Debug, Default)]
pub struct Slots(Arc<AtomicUsize>);

impl Slots {
    /// Takes a slot if one is free.
    pub fn reserve(&self) -> bool {
        self.0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < MAX_OUTBOX).then_some(used + 1)
            })
            .is_ok()
    }

    /// Frees a slot; never below zero.
    pub fn release(&self) {
        let _ = self
            .0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_sub(1)
            });
    }

    #[cfg(test)]
    pub fn used(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}

/// The local identity of one send; unrelated to Discord's IDs and nonces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(pub u64);

/// The message a send answers, as the user chose it. Text and author are only
/// for the composer/outbox preview and are never included in `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Reply {
    pub message: Snowflake,
    pub author: Arc<str>,
    pub content: Arc<str>,
    pub mention_author: bool,
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("message", &self.message)
            .field("mention_author", &self.mention_author)
            .finish_non_exhaustive()
    }
}

impl Reply {
    fn wire(&self) -> ReplyTo {
        ReplyTo {
            message: self.message,
            mention_author: self.mention_author,
        }
    }
}

fn bound_reply_text(value: Arc<str>, max_chars: usize) -> Arc<str> {
    if value.chars().take(max_chars + 1).count() <= max_chars {
        value
    } else {
        Arc::from(value.chars().take(max_chars).collect::<String>())
    }
}

/// Why a send with files did not finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendFailure {
    Send(SendError),
    /// A file could not be uploaded; no message was created.
    Upload(UploadError),
    /// The user stopped the upload; no message was created.
    Cancelled,
}

impl From<SendError> for SendFailure {
    fn from(error: SendError) -> Self {
        Self::Send(error)
    }
}

const CANCELLED: &str = "Upload cancelled. Retry to start it again.";
const NOT_ALLOWED: &str = "The channel was closed, or you cannot send messages in it.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for an earlier message of the same channel to be answered.
    Queued,
    InFlight,
    Failed(&'static str),
    Uncertain(&'static str),
}

struct Op {
    id: OpId,
    channel: Snowflake,
    content: Arc<str>,
    nonce: Nonce,
    mentions: MentionPolicy,
    reply: Option<Reply>,
    /// Files to upload with the message. They are re-checked and uploaded again
    /// on every attempt: a failed upload created nothing, and a repeated nonce
    /// makes Discord return a message it already made.
    files: Vec<UploadFile>,
    progress: Arc<UploadProgress>,
    phase: Phase,
}

/// A queued send with files: what the account worker uploads and then posts.
pub struct FilePost {
    content: String,
    nonce: Nonce,
    mentions: MentionPolicy,
    reply: Option<ReplyTo>,
    files: Vec<UploadFile>,
    progress: Arc<UploadProgress>,
}

impl FilePost {
    pub fn files(&self) -> &[UploadFile] {
        &self.files
    }

    pub fn progress(&self) -> Arc<UploadProgress> {
        Arc::clone(&self.progress)
    }

    /// The message that names the uploaded files.
    pub fn message(&self, uploaded: Vec<UploadedFile>) -> Result<OutgoingMessage, ComposeError> {
        Ok(
            OutgoingMessage::with_files(&self.content, uploaded, self.nonce, self.mentions)?
                .replying_to(self.reply),
        )
    }
}

/// What the user is told about an unconfirmed message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboxState {
    /// Queued or on its way to Discord.
    Sending,
    /// Discord did not create it; retrying or editing is safe.
    Failed(&'static str),
    /// It may or may not have been created. It resolves by itself if Discord
    /// reports it; retrying reuses its nonce.
    Uncertain(&'static str),
}

/// One row of the outbox as the view sees it. `Debug` omits the text.
#[derive(Clone, PartialEq, Eq)]
pub struct OutboxItem {
    pub id: OpId,
    pub channel_id: Snowflake,
    pub content: Arc<str>,
    pub reply: Option<Reply>,
    pub state: OutboxState,
    /// Files this send uploads.
    pub files: Vec<UploadFile>,
    /// Bytes uploaded and in total, while the files are being sent.
    pub upload: Option<(u64, u64)>,
}

impl fmt::Debug for OutboxItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboxItem")
            .field("id", &self.id)
            .field("channel_id", &self.channel_id)
            .field("reply", &self.reply.as_ref().map(|reply| reply.message))
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// What a transport answer did to its operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finished {
    /// Discord created the message; the operation is gone.
    Confirmed,
    /// The failure is shown; the operation stays for the user to decide.
    Kept,
    /// The operation no longer exists (the Gateway confirmed it first).
    Unknown,
}

/// A fixed, secret-free sentence for a failed send.
fn reason(error: SendError, reply: bool) -> &'static str {
    match error {
        SendError::NotSent(error) => match error {
            RestError::Refused(refusal) => refusal_reason(refusal, reply),
            RestError::PermissionDenied => {
                "Discord did not allow this message or emoji here. Check channel permissions and animated/external emoji entitlement."
            }
            RestError::ResourceGone => "This channel is no longer available.",
            RestError::AuthenticationRequired => "Discord no longer accepts this login.",
            RestError::Http(status) if status.as_u16() == 400 && reply => {
                "Discord rejected the reply; the message it answers may be deleted, unavailable, not replyable, or the text may be too long."
            }
            RestError::Http(status) if status.as_u16() == 400 => {
                "Discord rejected the message; it may be too long or empty, or a custom emoji may be unavailable or not permitted for this account."
            }
            RestError::Http(_) => "Discord rejected the message.",
            RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)) => {
                "Could not reach Discord; the message was not sent."
            }
            RestError::WriteNotVerified(_) => fastcord_discord::WRITE_NOT_VERIFIED,
            _ => "The message could not be sent.",
        },
        SendError::Ambiguous(error) => match error {
            RestError::Retryable(RetryableFailure::Server(_)) => {
                "Discord reported a server error; the message may have been delivered."
            }
            RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout)) => {
                "Discord did not answer in time; the message may have been delivered."
            }
            RestError::Retryable(RetryableFailure::Network(_)) => {
                "The connection failed while sending; the message may have been delivered."
            }
            _ => "Discord's answer could not be read; the message may have been delivered.",
        },
    }
}

/// Sentences for the Discord error codes a send is known to meet.
fn known_refusal(code: u32) -> Option<&'static str> {
    Some(match code {
        50013 => {
            "Discord says you lack permission to send here (50013). Check your role and the channel's permissions."
        }
        50001 => "Discord says you cannot access this channel (50001).",
        50007 => "Discord says this user cannot receive messages from you (50007).",
        40002 => {
            "Discord requires you to verify your account before sending (40002). Verify your e-mail or phone in an official client."
        }
        40003 => "You are opening direct messages too fast (40003). Wait a while and try again.",
        40004 => "Discord has temporarily disabled sending messages for this account (40004).",
        20016 => "Slowmode is on in this channel (20016). Wait a moment and try again.",
        _ => return None,
    })
}

/// Distinct dynamic reasons kept for the process lifetime; a refusal beyond
/// this many falls back to a fixed sentence. (`OutboxState` stays `Copy`.)
const MAX_DYNAMIC_REASONS: usize = 32;

fn intern(text: String) -> Option<&'static str> {
    static SEEN: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().ok()?;
    if let Some(known) = seen.iter().find(|known| **known == text) {
        return Some(known);
    }
    if seen.len() >= MAX_DYNAMIC_REASONS {
        return None;
    }
    let leaked: &'static str = Box::leak(text.into_boxed_str());
    seen.push(leaked);
    Some(leaked)
}

fn refusal_reason(refusal: Refusal, reply: bool) -> &'static str {
    if let Some(text) = refusal.code.and_then(known_refusal) {
        return text;
    }
    let fallback = || {
        reason(
            SendError::NotSent(if refusal.status.as_u16() == 403 {
                RestError::PermissionDenied
            } else {
                RestError::Http(refusal.status)
            }),
            reply,
        )
    };
    // AutoMod refused the text (200000 blocked, 200001 flagged): Discord's own
    // reason, which names the rule when it gives one. The text stays here to
    // change; it is never sent again by itself.
    if let (Some(code @ (200_000 | 200_001)), message) = (refusal.code, refusal.message()) {
        let reason = if message.is_empty() {
            "AutoMod blocked this message".to_owned()
        } else {
            format!("AutoMod blocked this message: {message}")
        };
        if let Some(text) = intern(format!(
            "{reason} ({code}). Nothing was sent; change the text, then retry."
        )) {
            return text;
        }
    }
    // 50035 (invalid form body) and bodies without a code keep the specific
    // 400 wording; any other code is shown as Discord said it.
    let Some(code) = refusal.code.filter(|code| *code != 50035) else {
        return fallback();
    };
    let message = refusal.message();
    let text = if message.is_empty() {
        format!("Discord refused the send ({code}).")
    } else {
        format!("Discord refused the send: {message} ({code}).")
    };
    intern(text).unwrap_or_else(fallback)
}

fn compose_reason(error: ComposeError) -> &'static str {
    match error {
        ComposeError::Empty => "A message needs some text.",
        ComposeError::TooLong => "The message is too long for Discord.",
        ComposeError::VoiceMessageAlone => "A voice message cannot carry text or other files.",
    }
}

enum Taken {
    Plain(OutgoingMessage),
    Files(FilePost),
}

#[derive(Default)]
pub struct Outbox {
    ops: VecDeque<Op>,
    next_id: u64,
    slots: Slots,
}

impl Outbox {
    /// An outbox whose departures free `slots`, the capacity the UI reserves from.
    pub fn new(slots: Slots) -> Self {
        Self {
            slots,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    #[cfg(test)]
    pub fn nonce_of(&self, id: OpId) -> Option<Nonce> {
        self.ops.iter().find(|op| op.id == id).map(|op| op.nonce)
    }

    fn remove(&mut self, at: usize) {
        if self.ops.remove(at).is_some() {
            self.slots.release();
        }
    }

    /// Records a new send. A channel the user cannot send to is recorded as an
    /// already failed operation, so the text is never lost.
    #[cfg(test)]
    pub fn enqueue(
        &mut self,
        channel: Snowflake,
        content: &str,
        mentions: MentionPolicy,
        reply: Option<Reply>,
        nonce: Nonce,
        allowed: bool,
    ) -> OpId {
        self.enqueue_with_files(
            channel,
            content,
            Vec::new(),
            mentions,
            reply,
            nonce,
            allowed,
        )
    }

    /// [`enqueue`](Self::enqueue) for a message that uploads `files` too.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_with_files(
        &mut self,
        channel: Snowflake,
        content: &str,
        files: Vec<UploadFile>,
        mentions: MentionPolicy,
        reply: Option<Reply>,
        nonce: Nonce,
        allowed: bool,
    ) -> OpId {
        let reply = reply.map(|mut reply| {
            reply.author = bound_reply_text(reply.author, MAX_REPLY_AUTHOR_CHARS);
            reply.content = bound_reply_text(reply.content, REPLY_PREVIEW_CHARS);
            reply
        });
        self.next_id += 1;
        let id = OpId(self.next_id);
        self.ops.push_back(Op {
            id,
            channel,
            content: Arc::from(content),
            nonce,
            mentions,
            reply,
            files,
            progress: Arc::default(),
            phase: if allowed {
                Phase::Queued
            } else {
                Phase::Failed(NOT_ALLOWED)
            },
        });
        id
    }

    /// Starts every queued operation whose channel has nothing in flight, in
    /// the order the messages were written, and returns what to post.
    pub fn take_ready(&mut self) -> Vec<(OpId, Snowflake, OutgoingMessage)> {
        self.take(false)
            .into_iter()
            .filter_map(|(id, channel, taken)| match taken {
                Taken::Plain(message) => Some((id, channel, message)),
                Taken::Files(_) => None,
            })
            .collect()
    }

    /// [`take_ready`](Self::take_ready) for the sends that carry files.
    pub fn take_ready_files(&mut self) -> Vec<(OpId, Snowflake, FilePost)> {
        self.take(true)
            .into_iter()
            .filter_map(|(id, channel, taken)| match taken {
                Taken::Files(post) => Some((id, channel, post)),
                Taken::Plain(_) => None,
            })
            .collect()
    }

    /// Starts the queued operations of one kind. An operation of the other
    /// kind that is next in its channel holds back everything behind it, so
    /// messages with and without files keep the order they were written in.
    fn take(&mut self, files: bool) -> Vec<(OpId, Snowflake, Taken)> {
        let mut busy: Vec<Snowflake> = self
            .ops
            .iter()
            .filter(|op| op.phase == Phase::InFlight)
            .map(|op| op.channel)
            .collect();
        let mut ready = Vec::new();
        for op in &mut self.ops {
            if op.phase != Phase::Queued || busy.contains(&op.channel) {
                continue;
            }
            let reply = op.reply.as_ref().map(Reply::wire);
            if op.files.is_empty() == files {
                busy.push(op.channel);
                continue;
            }
            if files {
                if op.content.trim().chars().count() > MAX_CONTENT_CHARS {
                    op.phase = Phase::Failed(compose_reason(ComposeError::TooLong));
                    continue;
                }
                op.phase = Phase::InFlight;
                busy.push(op.channel);
                ready.push((
                    op.id,
                    op.channel,
                    Taken::Files(FilePost {
                        content: op.content.to_string(),
                        nonce: op.nonce,
                        mentions: op.mentions,
                        reply,
                        files: op.files.clone(),
                        progress: Arc::clone(&op.progress),
                    }),
                ));
                continue;
            }
            match OutgoingMessage::new(&op.content, op.nonce, op.mentions) {
                Ok(message) => {
                    let message = message.replying_to(reply);
                    op.phase = Phase::InFlight;
                    busy.push(op.channel);
                    ready.push((op.id, op.channel, Taken::Plain(message)));
                }
                Err(error) => op.phase = Phase::Failed(compose_reason(error)),
            }
        }
        ready
    }

    /// Applies a failure that is not a send's: an upload that failed or was
    /// stopped. Nothing was created, so the operation is plainly failed.
    pub fn fail(&mut self, id: OpId, failure: SendFailure) -> Finished {
        let reason = match failure {
            SendFailure::Send(error) => return self.finish(id, Err(error)),
            SendFailure::Upload(error) => error.reason(),
            SendFailure::Cancelled => CANCELLED,
        };
        match self
            .ops
            .iter_mut()
            .find(|op| op.id == id && op.phase == Phase::InFlight)
        {
            Some(op) => {
                op.phase = Phase::Failed(reason);
                Finished::Kept
            }
            None => Finished::Unknown,
        }
    }

    /// Applies a transport answer to the operation that was in flight.
    pub fn finish(&mut self, id: OpId, result: Result<(), SendError>) -> Finished {
        let Some(at) = self
            .ops
            .iter()
            .position(|op| op.id == id && op.phase == Phase::InFlight)
        else {
            return Finished::Unknown;
        };
        match result {
            Ok(()) => {
                self.remove(at);
                Finished::Confirmed
            }
            Err(error) => {
                let op = &mut self.ops[at];
                let reason = reason(error, op.reply.is_some());
                op.phase = if error.is_ambiguous() {
                    Phase::Uncertain(reason)
                } else {
                    Phase::Failed(reason)
                };
                Finished::Kept
            }
        }
    }

    /// The Gateway reported a message of the user's own with this nonce: the
    /// operation is done, whatever state it was in. Returns whether one was.
    pub fn confirm(&mut self, channel: Snowflake, wire_nonce: &str) -> bool {
        let Some(at) = self
            .ops
            .iter()
            .position(|op| op.channel == channel && op.nonce.matches(wire_nonce))
        else {
            return false;
        };
        self.remove(at);
        true
    }

    /// An explicit retry of a failed or unconfirmed send. It is posted again
    /// with its original nonce.
    pub fn retry(&mut self, id: OpId) -> bool {
        match self.ops.iter_mut().find(|op| op.id == id) {
            Some(op) if matches!(op.phase, Phase::Failed(_) | Phase::Uncertain(_)) => {
                op.phase = Phase::Queued;
                true
            }
            _ => false,
        }
    }

    /// Drops a failed or unconfirmed send at the user's request.
    pub fn discard(&mut self, id: OpId) -> bool {
        let Some(at) = self.ops.iter().position(|op| {
            op.id == id && matches!(op.phase, Phase::Failed(_) | Phase::Uncertain(_))
        }) else {
            return false;
        };
        self.remove(at);
        true
    }

    /// The operation's channel, for checking a retry against current permissions.
    pub fn channel_of(&self, id: OpId) -> Option<Snowflake> {
        self.ops.iter().find(|op| op.id == id).map(|op| op.channel)
    }

    /// Every unconfirmed message, oldest first (at most [`MAX_OUTBOX`]). All
    /// channels are listed so that one which can no longer be opened cannot
    /// hold outbox slots the user is unable to discard.
    pub fn items(&self) -> Vec<OutboxItem> {
        self.ops
            .iter()
            .map(|op| OutboxItem {
                id: op.id,
                channel_id: op.channel,
                content: Arc::clone(&op.content),
                reply: op.reply.clone(),
                files: op.files.clone(),
                upload: (op.phase == Phase::InFlight && !op.files.is_empty())
                    .then(|| (op.progress.sent(), op.progress.total())),
                state: match op.phase {
                    Phase::Queued | Phase::InFlight => OutboxState::Sending,
                    Phase::Failed(reason) => OutboxState::Failed(reason),
                    Phase::Uncertain(reason) => OutboxState::Uncertain(reason),
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use fastcord_discord::NonceGenerator;
    use fastcord_discord::StatusCode;

    use super::*;

    const A: Snowflake = Snowflake(500);
    const B: Snowflake = Snowflake(501);

    struct Fixture {
        outbox: Outbox,
        nonces: NonceGenerator,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                outbox: Outbox::default(),
                nonces: NonceGenerator::default(),
            }
        }

        fn send(&mut self, channel: Snowflake, text: &str) -> OpId {
            let nonce = self.nonces.next(1_790_000_000_000, 0);
            self.outbox
                .enqueue(channel, text, MentionPolicy::default(), None, nonce, true)
        }

        fn states(&self, channel: Snowflake) -> Vec<OutboxState> {
            self.outbox
                .items()
                .into_iter()
                .filter(|item| item.channel_id == channel)
                .map(|item| item.state)
                .collect()
        }
    }

    fn sample_file(name: &str, bytes: &[u8]) -> UploadFile {
        let dir = std::env::temp_dir().join(format!("fastcord-outbox-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        UploadFile::inspect(&path).unwrap()
    }

    fn send_files(f: &mut Fixture, channel: Snowflake, text: &str, name: &str) -> OpId {
        let nonce = f.nonces.next(1_790_000_000_000, 0);
        let file = sample_file(name, b"data");
        f.outbox.enqueue_with_files(
            channel,
            text,
            vec![file],
            MentionPolicy::default(),
            None,
            nonce,
            true,
        )
    }

    #[test]
    fn a_send_with_files_keeps_its_place_and_a_failed_upload_creates_nothing() {
        let mut f = Fixture::new();
        let files = send_files(&mut f, A, "", "order-a.png");
        let after = f.send(A, "after the files");
        // The plain message waits behind the earlier one that carries files.
        assert!(f.outbox.take_ready().is_empty());
        let started = f.outbox.take_ready_files();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].0, files);
        // A file-only message has no text and is still a valid post.
        let uploaded = Vec::new();
        assert!(started[0].2.message(uploaded).is_err(), "no files, no text");
        let item = &f.outbox.items()[0];
        assert_eq!(item.files.len(), 1);
        assert_eq!(item.upload, Some((0, 0)));
        assert_eq!(item.state, OutboxState::Sending);

        // A rejected upload is a plain failure and holds nothing back for good.
        let rejected = SendFailure::Upload(UploadError::Rejected(403));
        assert_eq!(f.outbox.fail(files, rejected), Finished::Kept);
        assert!(
            matches!(f.states(A)[0], OutboxState::Failed(reason) if reason.contains("refused"))
        );
        assert!(
            f.outbox.take_ready_files().is_empty(),
            "never retried by itself"
        );
        // The retry is explicit; the message behind it still waits for it.
        assert!(f.outbox.retry(files));
        assert!(f.outbox.take_ready().is_empty());
        assert_eq!(f.outbox.take_ready_files().len(), 1);
        assert_eq!(f.outbox.finish(files, Ok(())), Finished::Confirmed);
        let next: Vec<OpId> = f.outbox.take_ready().iter().map(|(id, ..)| *id).collect();
        assert_eq!(next, [after]);
    }

    #[test]
    fn cancelling_an_upload_fails_it_for_a_retry_and_ambiguous_posts_stay_uncertain() {
        let mut f = Fixture::new();
        let id = send_files(&mut f, A, "caption", "cancel-a.png");
        f.outbox.take_ready_files();
        assert_eq!(f.outbox.fail(id, SendFailure::Cancelled), Finished::Kept);
        let shown = f.outbox.items();
        assert!(matches!(shown[0].state, OutboxState::Failed(r) if r.contains("cancelled")));
        assert_eq!(shown[0].upload, None);
        assert_eq!(shown[0].files.len(), 1, "the files stay for the retry");
        // Only an upload in flight can be failed; anything else is unknown.
        assert_eq!(f.outbox.fail(id, SendFailure::Cancelled), Finished::Unknown);
        assert!(f.outbox.retry(id));
        f.outbox.take_ready_files();
        // A post whose answer was lost keeps the nonce and says it is unconfirmed.
        assert_eq!(
            f.outbox.fail(id, SendFailure::Send(ambiguous())),
            Finished::Kept
        );
        assert!(matches!(f.states(A)[0], OutboxState::Uncertain(_)));
        assert_eq!(f.outbox.nonce_of(id), f.outbox.nonce_of(id));
    }

    #[test]
    fn too_long_text_with_files_is_failed_without_uploading_anything() {
        let mut f = Fixture::new();
        let long = "x".repeat(MAX_CONTENT_CHARS + 1);
        send_files(&mut f, A, &long, "long-a.png");
        assert!(f.outbox.take_ready_files().is_empty());
        assert!(matches!(f.states(A)[0], OutboxState::Failed(_)));
    }

    fn ambiguous() -> SendError {
        SendError::Ambiguous(RestError::Retryable(RetryableFailure::Network(
            NetworkFailure::Timeout,
        )))
    }

    #[test]
    fn a_channel_has_one_post_in_flight_so_messages_keep_their_order() {
        let mut f = Fixture::new();
        let first = f.send(A, "one");
        let second = f.send(A, "two");
        let other = f.send(B, "elsewhere");
        let ready = f.outbox.take_ready();
        let started: Vec<OpId> = ready.iter().map(|(id, ..)| *id).collect();
        assert_eq!(started, [first, other], "the second waits behind the first");
        // Nothing more starts while they are in flight.
        assert!(f.outbox.take_ready().is_empty());
        assert_eq!(f.outbox.finish(first, Ok(())), Finished::Confirmed);
        let next: Vec<OpId> = f.outbox.take_ready().iter().map(|(id, ..)| *id).collect();
        assert_eq!(next, [second]);
        assert_eq!(f.states(A), [OutboxState::Sending]);
    }

    #[test]
    fn a_definite_refusal_is_failed_and_an_ambiguous_one_is_uncertain_and_neither_reposts() {
        let mut f = Fixture::new();
        let refused = f.send(A, "refused");
        let lost = f.send(B, "lost");
        f.outbox.take_ready();
        let denied = SendError::NotSent(RestError::PermissionDenied);
        assert_eq!(f.outbox.finish(refused, Err(denied)), Finished::Kept);
        assert_eq!(f.outbox.finish(lost, Err(ambiguous())), Finished::Kept);
        assert!(matches!(f.states(A)[0], OutboxState::Failed(text) if text.contains("allow")));
        assert!(
            matches!(f.states(B)[0], OutboxState::Uncertain(text) if text.contains("may have been delivered"))
        );
        // However often the program looks, it posts nothing by itself.
        for _ in 0..5 {
            assert!(f.outbox.take_ready().is_empty());
        }
        // An answer for an operation that is not in flight changes nothing.
        assert_eq!(f.outbox.finish(refused, Ok(())), Finished::Unknown);
        assert_eq!(f.outbox.len(), 2);
    }

    #[test]
    fn custom_emoji_rejections_remain_visible_with_source_and_without_reposting() {
        for error in [
            RestError::PermissionDenied,
            RestError::Http(StatusCode::BAD_REQUEST),
        ] {
            let mut f = Fixture::new();
            let source = "👩🏽‍💻 <:server:12345> <a:external:45678>";
            let id = f.send(A, source);
            assert_eq!(f.outbox.take_ready().len(), 1);
            assert_eq!(
                f.outbox.finish(id, Err(SendError::NotSent(error))),
                Finished::Kept
            );
            let item = f.outbox.items().remove(0);
            assert_eq!(item.content.as_ref(), source);
            let OutboxState::Failed(message) = item.state else {
                panic!("a definite rejection must be shown as failed");
            };
            assert!(message.contains("emoji"));
            assert!(f.outbox.take_ready().is_empty());
        }
    }

    #[test]
    fn a_reply_keeps_its_target_and_choice_through_failure_and_retry() {
        let mut f = Fixture::new();
        let reply = Reply {
            message: Snowflake(42),
            author: Arc::from("Other"),
            content: Arc::from("original text"),
            mention_author: false,
        };
        let nonce = f.nonces.next(1_790_000_000_000, 0);
        let id = f.outbox.enqueue(
            A,
            "answer",
            MentionPolicy::default(),
            Some(reply.clone()),
            nonce,
            true,
        );
        let posted = f.outbox.take_ready().remove(0).2;
        let shown = format!("{posted:?}");
        assert!(
            shown.contains("message: Snowflake(42)") && shown.contains("mention_author: false"),
            "{shown}"
        );
        // A refused reply says that its target may be gone.
        let refused = SendError::NotSent(RestError::Http(StatusCode::BAD_REQUEST));
        f.outbox.finish(id, Err(refused));
        let item = f.outbox.items().remove(0);
        assert_eq!(item.reply, Some(reply));
        assert!(matches!(item.state, OutboxState::Failed(text) if text.contains("answers")));
        // A retry posts the same reply with the same nonce.
        assert!(f.outbox.retry(id));
        assert_eq!(f.outbox.take_ready().remove(0).2, posted);
        // A plain message refused the same way is not described as a reply.
        let plain = f.send(B, "plain");
        f.outbox.take_ready();
        f.outbox.finish(plain, Err(refused));
        assert!(matches!(f.states(B)[0], OutboxState::Failed(text) if !text.contains("answers")));
        // The author's name is display data and never reaches Debug output.
        let shown = format!("{:?}", f.outbox.items());
        assert!(!shown.contains("Other") && !shown.contains("original text"));
    }

    #[test]
    fn reply_preview_strings_are_bounded_at_the_outbox_boundary() {
        let mut outbox = Outbox::default();
        let nonce = NonceGenerator::default().next(1_790_000_000_000, 0);
        outbox.enqueue(
            A,
            "answer",
            MentionPolicy::default(),
            Some(Reply {
                message: Snowflake(42),
                author: Arc::from("é".repeat(MAX_REPLY_AUTHOR_CHARS + 1)),
                content: Arc::from("🙂".repeat(fastcord_model::REPLY_PREVIEW_CHARS + 1)),
                mention_author: false,
            }),
            nonce,
            true,
        );
        let reply = outbox.items().remove(0).reply.unwrap();
        assert_eq!(reply.author.chars().count(), MAX_REPLY_AUTHOR_CHARS);
        assert_eq!(
            reply.content.chars().count(),
            fastcord_model::REPLY_PREVIEW_CHARS
        );
    }

    #[test]
    fn retry_is_explicit_reuses_the_nonce_and_only_applies_to_unresolved_sends() {
        let mut f = Fixture::new();
        let id = f.send(A, "hello");
        let first = f.outbox.take_ready().remove(0).2;
        // Sending operations cannot be retried or discarded.
        assert!(!f.outbox.retry(id));
        assert!(!f.outbox.discard(id));
        f.outbox.finish(id, Err(ambiguous()));
        assert!(f.outbox.retry(id));
        assert_eq!(f.states(A), [OutboxState::Sending]);
        let again = f.outbox.take_ready().remove(0).2;
        assert_eq!(first.nonce(), again.nonce(), "same nonce: Discord dedupes");
        assert_eq!(first, again);
        assert!(!f.outbox.retry(OpId(999)));
        assert_eq!(f.outbox.finish(id, Ok(())), Finished::Confirmed);
        assert_eq!(f.outbox.len(), 0);
        assert!(!f.outbox.retry(id), "confirmed operations are gone");
    }

    #[test]
    fn gateway_confirmation_by_nonce_resolves_in_any_state_without_a_second_post() {
        let mut f = Fixture::new();
        let in_flight = f.send(A, "in flight");
        let uncertain = f.send(B, "uncertain");
        let queued = f.send(A, "queued behind");
        f.outbox.take_ready();
        f.outbox.finish(uncertain, Err(ambiguous()));
        let wire = |f: &Fixture, id: OpId| {
            f.outbox
                .ops
                .iter()
                .find(|op| op.id == id)
                .unwrap()
                .nonce
                .to_string()
        };
        // Another channel's identical nonce text is not ours.
        let nonce = wire(&f, in_flight);
        assert!(!f.outbox.confirm(B, &nonce));
        assert!(!f.outbox.confirm(A, "not a nonce"));
        // Gateway first: the REST answer that follows finds nothing to confirm.
        assert!(f.outbox.confirm(A, &nonce));
        assert_eq!(f.outbox.finish(in_flight, Ok(())), Finished::Unknown);
        assert!(f.outbox.confirm(B, &wire(&f, uncertain)));
        assert!(f.states(B).is_empty());
        // The message queued behind a confirmed one may now go out.
        let ready = f.outbox.take_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0, queued);
        // Confirming twice (a replayed event) does nothing.
        assert!(!f.outbox.confirm(A, &nonce));
    }

    #[test]
    fn discard_is_only_for_failed_or_unconfirmed_sends() {
        let mut f = Fixture::new();
        let id = f.send(A, "x");
        f.outbox.take_ready();
        f.outbox
            .finish(id, Err(SendError::NotSent(RestError::ResourceGone)));
        assert!(f.outbox.discard(id));
        assert_eq!(f.outbox.len(), 0);
        assert!(!f.outbox.discard(id));
    }

    #[test]
    fn a_channel_that_cannot_be_sent_to_keeps_the_text_as_a_failed_operation() {
        let mut outbox = Outbox::default();
        let nonce = NonceGenerator::default().next(1_790_000_000_000, 0);
        let id = outbox.enqueue(
            A,
            "typed text",
            MentionPolicy::default(),
            None,
            nonce,
            false,
        );
        assert!(outbox.take_ready().is_empty(), "never posted");
        let items = outbox.items();
        assert_eq!(&*items[0].content, "typed text");
        assert!(matches!(items[0].state, OutboxState::Failed(text) if text.contains("closed")));
        assert!(outbox.retry(id), "an explicit retry is still possible");
    }

    #[test]
    fn drafts_that_cannot_be_sent_fail_before_any_request() {
        let mut f = Fixture::new();
        let empty = f.send(A, "  \n ");
        let long = f.send(B, &"x".repeat(fastcord_discord::MAX_CONTENT_CHARS + 1));
        assert!(f.outbox.take_ready().is_empty());
        assert!(matches!(f.states(A)[0], OutboxState::Failed(t) if t.contains("needs some text")));
        assert!(matches!(f.states(B)[0], OutboxState::Failed(t) if t.contains("too long")));
        assert!(f.outbox.discard(empty) && f.outbox.discard(long));
    }

    #[test]
    fn every_failure_kind_has_fixed_text_without_request_details() {
        let cases = [
            SendError::NotSent(RestError::PermissionDenied),
            SendError::NotSent(RestError::ResourceGone),
            SendError::NotSent(RestError::AuthenticationRequired),
            SendError::NotSent(RestError::Http(StatusCode::BAD_REQUEST)),
            SendError::NotSent(RestError::Http(StatusCode::PAYLOAD_TOO_LARGE)),
            SendError::NotSent(RestError::Retryable(RetryableFailure::Network(
                NetworkFailure::Connection,
            ))),
            SendError::NotSent(RestError::InvalidRoute),
            ambiguous(),
            SendError::Ambiguous(RestError::Retryable(RetryableFailure::Server(
                StatusCode::BAD_GATEWAY,
            ))),
            SendError::Ambiguous(RestError::Retryable(RetryableFailure::Network(
                NetworkFailure::Body,
            ))),
            SendError::Ambiguous(RestError::InvalidJson),
            SendError::Ambiguous(RestError::ResponseBodyTooLarge),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for error in cases {
            let text = reason(error, false);
            assert!(text.ends_with('.') && !text.contains("502") && !text.contains("http"));
            // Ambiguity is always spelled out; a refusal never claims it.
            assert_eq!(
                text.contains("may have been delivered"),
                error.is_ambiguous(),
                "{text}"
            );
            seen.insert(text);
        }
        assert!(seen.len() >= 8, "failures stay distinguishable: {seen:?}");
    }

    #[test]
    fn debug_never_prints_the_text() {
        let mut f = Fixture::new();
        f.send(A, "private words");
        let shown = format!("{:?}", f.outbox.items());
        assert!(!shown.contains("private") && shown.contains("OutboxItem"));
    }

    #[test]
    fn slots_bound_the_outbox_and_every_departure_frees_one() {
        let slots = Slots::default();
        let mut outbox = Outbox::new(slots.clone());
        let mut nonces = NonceGenerator::default();
        let mut ids = Vec::new();
        for n in 0..MAX_OUTBOX {
            assert!(slots.reserve(), "slot {n}");
            let nonce = nonces.next(1_790_000_000_000, 0);
            ids.push(outbox.enqueue(A, "x", MentionPolicy::default(), None, nonce, true));
        }
        assert!(!slots.reserve(), "the outbox is full");
        assert_eq!(slots.used(), MAX_OUTBOX);
        // A REST confirmation, a Gateway confirmation, and a discard each free one.
        let first = outbox.take_ready().remove(0).0;
        assert_eq!(outbox.finish(first, Ok(())), Finished::Confirmed);
        assert_eq!(slots.used(), MAX_OUTBOX - 1);
        let second = outbox.take_ready().remove(0).0;
        let wire = outbox.ops[0].nonce.to_string();
        assert!(outbox.confirm(A, &wire));
        assert_eq!(slots.used(), MAX_OUTBOX - 2);
        assert_eq!(outbox.finish(second, Ok(())), Finished::Unknown);
        assert_eq!(slots.used(), MAX_OUTBOX - 2, "nothing left, nothing freed");
        let third = outbox.take_ready().remove(0).0;
        outbox.finish(third, Err(ambiguous()));
        assert!(outbox.discard(third));
        assert_eq!(slots.used(), MAX_OUTBOX - 3);
        // A kept failure does not free its slot.
        let fourth = outbox.take_ready().remove(0).0;
        outbox.finish(fourth, Err(SendError::NotSent(RestError::PermissionDenied)));
        assert_eq!(slots.used(), MAX_OUTBOX - 3);
        assert_eq!(slots.used(), outbox.len());
        // Releasing never underflows.
        let empty = Slots::default();
        empty.release();
        assert_eq!(empty.used(), 0);
        assert!(empty.reserve());
    }

    fn refused(status: u16, body: &str) -> SendError {
        let status = StatusCode::from_u16(status).unwrap();
        SendError::from_transport(RestError::Refused(Refusal::from_body(
            status,
            body.as_bytes(),
        )))
    }

    #[test]
    fn refused_sends_say_what_discord_said_and_stay_not_sent() {
        let text = |status, body: &str| reason(refused(status, body), false);
        assert!(
            text(403, r#"{"code":50013,"message":"Missing Permissions"}"#).contains("permission")
        );
        let verify = text(
            403,
            r#"{"code":40002,"message":"You need to verify your account"}"#,
        );
        assert!(verify.contains("verify your account") && verify.contains("40002"));
        // Unmapped code: Discord's wording plus the code.
        assert_eq!(
            text(403, r#"{"code":99999,"message":"Account restricted"}"#),
            "Discord refused the send: Account restricted (99999)."
        );
        // No usable body: the old fixed sentences.
        assert!(text(403, "<html>").contains("channel permissions"));
        assert!(text(400, r#"{"code":50035,"message":"Invalid Form Body"}"#).contains("too long"));
        // Oversized, control characters, secret-shaped words.
        assert!(
            text(
                403,
                &format!(r#"{{"code":7,"message":"{}"}}"#, "y".repeat(9000))
            )
            .contains("channel permissions")
        );
        let dirty = text(
            403,
            r#"{"code":77777,"message":"x\u0007\n me@example.test"}"#,
        );
        assert_eq!(dirty, "Discord refused the send: x ... (77777).");
        assert!(!refused(403, "{}").is_ambiguous());
    }

    #[test]
    fn an_automod_block_names_discords_reason_and_keeps_the_text_without_resending() {
        let text = |body: &str| reason(refused(400, body), false);
        let blocked =
            text(r#"{"code":200000,"message":"Message blocked by AutoMod: Friendly words"}"#);
        assert_eq!(
            blocked,
            "AutoMod blocked this message: Message blocked by AutoMod: Friendly words (200000). Nothing was sent; change the text, then retry."
        );
        assert!(text(r#"{"code":200001,"message":"Flagged"}"#).contains("(200001)"));
        assert!(text(r#"{"code":200000}"#).starts_with("AutoMod blocked this message ("));
        // The send stays in the outbox as a failure and posts nothing again.
        let mut f = Fixture::new();
        let id = f.send(A, "blocked text");
        f.outbox.take_ready();
        let error = refused(400, r#"{"code":200000,"message":"Blocked"}"#);
        assert_eq!(f.outbox.finish(id, Err(error)), Finished::Kept);
        assert!(matches!(f.states(A)[0], OutboxState::Failed(t) if t.contains("AutoMod")));
        for _ in 0..3 {
            assert!(f.outbox.take_ready().is_empty());
        }
    }
}
