//! Creating messages and replies (`POST /channels/{id}/messages`) with a stable
//! nonce and an explicit mention policy (SPEC §5.1, §5.2).
//!
//! A send is a write whose outcome can be unknowable: a connection that drops
//! after the request left may or may not have created the message. This module
//! therefore never retries and never guesses. It classifies every failure as
//! either [`SendError::NotSent`] (Discord definitely did not create a message)
//! or [`SendError::Ambiguous`] (it may have), and sends every attempt with the
//! operation's [`Nonce`] and `enforce_nonce: true`, so that an explicit retry of
//! an ambiguous send makes Discord return the message it already created
//! instead of creating a second one (within the window in which Discord keeps
//! nonces; see `docs/PROTOCOL.md`). Recognizing the message when it comes back
//! over the Gateway is the nonce's other job: [`Nonce::matches`].

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use fastcord_model::{DISCORD_EPOCH_MS, Message, Snowflake};
use rand_core::{OsRng, RngCore};
use serde::Serialize;

use crate::error::RetryableFailure;
use crate::poll::PollDraft;
use crate::upload::UploadedFile;

/// `IS_VOICE_MESSAGE` in a new message's `flags`.
const VOICE_MESSAGE_FLAG: u32 = 1 << 13;
/// `SUPPRESS_NOTIFICATIONS` in a new message's `flags`: no push or unread ping.
const SILENT_FLAG: u32 = 1 << 12;
use crate::{Clock, Method, NetworkFailure, Priority, RestClient, RestError, RestRequest, Route};

/// Longest message the composer lets a send reach Discord with: Discord's
/// limit is 2,000 characters, or 4,000 with Nitro, and the server stays
/// authoritative about which applies. Anything longer cannot succeed.
pub const MAX_CONTENT_CHARS: usize = 4_000;

/// Low bits of the nonce that carry entropy rather than the timestamp.
const NONCE_ENTROPY_BITS: u32 = 22;
const NONCE_ENTROPY_MASK: u64 = (1 << NONCE_ENTROPY_BITS) - 1;
/// Timestamps beyond this many milliseconds since the Discord epoch would not
/// fit the 64-bit nonce (about 139 years after 2015).
const NONCE_TIME_MASK: u64 = (1 << (64 - NONCE_ENTROPY_BITS)) - 1;

/// Identifies one send operation to Discord and, over the Gateway, back to
/// us. Snowflake-shaped (time in the high bits, like the official client's),
/// at most 19 decimal digits, and sent as a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Nonce(u64);

impl Nonce {
    /// Whether the nonce Discord reported on a message (`Message::nonce`) is this one.
    pub fn matches(self, wire: &str) -> bool {
        wire == self.0.to_string()
    }
}

impl fmt::Display for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for Nonce {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Produces nonces that are unique within the process and, because enforcement
/// is per author, very unlikely to equal one chosen by the user's other
/// clients: the official client's nonces have no entropy in their low bits,
/// these have 22 random ones.
#[derive(Debug, Default)]
pub struct NonceGenerator {
    last: u64,
}

impl NonceGenerator {
    /// A nonce for a send started at `unix_ms`, with `entropy` in its low bits.
    /// Strictly greater than every nonce this generator produced before.
    pub fn next(&mut self, unix_ms: u64, entropy: u64) -> Nonce {
        let time = unix_ms.saturating_sub(DISCORD_EPOCH_MS) & NONCE_TIME_MASK;
        let candidate = (time << NONCE_ENTROPY_BITS) | (entropy & NONCE_ENTROPY_MASK);
        self.last = candidate.max(self.last.saturating_add(1));
        Nonce(self.last)
    }

    /// [`next`](Self::next) with the system clock and operating-system entropy.
    pub fn next_now(&mut self) -> Nonce {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        self.next(unix_ms, OsRng.next_u64())
    }
}

/// Which mentions in a message's text may notify anyone. Always sent in full
/// as `allowed_mentions.parse`, never left to Discord's default, so what the
/// user chose is what happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MentionPolicy {
    /// `<@id>` mentions of users the user typed.
    pub users: bool,
    /// `<@&id>` role mentions.
    pub roles: bool,
    /// `@everyone` and `@here`.
    pub everyone: bool,
}

impl Default for MentionPolicy {
    /// Mentions the user spelled out notify; mass pings need a deliberate choice.
    fn default() -> Self {
        Self {
            users: true,
            roles: true,
            everyone: false,
        }
    }
}

impl MentionPolicy {
    pub(crate) fn parse(self) -> Vec<&'static str> {
        let mut parse = Vec::with_capacity(3);
        if self.users {
            parse.push("users");
        }
        if self.roles {
            parse.push("roles");
        }
        if self.everyone {
            parse.push("everyone");
        }
        parse
    }
}

/// The message a new message answers, and whether its author is notified.
/// A reply always answers a message of the channel it is posted to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyTo {
    pub message: Snowflake,
    /// Sent as `allowed_mentions.replied_user`: never left to Discord's
    /// default, so the user's choice is what happens.
    pub mention_author: bool,
}

/// Why a draft cannot be sent at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposeError {
    /// Nothing but whitespace.
    Empty,
    /// Longer than [`MAX_CONTENT_CHARS`].
    TooLong,
    /// A voice message has no text and no other file.
    VoiceMessageAlone,
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "a message needs some text",
            Self::TooLong => "the message is too long for Discord",
            Self::VoiceMessageAlone => "a voice message cannot carry text or other files",
        })
    }
}

impl std::error::Error for ComposeError {}

/// The "@silent" convention of Discord's own clients: a draft that starts with
/// `@silent` (any case) followed by whitespace or nothing is sent without
/// notifications and without that prefix. Returns whether it was there and the
/// rest, untrimmed.
pub fn strip_silent(content: &str) -> (bool, &str) {
    strip_prefix_word(content, "@silent")
}

/// The "/tts" convention: a draft that starts with `/tts` (any case) followed
/// by whitespace or nothing is a text-to-speech message, without that prefix.
/// Whether the account may send one is the caller's check (`SEND_TTS_MESSAGES`).
pub fn strip_tts(content: &str) -> (bool, &str) {
    strip_prefix_word(content, "/tts")
}

fn strip_prefix_word<'a>(content: &'a str, prefix: &str) -> (bool, &'a str) {
    let content = content.trim_start();
    let prefix_len = prefix.len();
    match content.get(..prefix_len) {
        Some(head) if head.eq_ignore_ascii_case(prefix) => {
            let rest = &content[prefix_len..];
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return (true, rest);
            }
            (false, content)
        }
        _ => (false, content),
    }
}

/// A validated message ready to send. `Debug` never prints the text.
#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    content: String,
    nonce: Nonce,
    mentions: MentionPolicy,
    reply: Option<ReplyTo>,
    files: Vec<UploadedFile>,
    sticker: Option<Snowflake>,
    forward: Option<ForwardSource>,
    poll: Option<PollDraft>,
    silent: bool,
    tts: bool,
}

impl OutgoingMessage {
    /// Surrounding whitespace is trimmed, as Discord does; interior text is sent as written.
    pub fn new(content: &str, nonce: Nonce, mentions: MentionPolicy) -> Result<Self, ComposeError> {
        let (tts, content) = strip_tts(content);
        let (silent, content) = strip_silent(content);
        let content = content.trim();
        if content.is_empty() {
            return Err(ComposeError::Empty);
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(ComposeError::TooLong);
        }
        Ok(Self {
            content: content.to_owned(),
            nonce,
            mentions,
            reply: None,
            files: Vec::new(),
            sticker: None,
            forward: None,
            poll: None,
            silent,
            tts,
        })
    }

    /// A message that carries uploaded files; its text may be empty.
    pub fn with_files(
        content: &str,
        files: Vec<UploadedFile>,
        nonce: Nonce,
        mentions: MentionPolicy,
    ) -> Result<Self, ComposeError> {
        let (tts, content) = strip_tts(content);
        let (silent, content) = strip_silent(content);
        let content = content.trim();
        if content.is_empty() && files.is_empty() {
            return Err(ComposeError::Empty);
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(ComposeError::TooLong);
        }
        if files.iter().any(|file| file.voice.is_some())
            && (files.len() != 1 || !content.is_empty())
        {
            return Err(ComposeError::VoiceMessageAlone);
        }
        Ok(Self {
            content: content.to_owned(),
            nonce,
            mentions,
            reply: None,
            files,
            sticker: None,
            forward: None,
            poll: None,
            silent,
            tts,
        })
    }

    /// A message that carries one sticker and no text (`sticker_ids`).
    pub fn with_sticker(sticker: Snowflake, nonce: Nonce, mentions: MentionPolicy) -> Self {
        Self {
            content: String::new(),
            nonce,
            mentions,
            reply: None,
            files: Vec::new(),
            sticker: Some(sticker),
            forward: None,
            poll: None,
            silent: false,
            tts: false,
        }
    }

    /// A forward of `source` to the channel this is posted to: Discord makes
    /// the snapshot, so the message carries no text or files of its own.
    pub fn forward(source: ForwardSource, nonce: Nonce) -> Self {
        Self {
            content: String::new(),
            nonce,
            mentions: MentionPolicy::default(),
            reply: None,
            files: Vec::new(),
            sticker: None,
            forward: Some(source),
            poll: None,
            silent: false,
            tts: false,
        }
    }

    /// A message that carries only a poll.
    pub fn with_poll(poll: PollDraft, nonce: Nonce) -> Self {
        Self {
            content: String::new(),
            nonce,
            mentions: MentionPolicy::default(),
            reply: None,
            files: Vec::new(),
            sticker: None,
            forward: None,
            poll: Some(poll),
            silent: false,
            tts: false,
        }
    }

    /// Makes the message a reply. Discord is asked to refuse it if the
    /// message it answers no longer exists (`fail_if_not_exists`), so a reply
    /// is never silently posted as a plain message.
    pub fn replying_to(mut self, reply: Option<ReplyTo>) -> Self {
        self.reply = reply;
        self
    }

    pub fn nonce(&self) -> Nonce {
        self.nonce
    }

    fn body(&self) -> CreateMessage<'_> {
        CreateMessage {
            content: &self.content,
            nonce: self.nonce,
            enforce_nonce: true,
            tts: self.tts,
            flags: if self.files.iter().any(|file| file.voice.is_some()) {
                VOICE_MESSAGE_FLAG
            } else {
                0
            } | if self.silent { SILENT_FLAG } else { 0 },
            allowed_mentions: AllowedMentions {
                parse: self.mentions.parse(),
                replied_user: self.reply.map(|reply| reply.mention_author),
            },
            message_reference: match (self.forward, self.reply) {
                (Some(source), _) => Some(WireReference {
                    kind: 1,
                    message_id: source.message,
                    channel_id: Some(source.channel),
                    guild_id: source.guild,
                    fail_if_not_exists: true,
                }),
                (None, Some(reply)) => Some(WireReference {
                    kind: 0,
                    message_id: reply.message,
                    channel_id: None,
                    guild_id: None,
                    fail_if_not_exists: true,
                }),
                (None, None) => None,
            },
            sticker_ids: self.sticker.into_iter().collect(),
            poll: self.poll.as_ref(),
            attachments: wire_attachments(&self.files),
        }
    }
}

pub(crate) fn wire_attachments(files: &[UploadedFile]) -> Vec<WireAttachment<'_>> {
    files
        .iter()
        .map(|file| WireAttachment {
            id: file.id.to_string(),
            filename: &file.filename,
            uploaded_filename: &file.uploaded_filename,
            duration_secs: file
                .voice
                .as_ref()
                .map(|voice| voice.duration_ms as f64 / 1000.0),
            waveform: file.voice.as_ref().map(|voice| voice.waveform.as_str()),
            description: &file.description,
        })
        .collect()
}

impl fmt::Debug for OutgoingMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutgoingMessage")
            .field("nonce", &self.nonce)
            .field("content_chars", &self.content.chars().count())
            .field("mentions", &self.mentions)
            .field("reply", &self.reply)
            .field("files", &self.files.len())
            .field("sticker", &self.sticker.is_some())
            .field("forward", &self.forward.is_some())
            .field("poll", &self.poll.is_some())
            .field("silent", &self.silent)
            .field("tts", &self.tts)
            .finish()
    }
}

#[derive(Serialize)]
struct CreateMessage<'a> {
    content: &'a str,
    nonce: Nonce,
    enforce_nonce: bool,
    tts: bool,
    flags: u32,
    allowed_mentions: AllowedMentions,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_reference: Option<WireReference>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sticker_ids: Vec<Snowflake>,
    #[serde(skip_serializing_if = "Option::is_none")]
    poll: Option<&'a PollDraft>,
    /// Present exactly when files were uploaded for this message.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<WireAttachment<'a>>,
}

#[derive(Serialize)]
pub(crate) struct WireAttachment<'a> {
    id: String,
    filename: &'a str,
    uploaded_filename: &'a str,
    /// Voice messages only.
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform: Option<&'a str>,
    #[serde(skip_serializing_if = "str::is_empty")]
    description: &'a str,
}

#[derive(Serialize)]
struct AllowedMentions {
    parse: Vec<&'static str>,
    /// Present exactly for replies.
    #[serde(skip_serializing_if = "Option::is_none")]
    replied_user: Option<bool>,
}

/// The message a forward copies. The IDs are the user's own view of it and go
/// only to Discord as the `message_reference`; nothing else about the source is
/// sent or kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForwardSource {
    pub channel: Snowflake,
    pub message: Snowflake,
    pub guild: Option<Snowflake>,
}

/// `message_reference`: type 0 (`DEFAULT`) for a reply, type 1 for a forward.
#[derive(Serialize)]
struct WireReference {
    #[serde(rename = "type")]
    kind: u8,
    message_id: Snowflake,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_id: Option<Snowflake>,
    #[serde(skip_serializing_if = "Option::is_none")]
    guild_id: Option<Snowflake>,
    fail_if_not_exists: bool,
}

/// How a failed send ended. Neither case is ever retried by the transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    /// Discord definitely did not create the message: nothing left this
    /// machine, or Discord answered with a refusal. The user may fix the cause
    /// and retry.
    NotSent(RestError),
    /// The request may have reached Discord and created the message (the
    /// connection or response failed, or Discord errored on its side). Show it
    /// as unconfirmed; only the user may retry, and the retry reuses the nonce.
    Ambiguous(RestError),
}

impl SendError {
    /// The transport error behind the classification.
    pub fn cause(self) -> RestError {
        match self {
            Self::NotSent(error) | Self::Ambiguous(error) => error,
        }
    }

    pub fn is_ambiguous(self) -> bool {
        matches!(self, Self::Ambiguous(_))
    }

    /// Classifies a transport failure of the `POST`.
    pub fn from_transport(error: RestError) -> Self {
        match error {
            // The request may have been processed before the answer was lost,
            // or Discord failed halfway: a message may exist. (A connect
            // timeout also lands here; it cannot be told from a read timeout.)
            RestError::Retryable(
                RetryableFailure::Server(_)
                | RetryableFailure::Network(
                    NetworkFailure::Timeout | NetworkFailure::Body | NetworkFailure::Other,
                ),
            )
            | RestError::ResponseBodyTooLarge => Self::Ambiguous(error),
            // Never connected: nothing was sent.
            RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection))
            // Refusals, and failures before anything was dispatched.
            | RestError::AccountVerificationRequired
            | RestError::NotificationWritesPaused
            | RestError::WriteNotVerified(_)
            | RestError::WritesPaused(_)
            | RestError::RateLimited
            | RestError::AuthenticationRequired
            | RestError::PermissionDenied
            | RestError::Refused(_)
            | RestError::ResourceGone
            | RestError::Http(_)
            | RestError::InvalidToken
            | RestError::InvalidRoute
            | RestError::InvalidJson
            | RestError::InvalidRateLimit
            | RestError::ClientConfiguration
            | RestError::RequestBodyTooLarge
            | RestError::SchedulerCapacityExceeded => Self::NotSent(error),
        }
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSent(error) => write!(f, "message not sent: {error}"),
            Self::Ambiguous(error) => write!(f, "message delivery unconfirmed: {error}"),
        }
    }
}

impl std::error::Error for SendError {}

fn create_route(channel: Snowflake) -> Result<Route, RestError> {
    Route::new(Method::POST, &format!("/channels/{channel}/messages"))
}

/// Decodes the created message. A success status with a body that is not the
/// message of this channel means a message probably exists that we cannot
/// identify, so it is ambiguous rather than a refusal.
pub(crate) fn parse_created(body: &[u8], channel: Snowflake) -> Result<Message, SendError> {
    serde_json::from_slice::<Message>(body)
        .ok()
        .filter(|message| message.channel_id == channel)
        .ok_or(SendError::Ambiguous(RestError::InvalidJson))
}

impl<C: Clock> RestClient<C> {
    /// Posts one message once. Returns the message Discord created (or, for a
    /// repeated nonce, the one it already had). Never retries: see [`SendError`].
    /// A `429` is the one exception inside the transport: it proves nothing was
    /// created, so the scheduler waits as told and posts again.
    pub async fn create_message(
        &self,
        channel: Snowflake,
        message: &OutgoingMessage,
    ) -> Result<Message, SendError> {
        let request = create_route(channel)
            .and_then(|route| {
                RestRequest::new(route, Priority::UserWrite)
                    .with_error_detail()
                    .json(&message.body())
            })
            .map_err(SendError::NotSent)?;
        let response = self
            .execute(request)
            .await
            .map_err(SendError::from_transport)?;
        parse_created(response.body(), channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserToken;
    use crate::gateway::{ClientProperties, HostOs};

    const REQUEST: &str = include_str!("../../../fixtures/rest/message-create-request.json");
    const RESPONSE: &str = include_str!("../../../fixtures/rest/message-create-response.json");
    const NOW_MS: u64 = 1_790_000_000_000;

    fn nonce(n: u64) -> Nonce {
        let mut generator = NonceGenerator::default();
        generator.next(NOW_MS, n)
    }

    #[test]
    fn request_body_matches_the_recorded_shape_value_for_value() {
        // The wire form is what the recorded request holds; its nonce is spelled out.
        let message = OutgoingMessage::new(
            "  hello from fastcord \u{e9} <@2>\n",
            Nonce(1_290_000_000_000_000_001),
            MentionPolicy::default(),
        )
        .unwrap();
        let sent: serde_json::Value = serde_json::to_value(message.body()).unwrap();
        let recorded: serde_json::Value = serde_json::from_str(REQUEST).unwrap();
        assert_eq!(sent, recorded);
        // The wire nonce is a string, never a JSON integer that could lose precision.
        assert!(sent["nonce"].is_string());
    }

    #[test]
    fn a_forward_names_its_source_and_carries_nothing_else() {
        let source = ForwardSource {
            channel: Snowflake(5),
            message: Snowflake(6),
            guild: Some(Snowflake(7)),
        };
        let sent = serde_json::to_value(
            OutgoingMessage::forward(source, Nonce(1_290_000_000_000_000_006)).body(),
        )
        .unwrap();
        assert_eq!(sent["content"], "");
        assert_eq!(sent["enforce_nonce"], true);
        let reference = &sent["message_reference"];
        assert_eq!(reference["type"], 1);
        assert_eq!(reference["message_id"], "6");
        assert_eq!(reference["channel_id"], "5");
        assert_eq!(reference["guild_id"], "7");
        assert!(sent.get("sticker_ids").is_none() && sent.get("attachments").is_none());
        let dm = ForwardSource {
            guild: None,
            ..source
        };
        let sent = serde_json::to_value(OutgoingMessage::forward(dm, Nonce(1)).body()).unwrap();
        assert!(sent["message_reference"].get("guild_id").is_none());
    }

    #[test]
    fn a_poll_message_has_no_text_and_carries_the_poll_with_the_nonce() {
        let draft = PollDraft::new(
            "Lunch?",
            &["Pizza".to_owned(), "Soup".to_owned()],
            24,
            false,
        )
        .unwrap();
        let message = OutgoingMessage::with_poll(draft, Nonce(1_290_000_000_000_000_007));
        let sent = serde_json::to_value(message.body()).unwrap();
        assert_eq!(sent["content"], "");
        assert_eq!(sent["enforce_nonce"], true);
        assert_eq!(sent["nonce"], "1290000000000000007");
        assert_eq!(sent["poll"]["duration"], 24);
        assert_eq!(sent["poll"]["answers"][1]["poll_media"]["text"], "Soup");
        assert!(sent.get("sticker_ids").is_none() && sent.get("message_reference").is_none());
        assert!(!format!("{message:?}").contains("Pizza"));
        let plain = OutgoingMessage::new("x", Nonce(1), MentionPolicy::default()).unwrap();
        assert!(
            serde_json::to_value(plain.body())
                .unwrap()
                .get("poll")
                .is_none()
        );
    }

    #[test]
    fn a_sticker_message_has_no_text_and_names_the_sticker() {
        let message = OutgoingMessage::with_sticker(
            Snowflake(77),
            Nonce(1_290_000_000_000_000_005),
            MentionPolicy::default(),
        );
        let sent = serde_json::to_value(message.body()).unwrap();
        assert_eq!(sent["content"], "");
        assert_eq!(sent["sticker_ids"], serde_json::json!(["77"]));
        assert_eq!(sent["enforce_nonce"], true);
        assert!(sent.get("attachments").is_none());
    }

    #[test]
    fn a_message_with_files_may_have_no_text_and_names_each_upload() {
        let file = UploadedFile {
            id: 0,
            filename: "photo.png".to_owned(),
            uploaded_filename: "token/photo.png".to_owned(),
            voice: None,
            description: String::new(),
        };
        assert_eq!(
            OutgoingMessage::with_files("  ", Vec::new(), Nonce(1), MentionPolicy::default()).err(),
            Some(ComposeError::Empty)
        );
        let message = OutgoingMessage::with_files(
            "",
            vec![file],
            Nonce(1_290_000_000_000_000_003),
            MentionPolicy::default(),
        )
        .unwrap();
        let sent = serde_json::to_value(message.body()).unwrap();
        assert_eq!(sent["content"], "");
        assert_eq!(
            sent["attachments"],
            serde_json::json!([{"id": "0", "filename": "photo.png", "uploaded_filename": "token/photo.png"}])
        );
        // Plain messages never mention attachments.
        let plain = OutgoingMessage::new("x", Nonce(2), MentionPolicy::default()).unwrap();
        assert!(
            serde_json::to_value(plain.body())
                .unwrap()
                .get("attachments")
                .is_none()
        );
        assert!(!format!("{message:?}").contains("token/photo.png"));
    }

    #[test]
    fn slash_tts_sets_the_tts_flag_and_is_removed_from_the_text() {
        let body = |text: &str| {
            let message = OutgoingMessage::new(text, Nonce(9), MentionPolicy::default()).unwrap();
            serde_json::to_value(message.body()).unwrap()
        };
        let sent = body("  /TTS  hello there");
        assert_eq!(
            (sent["tts"].as_bool(), sent["content"].as_str()),
            (Some(true), Some("hello there"))
        );
        assert_eq!(body("plain")["tts"], false);
        // Only a leading word.
        assert_eq!(body("/ttsx hi")["tts"], false);
        assert_eq!(body("say /tts")["content"], "say /tts");
        assert_eq!(body("/tts @silent hi")["flags"], 4096);
        assert_eq!(
            OutgoingMessage::new("/tts", Nonce(9), MentionPolicy::default()).err(),
            Some(ComposeError::Empty)
        );
    }

    #[test]
    fn at_silent_sets_the_flag_and_is_removed_from_the_text() {
        let body = |text: &str| {
            let message = OutgoingMessage::new(text, Nonce(9), MentionPolicy::default()).unwrap();
            serde_json::to_value(message.body()).unwrap()
        };
        let sent = body("  @SILENT  hello there");
        assert_eq!(
            (sent["flags"].as_u64(), sent["content"].as_str()),
            (Some(4096), Some("hello there"))
        );
        // Only a leading word: not a longer word, not mid-text.
        assert_eq!(body("@silently ok")["flags"], 0);
        assert_eq!(body("ok @silent")["content"], "ok @silent");
        assert_eq!(
            body(
                "@silent
line"
            )["content"],
            "line"
        );
        // Nothing left: not a message.
        assert_eq!(
            OutgoingMessage::new("@silent", Nonce(9), MentionPolicy::default()).err(),
            Some(ComposeError::Empty)
        );
        // With a file the text may be empty, and the flag still applies; a
        // description and a spoiler name reach the attachment.
        let file = UploadedFile {
            id: 0,
            filename: "SPOILER_a.png".to_owned(),
            uploaded_filename: "t/a.png".to_owned(),
            voice: None,
            description: "a cat".to_owned(),
        };
        let message =
            OutgoingMessage::with_files("@silent", vec![file], Nonce(9), MentionPolicy::default())
                .unwrap();
        let sent = serde_json::to_value(message.body()).unwrap();
        assert_eq!(sent["flags"], 4096);
        assert_eq!(sent["attachments"][0]["description"], "a cat");
        assert_eq!(sent["attachments"][0]["filename"], "SPOILER_a.png");
        assert!(!format!("{message:?}").contains("a cat"));
    }

    #[test]
    fn a_voice_message_carries_the_flag_duration_and_waveform_and_nothing_else() {
        let voice = |name: &str| UploadedFile {
            id: 0,
            filename: name.to_owned(),
            uploaded_filename: "token/voice-message.ogg".to_owned(),
            voice: Some(crate::upload::VoiceMeta {
                duration_ms: 2500,
                waveform: "AAEC".to_owned(),
            }),
            description: String::new(),
        };
        let message = OutgoingMessage::with_files(
            "",
            vec![voice("voice-message.ogg")],
            Nonce(5),
            MentionPolicy::default(),
        )
        .unwrap();
        let sent = serde_json::to_value(message.body()).unwrap();
        assert_eq!(sent["flags"], 8192);
        assert_eq!(sent["content"], "");
        assert_eq!(
            sent["attachments"],
            serde_json::json!([{
                "id": "0", "filename": "voice-message.ogg",
                "uploaded_filename": "token/voice-message.ogg",
                "duration_secs": 2.5, "waveform": "AAEC"
            }])
        );
        // Text or a second file would make it an ordinary message: refused, never downgraded.
        assert_eq!(
            OutgoingMessage::with_files(
                "hi",
                vec![voice("voice-message.ogg")],
                Nonce(5),
                MentionPolicy::default()
            )
            .err(),
            Some(ComposeError::VoiceMessageAlone)
        );
        assert_eq!(
            OutgoingMessage::with_files(
                "",
                vec![voice("a.ogg"), voice("b.ogg")],
                Nonce(5),
                MentionPolicy::default()
            )
            .err(),
            Some(ComposeError::VoiceMessageAlone)
        );
        // Ordinary uploads keep flags 0 and never mention voice fields.
        let plain = UploadedFile {
            id: 0,
            filename: "a.ogg".to_owned(),
            uploaded_filename: "t/a.ogg".to_owned(),
            voice: None,
            description: String::new(),
        };
        let sent = serde_json::to_value(
            OutgoingMessage::with_files("", vec![plain], Nonce(6), MentionPolicy::default())
                .unwrap()
                .body(),
        )
        .unwrap();
        assert_eq!(sent["flags"], 0);
        assert!(sent["attachments"][0].get("waveform").is_none());
        assert!(!format!("{message:?}").contains("AAEC"));
    }

    #[test]
    fn reply_body_matches_the_recorded_shape_and_always_states_the_author_mention() {
        const REPLY_REQUEST: &str =
            include_str!("../../../fixtures/rest/message-reply-request.json");
        let target = Snowflake(1_289_999_999_999_999_999);
        let reply = |mention_author| {
            OutgoingMessage::new(
                "replying from fastcord",
                Nonce(1_290_000_000_000_000_002),
                MentionPolicy::default(),
            )
            .unwrap()
            .replying_to(Some(ReplyTo {
                message: target,
                mention_author,
            }))
        };
        let sent = serde_json::to_value(reply(false).body()).unwrap();
        let recorded: serde_json::Value = serde_json::from_str(REPLY_REQUEST).unwrap();
        assert_eq!(sent, recorded);
        // The ID is a decimal string, and Discord must refuse the reply rather
        // than post a plain message when the target no longer exists.
        assert_eq!(
            sent["message_reference"]["message_id"],
            "1289999999999999999"
        );
        assert_eq!(sent["message_reference"]["fail_if_not_exists"], true);
        // Mentioning the author is the user's explicit choice either way.
        let pinging = serde_json::to_value(reply(true).body()).unwrap();
        assert_eq!(
            pinging["allowed_mentions"],
            serde_json::json!({"parse": ["users", "roles"], "replied_user": true})
        );
        // Without a reply the body has neither field, exactly as before.
        let plain = OutgoingMessage::new("hi", nonce(1), MentionPolicy::default())
            .unwrap()
            .replying_to(None);
        let plain = serde_json::to_value(plain.body()).unwrap();
        assert!(plain.get("message_reference").is_none());
        assert!(plain["allowed_mentions"].get("replied_user").is_none());
        // Debug shows the reply's target and choice, never the text.
        let shown = format!("{:?}", reply(true));
        assert!(shown.contains("mention_author: true") && !shown.contains("replying from"));
    }

    #[test]
    fn a_created_reply_decodes_with_its_reference_and_preview() {
        const REPLY_RESPONSE: &str =
            include_str!("../../../fixtures/rest/message-reply-response.json");
        let message = parse_created(REPLY_RESPONSE.as_bytes(), Snowflake(500)).unwrap();
        assert_eq!(message.kind, fastcord_model::REPLY_KIND);
        assert!(Nonce(1_290_000_000_000_000_002).matches(message.nonce.as_deref().unwrap()));
        assert_eq!(
            message.replied_to(),
            Some(Snowflake(1_289_999_999_999_999_999))
        );
        let fastcord_model::Referenced::Message(preview) = &message.referenced_message else {
            panic!("the created reply carries its target");
        };
        assert_eq!(preview.author.display_name(), "other_fixture");
        assert_eq!(preview.content, "the message being replied to");
    }

    #[test]
    fn mention_policy_is_always_sent_in_full() {
        let parse = |policy: MentionPolicy| {
            let message = OutgoingMessage::new("hi", nonce(1), policy).unwrap();
            serde_json::to_value(message.body()).unwrap()["allowed_mentions"].clone()
        };
        assert_eq!(
            parse(MentionPolicy::default()),
            serde_json::json!({"parse": ["users", "roles"]}),
            "the default never pings everyone"
        );
        assert_eq!(
            parse(MentionPolicy {
                users: false,
                roles: false,
                everyone: false
            }),
            serde_json::json!({"parse": []}),
            "an empty list explicitly allows nothing; the field is never omitted"
        );
        assert_eq!(
            parse(MentionPolicy {
                users: true,
                roles: false,
                everyone: true
            }),
            serde_json::json!({"parse": ["users", "everyone"]})
        );
    }

    #[test]
    fn drafts_are_trimmed_and_bounded_before_anything_is_sent() {
        let policy = MentionPolicy::default();
        assert_eq!(
            OutgoingMessage::new(" \n\t ", nonce(1), policy),
            Err(ComposeError::Empty)
        );
        assert_eq!(
            OutgoingMessage::new("", nonce(1), policy),
            Err(ComposeError::Empty)
        );
        let at_limit = "\u{e9}".repeat(MAX_CONTENT_CHARS);
        assert!(OutgoingMessage::new(&at_limit, nonce(1), policy).is_ok());
        let over = format!("{at_limit}x");
        assert_eq!(
            OutgoingMessage::new(&over, nonce(1), policy),
            Err(ComposeError::TooLong),
            "characters, not bytes, are counted"
        );
        let inner = OutgoingMessage::new("  a \n b  ", nonce(1), policy).unwrap();
        assert_eq!(inner.content, "a \n b", "only the ends are trimmed");
        // Debug names the size, never the text.
        let shown = format!("{inner:?}");
        assert!(!shown.contains(" b") && shown.contains("content_chars: 5"));
    }

    #[test]
    fn nonces_are_unique_monotonic_snowflake_text_that_fit_discords_limit() {
        let mut generator = NonceGenerator::default();
        let first = generator.next(NOW_MS, 0x12_3456);
        // The same millisecond with the same entropy cannot repeat a nonce.
        let second = generator.next(NOW_MS, 0x12_3456);
        assert!(second.0 > first.0);
        // A clock stepping backwards cannot repeat or reorder them either.
        let third = generator.next(NOW_MS - 10_000, 0);
        assert!(third.0 > second.0);
        assert_eq!(
            first.0 >> NONCE_ENTROPY_BITS,
            NOW_MS - DISCORD_EPOCH_MS,
            "the time is in the snowflake position"
        );
        assert_eq!(first.0 & NONCE_ENTROPY_MASK, 0x12_3456);
        for nonce in [first, second, third] {
            let text = nonce.to_string();
            assert!(text.len() <= 19 && text.len() <= 25, "{text}");
            assert!(nonce.matches(&text));
        }
        // Entropy outside its 22 bits cannot leak into the timestamp.
        assert_eq!(
            NonceGenerator::default().next(NOW_MS, u64::MAX).0 >> NONCE_ENTROPY_BITS,
            NOW_MS - DISCORD_EPOCH_MS
        );
        // A saturated generator stays well-defined.
        let mut saturated = NonceGenerator { last: u64::MAX };
        assert_eq!(saturated.next(NOW_MS, 1).0, u64::MAX);
        // The real clock and entropy produce distinct values.
        let mut live = NonceGenerator::default();
        let a = live.next_now();
        let b = live.next_now();
        assert_ne!(a, b);
    }

    #[test]
    fn nonce_matches_only_its_exact_decimal_text() {
        let nonce = Nonce(1_290_000_000_000_000_001);
        assert!(nonce.matches("1290000000000000001"));
        for other in [
            "1290000000000000002",
            "",
            " 1290000000000000001",
            "1290000000000000001 ",
            "+1290000000000000001",
            "01290000000000000001",
            "0x11",
            "abc",
        ] {
            assert!(!nonce.matches(other), "{other:?}");
        }
    }

    #[test]
    fn failures_are_classified_by_whether_a_message_may_exist() {
        use RestError::*;
        let server = |code: u16| {
            Retryable(RetryableFailure::Server(
                reqwest::StatusCode::from_u16(code).unwrap(),
            ))
        };
        let network = |kind| Retryable(RetryableFailure::Network(kind));
        // May exist: the answer could simply have been lost.
        for ambiguous in [
            network(NetworkFailure::Timeout),
            network(NetworkFailure::Body),
            network(NetworkFailure::Other),
            server(500),
            server(502),
            server(503),
            server(504),
            ResponseBodyTooLarge,
        ] {
            assert_eq!(
                SendError::from_transport(ambiguous),
                SendError::Ambiguous(ambiguous),
                "{ambiguous:?}"
            );
        }
        // Does not exist.
        for refused in [
            network(NetworkFailure::Connection),
            AuthenticationRequired,
            PermissionDenied,
            ResourceGone,
            Http(reqwest::StatusCode::BAD_REQUEST),
            Http(reqwest::StatusCode::PAYLOAD_TOO_LARGE),
            InvalidToken,
            InvalidRoute,
            InvalidJson,
            InvalidRateLimit,
            ClientConfiguration,
            RequestBodyTooLarge,
            SchedulerCapacityExceeded,
        ] {
            assert_eq!(
                SendError::from_transport(refused),
                SendError::NotSent(refused),
                "{refused:?}"
            );
        }
        let error = SendError::Ambiguous(server(502));
        assert!(error.is_ambiguous());
        assert_eq!(error.cause(), server(502));
        assert!(!SendError::NotSent(PermissionDenied).is_ambiguous());
        assert!(error.to_string().contains("unconfirmed"));
    }

    #[test]
    fn created_message_decodes_with_its_nonce_and_must_belong_to_the_channel() {
        let message = parse_created(RESPONSE.as_bytes(), Snowflake(500)).unwrap();
        assert_eq!(message.id, Snowflake(1_290_000_000_000_000_099));
        assert_eq!(message.content, "hello from fastcord \u{e9} <@2>");
        assert_eq!(message.nonce.as_deref(), Some("1290000000000000001"));
        assert!(Nonce(1_290_000_000_000_000_001).matches(message.nonce.as_deref().unwrap()));
        // A success status with a body we cannot trust is ambiguous, not a refusal.
        for (body, channel) in [
            (RESPONSE.as_bytes(), Snowflake(501)),
            (b"{}".as_slice(), Snowflake(500)),
            (b"not json".as_slice(), Snowflake(500)),
            (b"".as_slice(), Snowflake(500)),
        ] {
            assert_eq!(
                parse_created(body, channel),
                Err(SendError::Ambiguous(RestError::InvalidJson))
            );
        }
    }

    #[test]
    fn route_is_the_channel_post_with_the_channel_as_major_parameter() {
        let route = create_route(Snowflake(500)).unwrap();
        assert_eq!(route.key().method(), &Method::POST);
        assert_eq!(route.key().major().channel_id(), Some(Snowflake(500)));
        assert_eq!(
            route.url.as_str(),
            "https://discord.com/api/v10/channels/500/messages"
        );
    }

    #[tokio::test]
    async fn a_stopped_account_never_posts_and_reports_not_sent() {
        let properties = ClientProperties::web(HostOs::Linux, "en-US", 1);
        let client = RestClient::new(
            UserToken::new("offline-test-credential".to_owned()),
            properties,
        )
        .unwrap();
        client.stop_authenticated_work();
        let message = OutgoingMessage::new("hello", nonce(7), MentionPolicy::default()).unwrap();
        assert_eq!(
            client.create_message(Snowflake(500), &message).await,
            Err(SendError::NotSent(RestError::AuthenticationRequired))
        );
    }

    #[test]
    fn unicode_sequences_and_custom_tokens_round_trip_through_the_real_post_body() {
        let source = "👩🏽‍💻 👍🏻 🇺🇦 ❤️ <:sample:12345> <a:party:45678>";
        let message = OutgoingMessage::new(source, nonce(15), MentionPolicy::default()).unwrap();
        let body = serde_json::to_value(message.body()).unwrap();
        assert_eq!(body["content"], source);
        let encoded = serde_json::to_string(&body).unwrap();
        let decoded: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded["content"], source);
    }
}
