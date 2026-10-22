//! Accounts, sessions and the read-only security overview (SPEC §16, issue #107).
//!
//! **Accounts** lists the saved logins (token in the native store, label from
//! `accounts`); Switch and Remove are decided by `main.rs`, which tears the
//! current account down exactly as logout does before the chosen one signs in,
//! so only one session ever exists. **Security** shows whether two-factor is
//! on, the email is verified and a phone is present, and hands password, MFA,
//! passkey and recovery changes to the official client. **Sessions** lists
//! `GET /auth/sessions` and logs one out with `POST /auth/sessions/logout`
//! after an explicit Confirm: one request, never retried. Discord may demand
//! multi-factor or password re-authentication (code 60003); that is shown as
//! Discord's refusal with "Confirm in the official client" and is not answered
//! here. Both session routes are unverified (`docs/PROTOCOL.md`).

use fastcord_discord::{
    AuthSession, MFA_REQUIRED, RestClient, RestError, RetryableFailure, SecurityOverview,
    is_mfa_required,
};
use fastcord_model::Snowflake;
use iced::widget::{button, column, row, scrollable, text};
use iced::{Element, Length, Task};

use crate::accounts::Index;

/// Where password, MFA, passkeys and recovery are changed.
pub const SETTINGS_URL: &str = "https://discord.com/settings/account";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pending {
    Logout(String),
    Remove(Snowflake),
}

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    Reload,
    Sessions(u64, Result<Vec<AuthSession>, RestError>),
    Overview(u64, Result<SecurityOverview, RestError>),
    Ask(Pending),
    Cancel,
    Confirm,
    LoggedOut(u64, String, Result<(), RestError>),
    Switch(Snowflake),
    OpenWeb,
    Logout,
}

/// What `main.rs` must do, because it owns the account phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Switch(Snowflake),
    Remove(Snowflake),
    OpenWeb,
    Logout,
}

#[derive(Debug, Default)]
pub struct State {
    open: bool,
    sessions: Option<Vec<AuthSession>>,
    overview: Option<SecurityOverview>,
    loading: bool,
    pending: Option<Pending>,
    /// A logout request is in flight.
    busy: bool,
    notice: Option<String>,
    generation: u64,
}

fn describe(error: RestError) -> String {
    if is_mfa_required(&error) {
        let RestError::Refused(refusal) = error else {
            unreachable!("is_mfa_required matches only a refusal");
        };
        return format!(
            "Discord wants multi-factor or password re-authentication for this (code {MFA_REQUIRED}: {}). fastcord does not answer it. Confirm in the official client.",
            refusal.message()
        );
    }
    match error {
        RestError::Retryable(RetryableFailure::Network(_) | RetryableFailure::Server(_)) => {
            "Discord did not confirm. The session may have been logged out; reload the list before trying again.".to_owned()
        }
        RestError::PermissionDenied => {
            "Discord refused this. Confirm in the official client.".to_owned()
        }
        other => crate::invite::describe(other),
    }
}

impl State {
    /// Logout or account change.
    pub fn clear(&mut self) {
        *self = Self {
            generation: self.generation.wrapping_add(1),
            ..Self::default()
        };
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn fail(&mut self, notice: &str) {
        self.notice = Some(notice.to_owned());
    }

    fn load(&mut self, client: &RestClient) -> Task<Event> {
        if self.loading {
            return Task::none();
        }
        self.loading = true;
        self.notice = None;
        let generation = self.generation;
        let (sessions, overview) = (client.clone(), client.clone());
        Task::batch([
            Task::perform(
                async move { sessions.auth_sessions().await },
                move |result| Event::Sessions(generation, result),
            ),
            Task::perform(
                async move { overview.security_overview().await },
                move |result| Event::Overview(generation, result),
            ),
        ])
    }

    fn finish_logout(&mut self, hash: &str, result: Result<(), RestError>) {
        if !self.busy {
            return;
        }
        self.busy = false;
        match result {
            Ok(()) => {
                if let Some(list) = &mut self.sessions {
                    list.retain(|session| session.id_hash != hash);
                }
                self.notice = Some("That session was logged out.".to_owned());
            }
            Err(error) => self.notice = Some(describe(error)),
        }
    }

    pub fn update(&mut self, event: Event, client: &RestClient) -> (Task<Event>, Option<Action>) {
        match event {
            Event::Toggle => {
                self.open = !self.open;
                self.pending = None;
                if self.open && self.sessions.is_none() {
                    return (self.load(client), None);
                }
            }
            Event::Reload => {
                self.pending = None;
                return (self.load(client), None);
            }
            Event::Sessions(generation, result) if generation == self.generation => {
                self.loading = false;
                match result {
                    Ok(list) => self.sessions = Some(list),
                    Err(error) => self.notice = Some(describe(error)),
                }
            }
            Event::Overview(generation, Ok(overview)) if generation == self.generation => {
                self.overview = Some(overview);
            }
            Event::Ask(pending) if !self.busy => self.pending = Some(pending),
            Event::Cancel => self.pending = None,
            Event::Confirm if !self.busy => match self.pending.take() {
                Some(Pending::Logout(hash)) => {
                    self.busy = true;
                    self.notice = None;
                    let generation = self.generation;
                    let client = client.clone();
                    let sent = hash.clone();
                    return (
                        Task::perform(
                            async move { client.logout_sessions(&[hash]).await },
                            move |result| Event::LoggedOut(generation, sent.clone(), result),
                        ),
                        None,
                    );
                }
                Some(Pending::Remove(id)) => return (Task::none(), Some(Action::Remove(id))),
                None => {}
            },
            Event::LoggedOut(generation, hash, result) if generation == self.generation => {
                self.finish_logout(&hash, result);
            }
            Event::Switch(id) => return (Task::none(), Some(Action::Switch(id))),
            Event::OpenWeb => return (Task::none(), Some(Action::OpenWeb)),
            Event::Logout => return (Task::none(), Some(Action::Logout)),
            _ => {}
        }
        (Task::none(), None)
    }

    /// A bare 401 stops the account like every other request. A 401 that
    /// carries Discord's re-authentication demand (60003) does not.
    pub fn is_rejected(event: &Event) -> bool {
        let error = match event {
            Event::Sessions(_, Err(error))
            | Event::Overview(_, Err(error))
            | Event::LoggedOut(_, _, Err(error)) => error,
            _ => return false,
        };
        match error {
            RestError::AuthenticationRequired => true,
            RestError::Refused(refusal) => {
                refusal.status.as_u16() == 401 && refusal.code != Some(MFA_REQUIRED)
            }
            _ => false,
        }
    }
}

fn flag(value: Option<bool>, yes: &str, no: &str) -> String {
    match value {
        Some(true) => yes.to_owned(),
        Some(false) => no.to_owned(),
        None => "unknown".to_owned(),
    }
}

fn session_line(session: &AuthSession) -> String {
    let mut parts: Vec<&str> = [&session.platform, &session.os, &session.location]
        .into_iter()
        .map(String::as_str)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        parts.push("Unknown client");
    }
    let mut line = parts.join(" \u{b7} ");
    if let Some(when) = &session.last_used {
        // Date and minutes of Discord's ISO 8601 text.
        line.push_str(&format!(
            " \u{b7} last used {}",
            when.get(..16).unwrap_or(when).replace('T', " ")
        ));
    }
    line
}

/// The panel above the conversation; `None` while closed. `can_switch` is
/// false for a memory-only session, whose login would be lost by switching.
pub fn view<'a>(
    state: &'a State,
    index: &'a Index,
    active: Snowflake,
    can_switch: bool,
) -> Option<Element<'a, Event>> {
    if !state.open {
        return None;
    }
    let mut body = column![text("Saved accounts").size(14)].spacing(6);
    for entry in index.entries() {
        let mut line = row![text(entry.label()).size(13).width(Length::Fill)].spacing(8);
        if entry.id == active {
            line = line
                .push(text("Active").size(12).style(text::secondary))
                .push(button(text("Log out of fastcord").size(12)).on_press(Event::Logout));
        } else {
            line =
                line.push(button(text("Switch").size(12)).on_press_maybe(
                    (can_switch && !state.busy).then_some(Event::Switch(entry.id)),
                ))
                .push(
                    button(text("Remove").size(12)).on_press(Event::Ask(Pending::Remove(entry.id))),
                );
        }
        body = body.push(line);
    }
    body = body.push(
        text(if can_switch {
            "Switching logs this account out of fastcord first; only one account is ever connected."
        } else {
            "This session is memory-only; switching would lose it, so Switch is off."
        })
        .size(12)
        .style(text::secondary),
    );
    let overview = state.overview.unwrap_or_default();
    body = body
        .push(text("Security").size(14))
        .push(text(format!(
            "Two-factor authentication: {}",
            flag(overview.mfa_enabled, "on", "off")
        )).size(13))
        .push(text(format!(
            "Email: {}",
            flag(overview.email_verified, "verified", "not verified")
        )).size(13))
        .push(text(format!(
            "Phone number: {}",
            flag(overview.phone_present, "added", "none")
        )).size(13))
        .push(
            text("Changing the password, two-factor authentication, passkeys and account recovery is done in the official client or Discord's website, not here.")
                .size(12)
                .style(text::secondary),
        )
        .push(button(text("Open account settings in the browser").size(12)).on_press(Event::OpenWeb))
        .push(
            row![
                text("Sessions").size(14).width(Length::Fill),
                button(text("Reload").size(12)).on_press_maybe((!state.loading).then_some(Event::Reload)),
            ],
        );
    if state.loading && state.sessions.is_none() {
        body = body.push(text("Loading\u{2026}").size(12));
    }
    for session in state.sessions.iter().flatten() {
        body = body.push(
            row![
                text(session_line(session)).size(12).width(Length::Fill),
                button(text("Log out").size(12)).on_press_maybe(
                    (!state.busy).then(|| Event::Ask(Pending::Logout(session.id_hash.clone()))),
                ),
            ]
            .spacing(8),
        );
    }
    if let Some(list) = &state.sessions {
        body = body.push(
            text(if list.is_empty() {
                "Discord listed no sessions."
            } else {
                "One of these is probably fastcord; logging it out ends this login."
            })
            .size(12)
            .style(text::secondary),
        );
    }
    if let Some(pending) = &state.pending {
        let question = match pending {
            Pending::Logout(_) => "Log that session out?".to_owned(),
            Pending::Remove(id) => format!(
                "Forget the saved login for {}? Its token is deleted from this computer; the account stays logged in elsewhere.",
                index.get(*id).map_or_else(|| id.to_string(), |e| e.label())
            ),
        };
        body = body.push(
            row![
                text(question).size(13).width(Length::Fill),
                button(text("Confirm").size(12))
                    .on_press_maybe((!state.busy).then_some(Event::Confirm)),
                button(text("Cancel").size(12)).on_press(Event::Cancel),
            ]
            .spacing(8),
        );
    }
    if let Some(notice) = &state.notice {
        body = body.push(text(notice.clone()).size(12).style(text::danger));
    }
    Some(
        column![
            row![
                text("Accounts and security").size(13).width(Length::Fill),
                button(text("Close").size(13)).on_press(Event::Toggle),
            ],
            scrollable(body).height(Length::Fixed(320.0)),
        ]
        .spacing(6)
        .max_width(720)
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    fn client() -> RestClient {
        RestClient::new(
            fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
            fastcord_discord::gateway::ClientProperties::web(
                fastcord_discord::gateway::HostOs::Linux,
                "en-US",
                1,
            ),
        )
        .unwrap()
    }

    fn session(hash: &str) -> AuthSession {
        AuthSession {
            id_hash: hash.to_owned(),
            os: "Windows".into(),
            platform: "Desktop".into(),
            location: String::new(),
            last_used: Some("2026-10-20T08:15:00.000000+00:00".into()),
        }
    }

    fn mfa_refusal(status: StatusCode) -> RestError {
        RestError::Refused(fastcord_discord::Refusal::from_body(
            status,
            include_bytes!("../../../fixtures/rest/auth-sessions-mfa-required-synthetic.json"),
        ))
    }

    fn listed() -> State {
        State {
            open: true,
            sessions: Some(vec![session("a"), session("b")]),
            ..State::default()
        }
    }

    #[test]
    fn a_logout_needs_an_explicit_confirm_and_is_sent_once() {
        let client = client();
        let mut state = listed();
        // Confirm with nothing asked sends nothing.
        let _ = state.update(Event::Confirm, &client);
        assert!(!state.busy);
        // Asking is not sending.
        let _ = state.update(Event::Ask(Pending::Logout("a".into())), &client);
        assert!(!state.busy && state.pending.is_some());
        // Cancel drops the question.
        let _ = state.update(Event::Cancel, &client);
        assert!(state.pending.is_none());
        let _ = state.update(Event::Ask(Pending::Logout("a".into())), &client);
        let _ = state.update(Event::Confirm, &client);
        assert!(state.busy && state.pending.is_none());
        // While in flight nothing new can be asked or confirmed.
        let _ = state.update(Event::Ask(Pending::Logout("b".into())), &client);
        assert!(state.pending.is_none());
        let _ = state.update(Event::Confirm, &client);
        let generation = state.generation;
        let _ = state.update(Event::LoggedOut(generation, "a".into(), Ok(())), &client);
        assert!(!state.busy);
        let hashes: Vec<_> = state
            .sessions
            .iter()
            .flatten()
            .map(|s| &s.id_hash)
            .collect();
        assert_eq!(hashes, ["b"]);
    }

    #[test]
    fn an_mfa_demand_is_shown_with_discords_reason_and_not_retried() {
        let client = client();
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let mut state = listed();
            let _ = state.update(Event::Ask(Pending::Logout("a".into())), &client);
            let _ = state.update(Event::Confirm, &client);
            assert!(state.busy);
            let generation = state.generation;
            let refused = Err(mfa_refusal(status));
            let event = Event::LoggedOut(generation, "a".into(), refused);
            // Not a revoked token: the account is not stopped.
            assert!(!State::is_rejected(&event));
            let (task, action) = state.update(event, &client);
            assert!(action.is_none());
            drop(task);
            assert!(!state.busy, "no request stays in flight");
            assert!(state.pending.is_none(), "nothing is queued to retry");
            assert_eq!(state.sessions.as_ref().unwrap().len(), 2);
            let notice = state.notice.as_deref().unwrap();
            assert!(
                notice.contains("Multi-factor authentication required"),
                "{notice}"
            );
            assert!(
                notice.contains("Confirm in the official client"),
                "{notice}"
            );
            // A late duplicate answer changes nothing.
            let generation = state.generation;
            let _ = state.update(Event::LoggedOut(generation, "a".into(), Ok(())), &client);
            assert_eq!(state.sessions.as_ref().unwrap().len(), 2);
        }
        // An ordinary 401 still stops the account.
        let bare = Event::Sessions(0, Err(RestError::AuthenticationRequired));
        assert!(State::is_rejected(&bare));
    }

    #[test]
    fn an_ambiguous_failure_says_it_may_have_been_applied() {
        let mut state = listed();
        state.busy = true;
        let error = RestError::Retryable(RetryableFailure::Server(StatusCode::BAD_GATEWAY));
        state.finish_logout("a", Err(error));
        assert!(state.notice.as_deref().unwrap().contains("may have been"));
        assert_eq!(state.sessions.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn remove_and_switch_are_handed_to_the_app_and_stale_answers_are_dropped() {
        let client = client();
        let mut state = listed();
        let (_, action) = state.update(Event::Switch(Snowflake(5)), &client);
        assert_eq!(action, Some(Action::Switch(Snowflake(5))));
        let _ = state.update(Event::Ask(Pending::Remove(Snowflake(6))), &client);
        let (_, action) = state.update(Event::Confirm, &client);
        assert_eq!(action, Some(Action::Remove(Snowflake(6))));
        let before = state.generation;
        state.clear();
        let _ = state.update(Event::Sessions(before, Ok(vec![session("z")])), &client);
        assert!(state.sessions.is_none());
    }

    #[test]
    fn the_view_builds_and_the_session_line_reads_well() {
        let mut index = Index::default();
        index.reconcile(&[Snowflake(1), Snowflake(2)]);
        let mut state = listed();
        state.overview = Some(SecurityOverview {
            mfa_enabled: Some(true),
            email_verified: Some(true),
            phone_present: Some(false),
        });
        state.pending = Some(Pending::Remove(Snowflake(2)));
        assert!(view(&state, &index, Snowflake(1), true).is_some());
        assert!(view(&State::default(), &index, Snowflake(1), true).is_none());
        assert_eq!(
            session_line(&session("a")),
            "Desktop \u{b7} Windows \u{b7} last used 2026-10-20 08:15"
        );
    }
}
