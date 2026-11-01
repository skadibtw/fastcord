//! Explicit selected-word spelling, with account-local editor ownership.
use super::*;
use fastcord_platform::spellcheck::{self, Error, Reply};
use iced::Task;
use iced::widget::{column, row};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(1);
#[derive(Clone, Debug)]
pub enum Event {
    Open,
    Language(u64, String),
    Done(u64, Result<Reply, Error>),
    Apply(u64, usize),
    Close(u64),
}
#[derive(Default)]
pub(super) struct State {
    serial: u64,
    task: Option<iced::task::Handle>,
    original: Option<(String, iced::widget::text_editor::Cursor)>,
    languages: Vec<String>,
    suggestions: Vec<String>,
    notice: Option<&'static str>,
}
impl Composer {
    #[cfg(feature = "fake-discord")]
    pub(crate) fn spelling_serial(&self) -> u64 {
        self.spelling.serial
    }

    pub(super) fn cancel_spelling(&mut self) {
        self.spelling = State::default();
    }
    fn spelling_matches(&self) -> bool {
        self.spelling
            .original
            .as_ref()
            .is_some_and(|(text, cursor)| {
                *text == self.content.text() && *cursor == self.content.cursor()
            })
    }
    pub fn spelling(&mut self, event: Event) -> Task<super::Event> {
        match event {
            Event::Open => {
                self.cancel_spelling();
                self.spelling.serial = NEXT.fetch_add(1, Ordering::Relaxed);
                if !self
                    .content
                    .selection()
                    .as_deref()
                    .is_some_and(spellcheck::valid_word)
                {
                    self.spelling.notice = Some("Select one word to check its spelling.");
                    return Task::none();
                }
                self.spelling.original = Some((self.content.text(), self.content.cursor()));
                self.start_spelling(None)
            }
            Event::Language(serial, language)
                if serial == self.spelling.serial
                    && self.spelling_matches()
                    && self.spelling.task.is_none()
                    && self.spelling.languages.contains(&language) =>
            {
                let word = self.content.selection().unwrap_or_default();
                self.start_spelling(Some((word, language)))
            }
            Event::Done(serial, result)
                if serial == self.spelling.serial && self.spelling_matches() =>
            {
                self.spelling.task = None;
                self.spelling.notice = None;
                match result {
                    Ok(Reply::Dictionaries(languages)) => self.spelling.languages = languages,
                    Ok(Reply::Suggestions(words)) => {
                        self.spelling.languages.clear();
                        if words.is_empty() {
                            self.spelling.notice = Some("No spelling suggestions.");
                        }
                        self.spelling.suggestions = words;
                    }
                    Err(error) => {
                        self.spelling.notice = Some(match error {
                            Error::Unsupported => "Spelling is not supported on this platform.",
                            Error::Unavailable => {
                                "Local spelling service or dictionary is unavailable."
                            }
                            Error::Busy => "Spelling is busy. Try again shortly.",
                            _ => "Spelling check failed.",
                        })
                    }
                }
                Task::none()
            }
            Event::Apply(serial, index)
                if serial == self.spelling.serial && self.spelling_matches() =>
            {
                if let Some(word) = self.spelling.suggestions.get(index).cloned() {
                    self.insert_emoji(&word);
                }
                self.cancel_spelling();
                Task::none()
            }
            Event::Close(serial) if serial == self.spelling.serial => {
                self.cancel_spelling();
                Task::none()
            }
            _ => Task::none(),
        }
    }
    fn start_spelling(&mut self, request: Option<(String, String)>) -> Task<super::Event> {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        self.spelling.serial = serial;
        self.spelling.notice = Some("Checking local dictionaries…");
        self.spelling.suggestions.clear();
        let (task, handle) = Task::perform(spellcheck::request(request), move |result| {
            super::Event::Spelling(Event::Done(serial, result))
        })
        .abortable();
        self.spelling.task = Some(handle.abort_on_drop());
        task
    }
}
pub(super) fn view(composer: &Composer) -> Option<Element<'_, super::Event>> {
    let state = &composer.spelling;
    if state.original.is_none()
        && state.notice.is_none()
        && state.languages.is_empty()
        && state.suggestions.is_empty()
    {
        return None;
    }
    let heading = state.notice.unwrap_or(if state.languages.is_empty() {
        "Spelling suggestions"
    } else {
        "Choose an installed spelling dictionary:"
    });
    // Keep context and dismissal outside the single choice scroller. Fill
    // constrains wrapped labels to the composer instead of a narrow viewport.
    let header = row![
        text(tr(heading)).size(12).width(Length::Fill),
        small_button(
            "Close spelling",
            super::Event::Spelling(Event::Close(state.serial))
        ),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center)
    .width(Length::Fill);
    let mut rows = column![header].spacing(4).width(Length::Fill);
    if !state.languages.is_empty() || !state.suggestions.is_empty() {
        let mut choices = column![].spacing(2).width(Length::Fill);
        for language in &state.languages {
            choices = choices.push(button(text(language).size(12)).on_press(
                super::Event::Spelling(Event::Language(state.serial, language.clone())),
            ));
        }
        for (index, word) in state.suggestions.iter().enumerate() {
            choices = choices.push(
                button(text(word).size(12))
                    .on_press(super::Event::Spelling(Event::Apply(state.serial, index))),
            );
        }
        rows = rows.push(
            container(
                scrollable(choices)
                    .width(Length::Fill)
                    .height(Length::Shrink),
            )
            .width(Length::Fill)
            .max_height(100),
        );
    }
    Some(rows.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn selected(word: &str) -> Composer {
        let mut composer = Composer::default();
        composer.select(Some(Snowflake(1)));
        composer.insert_emoji(word);
        composer.perform(Action::SelectAll);
        composer.spelling.original = Some((composer.content.text(), composer.content.cursor()));
        composer.spelling.serial = 991;
        composer
    }
    #[test]
    fn replacement_preserves_unicode_and_is_rejected_after_motion_or_channel_change() {
        let mut composer = selected("café");
        composer.spelling.suggestions = vec!["cafés".into()];
        let _ = composer.spelling(Event::Apply(991, 0));
        assert_eq!(composer.content.text().trim_end(), "cafés");
        let mut composer = selected("helo");
        composer.spelling.suggestions = vec!["hello".into()];
        composer.perform(Action::Move(Motion::Left));
        let _ = composer.spelling(Event::Apply(991, 0));
        assert_eq!(composer.content.text().trim_end(), "helo");
        composer.select(Some(Snowflake(2)));
        let _ = composer.spelling(Event::Done(
            991,
            Ok(Reply::Suggestions(vec!["hello".into()])),
        ));
        assert!(composer.spelling.suggestions.is_empty());
    }
    #[test]
    fn nfd_and_ime_committed_selections_keep_exact_unicode() {
        for (before, after) in [
            ("cafe\u{301}", "cafe\u{301}s"),
            ("привеет", "привет"),
            ("한글", "한글로"),
        ] {
            assert!(spellcheck::valid_word(before));
            let mut composer = selected(before);
            assert_eq!(composer.content.selection().as_deref(), Some(before));
            composer.spelling.suggestions = vec![after.into()];
            let _ = composer.spelling(Event::Apply(991, 0));
            assert_eq!(composer.content.text().trim_end(), after);
            assert!(composer.content.selection().is_none());
        }
    }
    #[test]
    fn old_view_actions_cannot_affect_a_replacement_session() {
        let mut composer = selected("helo");
        let _ = composer.spelling(Event::Open);
        let old = composer.spelling.serial;
        let _ = composer.spelling(Event::Done(
            old,
            Ok(Reply::Suggestions(vec!["hello".into()])),
        ));
        // A new session can have the same exact text and selection; the epoch
        // must protect buttons independently of text/cursor equality.
        let _ = composer.spelling(Event::Open);
        let picker = composer.spelling.serial;
        let _ = composer.spelling(Event::Done(
            picker,
            Ok(Reply::Dictionaries(vec!["en_US".into()])),
        ));
        let _ = composer.spelling(Event::Language(old, "en_US".into()));
        assert!(composer.spelling.task.is_none());
        let _ = composer.spelling(Event::Language(picker, "en_US".into()));
        let current = composer.spelling.serial;
        let _ = composer.spelling(Event::Done(
            current,
            Ok(Reply::Suggestions(vec!["help".into()])),
        ));
        let _ = composer.spelling(Event::Apply(old, 0));
        let _ = composer.spelling(Event::Close(old));
        assert_eq!(composer.content.selection().as_deref(), Some("helo"));
        assert_eq!(composer.spelling.suggestions, ["help"]);
        assert!(composer.spelling.task.is_none());
        assert_eq!(composer.spelling.serial, current);
        let _ = composer.spelling(Event::Apply(current, 0));
        assert_eq!(composer.content.text().trim_end(), "help");
    }
    #[test]
    fn account_reset_cannot_accept_old_result_even_with_identical_selection() {
        let mut composer = selected("helo");
        composer.clear();
        composer.select(Some(Snowflake(1)));
        composer.insert_emoji("helo");
        composer.perform(Action::SelectAll);
        let _ = composer.spelling(Event::Done(
            991,
            Ok(Reply::Suggestions(vec!["hello".into()])),
        ));
        assert!(composer.spelling.suggestions.is_empty());
    }
}
