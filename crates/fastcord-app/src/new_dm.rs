//! "New message": pick friends by name to open a conversation, or several
//! to start a group. Typing IDs is still possible, as a secondary way inside
//! the same dialog. Opening goes through the existing private-conversation
//! request, so its limits and notices are unchanged.

use fastcord_model::{Relationship, RelationshipKind, Snowflake};
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Alignment, Element, Length};

use crate::i18n::{tr, trn};
use crate::{Message, style, ui};

/// Rows shown at once; the search narrows the rest.
const SHOWN: usize = 50;
/// A private conversation holds at most this many other people.
const MAX_PEOPLE: usize = 9;

#[derive(Clone, Debug)]
pub enum Event {
    Open,
    Close,
    Query(String),
    Toggle(Snowflake),
    Ids(String),
    Create,
}

#[derive(Debug, Default)]
pub struct State {
    pub open: bool,
    query: String,
    selected: Vec<Snowflake>,
    ids: String,
}

impl State {
    pub fn update(&mut self, event: Event) {
        match event {
            Event::Open => {
                *self = Self {
                    open: true,
                    ..Self::default()
                }
            }
            Event::Close => *self = Self::default(),
            Event::Query(query) => self.query = query.chars().take(100).collect(),
            Event::Toggle(id) => {
                if let Some(at) = self.selected.iter().position(|s| *s == id) {
                    self.selected.remove(at);
                } else if self.selected.len() < MAX_PEOPLE {
                    self.selected.push(id);
                }
            }
            Event::Ids(ids) => self.ids = ids.chars().take(256).collect(),
            Event::Create => {}
        }
    }

    /// The recipients as the private-conversation request takes them: the
    /// chosen friends, then any typed IDs.
    pub fn recipients(&self) -> String {
        let mut all: Vec<String> = self.selected.iter().map(|id| id.0.to_string()).collect();
        if !self.ids.trim().is_empty() {
            all.push(self.ids.trim().to_owned());
        }
        all.join(",")
    }

    fn ready(&self) -> bool {
        !self.selected.is_empty() || !self.ids.trim().is_empty()
    }
}

fn matches(relationship: &Relationship, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || relationship
            .user
            .display_name()
            .to_lowercase()
            .contains(&query)
        || relationship.user.username.to_lowercase().contains(&query)
}

pub fn view<'a>(
    state: &'a State,
    relationships: &'a [Relationship],
    notice: Option<&'a str>,
) -> Option<Element<'a, Message>> {
    if !state.open {
        return None;
    }
    let event = Message::NewDm;
    let friends: Vec<_> = relationships
        .iter()
        .filter(|r| r.kind == RelationshipKind::Friend && matches(r, &state.query))
        .take(SHOWN)
        .collect();
    let mut list = column![].spacing(2);
    for friend in &friends {
        let user = &friend.user;
        let chosen = state.selected.contains(&user.id);
        list = list.push(
            button(
                row![
                    ui::initials(user.display_name(), user.id.0, 32.0),
                    column![
                        text(user.display_name().to_owned()).size(14),
                        text(format!("@{}", user.username))
                            .size(12)
                            .color(style::MUTED),
                    ]
                    .width(Length::Fill),
                    crate::icons::sized(
                        if chosen {
                            crate::icons::Icon::Check
                        } else {
                            crate::icons::Icon::Plus
                        },
                        if chosen { style::BRIGHT } else { style::MUTED },
                        16.0,
                    ),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .on_press(event(Event::Toggle(user.id)))
            .width(Length::Fill)
            .padding([6, 8])
            .style(style::row_button(chosen, chosen)),
        );
    }
    if friends.is_empty() {
        list = list.push(ui::note(if state.query.trim().is_empty() {
            tr("You have no friends here yet. Add people by ID below.")
        } else {
            tr("No friend matches this name.")
        }));
    }
    let typed = state
        .ids
        .split([',', ' '])
        .filter(|id| !id.is_empty())
        .count();
    let action = if state.selected.len() + typed > 1 {
        tr("Create group chat")
    } else {
        tr("Open conversation")
    };
    let mut body = column![
        text_input(tr("Find a friend by name"), &state.query)
            .id("new-dm-search")
            .on_input(|q| Message::NewDm(Event::Query(q)))
            .padding(10),
    ]
    .spacing(12);
    if !state.selected.is_empty() {
        body = body.push(ui::note(trn(
            state.selected.len() as u64,
            "{} person selected",
            "{} people selected",
        )));
    }
    body = body
        .push(
            container(scrollable(list).height(Length::Fill))
                .height(Length::Fixed(280.0))
                .width(Length::Fill),
        )
        .push(ui::label(tr("OR ADD BY USER ID")))
        .push(
            text_input(tr("IDs separated by commas"), &state.ids)
                .on_input(|ids| Message::NewDm(Event::Ids(ids)))
                .on_submit_maybe(state.ready().then_some(event(Event::Create)))
                .padding(10),
        );
    if let Some(notice) = notice {
        body = body.push(ui::note(tr(notice)));
    }
    body = body.push(ui::primary(
        action,
        state.ready().then_some(event(Event::Create)),
    ));
    Some(ui::modal(
        ui::dialog(tr("New message"), body, event(Event::Close), 460.0),
        event(Event::Close),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipients_join_chosen_friends_and_typed_ids_and_stop_at_nine() {
        let mut state = State::default();
        state.update(Event::Open);
        assert!(!state.ready());
        for id in 1..=12 {
            state.update(Event::Toggle(Snowflake(id)));
        }
        assert_eq!(state.selected.len(), MAX_PEOPLE);
        state.update(Event::Toggle(Snowflake(1)));
        state.update(Event::Ids(" 77, 78 ".into()));
        assert_eq!(state.recipients(), "2,3,4,5,6,7,8,9,77, 78");
        state.update(Event::Close);
        assert!(!state.open && state.recipients().is_empty());
    }
}
