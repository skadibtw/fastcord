//! Poll requests the user made that Discord has not answered yet (SPEC §9).
//!
//! The message store holds only what Discord reported: counts per answer and
//! whether the account voted. A vote request sets the account's complete
//! selection for one poll, an end request closes the user's own poll. At most
//! one request per poll is in flight, so two clicks can never race each other
//! to Discord, and nothing is repeated by the program: a failure is shown once
//! and the user may click again (setting the same selection twice is the same
//! as setting it once, so a retry after an ambiguous failure cannot double a
//! vote). The store changes only when Discord reports it, by the REST answer or
//! the Gateway event, whichever comes first, so a failure leaves nothing to
//! undo. The ledger holds at most [`MAX_PENDING`] small entries.

use fastcord_discord::{NetworkFailure, RestError, RetryableFailure};
use fastcord_model::Snowflake;

/// Unfinished requests across all channels.
pub const MAX_PENDING: usize = 8;
/// Names listed when the user asks who voted for an answer.
pub const MAX_VOTERS: usize = 25;

pub const BUSY: &str = "That poll is still being updated.";
pub const FULL: &str = "Too many poll changes are still being saved. Try again in a moment.";
pub const ENDED: &str = "This poll has ended.";
pub const NO_POLL: &str = "That message has no poll.";
pub const NOT_YOURS: &str = "Only the person who made a poll can end it.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Make the account's votes exactly these answers (empty withdraws).
    Vote(Vec<u32>),
    /// End the poll now.
    End,
}

struct Op {
    channel: Snowflake,
    message: Snowflake,
}

/// A fixed, secret-free sentence for a request Discord did not accept.
pub fn failure(kind: &Kind, error: RestError) -> &'static str {
    let ending = matches!(kind, Kind::End);
    match error {
        RestError::PermissionDenied if ending => "Discord did not allow ending this poll.",
        RestError::PermissionDenied => "Discord did not allow voting in this poll.",
        RestError::ResourceGone => "That poll is no longer available.",
        RestError::Http(status) if status.as_u16() == 400 => {
            "Discord rejected the request. The poll may have ended."
        }
        RestError::Http(_) if ending => "Discord rejected ending the poll.",
        RestError::Http(_) => "Discord rejected the vote.",
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            "Discord is rate limiting this. Try again shortly."
        }
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)) => {
            "Could not reach Discord; the poll was not changed."
        }
        RestError::Retryable(_) | RestError::ResponseBodyTooLarge | RestError::InvalidJson => {
            "Discord did not confirm the change; it may have been applied. Check the poll before trying again."
        }
        RestError::WriteNotVerified(_) => fastcord_discord::WRITE_NOT_VERIFIED,
        _ => "The poll could not be changed.",
    }
}

/// A fixed, secret-free sentence for a voter list that did not load.
pub fn voters_failure(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied => "Discord did not allow listing the voters.",
        RestError::ResourceGone => "That poll is no longer available.",
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)) => {
            "Could not reach Discord to list the voters."
        }
        _ => "The voters could not be loaded.",
    }
}

/// The user's unfinished poll requests.
#[derive(Default)]
pub struct Polls {
    ops: Vec<Op>,
}

impl Polls {
    /// Records a request as being made. A poll that already has one in flight,
    /// or a full ledger, refuses with the sentence to show.
    pub fn begin(&mut self, channel: Snowflake, message: Snowflake) -> Result<(), &'static str> {
        if self.busy(channel, message) {
            return Err(BUSY);
        }
        if self.ops.len() >= MAX_PENDING {
            return Err(FULL);
        }
        self.ops.push(Op { channel, message });
        Ok(())
    }

    /// Discord answered (or the request ended).
    pub fn finish(&mut self, channel: Snowflake, message: Snowflake) {
        self.ops
            .retain(|op| !(op.channel == channel && op.message == message));
    }

    pub fn busy(&self, channel: Snowflake, message: Snowflake) -> bool {
        self.ops
            .iter()
            .any(|op| op.channel == channel && op.message == message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_request_per_poll_and_the_ledger_is_bounded() {
        let mut polls = Polls::default();
        let (c, m) = (Snowflake(1), Snowflake(2));
        assert_eq!(polls.begin(c, m), Ok(()));
        assert_eq!(polls.begin(c, m), Err(BUSY));
        assert!(polls.busy(c, m) && !polls.busy(c, Snowflake(3)));
        polls.finish(c, m);
        assert!(!polls.busy(c, m));
        for n in 0..MAX_PENDING as u64 {
            assert_eq!(polls.begin(c, Snowflake(10 + n)), Ok(()));
        }
        assert_eq!(polls.begin(c, Snowflake(99)), Err(FULL));
    }

    #[test]
    fn failures_are_fixed_sentences_that_separate_unreached_from_unconfirmed() {
        let vote = Kind::Vote(vec![2]);
        let unreached = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection));
        assert!(failure(&vote, unreached).contains("was not changed"));
        let lost = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout));
        assert!(failure(&vote, lost).contains("may have been applied"));
        assert!(failure(&Kind::End, RestError::PermissionDenied).contains("ending"));
        assert!(failure(&vote, RestError::PermissionDenied).contains("voting"));
        assert!(failure(&vote, RestError::ResourceGone).contains("no longer"));
        assert!(voters_failure(RestError::ResourceGone).contains("no longer"));
    }
}
