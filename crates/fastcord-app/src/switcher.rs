//! The quick switcher, keyboard navigation and the shortcuts help
//! (milestone 50).
//!
//! The switcher is an overlay with a search box over the conversations the
//! reducer already holds (`navigation.quick`: accessible channels, threads,
//! DMs and group DMs, at most 50, matched by name; no history is loaded and
//! nothing goes to the network). Selecting one opens it exactly like a sidebar
//! click: a voice or Stage channel opens its text chat, never the call.
//!
//! Keys: only chords with a modifier (or F1/Escape/arrows while an overlay is
//! open) are interpreted here, never a plain text key, so an IME composition is
//! left alone. `Ctrl+K` is matched on the physical key so it works on any
//! keyboard layout (Cyrillic, Japanese, ...). `Ctrl` is `Cmd` on macOS.
//!
//! | Keys | Action |
//! |---|---|
//! | `Ctrl+K` | Quick switcher |
//! | `Alt+Up` / `Alt+Down` | Previous / next channel (or DM) |
//! | `Alt+Shift+Up` / `Alt+Shift+Down` | Previous / next unread conversation |
//! | `Ctrl+Alt+Up` / `Ctrl+Alt+Down` | Previous / next server |
//! | `Escape` | Close overlay, then cancel edit or reply |
//! | `F1`, `Ctrl+Shift+?` | Shortcuts help |

use fastcord_discord::state::navigation::{QuickEntry, QuickKind, Step, StepScope};
use fastcord_model::Snowflake;
use iced::keyboard::{self, Key, Modifiers};
use iced::widget::{
    button, column, container, mouse_area, opaque, row, scrollable, space, text, text_input,
};
use iced::{Element, Length};

use crate::style;

pub const INPUT_ID: &str = "quick-switcher-input";

/// A chord the app interprets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    Switcher,
    Help,
    /// `captured`: a focused widget already used the key (the composer's own
    /// Escape binding), so the composer is not asked again.
    Escape {
        captured: bool,
    },
    /// Up (-1) or down (+1) in the switcher's results.
    Move(i32),
    Step(Step),
}

/// Classifies a key press. Pure: plain text keys never match.
pub fn shortcut(
    key: &Key,
    physical: &keyboard::key::Physical,
    modifiers: Modifiers,
    captured: bool,
) -> Option<Shortcut> {
    use keyboard::key::{Code, Named, Physical};
    let alt = modifiers.alt();
    let shift = modifiers.shift();
    let ctrl = modifiers.control();
    let command = modifiers.command();
    let on = |code: Code| matches!(physical, Physical::Code(c) if *c == code);
    match key.as_ref() {
        Key::Named(Named::F1) => return Some(Shortcut::Help),
        Key::Named(Named::Escape) if modifiers.is_empty() => {
            return Some(Shortcut::Escape { captured });
        }
        Key::Named(direction @ (Named::ArrowUp | Named::ArrowDown)) => {
            let forward = direction == Named::ArrowDown;
            let scope = match (ctrl, alt, shift, modifiers.logo()) {
                (false, false, false, false) => {
                    return Some(Shortcut::Move(if forward { 1 } else { -1 }));
                }
                (false, true, false, false) => StepScope::Channel,
                (false, true, true, false) => StepScope::Unread,
                (true, true, false, false) => StepScope::Server,
                _ => return None,
            };
            return Some(Shortcut::Step(Step { scope, forward }));
        }
        _ => {}
    }
    if command && !alt && !shift && on(Code::KeyK) {
        return Some(Shortcut::Switcher);
    }
    let question = matches!(key.as_ref(), Key::Character("?")) || on(Code::Slash);
    (command && shift && !alt && question).then_some(Shortcut::Help)
}

#[derive(Clone, Debug)]
pub enum Event {
    Query(String),
    Move(i32),
    /// Enter in the search box: open the highlighted result.
    Submit,
    /// A result was clicked.
    Pick(usize),
    Close,
}

/// What the app does after an event.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Open(QuickEntry),
}

#[derive(Default)]
pub struct State {
    open: bool,
    help: bool,
    query: String,
    selected: usize,
}

impl State {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn help_open(&self) -> bool {
        self.help
    }

    /// The query to keep the reducer's result list for, while the switcher is open.
    pub fn wanted(&self) -> Option<&str> {
        self.open.then_some(self.query.as_str())
    }

    pub fn toggle(&mut self) {
        self.help = false;
        self.query.clear();
        self.selected = 0;
        self.open = !self.open;
    }

    pub fn toggle_help(&mut self) {
        self.open = false;
        self.help = !self.help;
    }

    /// Closes whichever overlay is open. Returns whether one was.
    pub fn close(&mut self) -> bool {
        let was = self.open || self.help;
        self.open = false;
        self.help = false;
        self.query.clear();
        self.selected = 0;
        was
    }

    pub fn update(&mut self, event: Event, entries: &[QuickEntry]) -> Action {
        match event {
            Event::Query(query) => {
                self.query = query
                    .chars()
                    .take(fastcord_discord::state::navigation::MAX_QUICK_QUERY_CHARS)
                    .collect();
                self.selected = 0;
            }
            Event::Move(delta) => {
                let last = entries.len().saturating_sub(1) as i64;
                self.selected = (self.selected as i64 + i64::from(delta)).clamp(0, last) as usize;
            }
            Event::Submit => return self.pick(self.selected, entries),
            Event::Pick(index) => return self.pick(index, entries),
            Event::Close => {
                self.close();
            }
        }
        Action::None
    }

    fn pick(&mut self, index: usize, entries: &[QuickEntry]) -> Action {
        match entries.get(index) {
            Some(entry) => {
                let entry = entry.clone();
                self.close();
                Action::Open(entry)
            }
            None => Action::None,
        }
    }
}

fn marker(entry: &QuickEntry) -> &'static str {
    match entry.kind {
        QuickKind::Channel => "#",
        QuickKind::Voice => "\u{1f50a}",
        QuickKind::Thread => "\u{1f9f5}",
        QuickKind::Direct => "@",
        QuickKind::Group => "@",
    }
}

pub fn view<'a>(
    state: &'a State,
    entries: &'a [QuickEntry],
    has_draft: impl Fn(Snowflake) -> bool,
) -> Element<'a, Event> {
    let mut list = column![].spacing(2);
    for (index, entry) in entries.iter().enumerate() {
        let mut line = row![
            text(marker(entry)).size(14).width(22),
            text(&entry.name).size(14),
        ]
        .spacing(6)
        .align_y(iced::alignment::Vertical::Center);
        if !entry.guild_name.is_empty() {
            line = line.push(text(&entry.guild_name).size(12).color(style::MUTED));
        }
        if entry.kind == QuickKind::Voice {
            line = line.push(text("opens chat").size(11).color(style::MUTED));
        }
        line = line.push(space::horizontal());
        if has_draft(entry.channel) {
            line = line.push(text("draft").size(11).color(style::YELLOW));
        }
        if entry.mentions > 0 {
            line = line.push(
                text(format!("@{}", entry.mentions))
                    .size(12)
                    .color(style::RED),
            );
        } else if entry.unread {
            line = line.push(text("\u{2022}").size(14).color(style::BRIGHT));
        }
        list = list.push(
            button(line)
                .on_press(Event::Pick(index))
                .width(Length::Fill)
                .padding([5, 8])
                .style(style::row_button(index == state.selected, entry.unread)),
        );
    }
    if entries.is_empty() {
        list = list.push(
            text("No matching conversation.")
                .size(13)
                .color(style::MUTED),
        );
    }
    let card = container(
        column![
            text_input(
                "Jump to a conversation, or paste a Discord link",
                &state.query
            )
            .id(INPUT_ID)
            .on_input(Event::Query)
            .on_submit(Event::Submit)
            .size(15)
            .padding(8),
            scrollable(list).height(Length::Shrink),
            text("Enter opens, Up/Down chooses, Escape closes. A pasted Discord link opens too.")
                .size(11)
                .color(style::MUTED),
        ]
        .spacing(8),
    )
    .padding(12)
    .width(480)
    .max_height(460)
    .style(style::card);
    backdrop(card.into())
}

const HELP: &[(&str, &str)] = &[
    ("Ctrl+K", "Quick switcher"),
    ("Alt+Up / Alt+Down", "Previous / next channel or DM"),
    ("Alt+Shift+Up / Down", "Previous / next unread conversation"),
    ("Ctrl+Alt+Up / Down", "Previous / next server"),
    ("Escape", "Close overlay, cancel edit or reply"),
    ("F1 or Ctrl+Shift+?", "This list"),
    ("Enter / Shift+Enter", "Send / new line"),
];

pub fn help_view<'a>() -> Element<'a, Event> {
    let mut lines = column![text("Keyboard shortcuts").size(16)].spacing(6);
    for (keys, what) in HELP {
        lines = lines.push(row![text(*keys).size(13).width(190), text(*what).size(13)].spacing(8));
    }
    lines = lines.push(
        text("Ctrl is Cmd on macOS. Voice shortcuts are set in the audio settings.")
            .size(11)
            .color(style::MUTED),
    );
    lines = lines.push(crate::icons::close(Some(Event::Close)));
    backdrop(
        container(lines)
            .padding(16)
            .width(480)
            .style(style::card)
            .into(),
    )
}

/// A dimmed full-window layer: a click outside the card closes it.
fn backdrop<'a>(card: Element<'a, Event>) -> Element<'a, Event> {
    mouse_area(
        container(opaque(card))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(iced::Padding::new(0.0).top(80.0))
            .align_x(iced::alignment::Horizontal::Center)
            .style(|_| container::Style {
                background: Some(iced::Color::from_rgba(0.0, 0.0, 0.0, 0.5).into()),
                ..container::Style::default()
            }),
    )
    .on_press(Event::Close)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyboard::key::{Code, Named, Physical};

    fn entry(id: u64, name: &str) -> QuickEntry {
        QuickEntry {
            channel: Snowflake(id),
            guild: Some(Snowflake(1)),
            guild_name: "S".to_owned(),
            name: name.to_owned(),
            kind: QuickKind::Channel,
            unread: false,
            mentions: 0,
        }
    }

    fn chord(key: Key, code: Code, modifiers: Modifiers) -> Option<Shortcut> {
        shortcut(&key, &Physical::Code(code), modifiers, false)
    }

    #[test]
    fn chords_map_and_plain_text_keys_never_do() {
        let ctrl = Modifiers::CTRL;
        let k = || Key::Character("k".into());
        if cfg!(target_os = "macos") {
            assert_eq!(
                chord(k(), Code::KeyK, Modifiers::LOGO),
                Some(Shortcut::Switcher)
            );
        } else {
            assert_eq!(chord(k(), Code::KeyK, ctrl), Some(Shortcut::Switcher));
            // A Cyrillic layout reports another character for the same key.
            assert_eq!(
                chord(Key::Character("\u{43b}".into()), Code::KeyK, ctrl),
                Some(Shortcut::Switcher)
            );
        }
        // No modifier: typing, including IME text, is never intercepted.
        assert_eq!(chord(k(), Code::KeyK, Modifiers::empty()), None);
        assert_eq!(
            chord(Key::Character("a".into()), Code::KeyA, Modifiers::SHIFT),
            None
        );
        assert_eq!(chord(k(), Code::KeyK, Modifiers::ALT), None);
        assert_eq!(
            chord(Key::Named(Named::Enter), Code::Enter, Modifiers::empty()),
            None
        );
        assert_eq!(
            chord(Key::Named(Named::Space), Code::Space, Modifiers::empty()),
            None
        );
        assert_eq!(
            chord(Key::Named(Named::F1), Code::F1, Modifiers::empty()),
            Some(Shortcut::Help)
        );
        assert_eq!(
            shortcut(
                &Key::Named(Named::Escape),
                &Physical::Code(Code::Escape),
                Modifiers::empty(),
                true
            ),
            Some(Shortcut::Escape { captured: true })
        );
        assert_eq!(
            chord(Key::Named(Named::Escape), Code::Escape, Modifiers::ALT),
            None
        );
    }

    #[test]
    fn arrows_step_by_modifier() {
        let step = |modifiers, down| {
            let key = Key::Named(if down {
                Named::ArrowDown
            } else {
                Named::ArrowUp
            });
            chord(key, Code::ArrowUp, modifiers)
        };
        let of = |scope, forward| Some(Shortcut::Step(Step { scope, forward }));
        assert_eq!(step(Modifiers::ALT, true), of(StepScope::Channel, true));
        assert_eq!(step(Modifiers::ALT, false), of(StepScope::Channel, false));
        assert_eq!(
            step(Modifiers::ALT | Modifiers::SHIFT, true),
            of(StepScope::Unread, true)
        );
        assert_eq!(
            step(Modifiers::CTRL | Modifiers::ALT, false),
            of(StepScope::Server, false)
        );
        assert_eq!(step(Modifiers::empty(), false), Some(Shortcut::Move(-1)));
        assert_eq!(step(Modifiers::empty(), true), Some(Shortcut::Move(1)));
        // Other combinations belong to the text editor.
        assert_eq!(step(Modifiers::SHIFT, true), None);
        assert_eq!(step(Modifiers::CTRL, true), None);
    }

    #[test]
    fn help_has_two_chords() {
        let ctrl_shift = if cfg!(target_os = "macos") {
            Modifiers::LOGO | Modifiers::SHIFT
        } else {
            Modifiers::CTRL | Modifiers::SHIFT
        };
        assert_eq!(
            chord(Key::Character("?".into()), Code::Slash, ctrl_shift),
            Some(Shortcut::Help)
        );
        assert_eq!(
            chord(Key::Character("/".into()), Code::Slash, Modifiers::empty()),
            None
        );
    }

    #[test]
    fn the_selection_is_clamped_and_submit_opens_it_and_closes() {
        let entries = [entry(1, "a"), entry(2, "b"), entry(3, "c")];
        let mut state = State::default();
        state.toggle();
        assert_eq!(state.wanted(), Some(""));
        state.update(Event::Move(-1), &entries);
        assert_eq!(state.selected, 0);
        state.update(Event::Move(5), &entries);
        assert_eq!(state.selected, 2);
        // A new query starts again from the best match.
        state.update(Event::Query("x".repeat(500)), &entries);
        assert_eq!(state.selected, 0);
        assert_eq!(state.query.chars().count(), 64);
        state.update(Event::Move(1), &entries);
        assert_eq!(
            state.update(Event::Submit, &entries),
            Action::Open(entries[1].clone())
        );
        assert!(!state.is_open() && state.wanted().is_none());
        // Nothing to open: stays open.
        state.toggle();
        assert_eq!(state.update(Event::Submit, &[]), Action::None);
        assert!(state.is_open());
        assert_eq!(state.update(Event::Pick(7), &entries), Action::None);
        assert!(state.close());
        assert!(!state.close());
    }

    #[test]
    fn help_and_switcher_exclude_each_other() {
        let mut state = State::default();
        state.toggle();
        state.toggle_help();
        assert!(state.help_open() && !state.is_open());
        state.toggle();
        assert!(state.is_open() && !state.help_open());
    }
}
