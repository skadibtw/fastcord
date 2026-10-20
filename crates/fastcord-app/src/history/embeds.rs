//! Hiding and restoring the link previews of a message on the history worker.
//!
//! One explicit user action, one request, made once. The request is checked
//! against the store (never against what a possibly stale UI showed): the
//! message must be held in the open channel and either be the account's own or
//! the caller must have said the account may change others' (Manage Messages;
//! Discord stays authoritative). At most [`MAX_IN_FLIGHT`] requests are
//! outstanding and one per message. Setting the same state twice is the same as
//! once, so a failure that may have taken effect can safely be tried again by
//! the user; nothing is repeated by the program. Discord's answer goes into the
//! store through the same partial update the Gateway's report uses, so
//! whichever arrives second changes nothing.

use fastcord_discord::{
    NetworkFailure, RestClient, RestError, RetryableFailure, edit_response_update, is_own_message,
};
use fastcord_model::{Message, Snowflake};

use super::{History, NOT_LOADED, NOT_OPEN};
use fastcord_discord::gateway::Dispatch;

/// Unfinished embed requests across the open channel.
pub const MAX_IN_FLIGHT: usize = 4;

pub const BUSY: &str = "That message's link previews are still being changed.";
pub const FULL: &str = "Too many link-preview changes are still being saved. Try again shortly.";
pub const NOT_ALLOWED: &str = "You can only change link previews of your own messages here.";

/// Discord's answer to a link-preview request of the user.
pub struct EmbedsDone {
    pub channel: Snowflake,
    pub message: Snowflake,
    pub result: Result<Box<Message>, RestError>,
}

/// A fixed, secret-free sentence for a request Discord did not accept.
pub fn failure(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied => {
            "Discord did not allow changing this message's link previews."
        }
        RestError::ResourceGone => "That message is no longer available.",
        RestError::InvalidRateLimit | RestError::SchedulerCapacityExceeded => {
            "Discord is rate limiting this. Try again shortly."
        }
        RestError::Http(_) => "Discord rejected the change to the link previews.",
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)) => {
            "Could not reach Discord; the link previews were not changed."
        }
        RestError::Retryable(_) | RestError::ResponseBodyTooLarge | RestError::InvalidJson => {
            "Discord did not confirm the change; it may have been applied. Trying again is safe."
        }
        _ => "The link previews could not be changed.",
    }
}

impl History {
    /// The user's explicit hide (`suppressed`) or restore of a message's link
    /// previews. `others`: the account may change messages that are not its own.
    pub fn set_embeds_suppressed(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        suppressed: bool,
        others: bool,
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
        let own = self.user.is_some_and(|user| is_own_message(&held, user));
        if !own && !(others && matches!(held.kind, 0 | 19)) {
            self.notice = Some(NOT_ALLOWED);
            return;
        }
        if self.embed_ops.contains(&(channel, message)) {
            self.notice = Some(BUSY);
            return;
        }
        if self.embed_ops.len() >= MAX_IN_FLIGHT {
            self.notice = Some(FULL);
            return;
        }
        if held.embeds_suppressed() == suppressed {
            return;
        }
        self.embed_ops.push((channel, message));
        let rest = rest.clone();
        self.embed_requests.push(Box::pin(async move {
            let result = rest
                .set_embeds_suppressed(channel, message, suppressed)
                .await
                .map(Box::new);
            EmbedsDone {
                channel,
                message,
                result,
            }
        }));
    }

    /// Applies Discord's answer. Returns whether Discord rejected the login.
    pub(super) fn finish_embeds(&mut self, done: EmbedsDone) -> bool {
        let EmbedsDone {
            channel,
            message,
            result,
            ..
        } = done;
        self.dirty = true;
        self.embed_ops.retain(|op| *op != (channel, message));
        match result {
            Ok(answer) => {
                let held = self.store.get(channel, message);
                if let Some(update) = edit_response_update(held.as_deref(), *answer) {
                    self.apply_store(&Dispatch::MessageUpdate(Box::new(update)));
                }
                false
            }
            Err(error) => {
                self.notice = Some(failure(error));
                error == RestError::AuthenticationRequired
            }
        }
    }
}
