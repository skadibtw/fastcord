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
    ComposeError, MentionPolicy, NetworkFailure, Nonce, OutgoingMessage, RestError,
    RetryableFailure, SendError,
};
use fastcord_model::Snowflake;

/// Unconfirmed messages, across all channels, the composer accepts before the
/// user has to retry or discard some.
pub const MAX_OUTBOX: usize = 16;

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
    phase: Phase,
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
    pub state: OutboxState,
}

impl fmt::Debug for OutboxItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboxItem")
            .field("id", &self.id)
            .field("channel_id", &self.channel_id)
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
fn reason(error: SendError) -> &'static str {
    match error {
        SendError::NotSent(error) => match error {
            RestError::PermissionDenied => "Discord did not allow you to send messages here.",
            RestError::ResourceGone => "This channel is no longer available.",
            RestError::AuthenticationRequired => "Discord no longer accepts this login.",
            RestError::Http(status) if status.as_u16() == 400 => {
                "Discord rejected the message; it may be too long or empty."
            }
            RestError::Http(_) => "Discord rejected the message.",
            RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)) => {
                "Could not reach Discord; the message was not sent."
            }
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

fn compose_reason(error: ComposeError) -> &'static str {
    match error {
        ComposeError::Empty => "A message needs some text.",
        ComposeError::TooLong => "The message is too long for Discord.",
    }
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
    pub fn enqueue(
        &mut self,
        channel: Snowflake,
        content: &str,
        mentions: MentionPolicy,
        nonce: Nonce,
        allowed: bool,
    ) -> OpId {
        self.next_id += 1;
        let id = OpId(self.next_id);
        self.ops.push_back(Op {
            id,
            channel,
            content: Arc::from(content),
            nonce,
            mentions,
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
            match OutgoingMessage::new(&op.content, op.nonce, op.mentions) {
                Ok(message) => {
                    op.phase = Phase::InFlight;
                    busy.push(op.channel);
                    ready.push((op.id, op.channel, message));
                }
                Err(error) => op.phase = Phase::Failed(compose_reason(error)),
            }
        }
        ready
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
                self.ops[at].phase = if error.is_ambiguous() {
                    Phase::Uncertain(reason(error))
                } else {
                    Phase::Failed(reason(error))
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
                .enqueue(channel, text, MentionPolicy::default(), nonce, true)
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
        let id = outbox.enqueue(A, "typed text", MentionPolicy::default(), nonce, false);
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
            let text = reason(error);
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
            ids.push(outbox.enqueue(A, "x", MentionPolicy::default(), nonce, true));
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
}
