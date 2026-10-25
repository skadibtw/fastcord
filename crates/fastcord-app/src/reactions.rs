//! Reactions the user asked for that Discord has not answered yet (SPEC §5.2, §9).
//!
//! The message store holds only what Discord reported: a count per emoji and
//! whether the user is among those who reacted. Adding or removing one's own
//! reaction is one explicit request about one (message, emoji). At most one
//! request per target is in flight, so an add and a remove of the same
//! reaction can never race each other to Discord, and nothing is ever repeated
//! by the program: a failure is shown once and the user may click again.
//!
//! An unfinished request is only an overlay for its message's row: the chip is
//! drawn as if Discord had accepted it, and disabled until Discord answers. The
//! store changes only when Discord (REST answer or Gateway event, whichever
//! comes first) reports the change, so a failed request leaves nothing to undo.
//!
//! At most [`MAX_PENDING`] requests exist, each holding one emoji (at most
//! `MAX_REACTION_EMOJI_NAME_BYTES` bytes of name), so the ledger is bounded
//! however many reactions the user clicks while Discord is unreachable.

use fastcord_discord::{NetworkFailure, RestError, RetryableFailure};
use fastcord_model::{Emoji, Snowflake};

/// Unfinished requests across all channels.
pub const MAX_PENDING: usize = 16;

pub const BUSY: &str = "That reaction is still being saved.";
pub const FULL: &str = "Too many reactions are still being saved. Try again in a moment.";

/// Names listed when the user asks who reacted with an emoji. One more is
/// requested, to tell whether Discord has others.
pub const MAX_USERS: usize = 25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// React with the emoji (a normal reaction).
    Add,
    /// Take the user's own reaction with the emoji back.
    Remove,
}

/// A request on its way to Discord, as its message's row shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub emoji: Emoji,
    pub action: Action,
}

struct Op {
    channel: Snowflake,
    message: Snowflake,
    pending: Pending,
}

impl Op {
    fn is(&self, channel: Snowflake, message: Snowflake, emoji: &Emoji) -> bool {
        self.channel == channel
            && self.message == message
            && self.pending.emoji.same_reaction(emoji)
    }
}

/// A fixed, secret-free sentence for a request Discord did not accept.
pub fn failure(action: Action, error: RestError) -> &'static str {
    let adding = action == Action::Add;
    match error {
        RestError::PermissionDenied if adding => crate::i18n::tr(
            "Discord did not allow this reaction. You may lack permission to add reactions here.",
        ),
        RestError::PermissionDenied => {
            crate::i18n::tr("Discord did not allow removing this reaction.")
        }
        RestError::ResourceGone => crate::i18n::tr("That message or emoji is no longer available."),
        RestError::Http(status) if adding && status.as_u16() == 400 => crate::i18n::tr(
            "Discord rejected the reaction. The message may already have the most different reactions it allows.",
        ),
        RestError::Http(_) if adding => crate::i18n::tr("Discord rejected the reaction."),
        RestError::Http(_) => crate::i18n::tr("Discord rejected removing the reaction."),
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)) => {
            crate::i18n::tr("Could not reach Discord; the reaction was not changed.")
        }
        RestError::Retryable(_) | RestError::ResponseBodyTooLarge | RestError::InvalidJson => {
            crate::i18n::tr(
                "Discord did not confirm the reaction change; it may have been applied.",
            )
        }
        RestError::WriteNotVerified(_) => crate::i18n::tr(fastcord_discord::WRITE_NOT_VERIFIED),
        _ => crate::i18n::tr("The reaction could not be changed."),
    }
}

/// A fixed, secret-free sentence for a list of who reacted that did not load.
pub fn users_failure(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied => {
            crate::i18n::tr("Discord did not allow listing who reacted.")
        }
        RestError::ResourceGone => {
            crate::i18n::tr("That reaction or message is no longer available.")
        }
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)) => {
            crate::i18n::tr("Could not reach Discord to list who reacted.")
        }
        _ => crate::i18n::tr("Who reacted could not be loaded."),
    }
}

/// The user's unfinished reaction requests.
#[derive(Default)]
pub struct Reactions {
    ops: Vec<Op>,
}

impl Reactions {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Records a request as being made. A target that already has one in
    /// flight, or a full ledger, refuses with the sentence to show.
    pub fn begin(
        &mut self,
        channel: Snowflake,
        message: Snowflake,
        emoji: &Emoji,
        action: Action,
    ) -> Result<(), &'static str> {
        if self.ops.iter().any(|op| op.is(channel, message, emoji)) {
            return Err(BUSY);
        }
        if self.ops.len() >= MAX_PENDING {
            return Err(FULL);
        }
        self.ops.push(Op {
            channel,
            message,
            pending: Pending {
                emoji: emoji.clone(),
                action,
            },
        });
        Ok(())
    }

    /// Discord answered (or the request ended). Returns whether one was in flight.
    pub fn finish(&mut self, channel: Snowflake, message: Snowflake, emoji: &Emoji) -> bool {
        let before = self.ops.len();
        self.ops.retain(|op| !op.is(channel, message, emoji));
        self.ops.len() != before
    }

    /// The message's unfinished requests, for its row. Allocates only when it
    /// has some.
    pub fn of(&self, channel: Snowflake, message: Snowflake) -> Vec<Pending> {
        self.ops
            .iter()
            .filter(|op| op.channel == channel && op.message == message)
            .map(|op| op.pending.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use fastcord_discord::StatusCode;

    use super::*;

    const CHANNEL: Snowflake = Snowflake(500);
    const MESSAGE: Snowflake = Snowflake(7);

    fn unicode(name: &str) -> Emoji {
        Emoji {
            id: None,
            name: Some(name.to_owned()),
            animated: false,
        }
    }

    fn custom(id: u64, name: &str) -> Emoji {
        Emoji {
            id: Some(Snowflake(id)),
            name: Some(name.to_owned()),
            animated: false,
        }
    }

    #[test]
    fn one_request_per_target_and_it_leaves_when_answered() {
        let mut ledger = Reactions::default();
        let thumbs = unicode("\u{1f44d}");
        assert_eq!(ledger.begin(CHANNEL, MESSAGE, &thumbs, Action::Add), Ok(()));
        // The same target is busy whatever the action; a renamed custom emoji
        // is still the same target.
        assert_eq!(
            ledger.begin(CHANNEL, MESSAGE, &thumbs, Action::Remove),
            Err(BUSY)
        );
        assert_eq!(
            ledger.begin(CHANNEL, MESSAGE, &custom(9, "old"), Action::Add),
            Ok(())
        );
        assert_eq!(
            ledger.begin(CHANNEL, MESSAGE, &custom(9, "renamed"), Action::Add),
            Err(BUSY)
        );
        // Other messages and channels are independent.
        assert_eq!(
            ledger.begin(CHANNEL, Snowflake(8), &thumbs, Action::Add),
            Ok(())
        );
        assert_eq!(
            ledger.begin(Snowflake(501), MESSAGE, &thumbs, Action::Add),
            Ok(())
        );
        assert_eq!(ledger.of(CHANNEL, MESSAGE).len(), 2);
        assert!(ledger.finish(CHANNEL, MESSAGE, &thumbs));
        assert!(!ledger.finish(CHANNEL, MESSAGE, &thumbs), "once");
        assert_eq!(
            ledger.of(CHANNEL, MESSAGE),
            [Pending {
                emoji: custom(9, "old"),
                action: Action::Add
            }]
        );
        assert_eq!(
            ledger.begin(CHANNEL, MESSAGE, &thumbs, Action::Remove),
            Ok(())
        );
    }

    #[test]
    fn the_ledger_is_bounded_however_many_reactions_are_clicked() {
        let mut ledger = Reactions::default();
        for id in 0..MAX_PENDING as u64 {
            assert_eq!(
                ledger.begin(CHANNEL, Snowflake(id), &unicode("\u{1f44d}"), Action::Add),
                Ok(())
            );
        }
        assert_eq!(
            ledger.begin(CHANNEL, Snowflake(99), &unicode("\u{1f44d}"), Action::Add),
            Err(FULL)
        );
        assert_eq!(ledger.len(), MAX_PENDING);
        // Rows of messages without requests allocate nothing.
        assert!(ledger.of(CHANNEL, Snowflake(99)).is_empty());
        assert!(ledger.finish(CHANNEL, Snowflake(0), &unicode("\u{1f44d}")));
        assert_eq!(
            ledger.begin(CHANNEL, Snowflake(99), &unicode("\u{1f44d}"), Action::Add),
            Ok(())
        );
    }

    #[test]
    fn failures_have_fixed_secret_free_sentences() {
        let status = |code| RestError::Http(StatusCode::from_u16(code).unwrap());
        let unconfirmed = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout));
        let unreachable =
            RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection));
        for action in [Action::Add, Action::Remove] {
            for error in [
                RestError::PermissionDenied,
                RestError::ResourceGone,
                status(400),
                status(500),
                unconfirmed,
                unreachable,
                RestError::InvalidJson,
                RestError::InvalidRoute,
            ] {
                let sentence = failure(action, error);
                assert!(sentence.ends_with('.'), "{sentence}");
                assert!(!sentence.contains("http"), "{sentence}");
            }
        }
        assert!(failure(Action::Add, RestError::PermissionDenied).contains("permission"));
        assert!(failure(Action::Add, status(400)).contains("different reactions"));
        assert!(failure(Action::Remove, status(400)).contains("removing"));
        assert!(failure(Action::Add, unreachable).contains("not changed"));
        assert!(failure(Action::Add, unconfirmed).contains("may have been applied"));
    }
}
