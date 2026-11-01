//! One account/session-owned, bounded draft over currently held authorized entities.
//! Opening, filtering and choosing perform no I/O. Only Submit queues an action.
use crate::i18n::{tr, trf};
use crate::ui::button;
use crate::{
    component_view::select_at,
    interactions::Action,
    timeline::{Row, Snapshot},
};
use fastcord_discord::state::navigation::EntityCandidate;
use fastcord_model::{EntityKind, EntityValue};
use fastcord_model::{Message, Select, Snowflake};
use iced::widget::{column, row, scrollable, text, text_input};
use iced::{Element, Length};

pub const PAGE: usize = 24;
pub const QUERY_CHARS: usize = 64;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    author: Snowflake,
    application: Option<Snowflake>,
    webhook: Option<Snowflake>,
    bot: bool,
}
impl Owner {
    pub fn of(message: &Message) -> Self {
        Self {
            author: message.author.id,
            application: message.application_id,
            webhook: message.webhook_id,
            bot: message.author.bot,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub account: Snowflake,
    pub generation: u64,
    pub session: u64,
    pub channel: Snowflake,
    pub message: Snowflake,
    pub revision: u64,
    pub select: usize,
    pub owner: Owner,
}
impl Target {
    pub fn for_row(row: &Row, owner: (Snowflake, u64, u64), select: usize) -> Self {
        Self {
            account: owner.0,
            generation: owner.1,
            session: owner.2,
            channel: row.message.channel_id,
            message: row.message.id,
            revision: row.revision,
            select,
            owner: Owner::of(&row.message),
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Open(Target),
    Query(Target, String),
    Page(Target, bool),
    Toggle(Target, EntityValue),
    Clear(Target),
    Submit(Target),
    Close(Target),
}
impl Event {
    fn target(&self) -> Target {
        match self {
            Self::Open(t)
            | Self::Query(t, _)
            | Self::Page(t, _)
            | Self::Toggle(t, _)
            | Self::Clear(t)
            | Self::Submit(t)
            | Self::Close(t) => *t,
        }
    }
}
pub struct State {
    pub target: Target,
    expected: Select,
    selected: Vec<EntityValue>,
    query: String,
    page: usize,
    notice: Option<&'static str>,
}
impl State {
    fn row(target: Target, snapshot: &Snapshot) -> Option<&Row> {
        if target.session != snapshot.interaction_session
            || !snapshot.can_send
            || snapshot.channel_id != Some(target.channel)
        {
            return None;
        }
        snapshot.rows.iter().find(|row| {
            row.message.id == target.message
                && row.hidden.is_none()
                && row.component_pending.is_none()
                && Owner::of(&row.message) == target.owner
                && row.message.component_application().is_some()
        })
    }
    pub fn reconcile(
        current: &mut Option<Self>,
        owner: Option<(Snowflake, u64, u64)>,
        snapshot: &Snapshot,
    ) {
        let Some(state) = current else {
            return;
        };
        let row = Self::row(state.target, snapshot).filter(|row| {
            owner
                == Some((
                    state.target.account,
                    state.target.generation,
                    state.target.session,
                ))
                && select_at(&row.message.components, state.target.select) == Some(&state.expected)
        });
        if let Some(row) = row {
            state.target.revision = row.revision;
        } else {
            *current = None;
        }
    }
    pub fn update(
        current: &mut Option<Self>,
        event: Event,
        owner: Option<(Snowflake, u64, u64)>,
        guild: Option<Snowflake>,
        snapshot: &Snapshot,
    ) -> Option<Action> {
        let target = event.target();
        if owner != Some((target.account, target.generation, target.session)) {
            return None;
        }
        let row = Self::row(target, snapshot)?;
        if row.revision != target.revision {
            return None;
        }
        let select = select_at(&row.message.components, target.select)?;
        if select.disabled || !(5..=8).contains(&select.kind) {
            return None;
        }
        if matches!(event, Event::Open(_)) {
            *current = Some(Self {
                target,
                expected: select.clone(),
                selected: select.default_values.clone(),
                query: String::new(),
                page: 0,
                notice: None,
            });
            return None;
        }
        let state = current
            .as_mut()
            .filter(|state| state.target == target && state.expected == *select)?;
        state.notice = None;
        match event {
            Event::Close(_) => *current = None,
            Event::Query(_, query) => {
                state.query = query.chars().take(QUERY_CHARS).collect();
                state.page = 0;
            }
            Event::Page(_, next) => {
                let count = state.filtered(snapshot).len().div_ceil(PAGE).max(1);
                let shown = state.page.min(count - 1);
                state.page = if next {
                    (shown + 1).min(count - 1)
                } else {
                    shown.saturating_sub(1)
                };
            }
            Event::Clear(_) => state.selected.clear(),
            Event::Toggle(_, value) => {
                if let Some(at) = state.selected.iter().position(|held| *held == value) {
                    state.selected.remove(at);
                } else if state.selected.len() < usize::from(select.max_values)
                    && state.available(snapshot, value)
                {
                    state.selected.push(value);
                }
            }
            Event::Submit(_) if state.can_submit(snapshot) => {
                return Some(Action::EntityComponent {
                    target,
                    guild,
                    expected: Box::new(select.clone()),
                    values: state.selected.clone(),
                });
            }
            _ => {}
        }
        None
    }
    pub fn refused(&mut self, stopped: bool) {
        self.notice = Some(if stopped {
            "The connection is stopped. Press Reconnect first, then retry."
        } else {
            "That is not possible right now (busy or disconnected); try again."
        });
    }
    #[cfg(test)]
    pub fn notice(&self) -> Option<&'static str> {
        self.notice
    }
    fn available(&self, snapshot: &Snapshot, value: EntityValue) -> bool {
        snapshot.entity_candidates.iter().any(|candidate| {
            candidate.value == value && self.expected.accepts(value, candidate.channel_type)
        })
    }
    fn can_submit(&self, snapshot: &Snapshot) -> bool {
        self.selected.len() >= usize::from(self.expected.min_values)
            && self.selected.len() <= usize::from(self.expected.max_values)
            && self
                .selected
                .iter()
                .all(|value| self.available(snapshot, *value))
    }
    fn filtered<'a>(&self, snapshot: &'a Snapshot) -> Vec<&'a EntityCandidate> {
        let query = self.query.to_lowercase();
        snapshot
            .entity_candidates
            .iter()
            .filter(|candidate| {
                self.expected
                    .accepts(candidate.value, candidate.channel_type)
                    && candidate.label.to_lowercase().contains(&query)
            })
            .collect()
    }
    pub fn view<'a>(&'a self, snapshot: &'a Snapshot) -> Element<'a, Event> {
        let target = self.target;
        let candidates = self.filtered(snapshot);
        let pages = candidates.len().div_ceil(PAGE).max(1);
        let page = self.page.min(pages - 1);
        let mut content = column![
            text(tr("Choose users, roles, or channels")).size(20),
            text(tr(
                "Only a limited set of choices already loaded in this server or conversation is available. Search stays local."
            ))
            .size(12),
            text_input(tr("Search available choices"), &self.query)
                .on_input(move |query| Event::Query(target, query))
        ]
        .spacing(8);
        if let Some(notice) = self.notice {
            content = content.push(text(tr(notice)));
        }
        let mut selected_rows = column![].spacing(4);
        for value in &self.selected {
            let candidate = snapshot.entity_candidates.iter().find(|candidate| {
                candidate.value == *value && self.expected.accepts(*value, candidate.channel_type)
            });
            let label = candidate.map_or_else(
                || trf("Unavailable selection: {}", &[&value.id.to_string()]),
                |candidate| label(candidate, snapshot),
            );
            selected_rows = selected_rows
                .push(button(text(format!("× {label}"))).on_press(Event::Toggle(target, *value)));
        }
        if !self.selected.is_empty() {
            content = content.push(scrollable(selected_rows).height(100));
        }
        let mut choices = column![].spacing(4);
        for candidate in candidates.iter().skip(page * PAGE).take(PAGE) {
            let selected = self.selected.contains(&candidate.value);
            let title = format!(
                "{} {}",
                if selected { "✓" } else { "+" },
                label(candidate, snapshot)
            );
            choices = choices.push(
                button(text(title)).width(Length::Fill).on_press_maybe(
                    (selected || self.selected.len() < usize::from(self.expected.max_values))
                        .then_some(Event::Toggle(target, candidate.value)),
                ),
            );
        }
        if candidates.is_empty() {
            choices = choices.push(text(tr("No matching choices.")));
        }
        content = content.push(scrollable(choices).height(240));
        content = content.push(
            row![
                button(text(tr("Previous")))
                    .on_press_maybe((page > 0).then_some(Event::Page(target, false))),
                text(format!("{} / {}", page + 1, pages)),
                button(text(tr("Next")))
                    .on_press_maybe((page + 1 < pages).then_some(Event::Page(target, true)))
            ]
            .spacing(8),
        );
        content
            .push(
                text(trf(
                    "Select {}–{} choices.",
                    &[
                        &self.expected.min_values.to_string(),
                        &self.expected.max_values.to_string(),
                    ],
                ))
                .size(12),
            )
            .push(
                row![
                    button(text(tr("Clear"))).on_press(Event::Clear(target)),
                    button(text(tr("Submit")))
                        .on_press_maybe(self.can_submit(snapshot).then_some(Event::Submit(target))),
                    button(text(tr("Cancel"))).on_press(Event::Close(target))
                ]
                .spacing(8),
            )
            .into()
    }
}
fn label(candidate: &EntityCandidate, snapshot: &Snapshot) -> String {
    let kind = match candidate.value.kind {
        EntityKind::User => tr("User"),
        EntityKind::Role => tr("Role"),
        EntityKind::Channel => tr("Channel"),
    };
    let duplicate = candidate.label.is_empty()
        || snapshot.entity_candidates.iter().any(|other| {
            other.value != candidate.value
                && other.value.kind == candidate.value.kind
                && other.label == candidate.label
        });
    if duplicate {
        format!("{kind}: {} ({})", candidate.label, candidate.value.id)
    } else {
        format!("{kind}: {}", candidate.label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    const ACCOUNT: Option<(Snowflake, u64, u64)> = Some((Snowflake(1), 7, 0));
    fn fixture() -> (Snapshot, Target) {
        let message: Message = serde_json::from_value(json!({"id":"7","channel_id":"5","author":{"id":"9","username":"app","bot":true},"content":"","timestamp":"2026-01-01T00:00:00Z",
            "components":[{"type":7,"custom_id":"entities","min_values":0,"max_values":2,"default_values":[{"id":"99","type":"role"}]}]})).unwrap();
        let row = Row::new(Arc::new(message), 1);
        let target = Target::for_row(&row, ACCOUNT.unwrap(), 0);
        let candidates = (10..70)
            .map(|id| EntityCandidate {
                value: EntityValue {
                    kind: EntityKind::User,
                    id: Snowflake(id),
                },
                label: format!("Пользователь {id}"),
                channel_type: None,
            })
            .collect();
        (
            Snapshot {
                channel_id: Some(Snowflake(5)),
                can_send: true,
                rows: vec![row],
                entity_candidates: candidates,
                ..Snapshot::default()
            },
            target,
        )
    }
    #[test]
    fn entity_draft_requires_explicit_submit_and_keeps_unavailable_defaults_until_removed() {
        let (snapshot, target) = fixture();
        let mut state = None;
        assert!(State::update(&mut state, Event::Open(target), ACCOUNT, None, &snapshot).is_none());
        assert_eq!(state.as_ref().unwrap().selected.len(), 1);
        assert!(!state.as_ref().unwrap().can_submit(&snapshot));
        assert!(
            State::update(&mut state, Event::Submit(target), ACCOUNT, None, &snapshot).is_none()
        );
        assert!(
            State::update(&mut state, Event::Clear(target), ACCOUNT, None, &snapshot).is_none()
        );
        let value = snapshot.entity_candidates[0].value;
        assert!(
            State::update(
                &mut state,
                Event::Toggle(target, value),
                ACCOUNT,
                None,
                &snapshot
            )
            .is_none()
        );
        let Some(Action::EntityComponent { values, .. }) =
            State::update(&mut state, Event::Submit(target), ACCOUNT, None, &snapshot)
        else {
            panic!()
        };
        assert_eq!(values, [value]);
        let mut unavailable = snapshot.clone();
        unavailable.entity_candidates.clear();
        assert!(
            State::update(
                &mut state,
                Event::Submit(target),
                ACCOUNT,
                None,
                &unavailable
            )
            .is_none()
        );
        assert_eq!(
            state.as_ref().unwrap().selected,
            [value],
            "never silently drop intent"
        );
    }
    #[test]
    fn entity_draft_rebases_unrelated_revisions_but_rejects_stale_account_application_and_controls()
    {
        let (mut snapshot, target) = fixture();
        let mut state = None;
        State::update(&mut state, Event::Open(target), ACCOUNT, None, &snapshot);
        State::update(
            &mut state,
            Event::Clear(target),
            Some((Snowflake(2), 7, 0)),
            None,
            &snapshot,
        );
        assert_eq!(state.as_ref().unwrap().selected.len(), 1);
        snapshot.rows[0].revision += 1;
        Arc::make_mut(&mut snapshot.rows[0].message).content = "unrelated".into();
        State::reconcile(&mut state, ACCOUNT, &snapshot);
        assert_eq!(state.as_ref().unwrap().selected.len(), 1);
        State::update(&mut state, Event::Clear(target), ACCOUNT, None, &snapshot);
        assert_eq!(
            state.as_ref().unwrap().selected.len(),
            1,
            "old widget event rejected"
        );
        Arc::make_mut(&mut snapshot.rows[0].message).application_id = Some(Snowflake(88));
        State::reconcile(&mut state, ACCOUNT, &snapshot);
        assert!(state.is_none());
        let (mut snapshot, target) = fixture();
        State::update(&mut state, Event::Open(target), ACCOUNT, None, &snapshot);
        Arc::make_mut(&mut snapshot.rows[0].message)
            .components
            .clear();
        State::reconcile(&mut state, ACCOUNT, &snapshot);
        assert!(state.is_none());
        let (snapshot, target) = fixture();
        State::update(&mut state, Event::Open(target), ACCOUNT, None, &snapshot);
        State::reconcile(&mut state, Some((Snowflake(1), 8, 0)), &snapshot);
        assert!(state.is_none());
        let (mut snapshot, target) = fixture();
        State::update(&mut state, Event::Open(target), ACCOUNT, None, &snapshot);
        snapshot.interaction_session = 1;
        State::reconcile(&mut state, Some((Snowflake(1), 7, 1)), &snapshot);
        assert!(
            state.is_none(),
            "replacement READY invalidates the same account's draft"
        );
    }
    #[test]
    fn entity_paging_previous_uses_the_displayed_page_after_pool_shrinks() {
        let (mut snapshot, target) = fixture();
        let mut state = None;
        State::update(&mut state, Event::Open(target), ACCOUNT, None, &snapshot);
        let target = state.as_ref().unwrap().target;
        for _ in 0..2 {
            State::update(
                &mut state,
                Event::Page(target, true),
                ACCOUNT,
                None,
                &snapshot,
            );
        }
        assert_eq!(state.as_ref().unwrap().page, 2);
        snapshot.entity_candidates.truncate(25);
        // The view clamps to the second of the two remaining pages.
        State::update(
            &mut state,
            Event::Page(target, false),
            ACCOUNT,
            None,
            &snapshot,
        );
        assert_eq!(
            state.as_ref().unwrap().page,
            0,
            "one Previous reaches the first displayed page"
        );
    }

    #[test]
    fn entity_query_and_visible_choices_are_bounded_and_unicode_case_insensitive() {
        let (snapshot, target) = fixture();
        let mut state = None;
        State::update(&mut state, Event::Open(target), ACCOUNT, None, &snapshot);
        State::update(
            &mut state,
            Event::Query(target, "ПОЛЬЗОВАТЕЛЬ".into()),
            ACCOUNT,
            None,
            &snapshot,
        );
        assert_eq!(state.as_ref().unwrap().filtered(&snapshot).len(), 60);
        State::update(
            &mut state,
            Event::Page(target, true),
            ACCOUNT,
            None,
            &snapshot,
        );
        assert_eq!(state.as_ref().unwrap().page, 1);
        State::update(
            &mut state,
            Event::Query(target, "Я".repeat(1000)),
            ACCOUNT,
            None,
            &snapshot,
        );
        assert_eq!(state.as_ref().unwrap().query.chars().count(), QUERY_CHARS);
        assert!(state.as_ref().unwrap().query.len() <= 256);
        assert_eq!(state.as_ref().unwrap().page, 0);
        let _ = state.as_ref().unwrap().view(&snapshot);
    }
}

/// Fake mode only: constructs the same scoped intents as the visible controls.
#[cfg(feature = "fake-discord")]
pub fn scripted(gateway: &crate::gateway::GatewayPanel, command: &str) -> Option<Event> {
    if let Some(rest) = command.strip_prefix("open ") {
        let (message, index) = rest.split_once(' ')?;
        let message = message.parse::<u64>().ok()?;
        let index = index.parse::<usize>().ok()?;
        let row = gateway
            .timeline
            .rows
            .iter()
            .find(|row| row.message.id.0 == message)?;
        let owner = (
            gateway.navigation.current_user.as_ref()?.id,
            gateway.id,
            gateway.timeline.interaction_session,
        );
        return Some(Event::Open(Target::for_row(row, owner, index)));
    }
    let target = gateway.entity_select.as_ref()?.target;
    Some(match command {
        "clear" => Event::Clear(target),
        "submit" => Event::Submit(target),
        "close" => Event::Close(target),
        "next" => Event::Page(target, true),
        "previous" => Event::Page(target, false),
        command if command.starts_with("query ") => {
            Event::Query(target, command[6..].chars().take(QUERY_CHARS).collect())
        }
        command if command.starts_with("toggle ") => {
            let (kind, id) = command[7..].split_once(' ')?;
            let kind = match kind {
                "user" => EntityKind::User,
                "role" => EntityKind::Role,
                "channel" => EntityKind::Channel,
                _ => return None,
            };
            Event::Toggle(
                target,
                EntityValue {
                    kind,
                    id: Snowflake(id.parse().ok()?),
                },
            )
        }
        _ => return None,
    })
}

#[cfg(test)]
mod widget_tests {
    use super::*;
    use crate::variable_list::harness::Ui;
    #[test]
    fn entity_select_widgets_offer_local_actions_and_submit_only_after_valid_selection() {
        let message: Message = serde_json::from_value(serde_json::json!({"id":"7","channel_id":"5","author":{"id":"9","username":"app","bot":true},"content":"","timestamp":"2026-01-01T00:00:00Z","components":[{"type":5,"custom_id":"user","min_values":1,"max_values":1}]})).unwrap();
        let row = Row::new(std::sync::Arc::new(message), 1);
        let target = Target::for_row(&row, (Snowflake(1), 7, 0), 0);
        let candidate = EntityCandidate {
            value: EntityValue {
                kind: EntityKind::User,
                id: Snowflake(10),
            },
            label: "Held user".into(),
            channel_type: None,
        };
        let snapshot = Snapshot {
            rows: vec![row],
            channel_id: Some(target.channel),
            can_send: true,
            entity_candidates: vec![candidate.clone()],
            ..Snapshot::default()
        };
        let mut state = None;
        State::update(
            &mut state,
            Event::Open(target),
            Some((target.account, target.generation, target.session)),
            None,
            &snapshot,
        );
        let clickable = |state: &State| {
            let mut ui = Ui::new(640.0, 720.0, state.view(&snapshot));
            ui.redraw();
            ui.draw();
            let mut events = Vec::new();
            for y in (0..700).step_by(6) {
                for x in (0..630).step_by(12) {
                    for event in ui.click(x as f32, y as f32) {
                        if !events.contains(&event) {
                            events.push(event);
                        }
                    }
                }
            }
            events
        };
        let before = clickable(state.as_ref().unwrap());
        assert!(before.contains(&Event::Toggle(target, candidate.value)));
        assert!(!before.contains(&Event::Submit(target)));
        State::update(
            &mut state,
            Event::Toggle(target, candidate.value),
            Some((target.account, target.generation, target.session)),
            None,
            &snapshot,
        );
        let after = clickable(state.as_ref().unwrap());
        assert!(after.contains(&Event::Submit(target)));
        assert!(after.contains(&Event::Clear(target)));
    }
}
