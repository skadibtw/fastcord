//! Interactions the user started by hand and Discord has not settled yet
//! (SPEC §9): a button press, a select choice, a slash command, a modal
//! answer.
//!
//! One click sends one request, never repeated by the program. The REST answer
//! (`204`) only says Discord took the request; the outcome is the Gateway's
//! `INTERACTION_SUCCESS` or `INTERACTION_FAILURE` for the request's nonce, or
//! nothing, in which case [`TIMEOUT`] ends the wait with "The application did
//! not respond". A late outcome of a settled nonce is ignored. The ledger
//! holds at most [`MAX_PENDING`] small entries; a message has at most one
//! pending component and a channel at most one pending command.

use std::time::Duration;

use fastcord_discord::{
    AppCommand, ArgError, CommandArg, NetworkFailure, Nonce, RestError, RetryableFailure,
};
use fastcord_model::Snowflake;

/// How long Discord has to settle an interaction.
pub const TIMEOUT: Duration = Duration::from_secs(15);
/// Unsettled interactions across all channels.
pub const MAX_PENDING: usize = 8;

pub const NO_RESPONSE: &str = "The application did not respond.";
pub const FAILED: &str = "The application could not handle that.";
pub const BUSY: &str = "That is still waiting for the application.";
pub const COMMAND_BUSY: &str = "A command is still waiting for the application in this channel.";
pub const FULL: &str = "Too many requests are waiting for applications. Try again in a moment.";
pub const NOT_READY: &str = "Not connected yet; try again in a moment.";
pub const NOT_ALLOWED: &str = "You cannot use applications in this channel.";
pub const STALE: &str = "That control is no longer on the message.";
pub const DISABLED: &str = "That control is disabled.";
pub const PREMIUM: &str = "Not available in this client.";
pub const BAD_CHOICE: &str = "That choice is not offered by the select.";
pub const NO_MODAL: &str = "That form is no longer open.";
pub const NOT_EPHEMERAL: &str = "Only messages meant for you can be dismissed.";

/// An explicit user action on an interaction, validated by the worker against
/// its own state and never against what a possibly stale UI showed.
#[derive(Clone, PartialEq)]
pub enum Action {
    /// A button press (`values` empty) or a select choice on a message.
    Component {
        channel: Snowflake,
        message: Snowflake,
        custom_id: String,
        values: Vec<String>,
    },
    /// Run a slash command, optionally through one subcommand.
    Command {
        channel: Snowflake,
        command: Box<AppCommand>,
        subcommand: Option<String>,
        args: Vec<CommandArg>,
    },
    /// Submit the open modal; one answer per input.
    Modal { values: Vec<String> },
    /// Close the open modal without answering.
    CloseModal,
    /// Remove a message meant only for the user from this client's view. Local
    /// only: nothing is sent.
    Dismiss {
        channel: Snowflake,
        message: Snowflake,
    },
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Component { .. } => "Action::Component",
            Self::Command { .. } => "Action::Command",
            Self::Modal { .. } => "Action::Modal",
            Self::CloseModal => "Action::CloseModal",
            Self::Dismiss { .. } => "Action::Dismiss",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Component {
        message: Snowflake,
        custom_id: String,
    },
    /// A slash command, by name.
    Command(String),
    Modal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub nonce: Nonce,
    pub channel: Snowflake,
    pub target: Target,
}

/// The unsettled interactions.
#[derive(Default)]
pub struct Ledger {
    entries: Vec<Entry>,
}

impl Ledger {
    /// Records an interaction as being made, or refuses with the sentence to
    /// show.
    pub fn begin(&mut self, entry: Entry) -> Result<(), &'static str> {
        let busy = self.entries.iter().any(|held| {
            held.channel == entry.channel
                && match (&held.target, &entry.target) {
                    (
                        Target::Component { message: a, .. },
                        Target::Component { message: b, .. },
                    ) => a == b,
                    (Target::Command(_), Target::Command(_)) | (Target::Modal, Target::Modal) => {
                        true
                    }
                    _ => false,
                }
        });
        if busy {
            return Err(if matches!(entry.target, Target::Command(_)) {
                COMMAND_BUSY
            } else {
                BUSY
            });
        }
        if self.entries.len() >= MAX_PENDING {
            return Err(FULL);
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Takes the entry Discord's outcome (or the timeout) is about.
    pub fn settle(&mut self, nonce: &str) -> Option<Entry> {
        let at = self
            .entries
            .iter()
            .position(|entry| entry.nonce.matches(nonce))?;
        Some(self.entries.remove(at))
    }

    pub fn settle_nonce(&mut self, nonce: Nonce) -> Option<Entry> {
        self.settle(&nonce.to_string())
    }

    /// Whether `nonce` is the answer to a modal.
    pub fn is_modal(&self, nonce: Nonce) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.nonce == nonce && entry.target == Target::Modal)
    }

    /// The `custom_id` of the component of `message` that is waiting.
    pub fn component(&self, channel: Snowflake, message: Snowflake) -> Option<&str> {
        self.entries.iter().find_map(|entry| match &entry.target {
            Target::Component {
                message: held,
                custom_id,
            } if entry.channel == channel && *held == message => Some(custom_id.as_str()),
            _ => None,
        })
    }

    /// The command waiting in `channel`.
    pub fn command(&self, channel: Snowflake) -> Option<&str> {
        self.entries.iter().find_map(|entry| match &entry.target {
            Target::Command(name) if entry.channel == channel => Some(name.as_str()),
            _ => None,
        })
    }

    pub fn modal_waiting(&self, channel: Snowflake) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.channel == channel && entry.target == Target::Modal)
    }

    /// The newest entry's nonce.
    #[cfg(test)]
    pub fn last_nonce(&self) -> Option<Nonce> {
        self.entries.last().map(|entry| entry.nonce)
    }

    /// Puts a button entry back, as if it had just been sent.
    #[cfg(test)]
    pub fn restore_for_test(
        &mut self,
        nonce: Nonce,
        channel: Snowflake,
        message: Snowflake,
        custom_id: &str,
    ) {
        self.entries.clear();
        self.entries.push(Entry {
            nonce,
            channel,
            target: Target::Component {
                message,
                custom_id: custom_id.to_owned(),
            },
        });
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// A fixed, secret-free sentence for a request Discord did not accept.
pub fn failure(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied => {
            crate::i18n::tr("Discord did not allow that. You may lack the permission.")
        }
        RestError::ResourceGone => {
            crate::i18n::tr("That message, command or application is no longer available.")
        }
        RestError::Http(status) if status.as_u16() == 400 => {
            crate::i18n::tr("Discord rejected the request. The control may be out of date.")
        }
        RestError::Http(_) => crate::i18n::tr("Discord rejected the request."),
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            crate::i18n::tr("Discord is rate limiting this. Try again shortly.")
        }
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)) => {
            crate::i18n::tr("Could not reach Discord; nothing was sent.")
        }
        RestError::Retryable(_) | RestError::ResponseBodyTooLarge | RestError::InvalidJson => {
            crate::i18n::tr(
                "Discord did not confirm the request; it may have been received. Check the conversation before trying again.",
            )
        }
        RestError::WriteNotVerified(_) => crate::i18n::tr(fastcord_discord::WRITE_NOT_VERIFIED),
        _ => crate::i18n::tr("The request could not be sent."),
    }
}

/// A fixed sentence for a typed option that cannot be sent.
pub fn argument(error: ArgError) -> &'static str {
    match error {
        ArgError::Missing => crate::i18n::tr("A required option is empty."),
        ArgError::NotAnInteger => crate::i18n::tr("An option needs a whole number."),
        ArgError::NotANumber => crate::i18n::tr("An option needs a number."),
        ArgError::NotYesOrNo => crate::i18n::tr("An option needs yes or no."),
        ArgError::NotAnId => {
            crate::i18n::tr("A user, channel or role option needs an ID or a mention.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One stable nonce per `n` for this test thread (nonces come from one
    /// process-wide sequence, so they are drawn once and remembered).
    fn nonce(n: u64) -> Nonce {
        thread_local! {
            static DRAWN: std::cell::RefCell<std::collections::HashMap<u64, Nonce>> =
                Default::default();
        }
        DRAWN.with_borrow_mut(|drawn| {
            *drawn
                .entry(n)
                .or_insert_with(|| fastcord_discord::NonceGenerator.next(1_800_000_000_000 + n))
        })
    }

    fn button(message: u64, custom_id: &str, n: u64) -> Entry {
        Entry {
            nonce: nonce(n),
            channel: Snowflake(5),
            target: Target::Component {
                message: Snowflake(message),
                custom_id: custom_id.to_owned(),
            },
        }
    }

    #[test]
    fn one_pending_control_per_message_one_command_per_channel_and_a_bounded_ledger() {
        let mut ledger = Ledger::default();
        assert_eq!(ledger.begin(button(7, "a", 1)), Ok(()));
        assert_eq!(ledger.begin(button(7, "b", 2)), Err(BUSY));
        assert_eq!(ledger.begin(button(8, "a", 3)), Ok(()));
        assert_eq!(ledger.component(Snowflake(5), Snowflake(7)), Some("a"));
        assert_eq!(ledger.component(Snowflake(6), Snowflake(7)), None);
        let command = |n| Entry {
            nonce: nonce(n),
            channel: Snowflake(5),
            target: Target::Command("roll".into()),
        };
        assert_eq!(ledger.begin(command(4)), Ok(()));
        assert_eq!(ledger.begin(command(5)), Err(COMMAND_BUSY));
        assert_eq!(ledger.command(Snowflake(5)), Some("roll"));
        let modal = Entry {
            nonce: nonce(6),
            channel: Snowflake(5),
            target: Target::Modal,
        };
        assert_eq!(ledger.begin(modal), Ok(()));
        assert!(ledger.modal_waiting(Snowflake(5)));
        for message in 20..30 {
            let _ = ledger.begin(button(message, "x", message));
        }
        assert_eq!(ledger.begin(button(99, "x", 99)), Err(FULL));
        assert!(ledger.entries.len() <= MAX_PENDING);
    }

    #[test]
    fn an_outcome_settles_its_nonce_once_and_a_late_one_finds_nothing() {
        let mut ledger = Ledger::default();
        ledger.begin(button(7, "a", 1)).unwrap();
        assert!(ledger.settle("12345").is_none());
        let settled = ledger.settle(&nonce(1).to_string()).unwrap();
        assert_eq!(settled.channel, Snowflake(5));
        assert!(ledger.settle(&nonce(1).to_string()).is_none());
        assert_eq!(ledger.component(Snowflake(5), Snowflake(7)), None);
        ledger.begin(button(7, "a", 2)).unwrap();
        assert!(ledger.settle_nonce(nonce(2)).is_some());
    }

    #[test]
    fn failures_are_fixed_sentences_that_separate_unreached_from_unconfirmed() {
        let unreached = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection));
        assert!(failure(unreached).contains("nothing was sent"));
        let lost = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout));
        assert!(failure(lost).contains("may have been received"));
        assert!(failure(RestError::PermissionDenied).contains("permission"));
        assert!(failure(RestError::ResourceGone).contains("no longer"));
        assert!(failure(RestError::InvalidRateLimit).contains("rate limiting"));
        assert_eq!(
            format!(
                "{:?}",
                Action::Modal {
                    values: vec!["secret".into()]
                }
            ),
            "Action::Modal"
        );
        assert!(argument(ArgError::NotAnId).contains("mention"));
    }
}
