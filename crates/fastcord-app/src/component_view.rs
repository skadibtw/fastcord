//! Drawing of a message's components (SPEC §9): action rows with buttons and
//! selects, and the Components V2 layout pieces this client knows. Nothing
//! here sends anything; a press becomes a [`Event`] the worker checks again
//! before a request is made.
//!
//! * A pending control (its request is unanswered) and every other control of
//!   the same message are drawn disabled.
//! * Link buttons open through the ordinary link path. Premium buttons are
//!   disabled with a note: purchases are outside this client.
//! * String selects with multiple values keep one bounded local draft and send
//!   only on Submit. Entity selects open a bounded local picker.
//! * Spoiler containers are a childless placeholder; component-scoped reveal is
//!   not implemented. Ordinary containers show their optional RGB accent.
//! * Anything else (thumbnails, galleries, files, unknown types) is a plain
//!   "Unsupported component" line.

use crate::i18n::{tr, trf};
use crate::ui::button;
use fastcord_model::{BUTTON_LINK, Button, Component, Select, Snowflake};
use iced::widget::{column, container, row, text};
use iced::{Element, Length};

use crate::timeline::{Event, Row};

/// One local multi-select draft (at most 25 bits, no text or request queue).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choices {
    pub channel: Snowflake,
    pub message: Snowflake,
    pub revision: u64,
    pub select: usize,
    pub mask: u32,
}

pub fn select_at(items: &[Component], mut index: usize) -> Option<&Select> {
    fn walk<'a>(items: &'a [Component], index: &mut usize) -> Option<&'a Select> {
        for item in items {
            match item {
                Component::Select(select) => {
                    if *index == 0 {
                        return Some(select);
                    }
                    *index -= 1;
                }
                Component::Row(body) | Component::Container { body, .. } => {
                    if let Some(found) = walk(body, index) {
                        return Some(found);
                    }
                }
                Component::Section { body, accessory } => {
                    if let Some(found) = walk(body, index) {
                        return Some(found);
                    }
                    if let Some(accessory) = accessory
                        && let Some(found) = walk(std::slice::from_ref(accessory.as_ref()), index)
                    {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        None
    }
    walk(items, &mut index)
}

fn default_mask(select: &Select) -> u32 {
    select
        .options
        .iter()
        .take(25)
        .enumerate()
        .fold(0, |mask, (at, option)| {
            mask | if option.default { 1 << at } else { 0 }
        })
}

/// Validate stale UI input against the current row before changing local choices.
pub fn toggle(current: Option<Choices>, row: &Row, key: Choices, option: usize) -> Option<Choices> {
    if row.message.id != key.message
        || row.message.channel_id != key.channel
        || row.revision != key.revision
        || row.component_pending.is_some()
    {
        return current;
    }
    let Some(select) = select_at(&row.message.components, key.select) else {
        return current;
    };
    if select.disabled
        || select.kind != 3
        || select.max_values <= 1
        || option >= select.options.len()
        || option >= 25
    {
        return current;
    }
    let mut next = key;
    next.mask = current
        .filter(|held| same_select(*held, key))
        .map_or_else(|| default_mask(select), |held| held.mask);
    let bit = 1 << option;
    if next.mask & bit == 0 && next.mask.count_ones() >= u32::from(select.max_values) {
        return current;
    }
    next.mask ^= bit;
    Some(next)
}

fn same_select(a: Choices, b: Choices) -> bool {
    (a.channel, a.message, a.revision, a.select) == (b.channel, b.message, b.revision, b.select)
}

struct Cx<'a> {
    channel_id: Snowflake,
    message_id: Snowflake,
    /// The `custom_id` waiting for Discord; every control is locked meanwhile.
    pending: Option<&'a str>,
    revision: u64,
    choices: Option<Choices>,
    entity_target: Option<crate::entity_select::Target>,
    next_select: std::cell::Cell<usize>,
}

/// All components of a row's message.
pub fn view(
    row: &Row,
    channel_id: Snowflake,
    choices: Option<Choices>,
    owner: Option<(Snowflake, u64, u64)>,
) -> Element<'_, Event> {
    let cx = Cx {
        channel_id,
        message_id: row.message.id,
        pending: row.component_pending.as_deref(),
        revision: row.revision,
        choices,
        entity_target: owner
            .filter(|_| row.message.component_application().is_some())
            .map(|owner| crate::entity_select::Target::for_row(row, owner, 0)),
        next_select: std::cell::Cell::new(0),
    };
    column(row.message.components.iter().map(|c| component(c, &cx)))
        .spacing(4)
        .into()
}

fn component<'a>(item: &'a Component, cx: &Cx<'a>) -> Element<'a, Event> {
    match item {
        Component::Row(items) => row(items.iter().map(|c| component(c, cx)))
            .spacing(6)
            .wrap()
            .into(),
        Component::Button(b) => push_button(b, cx),
        Component::Select(s) => {
            let index = cx.next_select.get();
            cx.next_select.set(index + 1);
            select(s, cx, index)
        }
        Component::Text(content) => text(content.as_str())
            .size(14)
            .wrapping(text::Wrapping::WordOrGlyph)
            .into(),
        Component::Section { body, accessory } => {
            let mut line = row![
                column(body.iter().map(|c| component(c, cx)))
                    .spacing(2)
                    .width(Length::Fill)
            ]
            .spacing(8);
            if let Some(accessory) = accessory {
                line = line.push(component(accessory, cx));
            }
            line.into()
        }
        Component::Container { body, accent } => {
            let content = container(column(body.iter().map(|c| component(c, cx))).spacing(4))
                .padding(8)
                .width(Length::Fill);
            let content: Element<'a, Event> = if let Some(rgb) = accent {
                let color = iced::Color::from_rgb8((rgb >> 16) as u8, (rgb >> 8) as u8, *rgb as u8);
                row![
                    container(iced::widget::Space::new().width(4).height(Length::Fill))
                        .width(4)
                        .height(Length::Fill)
                        .style(move |_| container::Style::default().background(color)),
                    content,
                ]
                .height(Length::Shrink)
                .into()
            } else {
                content.into()
            };
            container(content).style(container::rounded_box).into()
        }
        Component::HiddenSpoiler => text(tr("Hidden spoiler content"))
            .size(14)
            .color(crate::style::muted())
            .into(),
        Component::Separator => iced::widget::rule::horizontal(1).into(),
        Component::Unsupported(_) => text(tr("Unsupported component"))
            .size(12)
            .color(crate::style::muted())
            .into(),
    }
}

/// What pressing `b` asks for, if it can be pressed now.
fn button_event(b: &Button, cx: &Cx<'_>) -> Option<Event> {
    if b.premium || b.disabled {
        return None;
    }
    if b.style == BUTTON_LINK {
        return b.url.clone().map(Event::OpenLink);
    }
    let custom_id = b.custom_id.as_ref().filter(|_| cx.pending.is_none())?;
    Some(Event::Component {
        channel_id: cx.channel_id,
        message_id: cx.message_id,
        custom_id: custom_id.clone(),
        values: Vec::new(),
    })
}

/// What choosing `value` of `s` asks for, if the select can be used now.
fn option_event(s: &Select, value: &str, cx: &Cx<'_>) -> Option<Event> {
    (!s.disabled && s.kind == 3 && s.min_values <= 1 && cx.pending.is_none()).then(|| {
        Event::Component {
            channel_id: cx.channel_id,
            message_id: cx.message_id,
            custom_id: s.custom_id.clone(),
            values: vec![value.to_owned()],
        }
    })
}

fn push_button<'a>(b: &'a Button, cx: &Cx<'a>) -> Element<'a, Event> {
    let name = if b.label.is_empty() {
        tr("Button")
    } else {
        &b.label
    };
    let waiting = b.custom_id.is_some() && b.custom_id.as_deref() == cx.pending;
    let label = if b.premium {
        trf("{} (not available in this client)", &[&name])
    } else if b.style == BUTTON_LINK {
        trf("[link] {}", &[&name])
    } else if waiting {
        format!("{name}\u{2026}")
    } else {
        name.to_owned()
    };
    button(text(label).size(13).wrapping(text::Wrapping::WordOrGlyph))
        .padding([3, 10])
        .on_press_maybe(button_event(b, cx))
        .style(match b.style {
            1 => button::primary,
            3 => button::success,
            4 => button::danger,
            _ => button::secondary,
        })
        .into()
}

fn select<'a>(s: &'a Select, cx: &Cx<'a>, index: usize) -> Element<'a, Event> {
    let shown = if s.placeholder.is_empty() {
        tr("Select")
    } else {
        &s.placeholder
    };
    if s.kind != 3 {
        let event = cx
            .entity_target
            .filter(|_| !s.disabled && cx.pending.is_none())
            .map(|mut target| {
                target.select = index;
                Event::EntitySelect(crate::entity_select::Event::Open(target))
            });
        return button(text(shown)).on_press_maybe(event).into();
    }
    let waiting = cx.pending == Some(s.custom_id.as_str());
    let mut options = column![
        text(if waiting {
            format!("{shown}\u{2026}")
        } else {
            shown.to_owned()
        })
        .size(12)
        .color(crate::style::muted())
    ]
    .spacing(2);
    let key = Choices {
        channel: cx.channel_id,
        message: cx.message_id,
        revision: cx.revision,
        select: index,
        mask: 0,
    };
    let multiple = s.max_values > 1;
    let mask = cx
        .choices
        .filter(|held| same_select(*held, key))
        .map_or_else(|| default_mask(s), |held| held.mask);
    for (at, option) in s.options.iter().enumerate() {
        let label = if option.description.is_empty() {
            option.label.clone()
        } else {
            format!("{} \u{2013} {}", option.label, option.description)
        };
        options = options.push(
            button(
                text(format!(
                    "{} {label}",
                    if (multiple && mask & (1 << at) != 0) || (!multiple && option.default) {
                        "[x]"
                    } else {
                        "[ ]"
                    }
                ))
                .size(13)
                .wrapping(text::Wrapping::WordOrGlyph),
            )
            .width(Length::Fill)
            .padding([2, 8])
            .on_press_maybe(if multiple {
                (!s.disabled && cx.pending.is_none())
                    .then_some(Event::SelectChoice { key, option: at })
            } else {
                option_event(s, &option.value, cx)
            })
            .style(crate::style::secondary_button(false)),
        );
    }
    if multiple {
        let values: Vec<_> = s
            .options
            .iter()
            .enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, option)| option.value.clone())
            .collect();
        let valid =
            values.len() >= usize::from(s.min_values) && values.len() <= usize::from(s.max_values);
        options = options.push(button(text(tr("Submit"))).on_press_maybe(
            (valid && !s.disabled && cx.pending.is_none()).then_some(Event::Component {
                channel_id: cx.channel_id,
                message_id: cx.message_id,
                custom_id: s.custom_id.clone(),
                values,
            }),
        ));
    } else if s.min_values == 0 {
        options = options.push(button(text(tr("Clear"))).on_press_maybe(
            (!s.disabled && cx.pending.is_none()).then_some(Event::Component {
                channel_id: cx.channel_id,
                message_id: cx.message_id,
                custom_id: s.custom_id.clone(),
                values: Vec::new(),
            }),
        ));
    }
    container(options).max_width(360).into()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn row_with(components: serde_json::Value, pending: Option<&str>) -> Row {
        let message: fastcord_model::Message = serde_json::from_value(serde_json::json!({
            "id": "7", "channel_id": "5", "content": "",
            "timestamp": "2026-01-01T00:00:00.000000+00:00",
            "author": {"id": "9", "username": "app", "bot": true},
            "components": components
        }))
        .unwrap();
        let mut row = Row::new(Arc::new(message), 1);
        row.component_pending = pending.map(str::to_owned);
        row
    }

    /// Every event the message's controls can send now.
    fn pressable(row: &Row) -> Vec<Event> {
        let _ = view(row, Snowflake(5), None, None);
        let cx = Cx {
            channel_id: Snowflake(5),
            message_id: Snowflake(7),
            pending: row.component_pending.as_deref(),
            revision: row.revision,
            choices: None,
            entity_target: None,
            next_select: std::cell::Cell::new(0),
        };
        fn walk(items: &[Component], cx: &Cx<'_>, found: &mut Vec<Event>) {
            for item in items {
                match item {
                    Component::Row(items) | Component::Container { body: items, .. } => {
                        walk(items, cx, found)
                    }
                    Component::Button(b) => found.extend(button_event(b, cx)),
                    Component::Select(s) => found.extend(
                        s.options
                            .iter()
                            .filter_map(|o| option_event(s, &o.value, cx)),
                    ),
                    _ => {}
                }
            }
        }
        let mut found = Vec::new();
        walk(&row.message.components, &cx, &mut found);
        found
    }

    fn press(id: &str, values: &[&str]) -> Event {
        Event::Component {
            channel_id: Snowflake(5),
            message_id: Snowflake(7),
            custom_id: id.to_owned(),
            values: values.iter().map(|v| (*v).to_owned()).collect(),
        }
    }

    #[test]
    fn hidden_container_has_no_widget_actions_and_nested_visible_controls_still_work() {
        use iced::advanced::widget::{Id, Operation};
        #[derive(Default)]
        struct Labels(Vec<(String, iced::Rectangle)>, Vec<iced::Rectangle>);
        impl Operation for Labels {
            fn container(&mut self, _: Option<&Id>, bounds: iced::Rectangle) {
                self.1.push(bounds);
            }
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn text(&mut self, _: Option<&Id>, bounds: iced::Rectangle, value: &str) {
                self.0.push((value.to_owned(), bounds));
            }
        }
        let button =
            serde_json::json!({"type":2,"style":1,"label":"Visible action","custom_id":"visible"});
        for accent in [
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(0xFFFFFF),
            serde_json::json!(0x123456),
        ] {
            let visible = row_with(
                serde_json::json!([{"type":17,"accent_color":accent,"components":[
                    {"type":1,"components":[button.clone()]}
                ]}]),
                None,
            );
            let mut ui = crate::variable_list::harness::Ui::new(
                320.0,
                200.0,
                view(&visible, Snowflake(5), None, None),
            );
            assert!(
                ui.layout_height() > 32.0 && ui.layout_height() < 100.0,
                "accent bar follows natural content height: {}",
                ui.layout_height()
            );
            ui.redraw();
            ui.draw();
            ui.draw_with_theme(&iced::Theme::Light);
            let mut labels = Labels::default();
            ui.inspect(&mut labels);
            let stripes: Vec<_> = labels
                .1
                .iter()
                .filter(|bounds| bounds.width == 4.0)
                .collect();
            assert_eq!(
                stripes.len(),
                usize::from(!accent.is_null()),
                "only explicit RGB draws a stripe"
            );
            for stripe in stripes {
                assert!(
                    (stripe.height - ui.layout_height()).abs() < 0.01,
                    "stripe fills natural content height, not zero or the viewport: {stripe:?}"
                );
            }
            let bounds = labels
                .0
                .iter()
                .find(|(label, _)| label == "Visible action")
                .expect("valid container-row-button remains rendered")
                .1;
            assert!(bounds.width > 0.0 && bounds.height > 0.0);
            assert_eq!(
                ui.click(bounds.center_x(), bounds.center_y()),
                [press("visible", &[])],
                "nested button remains reachable with {accent}"
            );
        }
        let hidden = row_with(
            serde_json::json!([{"type":17,"spoiler":true,"components":[
                {"type":17,"components":[{"type":1,"components":[button]}]},
                {"type":6,"custom_id":"hidden-select"}
            ]}]),
            None,
        );
        assert!(pressable(&hidden).is_empty());
        let mixed = row_with(
            serde_json::json!([
                {"type":17,"spoiler":true,"components":[{"type":1,"components":[{"type":6,"custom_id":"hidden"}]}]},
                {"type":17,"components":[{"type":1,"components":[{"type":6,"custom_id":"visible-select"}]}]}
            ]),
            None,
        );
        assert_eq!(
            select_at(&mixed.message.components, 0).unwrap().custom_id,
            "visible-select"
        );
        assert!(select_at(&mixed.message.components, 1).is_none());
        assert!(select_at(&hidden.message.components, 0).is_none());
        let mut ui = crate::variable_list::harness::Ui::new(
            320.0,
            200.0,
            view(&hidden, Snowflake(5), None, None),
        );
        ui.redraw();
        ui.draw();
        let mut labels = Labels::default();
        ui.inspect(&mut labels);
        assert_eq!(
            labels
                .0
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>(),
            [tr("Hidden spoiler content")]
        );
        for (x, y) in [(4.0, 8.0), (36.0, 24.0), (100.0, 20.0)] {
            assert!(
                ui.click(x, y).is_empty(),
                "placeholder cannot reveal or send an action"
            );
        }
    }

    #[test]
    fn controls_lock_while_one_is_pending_and_premium_and_disabled_ones_never_interact() {
        let list = serde_json::json!([{"type": 1, "components": [
            {"type": 2, "style": 1, "label": "Yes", "custom_id": "yes"},
            {"type": 2, "style": 6, "sku_id": "1", "custom_id": "buy"},
            {"type": 2, "style": 5, "label": "Docs", "url": "https://example.test/"},
            {"type": 2, "style": 2, "label": "Off", "custom_id": "off", "disabled": true}
        ]}, {"type": 1, "components": [
            {"type": 3, "custom_id": "pick", "options": [{"label": "A", "value": "a"}]}
        ]}, {"type": 1, "components": [
            {"type": 5, "custom_id": "who"}
        ]}, {"type": 12}]);
        let link = Event::OpenLink("https://example.test/".to_owned());
        assert_eq!(
            pressable(&row_with(list.clone(), None)),
            [press("yes", &[]), link.clone(), press("pick", &["a"])]
        );
        // While one control waits, only the link still works.
        assert_eq!(pressable(&row_with(list, Some("yes"))), [link]);
    }

    fn multi_row() -> Row {
        row_with(
            serde_json::json!([{"type": 17, "components": [{"type": 1, "components": [{
                "type": 3, "custom_id": "multi", "min_values": 2, "max_values": 2,
                "options": [{"label": "A", "value": "a", "default": true},
                            {"label": "B", "value": "b"}, {"label": "C", "value": "c"}]
            }]}]}]),
            None,
        )
    }

    #[test]
    fn multi_select_drafts_are_bounded_and_stale_changes_are_refused() {
        let mut row = multi_row();
        let key = Choices {
            channel: Snowflake(5),
            message: Snowflake(7),
            revision: row.revision,
            select: 0,
            mask: 0,
        };
        assert_eq!(
            select_at(&row.message.components, 0).unwrap().custom_id,
            "multi"
        );
        assert!(select_at(&row.message.components, 1).is_none());
        let chosen = toggle(None, &row, key, 1).unwrap();
        assert_eq!(chosen.mask, 3, "default A plus chosen B");
        assert_eq!(
            toggle(Some(chosen), &row, key, 2),
            Some(chosen),
            "maximum two"
        );
        assert_eq!(toggle(Some(chosen), &row, key, 25), Some(chosen));
        let fewer = toggle(Some(chosen), &row, key, 0).unwrap();
        assert_eq!(
            fewer.mask, 2,
            "removing is allowed below minimum before Submit"
        );
        row.revision += 1;
        assert_eq!(
            toggle(Some(chosen), &row, key, 0),
            Some(chosen),
            "stale revision"
        );
        row.revision = key.revision;
        row.component_pending = Some("multi".into());
        assert_eq!(
            toggle(Some(chosen), &row, key, 0),
            Some(chosen),
            "pending request"
        );
    }

    #[test]
    fn multi_select_ui_emits_local_toggles_and_one_explicit_submission() {
        use crate::variable_list::harness::Ui;
        let row = multi_row();
        let key = Choices {
            channel: Snowflake(5),
            message: Snowflake(7),
            revision: row.revision,
            select: 0,
            mask: 3,
        };
        let clickable = |choices| {
            let mut ui = Ui::new(400.0, 400.0, view(&row, Snowflake(5), choices, None));
            ui.redraw();
            ui.draw();
            let mut found = Vec::new();
            for y in (0..250).step_by(4) {
                for x in (0..390).step_by(8) {
                    for event in ui.click(x as f32, y as f32) {
                        if !found.contains(&event) {
                            found.push(event);
                        }
                    }
                }
            }
            found
        };
        let before = clickable(None);
        assert!(
            before
                .iter()
                .any(|event| matches!(event, Event::SelectChoice { .. }))
        );
        assert!(
            !before
                .iter()
                .any(|event| matches!(event, Event::Component { .. })),
            "minimum not met"
        );
        let after = clickable(Some(key));
        let writes: Vec<_> = after
            .into_iter()
            .filter(|event| matches!(event, Event::Component { .. }))
            .collect();
        assert_eq!(writes, [press("multi", &["a", "b"])]);
    }

    #[test]
    fn layout_pieces_draw() {
        let list = serde_json::json!([{"type": 17, "components": [
            {"type": 10, "content": "hello"}, {"type": 14},
            {"type": 9, "components": [{"type": 10, "content": "side"}],
             "accessory": {"type": 2, "style": 1, "label": "Go", "custom_id": "go"}}
        ]}]);
        let _ = view(&row_with(list, None), Snowflake(5), None, None);
    }
}
