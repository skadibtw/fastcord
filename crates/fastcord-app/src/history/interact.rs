//! Interactions on the history worker (SPEC §9): button presses, select
//! choices, slash commands and modal answers. The rules live in
//! [`crate::interactions`]; this is the part that checks the store and the
//! session, sends the request once and settles it from the Gateway.

use std::sync::Arc;

use fastcord_discord::gateway::{Dispatch, MessageDelete};
use fastcord_discord::{
    InteractionContext, InteractionModal, InteractionOutcome, InteractionRequest, Nonce,
    RestClient, RestError,
};
use fastcord_model::{BUTTON_LINK, Component, Snowflake};

use super::{History, NOT_LOADED, NOT_OPEN};
use crate::interactions::{self as rules, Action, Entry, Target};

/// What an interaction request or its timer reports.
pub enum InteractionEvent {
    /// Discord answered the request itself.
    Sent(Nonce, Result<(), RestError>),
    /// [`rules::TIMEOUT`] passed.
    Expired(Nonce),
}

/// What the timeline shows of the worker's interactions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InteractionView {
    /// The slash command waiting for its application in the open channel.
    pub waiting: Option<String>,
    /// The form an application asked the user to fill in.
    pub modal: Option<Arc<InteractionModal>>,
    /// The modal's answer is on its way.
    pub modal_sending: bool,
}

impl History {
    fn interaction_target(&self, channel: Snowflake) -> Result<&str, &'static str> {
        if self.channel != Some(channel) {
            return Err(NOT_OPEN);
        }
        if self.sendable != Some(channel) {
            return Err(rules::NOT_ALLOWED);
        }
        self.session.as_deref().ok_or(rules::NOT_READY)
    }

    /// Sends `request` once and watches for its outcome. The ledger entry is
    /// made first, so the same control cannot be sent twice meanwhile.
    fn begin_interaction(
        &mut self,
        rest: &RestClient,
        nonce: Nonce,
        entry: Entry,
        request: InteractionRequest,
    ) {
        if let Err(notice) = self.interactions.begin(entry) {
            self.notice = Some(notice);
            return;
        }
        let rest = rest.clone();
        self.interaction_requests.push(Box::pin(async move {
            InteractionEvent::Sent(nonce, rest.send_interaction(&request).await)
        }));
        self.interaction_requests.push(Box::pin(async move {
            tokio::time::sleep(rules::TIMEOUT).await;
            InteractionEvent::Expired(nonce)
        }));
    }

    /// The user's explicit interaction. Everything is re-checked here against
    /// the store, the open channel, the permissions and the session.
    pub fn interact(&mut self, rest: &RestClient, action: Action) {
        self.dirty = true;
        self.notice = None;
        match action {
            Action::CloseModal => self.modal = None,
            Action::Dismiss { channel, message } => self.dismiss_ephemeral(channel, message),
            Action::Component {
                channel,
                message,
                custom_id,
                values,
            } => self.press(rest, channel, message, custom_id, values),
            Action::Command {
                channel,
                command,
                subcommand,
                args,
            } => {
                let session = match self.interaction_target(channel) {
                    Ok(session) => session.to_owned(),
                    Err(notice) => {
                        self.notice = Some(notice);
                        return;
                    }
                };
                let nonce = self.nonces.next_now();
                let context = InteractionContext {
                    session_id: &session,
                    guild_id: self.guild,
                    channel_id: channel,
                    nonce,
                };
                match InteractionRequest::command(&context, &command, subcommand.as_deref(), &args)
                {
                    Ok(request) => {
                        let entry = Entry {
                            nonce,
                            channel,
                            target: Target::Command(command.name.clone()),
                        };
                        self.begin_interaction(rest, nonce, entry, request);
                    }
                    Err(error) => self.notice = Some(rules::argument(error)),
                }
            }
            Action::Modal { values } => self.submit_modal(rest, values),
        }
    }

    fn press(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        custom_id: String,
        values: Vec<String>,
    ) {
        let session = match self.interaction_target(channel) {
            Ok(session) => session.to_owned(),
            Err(notice) => {
                self.notice = Some(notice);
                return;
            }
        };
        let Some(held) = self.store.get(channel, message) else {
            self.notice = Some(NOT_LOADED);
            return;
        };
        let Some(found) = Component::find(&held.components, &custom_id) else {
            self.notice = Some(rules::STALE);
            return;
        };
        let component_type = match found {
            Component::Button(button) => {
                if button.premium {
                    self.notice = Some(rules::PREMIUM);
                    return;
                }
                if button.disabled || button.style == BUTTON_LINK {
                    self.notice = Some(rules::DISABLED);
                    return;
                }
                2
            }
            Component::Select(select) => {
                if select.disabled {
                    self.notice = Some(rules::DISABLED);
                    return;
                }
                // Only a string select lists its choices; the others are shown
                // read-only because choosing a user, role or channel is not
                // supported here.
                let offered = select.kind == 3
                    && !values.is_empty()
                    && values.len() >= usize::from(select.min_values)
                    && values.len() <= usize::from(select.max_values)
                    && values
                        .iter()
                        .all(|v| select.options.iter().any(|o| o.value == *v));
                if !offered {
                    self.notice = Some(rules::BAD_CHOICE);
                    return;
                }
                select.kind
            }
            _ => {
                self.notice = Some(rules::STALE);
                return;
            }
        };
        let nonce = self.nonces.next_now();
        let context = InteractionContext {
            session_id: &session,
            guild_id: self.guild,
            channel_id: channel,
            nonce,
        };
        // The application is the message's author: a bot's user ID is its
        // application ID.
        let request = InteractionRequest::component(
            &context,
            held.author.id,
            message,
            held.flags,
            component_type,
            &custom_id,
            &values,
        );
        let entry = Entry {
            nonce,
            channel,
            target: Target::Component { message, custom_id },
        };
        self.begin_interaction(rest, nonce, entry, request);
    }

    fn submit_modal(&mut self, rest: &RestClient, values: Vec<String>) {
        let Some(modal) = self.modal.clone() else {
            self.notice = Some(rules::NO_MODAL);
            return;
        };
        let Some(channel) = modal.channel_id.or(self.channel) else {
            self.notice = Some(rules::NO_MODAL);
            return;
        };
        let session = match self.interaction_target(channel) {
            Ok(session) => session.to_owned(),
            Err(notice) => {
                self.notice = Some(notice);
                return;
            }
        };
        if values.len() != modal.inputs.len() {
            self.notice = Some(rules::NO_MODAL);
            return;
        }
        for (input, value) in modal.inputs.iter().zip(&values) {
            if let Some(problem) = input.check(value) {
                self.notice = Some(problem);
                return;
            }
        }
        let nonce = self.nonces.next_now();
        let context = InteractionContext {
            session_id: &session,
            guild_id: self.guild,
            channel_id: channel,
            nonce,
        };
        let request = InteractionRequest::modal_submit(&context, &modal, &values);
        let entry = Entry {
            nonce,
            channel,
            target: Target::Modal,
        };
        self.begin_interaction(rest, nonce, entry, request);
    }

    /// Removes a message only the user sees from this client's view. Nothing is
    /// sent to Discord, which has no such message for anyone else.
    fn dismiss_ephemeral(&mut self, channel: Snowflake, message: Snowflake) {
        if self.channel != Some(channel) {
            self.notice = Some(NOT_OPEN);
            return;
        }
        let Some(held) = self.store.get(channel, message) else {
            self.notice = Some(NOT_LOADED);
            return;
        };
        if !held.is_ephemeral() {
            self.notice = Some(rules::NOT_EPHEMERAL);
            return;
        }
        self.apply_store(&Dispatch::MessageDelete(MessageDelete {
            id: message,
            channel_id: channel,
            guild_id: None,
        }));
    }

    /// Gateway events about interactions. An outcome or a modal whose nonce is
    /// not one this client is waiting for (settled, timed out, another
    /// client's) changes nothing.
    pub(super) fn apply_interaction_event(&mut self, event: &Dispatch) {
        match event {
            Dispatch::Ready(ready) => {
                self.session = Some(ready.session_id.as_str().to_owned());
                self.interactions.clear();
            }
            Dispatch::InteractionSuccess(InteractionOutcome { nonce, .. }) => {
                if self.interactions.settle(nonce).is_some() {
                    self.dirty = true;
                }
            }
            Dispatch::InteractionFailure(InteractionOutcome { nonce, .. }) => {
                if self.interactions.settle(nonce).is_some() {
                    self.notice = Some(rules::FAILED);
                    self.dirty = true;
                }
            }
            Dispatch::InteractionModalCreate(modal) => {
                let Some(nonce) = modal.nonce.as_deref() else {
                    return;
                };
                if self
                    .interactions
                    .settle(nonce)
                    .is_some_and(|entry| self.channel == Some(entry.channel))
                {
                    self.modal = Some(Arc::new((**modal).clone()));
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    /// Applies what an interaction request or its timer reported. Returns
    /// whether Discord rejected the login.
    pub(super) fn finish_interaction(&mut self, event: InteractionEvent) -> bool {
        self.dirty = true;
        match event {
            InteractionEvent::Sent(nonce, Ok(())) => {
                // Discord took it. A modal answer is done with; the rest wait
                // for the Gateway or the timer.
                if self.interactions.is_modal(nonce) {
                    self.modal = None;
                }
                false
            }
            InteractionEvent::Sent(nonce, Err(error)) => {
                self.interactions.settle_nonce(nonce);
                self.notice = Some(rules::failure(error));
                error == RestError::AuthenticationRequired
            }
            InteractionEvent::Expired(nonce) => {
                if self.interactions.settle_nonce(nonce).is_some() {
                    self.notice = Some(rules::NO_RESPONSE);
                }
                false
            }
        }
    }

    /// The interaction state for the timeline snapshot of `channel`.
    pub(super) fn interaction_view(&self, channel: Snowflake) -> InteractionView {
        InteractionView {
            waiting: self.interactions.command(channel).map(str::to_owned),
            modal: self.modal.clone(),
            modal_sending: self.interactions.modal_waiting(channel),
        }
    }
}
