//! Creating messages (`POST /channels/{id}/messages`) with a stable nonce and an
//! explicit mention policy (SPEC §5.1, §5.2).
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
    fn parse(self) -> Vec<&'static str> {
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

/// Why a draft cannot be sent at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposeError {
    /// Nothing but whitespace.
    Empty,
    /// Longer than [`MAX_CONTENT_CHARS`].
    TooLong,
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "a message needs some text",
            Self::TooLong => "the message is too long for Discord",
        })
    }
}

impl std::error::Error for ComposeError {}

/// A validated message ready to send. `Debug` never prints the text.
#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    content: String,
    nonce: Nonce,
    mentions: MentionPolicy,
}

impl OutgoingMessage {
    /// Surrounding whitespace is trimmed, as Discord does; interior text is sent as written.
    pub fn new(content: &str, nonce: Nonce, mentions: MentionPolicy) -> Result<Self, ComposeError> {
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
        })
    }

    pub fn nonce(&self) -> Nonce {
        self.nonce
    }

    fn body(&self) -> CreateMessage<'_> {
        CreateMessage {
            content: &self.content,
            nonce: self.nonce,
            enforce_nonce: true,
            tts: false,
            flags: 0,
            allowed_mentions: AllowedMentions {
                parse: self.mentions.parse(),
            },
        }
    }
}

impl fmt::Debug for OutgoingMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutgoingMessage")
            .field("nonce", &self.nonce)
            .field("content_chars", &self.content.chars().count())
            .field("mentions", &self.mentions)
            .finish()
    }
}

#[derive(Serialize)]
struct CreateMessage<'a> {
    content: &'a str,
    nonce: Nonce,
    enforce_nonce: bool,
    tts: bool,
    flags: u8,
    allowed_mentions: AllowedMentions,
}

#[derive(Serialize)]
struct AllowedMentions {
    parse: Vec<&'static str>,
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
            | RestError::AuthenticationRequired
            | RestError::PermissionDenied
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
            .and_then(|route| RestRequest::new(route, Priority::UserWrite).json(&message.body()))
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
        let client = RestClient::new(UserToken::new("offline-test-credential".to_owned())).unwrap();
        client.stop_authenticated_work();
        let message = OutgoingMessage::new("hello", nonce(7), MentionPolicy::default()).unwrap();
        assert_eq!(
            client.create_message(Snowflake(500), &message).await,
            Err(SendError::NotSent(RestError::AuthenticationRequired))
        );
    }
}
