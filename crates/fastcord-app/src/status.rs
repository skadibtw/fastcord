//! Own status and presence dots (SPEC §16 milestone 46).
//!
//! Other people's presence comes from the reducer's shared table (READY,
//! PRESENCE_UPDATE); this module only draws it and holds what the account is
//! doing with its own status. A change is one explicit click and one
//! `PATCH /users/@me/settings`; Discord echoes it to every session as a
//! PRESENCE_UPDATE, which is what normally updates the display. Until it does
//! (invisible is never echoed as a dot we can see), the confirmed choice is
//! shown from `local`, which is dropped as soon as the table's own entry
//! differs from what it was when the choice was made.

use crate::ui::{button, primary_button};
use fastcord_discord::{RestClient, RestError, StatusChange};
use fastcord_model::{MAX_CUSTOM_STATUS_CHARS, Presence, Status};
use iced::font::Weight;
use iced::widget::{column, container, row, space, text, text_input};
use iced::{Color, Element, Font, Length, Task, alignment};

use crate::i18n::tr;
use crate::style;

/// What an account can choose (offline is never a choice).
pub const CHOICES: [Status; 4] = [Status::Online, Status::Idle, Status::Dnd, Status::Invisible];

const BOLD: Font = Font {
    weight: Weight::Bold,
    ..Font::DEFAULT
};

pub fn color(status: Status) -> Color {
    match status {
        Status::Online => style::GREEN,
        Status::Idle => style::YELLOW,
        Status::Dnd => style::RED,
        Status::Invisible | Status::Offline => style::MUTED,
    }
}

pub fn label(status: Status) -> &'static str {
    match status {
        Status::Online => tr("Online"),
        Status::Idle => tr("Idle"),
        Status::Dnd => tr("Do Not Disturb"),
        Status::Invisible => tr("Invisible"),
        Status::Offline => tr("Offline"),
    }
}

/// A small round dot; nothing (same size) when the presence is unknown, so an
/// unavailable presence is never drawn as offline.
pub fn dot<'a, M: 'a>(status: Option<Status>) -> Element<'a, M> {
    let size = 10.0;
    match status {
        Some(status) => container(space())
            .width(size)
            .height(size)
            .style(move |_| container::Style {
                background: Some(color(status).into()),
                border: iced::Border {
                    radius: (size / 2.0).into(),
                    ..iced::Border::default()
                },
                ..container::Style::default()
            })
            .into(),
        None => space().width(size).height(size).into(),
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    ToggleMenu,
    Pick(Status),
    Input(String),
    SaveCustom,
    ClearCustom,
    Done(u64, StatusChange, Option<Presence>, Result<(), RestError>),
}

/// A confirmed choice not yet seen coming back from the Gateway.
#[derive(Debug)]
struct Local {
    /// The table's own entry when the choice was made.
    base: Option<Presence>,
    status: Option<Status>,
    custom: Option<Option<String>>,
}

#[derive(Debug, Default)]
pub struct State {
    pub open: bool,
    input: String,
    pending: bool,
    notice: Option<String>,
    local: Option<Local>,
    /// Bumped on logout/new session: answers from an earlier one are dropped.
    epoch: u64,
}

impl State {
    pub fn reset(&mut self) {
        *self = Self {
            epoch: self.epoch + 1,
            ..Self::default()
        };
    }

    /// Logout purge: drops whatever a signed-out account left behind.
    pub fn purge(&mut self) {
        if self.open
            || self.pending
            || self.local.is_some()
            || self.notice.is_some()
            || !self.input.is_empty()
        {
            self.reset();
        }
    }

    /// The account's status: the confirmed local choice while the table still
    /// shows what it showed then, else the table.
    pub fn status(&self, own: Option<&Presence>) -> Option<Status> {
        match &self.local {
            Some(local) if local.base.as_ref() == own => local.status.or(own.map(|p| p.status)),
            _ => own.map(|p| p.status),
        }
    }

    pub fn custom<'a>(&'a self, own: Option<&'a Presence>) -> Option<&'a str> {
        match &self.local {
            Some(local) if local.base.as_ref() == own => match &local.custom {
                Some(custom) => custom.as_deref(),
                None => own.and_then(|p| p.custom.as_deref()),
            },
            _ => own.and_then(|p| p.custom.as_deref()),
        }
    }

    pub fn update(
        &mut self,
        event: Event,
        own: Option<&Presence>,
        rest: &RestClient,
    ) -> Task<Event> {
        let send = |this: &mut Self, change: StatusChange| {
            if this.pending {
                return Task::none();
            }
            this.pending = true;
            this.notice = None;
            let (rest, epoch, base) = (rest.clone(), this.epoch, own.cloned());
            Task::perform(
                {
                    let change = change.clone();
                    async move { rest.set_status(&change).await }
                },
                move |result| Event::Done(epoch, change.clone(), base.clone(), result),
            )
        };
        match event {
            Event::ToggleMenu => {
                self.open = !self.open;
                self.notice = None;
                self.input = self.custom(own).unwrap_or_default().to_owned();
            }
            Event::Pick(status) => return send(self, StatusChange::Status(status)),
            Event::Input(value) => {
                if value.chars().count() <= MAX_CUSTOM_STATUS_CHARS {
                    self.input = value;
                }
            }
            Event::SaveCustom => {
                return send(self, StatusChange::Custom(Some(self.input.clone())));
            }
            Event::ClearCustom => {
                self.input.clear();
                return send(self, StatusChange::Custom(None));
            }
            Event::Done(epoch, change, base, result) if epoch == self.epoch => {
                self.pending = false;
                match result {
                    Ok(()) => {
                        self.open = false;
                        let local = self.local.take().filter(|l| l.base == base);
                        let mut local = local.unwrap_or(Local {
                            base,
                            status: None,
                            custom: None,
                        });
                        match change {
                            StatusChange::Status(status) => local.status = Some(status),
                            StatusChange::Custom(text) => {
                                local.custom = Some(
                                    text.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()),
                                );
                            }
                        }
                        self.local = Some(local);
                    }
                    Err(error) => self.notice = Some(crate::friends::failure(error)),
                }
            }
            Event::Done(..) => {}
        }
        Task::none()
    }

    /// The panel above the user footer.
    pub fn menu<'a>(&'a self, own: Option<&Presence>, persistence: &'a str) -> Element<'a, Event> {
        let current = self.status(own);
        let mut choices = column![].spacing(2);
        for status in CHOICES {
            choices = choices.push(
                button(
                    row![dot(Some(status)), text(label(status)).size(13)]
                        .spacing(8)
                        .align_y(alignment::Vertical::Center),
                )
                .width(Length::Fill)
                .on_press_maybe((!self.pending).then_some(Event::Pick(status)))
                .style(style::row_button(current == Some(status), false)),
            );
        }
        let custom = row![
            text_input(tr("Custom status"), &self.input)
                .on_input(Event::Input)
                .on_submit(Event::SaveCustom)
                .size(13)
                .padding(6),
            primary_button(text(tr("Set")).size(12))
                .on_press_maybe((!self.pending).then_some(Event::SaveCustom))
                .style(style::action_button(false)),
            button(text(tr("Clear")).size(12))
                .on_press_maybe((!self.pending).then_some(Event::ClearCustom))
                .style(style::tool_button(true, false)),
        ]
        .spacing(4)
        .align_y(alignment::Vertical::Center);
        container(
            column![
                text(tr("Status"))
                    .size(12)
                    .font(BOLD)
                    .color(crate::style::muted()),
                choices,
                custom,
                text(self.notice.as_deref().unwrap_or(persistence))
                    .size(11)
                    .color(crate::style::muted()),
            ]
            .spacing(6),
        )
        .padding([8, 10])
        .style(style::footer)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rest() -> RestClient {
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            fastcord_discord::UserToken::new("dummy".into()),
            fastcord_discord::gateway::ClientProperties::bundled(
                fastcord_discord::gateway::HostOs::Linux,
                "en-US",
            ),
        )
        .unwrap()
    }

    fn own(status: Status, custom: Option<&str>) -> Presence {
        Presence {
            status,
            custom: custom.map(str::to_owned),
        }
    }

    fn confirmed(state: &mut State, change: StatusChange, base: Option<Presence>) {
        state.pending = true;
        let rest = rest();
        let _ = state.update(
            Event::Done(state.epoch, change, base.clone(), Ok(())),
            base.as_ref(),
            &rest,
        );
    }

    #[test]
    fn a_confirmed_choice_shows_until_the_table_moves_on() {
        let mut state = State::default();
        let base = Some(own(Status::Online, Some("hi")));
        assert_eq!(state.status(base.as_ref()), Some(Status::Online));
        confirmed(
            &mut state,
            StatusChange::Status(Status::Invisible),
            base.clone(),
        );
        assert_eq!(state.status(base.as_ref()), Some(Status::Invisible));
        assert_eq!(state.custom(base.as_ref()), Some("hi"));
        confirmed(
            &mut state,
            StatusChange::Custom(Some("  lunch ".into())),
            base.clone(),
        );
        assert_eq!(state.custom(base.as_ref()), Some("lunch"));
        assert_eq!(state.status(base.as_ref()), Some(Status::Invisible));
        // Another client changes the status: the table wins again.
        let moved = Some(own(Status::Dnd, None));
        assert_eq!(state.status(moved.as_ref()), Some(Status::Dnd));
        assert_eq!(state.custom(moved.as_ref()), None);
        // Blank custom text clears it.
        confirmed(
            &mut state,
            StatusChange::Custom(Some(" ".into())),
            base.clone(),
        );
        assert_eq!(state.custom(base.as_ref()), None);
    }

    #[test]
    fn the_menu_closes_after_a_confirmed_choice_and_stays_on_failure() {
        let mut state = State::default();
        let base = Some(own(Status::Online, None));
        for change in [
            StatusChange::Status(Status::Idle),
            StatusChange::Custom(Some("x".into())),
            StatusChange::Custom(None),
        ] {
            state.open = true;
            confirmed(&mut state, change, base.clone());
            assert!(!state.open);
        }
        state.open = true;
        let _ = state.update(
            Event::Done(
                state.epoch,
                StatusChange::Status(Status::Dnd),
                base.clone(),
                Err(RestError::AuthenticationRequired),
            ),
            base.as_ref(),
            &rest(),
        );
        assert!(state.open && state.notice.is_some());
    }

    #[test]
    fn unknown_presence_stays_unknown_and_stale_answers_are_dropped() {
        let mut state = State::default();
        assert_eq!(state.status(None), None);
        let old_epoch = state.epoch;
        state.reset();
        let rest = rest();
        let _ = state.update(
            Event::Done(old_epoch, StatusChange::Status(Status::Idle), None, Ok(())),
            None,
            &rest,
        );
        assert_eq!(state.status(None), None);
    }
}
