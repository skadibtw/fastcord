//! Edits and deletions of the user's own messages that Discord has not
//! answered yet, or did not accept (SPEC §2.1, §5.2).
//!
//! A change is one explicit user action on one message: there is at most one
//! per message, a new action on a message whose change is still being saved is
//! refused, and a new action on a message whose change failed replaces it.
//! Nothing is ever repeated by the program. Both requests are idempotent, so a
//! failed change may be retried safely, but only the user's explicit Retry does
//! it. A change leaves when Discord answers, when the Gateway reports the same
//! outcome (the message now has the failed edit's text, or it was deleted), or
//! when the user dismisses it.
//!
//! At most [`MAX_CHANGES`] exist, each holding at most `MAX_CONTENT_CHARS`
//! characters of new text, so the tracker is bounded however many actions the
//! user takes while Discord is unreachable.

use std::fmt;
use std::sync::Arc;

use fastcord_discord::{EditedMessage, NetworkFailure, RestError, RetryableFailure};
use fastcord_model::Snowflake;

/// Unfinished changes across all channels.
pub const MAX_CHANGES: usize = 16;

pub const BUSY: &str = "A change to that message is still being saved.";
pub const FULL: &str = "Too many unsaved changes. Retry or dismiss some first.";

/// What the user asked for. `Debug` never prints the edit's text.
#[derive(Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// Replace the text with this (trimmed) text.
    Edit(Arc<str>),
    Delete,
}

impl fmt::Debug for ChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Edit(_) => "Edit",
            Self::Delete => "Delete",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeState {
    /// On its way to Discord.
    Saving,
    /// Discord did not accept it, or did not answer; the reason is fixed and
    /// secret-free.
    Failed(&'static str),
}

/// A message's unfinished change, as its timeline row shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowChange {
    pub kind: ChangeKind,
    pub state: ChangeState,
}

impl RowChange {
    pub fn saving(&self) -> bool {
        self.state == ChangeState::Saving
    }

    /// The text of an edit Discord did not accept, to edit again.
    pub fn failed_edit(&self) -> Option<&str> {
        match (&self.kind, self.state) {
            (ChangeKind::Edit(text), ChangeState::Failed(_)) => Some(text),
            _ => None,
        }
    }
}

/// The request to make for a change that starts (again).
#[derive(Debug, PartialEq, Eq)]
pub enum ChangeRequest {
    Edit(EditedMessage),
    Delete,
}

struct Change {
    channel: Snowflake,
    message: Snowflake,
    kind: ChangeKind,
    /// The message has attachments, so an empty edit is valid.
    allow_empty: bool,
    state: ChangeState,
}

impl Change {
    fn is(&self, channel: Snowflake, message: Snowflake) -> bool {
        self.channel == channel && self.message == message
    }

    fn request(&self) -> Option<ChangeRequest> {
        match &self.kind {
            ChangeKind::Edit(text) => EditedMessage::new(text, self.allow_empty)
                .ok()
                .map(ChangeRequest::Edit),
            ChangeKind::Delete => Some(ChangeRequest::Delete),
        }
    }
}

/// A fixed, secret-free sentence for a failed edit.
pub fn edit_failure(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied => "Discord did not allow this edit.",
        RestError::Http(status) if status.as_u16() == 400 => {
            "Discord rejected the edit; it may be too long."
        }
        RestError::Http(_) => "Discord rejected the edit.",
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)) => {
            "Could not reach Discord; the edit was not saved."
        }
        RestError::Retryable(_) | RestError::ResponseBodyTooLarge | RestError::InvalidJson => {
            "Discord did not confirm the edit; it may have been saved. Retrying is safe."
        }
        _ => "The edit could not be saved.",
    }
}

/// A fixed, secret-free sentence for a failed deletion.
pub fn delete_failure(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied => "Discord did not allow deleting this message.",
        RestError::Http(_) => "Discord refused to delete the message.",
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)) => {
            "Could not reach Discord; the message was not deleted."
        }
        RestError::Retryable(_) | RestError::ResponseBodyTooLarge => {
            "Discord did not confirm the deletion; it may already be gone. Retrying is safe."
        }
        _ => "The message could not be deleted.",
    }
}

#[derive(Default)]
pub struct Changes {
    changes: Vec<Change>,
}

impl Changes {
    fn position(&self, channel: Snowflake, message: Snowflake) -> Option<usize> {
        self.changes
            .iter()
            .position(|change| change.is(channel, message))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Records a new change as being saved. A failed change of the same message
    /// is replaced; one still being saved, or a full tracker, refuses with the
    /// sentence to show.
    pub fn begin(
        &mut self,
        channel: Snowflake,
        message: Snowflake,
        kind: ChangeKind,
        allow_empty: bool,
    ) -> Result<(), &'static str> {
        let change = Change {
            channel,
            message,
            kind,
            allow_empty,
            state: ChangeState::Saving,
        };
        match self.position(channel, message) {
            Some(at) if self.changes[at].state == ChangeState::Saving => Err(BUSY),
            Some(at) => {
                self.changes[at] = change;
                Ok(())
            }
            None if self.changes.len() >= MAX_CHANGES => Err(FULL),
            None => {
                self.changes.push(change);
                Ok(())
            }
        }
    }

    /// Discord answered a change being saved: `Ok` removes it, `Err` keeps it
    /// as failed. Returns whether such a change existed.
    pub fn finish(
        &mut self,
        channel: Snowflake,
        message: Snowflake,
        outcome: Result<(), &'static str>,
    ) -> bool {
        let Some(at) = self
            .position(channel, message)
            .filter(|&at| self.changes[at].state == ChangeState::Saving)
        else {
            return false;
        };
        match outcome {
            Ok(()) => {
                self.changes.remove(at);
            }
            Err(reason) => self.changes[at].state = ChangeState::Failed(reason),
        }
        true
    }

    /// The user's explicit retry of a failed change: what to request again.
    pub fn retry(&mut self, channel: Snowflake, message: Snowflake) -> Option<ChangeRequest> {
        let at = self.position(channel, message)?;
        let change = &mut self.changes[at];
        if !matches!(change.state, ChangeState::Failed(_)) {
            return None;
        }
        let request = change.request()?;
        change.state = ChangeState::Saving;
        Some(request)
    }

    /// Drops a failed change at the user's request (or because it is moot).
    pub fn dismiss(&mut self, channel: Snowflake, message: Snowflake) -> bool {
        match self.position(channel, message) {
            Some(at) if matches!(self.changes[at].state, ChangeState::Failed(_)) => {
                self.changes.remove(at);
                true
            }
            _ => false,
        }
    }

    /// The Gateway reports the message's text: a failed edit to exactly that
    /// text did take effect after all. A change being saved waits for its answer.
    pub fn observed_text(&mut self, channel: Snowflake, message: Snowflake, text: &str) -> bool {
        match self.position(channel, message) {
            Some(at)
                if matches!(self.changes[at].state, ChangeState::Failed(_))
                    && matches!(&self.changes[at].kind, ChangeKind::Edit(edit) if **edit == *text.trim()) =>
            {
                self.changes.remove(at);
                true
            }
            _ => false,
        }
    }

    /// The Gateway reports these messages deleted: nothing about them is left
    /// to save or retry. An answer still on its way finds no change and only
    /// confirms what the store already shows.
    pub fn observed_delete(&mut self, channel: Snowflake, messages: &[Snowflake]) -> bool {
        let before = self.changes.len();
        self.changes
            .retain(|change| change.channel != channel || !messages.contains(&change.message));
        self.changes.len() != before
    }

    /// Drops failed changes of messages that are no longer held (the channel
    /// was closed, the message evicted): there is no row to show them on.
    /// Changes being saved stay until answered.
    pub fn retain_held(&mut self, held: impl Fn(Snowflake, Snowflake) -> bool) -> bool {
        let before = self.changes.len();
        self.changes.retain(|change| {
            change.state == ChangeState::Saving || held(change.channel, change.message)
        });
        self.changes.len() != before
    }

    /// The message's change, if it has one, for its row.
    pub fn of(&self, channel: Snowflake, message: Snowflake) -> Option<RowChange> {
        self.position(channel, message).map(|at| {
            let change = &self.changes[at];
            RowChange {
                kind: change.kind.clone(),
                state: change.state,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use fastcord_discord::StatusCode;

    use super::*;

    const C: Snowflake = Snowflake(500);

    fn edit(text: &str) -> ChangeKind {
        ChangeKind::Edit(Arc::from(text))
    }

    #[test]
    fn one_change_per_message_and_a_bounded_total() {
        let mut changes = Changes::default();
        changes.begin(C, Snowflake(1), edit("a"), false).unwrap();
        assert_eq!(
            changes.begin(C, Snowflake(1), ChangeKind::Delete, false),
            Err(BUSY),
            "still being saved"
        );
        // The same message ID in another channel is another message.
        changes
            .begin(Snowflake(501), Snowflake(1), ChangeKind::Delete, false)
            .unwrap();
        for n in 2..MAX_CHANGES as u64 {
            changes
                .begin(C, Snowflake(n), ChangeKind::Delete, false)
                .unwrap();
        }
        assert_eq!(changes.len(), MAX_CHANGES);
        assert_eq!(
            changes.begin(C, Snowflake(999), ChangeKind::Delete, false),
            Err(FULL)
        );
        // A failed change is replaced in place, even when the tracker is full.
        assert!(changes.finish(C, Snowflake(1), Err("refused")));
        changes
            .begin(C, Snowflake(1), ChangeKind::Delete, false)
            .unwrap();
        assert_eq!(changes.len(), MAX_CHANGES);
        assert_eq!(
            changes.of(C, Snowflake(1)).unwrap().kind,
            ChangeKind::Delete
        );
    }

    #[test]
    fn answers_retries_and_dismissals() {
        let mut changes = Changes::default();
        let id = Snowflake(7);
        assert!(!changes.finish(C, id, Ok(())), "nothing to finish");
        changes.begin(C, id, edit("new text"), false).unwrap();
        assert_eq!(changes.retry(C, id), None, "not failed");
        assert!(!changes.dismiss(C, id), "a change being saved stays");
        assert!(changes.finish(C, id, Err("no")));
        assert!(
            !changes.finish(C, id, Ok(())),
            "only one answer per attempt"
        );
        let row = changes.of(C, id).unwrap();
        assert_eq!(row.state, ChangeState::Failed("no"));
        assert_eq!(row.failed_edit(), Some("new text"));
        // Retry asks again with the same text.
        match changes.retry(C, id) {
            Some(ChangeRequest::Edit(request)) => assert_eq!(request.content(), "new text"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(changes.of(C, id).unwrap().saving());
        assert!(changes.finish(C, id, Ok(())));
        assert!(changes.of(C, id).is_none());
        // A failed deletion can be dismissed.
        changes.begin(C, id, ChangeKind::Delete, false).unwrap();
        changes.finish(C, id, Err("no"));
        assert_eq!(changes.of(C, id).unwrap().failed_edit(), None);
        assert!(changes.dismiss(C, id));
        assert_eq!(changes.len(), 0);
        // An empty edit is only requested again where attachments allow it.
        changes.begin(C, id, edit(""), true).unwrap();
        changes.finish(C, id, Err("no"));
        assert!(matches!(changes.retry(C, id), Some(ChangeRequest::Edit(_))));
    }

    #[test]
    fn the_gateway_resolves_failed_changes() {
        let mut changes = Changes::default();
        changes
            .begin(C, Snowflake(1), edit("fixed typo"), false)
            .unwrap();
        // While it is being saved, its own Gateway echo does not end it.
        assert!(!changes.observed_text(C, Snowflake(1), "fixed typo"));
        changes.finish(C, Snowflake(1), Err("timeout"));
        // Another text (another client's edit) leaves it for the user.
        assert!(!changes.observed_text(C, Snowflake(1), "something else"));
        assert!(changes.observed_text(C, Snowflake(1), "fixed typo"));
        assert_eq!(changes.len(), 0);
        // Deletion ends any change of the message, in that channel only.
        changes.begin(C, Snowflake(2), edit("x"), false).unwrap();
        changes
            .begin(C, Snowflake(3), ChangeKind::Delete, false)
            .unwrap();
        changes
            .begin(Snowflake(501), Snowflake(2), ChangeKind::Delete, false)
            .unwrap();
        assert!(changes.observed_delete(C, &[Snowflake(2), Snowflake(3), Snowflake(4)]));
        assert_eq!(changes.len(), 1);
        assert!(changes.of(Snowflake(501), Snowflake(2)).is_some());
        // Failed changes of messages no longer held go; saving ones stay.
        changes
            .begin(C, Snowflake(5), ChangeKind::Delete, false)
            .unwrap();
        changes.finish(C, Snowflake(5), Err("no"));
        assert!(changes.retain_held(|_, _| false));
        assert!(changes.of(C, Snowflake(5)).is_none());
        assert!(changes.of(Snowflake(501), Snowflake(2)).is_some());
    }

    #[test]
    fn failures_are_explained_without_details_from_the_network() {
        let timeout = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout));
        let refused = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection));
        assert!(edit_failure(timeout).contains("may have been saved"));
        assert!(edit_failure(refused).contains("not saved"));
        assert!(edit_failure(RestError::Http(StatusCode::BAD_REQUEST)).contains("too long"));
        assert!(edit_failure(RestError::PermissionDenied).contains("not allow"));
        assert!(delete_failure(timeout).contains("Retrying is safe"));
        assert!(delete_failure(refused).contains("not deleted"));
        assert!(delete_failure(RestError::PermissionDenied).contains("not allow"));
        let row = RowChange {
            kind: edit("private words"),
            state: ChangeState::Saving,
        };
        assert!(!format!("{row:?}").contains("private"));
    }
}
