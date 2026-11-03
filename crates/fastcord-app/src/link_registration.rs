//! Per-user installation action state, deliberately independent of account/session state.
use crate::{Message, i18n::tr, ui};
use fastcord_platform::link_handler::Error;
use iced::widget::{column, text};
use iced::{Element, Task};

#[derive(Default)]
pub(crate) struct State {
    serial: u64,
    pending: Option<u64>,
    result: Option<Result<(), Error>>,
}
impl State {
    pub(crate) fn begin(&mut self) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        self.serial = self.serial.wrapping_add(1);
        self.pending = Some(self.serial);
        self.result = None;
        Some(self.serial)
    }
    pub(crate) fn finish(&mut self, serial: u64, result: Result<(), Error>) {
        if self.pending == Some(serial) {
            self.pending = None;
            self.result = Some(result);
        }
    }
    #[cfg(feature = "fake-discord")]
    pub(crate) fn fake_conflict(&mut self) {
        if let Some(serial) = self.pending.or_else(|| self.begin()) {
            self.finish(serial, Err(Error::Conflict));
        }
    }
    pub(crate) fn view(&self) -> Element<'_, Message> {
        let supported = cfg!(any(windows, target_os = "linux"));
        let status = if self.pending.is_some() {
            "Registering fastcord links…"
        } else if let Some(result) = self.result {
            match result {
                Ok(()) => "fastcord links are registered for this installation.",
                Err(Error::Conflict) => {
                    "Another handler or installation owns fastcord links. Change it in your system settings."
                }
                Err(Error::UnsafeExecutable) => "This executable path cannot be registered safely.",
                Err(Error::Unsupported) => "Link registration is not supported on this desktop.",
                Err(Error::Failed) => {
                    "Link registration failed. Check your system settings before trying again."
                }
            }
        } else if supported {
            "Open fastcord:// channel and invite links with this installation. Invites still ask you to join."
        } else {
            "Link registration is not supported on this desktop."
        };
        let mut content = column![
            ui::button(text(tr("Register fastcord:// links"))).on_press_maybe(
                (supported && self.pending.is_none()).then_some(Message::RegisterLinks)
            ),
            text(tr(status)).size(12),
        ]
        .spacing(6);
        if crate::fake_mode() {
            content = content.push(
                text(tr(
                    "Fake mode: link registration is simulated; system settings stay unchanged.",
                ))
                .size(12),
            );
        }
        content.into()
    }
}
pub(crate) fn task(serial: u64, simulated: bool) -> Task<Message> {
    if simulated {
        return Task::done(Message::LinksRegistered(serial, Ok(())));
    }
    Task::perform(
        async {
            tokio::task::spawn_blocking(fastcord_platform::link_handler::register)
                .await
                .unwrap_or(Err(Error::Failed))
        },
        move |result| Message::LinksRegistered(serial, result),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::variable_list::harness::Ui;

    #[test]
    fn actual_registration_control_disables_while_pending_and_ignores_stale_completion() {
        let mut state = State::default();
        if cfg!(any(windows, target_os = "linux")) {
            let messages = Ui::new(340.0, 220.0, state.view()).click(20.0, 15.0);
            assert!(messages.iter().any(|m| matches!(m, Message::RegisterLinks)));
        }
        let first = state.begin().unwrap();
        assert!(state.begin().is_none());
        let messages = Ui::new(340.0, 220.0, state.view()).click(20.0, 15.0);
        assert!(
            messages.is_empty(),
            "pending control must not create another OS action"
        );
        state.finish(first, Err(Error::Conflict));
        assert_eq!(state.result, Some(Err(Error::Conflict)));
        let next = state.begin().unwrap();
        state.finish(first, Ok(()));
        assert_eq!(state.pending, Some(next));
        assert_eq!(state.result, None);
        state.finish(next, Ok(()));
        assert_eq!(state.result, Some(Ok(())));
        state.finish(next, Err(Error::Failed));
        assert_eq!(state.result, Some(Ok(())), "duplicate result is inert");
    }
}
