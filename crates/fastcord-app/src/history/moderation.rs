//! Message moderation on the history worker (issue #95): deleting other
//! people's messages, the "Select messages" mode with one bulk deletion, and
//! removing a reaction or all reactions of one emoji from the "who reacted"
//! list. Every action needs Manage Messages in the open server channel, is one
//! explicit click followed by an explicit Confirm, is checked here against the
//! store and the current permission (never against what a possibly stale UI
//! showed), sent once and never retried. The messages and reactions on screen
//! change only when Discord's Gateway reports the deletion or removal.
//!
//! A bulk deletion holds at most 100 messages, each younger than 14 days at the
//! moment it is selected and again when it is sent (the clock is injectable:
//! `History::clock`).

use std::collections::BTreeSet;
use std::sync::Arc;

use fastcord_discord::{
    BULK_MAX, BULK_MIN, BulkError, BulkRequestError, ReactionEmoji, RestClient, RestError,
    bulk_deletable,
};
use fastcord_model::{Message, Snowflake};

use super::{History, NOT_LOADED, NOT_OPEN, NOT_OWN};
use crate::timeline::{ReactionTarget, SelectView, UsersState};

pub const NO_MODERATION: &str = "You need Manage Messages in this channel.";
const TOO_MANY: &str = "At most 100 messages can be deleted at once.";
const TOO_OLD: &str = "Messages 14 days old or older cannot be deleted in bulk.";
const TOO_FEW: &str = "Select at least 2 messages.";
const BUSY: &str = "A moderation request is still waiting for Discord.";

/// The messages the user ticked in the open channel.
#[derive(Debug)]
pub struct Selection {
    channel: Snowflake,
    ids: BTreeSet<Snowflake>,
    confirming: bool,
    busy: bool,
}

pub enum ModKind {
    Bulk(Snowflake),
    /// The "who reacted" list with this serial, and the user removed (or all).
    Reaction(u64, ReactionTarget),
}

/// Discord's answer to a moderation request of the user.
pub struct ModDone {
    pub kind: ModKind,
    pub result: Result<(), BulkRequestError>,
}

fn system_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// A fixed, secret-free sentence for a refused or failed moderation request.
pub fn failure(error: BulkRequestError) -> &'static str {
    let error = match error {
        BulkRequestError::Invalid(BulkError::TooFew) => return TOO_FEW,
        BulkRequestError::Invalid(BulkError::TooMany) => return TOO_MANY,
        BulkRequestError::Invalid(BulkError::TooOld) => return TOO_OLD,
        BulkRequestError::Rest(error) => error,
    };
    match error {
        RestError::PermissionDenied => "Discord refused: you need Manage Messages here.",
        RestError::ResourceGone => "That message is already gone.",
        RestError::Retryable(_) => {
            "Discord did not confirm. It may have been applied; check the messages before trying again."
        }
        RestError::Refused(refusal) if refusal.code == Some(60003) => {
            "Discord asks for two-factor verification. Complete it in the official client; nothing was changed here."
        }
        RestError::Refused(refusal) if refusal.status.as_u16() == 403 => {
            "Discord refused: you need Manage Messages here."
        }
        RestError::Refused(refusal) if refusal.status.as_u16() == 404 => {
            "That message is already gone."
        }
        RestError::InvalidRateLimit | RestError::SchedulerCapacityExceeded => {
            "Discord is rate limiting this. Try again shortly."
        }
        RestError::Http(status) if status.as_u16() == 429 => {
            "Discord is rate limiting this. Try again shortly."
        }
        RestError::AccountVerificationRequired => {
            "Discord requires an account check. Requests are paused."
        }
        RestError::WriteNotVerified(_) => fastcord_discord::WRITE_NOT_VERIFIED,
        _ => "Discord did not accept the request.",
    }
}

impl History {
    fn clock_ms(&self) -> u64 {
        self.clock.map_or_else(system_ms, |clock| clock())
    }

    /// The held message the user may delete: their own, or anyone's where they
    /// have Manage Messages in the open channel.
    pub(super) fn deletable(
        &self,
        channel: Snowflake,
        message: Snowflake,
    ) -> Result<Arc<Message>, &'static str> {
        if self.channel != Some(channel) {
            return Err(NOT_OPEN);
        }
        let held = self.store.get(channel, message).ok_or(NOT_LOADED)?;
        let own = self
            .user
            .is_some_and(|user| fastcord_discord::is_own_message(&held, user));
        if own || self.moderable == Some(channel) {
            Ok(held)
        } else {
            Err(NOT_OWN)
        }
    }

    /// Whether the open channel allows moderating messages.
    fn may_moderate(&mut self, channel: Snowflake) -> bool {
        self.dirty = true;
        self.notice = None;
        if self.channel != Some(channel) {
            self.notice = Some(NOT_OPEN);
            false
        } else if self.moderable != Some(channel) {
            self.notice = Some(NO_MODERATION);
            false
        } else {
            true
        }
    }

    /// Starts or ends the "Select messages" mode of the open channel.
    pub fn select_messages(&mut self, channel: Snowflake, on: bool) {
        self.dirty = true;
        if self.selection.as_ref().is_some_and(|s| s.busy) {
            self.notice = Some(BUSY);
            return;
        }
        if !on {
            self.selection = None;
            return;
        }
        if self.may_moderate(channel) {
            self.selection = Some(Selection {
                channel,
                ids: BTreeSet::new(),
                confirming: false,
                busy: false,
            });
        }
    }

    /// Ticks or unticks one held message. Only messages young enough for a
    /// bulk deletion can be ticked, and at most 100.
    pub fn toggle_selected(&mut self, channel: Snowflake, message: Snowflake) {
        let now = self.clock_ms();
        let allowed = self.may_moderate(channel);
        let held = self.store.get(channel, message).is_some();
        let Some(selection) = self
            .selection
            .as_mut()
            .filter(|s| s.channel == channel && !s.busy)
        else {
            return;
        };
        if !allowed {
            self.selection = None;
            return;
        }
        selection.confirming = false;
        if selection.ids.remove(&message) {
            return;
        }
        self.notice = if !held {
            Some(NOT_LOADED)
        } else if !bulk_deletable(message, now) {
            Some(TOO_OLD)
        } else if selection.ids.len() >= BULK_MAX {
            Some(TOO_MANY)
        } else {
            selection.ids.insert(message);
            None
        };
    }

    /// The Delete button of the selection bar: asks for the Confirm.
    pub fn ask_bulk_delete(&mut self, channel: Snowflake) {
        if !self.may_moderate(channel) {
            return;
        }
        let Some(selection) = self
            .selection
            .as_mut()
            .filter(|s| s.channel == channel && !s.busy)
        else {
            return;
        };
        if selection.ids.len() < BULK_MIN {
            self.notice = Some(TOO_FEW);
        } else {
            selection.confirming = true;
        }
    }

    pub fn cancel_bulk_delete(&mut self) {
        if let Some(selection) = self.selection.as_mut().filter(|s| !s.busy) {
            selection.confirming = false;
            self.dirty = true;
        }
    }

    /// The Confirm: one request for the ticked messages, re-checked against the
    /// permission, the open channel and the age limit as of now.
    pub fn confirm_bulk_delete(&mut self, rest: &RestClient, channel: Snowflake) {
        if !self.may_moderate(channel) {
            return;
        }
        let now = self.clock_ms();
        let Some(selection) = self
            .selection
            .as_mut()
            .filter(|s| s.channel == channel && s.confirming && !s.busy)
        else {
            return;
        };
        let ids: Vec<Snowflake> = selection.ids.iter().copied().collect();
        selection.confirming = false;
        if ids.iter().any(|id| !bulk_deletable(*id, now)) {
            self.notice = Some(TOO_OLD);
            return;
        }
        selection.busy = true;
        let rest = rest.clone();
        self.mod_requests.push(Box::pin(async move {
            ModDone {
                kind: ModKind::Bulk(channel),
                result: rest.bulk_delete(channel, &ids, now).await,
            }
        }));
    }

    /// Forgets ticked messages that Discord reports deleted.
    pub(super) fn deselect(&mut self, ids: &[Snowflake]) {
        if let Some(selection) = &mut self.selection
            && ids.iter().any(|id| selection.ids.remove(id))
        {
            selection.confirming = false;
            self.dirty = true;
        }
    }

    /// What the timeline shows of the selection mode.
    pub(super) fn select_view(&self, channel: Snowflake) -> Option<SelectView> {
        self.selection
            .as_ref()
            .filter(|s| s.channel == channel)
            .map(|s| SelectView {
                ids: s.ids.clone(),
                confirming: s.confirming,
                busy: s.busy,
            })
    }

    /// Ends the mode when the account lost Manage Messages or left the channel.
    pub(super) fn prune_selection(&mut self) {
        if self
            .selection
            .as_ref()
            .is_some_and(|s| self.moderable != Some(s.channel) || self.channel != Some(s.channel))
        {
            self.selection = None;
            self.dirty = true;
        }
    }

    /// Asks for the Confirm of removing one user's reaction, or all reactions
    /// of the listed emoji.
    pub fn ask_reaction_removal(&mut self, target: ReactionTarget) {
        let Some(channel) = self.inspection.as_ref().map(|i| i.channel) else {
            return;
        };
        if !self.may_moderate(channel) {
            return;
        }
        let Some(inspection) = self
            .inspection
            .as_mut()
            .filter(|i| i.poll_answer.is_none() && !i.busy)
        else {
            return;
        };
        let listed = match (&inspection.state, target) {
            (UsersState::Loaded { .. }, ReactionTarget::All) => true,
            (UsersState::Loaded { ids, .. }, ReactionTarget::User(user)) => ids.contains(&user),
            _ => false,
        };
        if listed {
            inspection.confirm = Some(target);
        }
    }

    pub fn cancel_reaction_removal(&mut self) {
        if let Some(inspection) = self.inspection.as_mut().filter(|i| !i.busy) {
            inspection.confirm = None;
            self.dirty = true;
        }
    }

    /// The Confirm: one request, re-checked against the store and permission.
    pub fn confirm_reaction_removal(&mut self, rest: &RestClient) {
        let Some(channel) = self.inspection.as_ref().map(|i| i.channel) else {
            return;
        };
        if !self.may_moderate(channel) {
            return;
        }
        let Some(inspection) = self
            .inspection
            .as_mut()
            .filter(|i| i.poll_answer.is_none() && !i.busy)
        else {
            return;
        };
        let Some(target) = inspection.confirm.take() else {
            return;
        };
        let (message, serial) = (inspection.message, inspection.serial);
        if self.store.get(channel, message).is_none() {
            self.notice = Some(NOT_LOADED);
            return;
        }
        let Ok(emoji) = ReactionEmoji::new(&inspection.emoji) else {
            return;
        };
        inspection.busy = true;
        let rest = rest.clone();
        self.mod_requests.push(Box::pin(async move {
            let result = match target {
                ReactionTarget::User(user) => {
                    rest.remove_user_reaction(channel, message, &emoji, user)
                        .await
                }
                ReactionTarget::All => rest.clear_emoji_reactions(channel, message, &emoji).await,
            };
            ModDone {
                kind: ModKind::Reaction(serial, target),
                result: result.map_err(BulkRequestError::Rest),
            }
        }));
    }

    /// Applies Discord's answer. Returns whether Discord rejected the login.
    /// Nothing on screen changes for a success except what the answer itself
    /// settles (the ticked messages, the listed user); the Gateway reports the
    /// deletions and reaction removals.
    pub(super) fn finish_moderation(&mut self, done: ModDone) -> bool {
        self.dirty = true;
        let ModDone { kind, result } = done;
        let rejected = matches!(
            result,
            Err(BulkRequestError::Rest(RestError::AuthenticationRequired))
        );
        match kind {
            ModKind::Bulk(channel) => {
                if let Some(selection) = self.selection.as_mut().filter(|s| s.channel == channel) {
                    selection.busy = false;
                    selection.confirming = false;
                }
                match result {
                    Ok(()) => {
                        self.selection = None;
                        self.notice = Some("Messages deleted.");
                    }
                    Err(error) => self.notice = Some(failure(error)),
                }
            }
            ModKind::Reaction(serial, target) => {
                let Some(inspection) = self.inspection.as_mut().filter(|i| i.serial == serial)
                else {
                    return rejected;
                };
                inspection.busy = false;
                inspection.confirm = None;
                match result {
                    Ok(()) | Err(BulkRequestError::Rest(RestError::ResourceGone)) => {
                        match target {
                            ReactionTarget::All => self.inspection = None,
                            ReactionTarget::User(user) => {
                                if let UsersState::Loaded { names, ids, .. } = &mut inspection.state
                                    && let Some(at) = ids.iter().position(|id| *id == user)
                                {
                                    ids.remove(at);
                                    names.remove(at);
                                }
                            }
                        }
                        self.notice = Some("Reaction removed.");
                    }
                    Err(error) => self.notice = Some(failure(error)),
                }
            }
        }
        rejected
    }
}
