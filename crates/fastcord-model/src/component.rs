//! Interactive message components (buttons, selects) and the Components V2
//! layout pieces this client can draw. Parsed leniently and bounded: a
//! message never fails because of its components, and unknown ones become
//! [`Component::Unsupported`], which the UI draws read-only.

use std::fmt;

use crate::Snowflake;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// Components one message keeps in total (action rows and everything in them).
pub const MAX_COMPONENTS: usize = 40;
/// Deepest nesting kept: container, section or row, then its content.
pub const MAX_COMPONENT_DEPTH: usize = 3;
pub const MAX_COMPONENT_LABEL_CHARS: usize = 80;
pub const MAX_COMPONENT_TEXT_CHARS: usize = 2000;
pub const MAX_CUSTOM_ID_CHARS: usize = 100;
pub const MAX_COMPONENT_URL_CHARS: usize = 512;
pub const MAX_SELECT_OPTIONS: usize = 25;

/// Button style of a link button.
pub const BUTTON_LINK: u8 = 5;
/// Button style of a premium (SKU) button.
pub const BUTTON_PREMIUM: u8 = 6;
/// `flags` bit of a message only its recipient sees.
pub const EPHEMERAL_FLAG: u64 = 1 << 6;

#[derive(Clone, PartialEq, Eq)]
pub struct Button {
    pub style: u8,
    pub label: String,
    pub custom_id: Option<String>,
    /// Only link buttons have one.
    pub url: Option<String>,
    pub disabled: bool,
    /// A premium button: it needs a purchase, which this client never offers.
    pub premium: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub label: String,
    pub value: String,
    pub description: String,
    pub default: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntityKind {
    User,
    Role,
    Channel,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EntityValue {
    pub kind: EntityKind,
    pub id: Snowflake,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Select {
    /// 3 string, 5 user, 6 role, 7 mentionable, 8 channel.
    pub kind: u8,
    pub custom_id: String,
    pub placeholder: String,
    pub min_values: u8,
    pub max_values: u8,
    pub disabled: bool,
    /// Only a string select lists options.
    pub options: Vec<SelectOption>,
    /// Known channel types only; empty means no channel-type restriction.
    pub channel_types: Vec<u8>,
    pub default_values: Vec<EntityValue>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Component {
    /// Type 1.
    Row(Vec<Component>),
    /// Type 2.
    Button(Button),
    /// Types 3 and 5 to 8.
    Select(Select),
    /// Type 10, a text display.
    Text(String),
    /// Type 9: text with an optional button accessory.
    Section {
        body: Vec<Component>,
        accessory: Option<Box<Component>>,
    },
    /// Type 17.
    Container {
        body: Vec<Component>,
        accent: Option<u32>,
    },
    /// Hidden container: descendants are discarded before publication.
    HiddenSpoiler,
    /// Type 14.
    Separator,
    /// Anything else (thumbnails, galleries, files, unknown types), by type.
    Unsupported(u8),
}

impl fmt::Debug for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Component")
    }
}

fn cut(value: &Value, key: &str, max: usize) -> String {
    let text = value.get(key).and_then(Value::as_str).unwrap_or_default();
    match text.char_indices().nth(max) {
        Some((end, _)) => text[..end].to_owned(),
        None => text.to_owned(),
    }
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn small(value: &Value, key: &str, default: u8) -> u8 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map_or(default, |n| n.min(u64::from(u8::MAX)) as u8)
}

impl Select {
    pub fn accepts(&self, value: EntityValue, channel_type: Option<u8>) -> bool {
        let typed = matches!(
            (self.kind, value.kind),
            (5 | 7, EntityKind::User) | (6 | 7, EntityKind::Role) | (8, EntityKind::Channel)
        );
        typed
            && (value.kind != EntityKind::Channel
                || channel_type.is_some_and(|kind| {
                    known_channel_type(kind)
                        && (self.channel_types.is_empty() || self.channel_types.contains(&kind))
                }))
    }
}

pub fn known_channel_type(kind: u8) -> bool {
    matches!(kind, 0 | 2 | 4 | 5 | 10..=16)
}

fn entity_constraints(value: &Value, kind: u8) -> Option<(Vec<u8>, Vec<EntityValue>)> {
    let minimum = value.get("min_values").map_or(Some(1), Value::as_u64)?;
    let maximum = value.get("max_values").map_or(Some(1), Value::as_u64)?;
    let custom = value.get("custom_id")?.as_str()?;
    if custom.is_empty()
        || custom.chars().count() > MAX_CUSTOM_ID_CHARS
        || minimum > maximum
        || !(1..=25).contains(&maximum)
        || value.get("disabled").is_some_and(|v| !v.is_boolean())
        || value.get("required").is_some()
        || value.get("options").is_some()
    {
        return None;
    }
    let mut channels = Vec::new();
    if let Some(values) = value.get("channel_types") {
        if kind != 8 {
            return None;
        }
        let values = values.as_array()?;
        if values.len() > 16 {
            return None;
        }
        for value in values {
            let channel = u8::try_from(value.as_u64()?).ok()?;
            if !known_channel_type(channel) || channels.contains(&channel) {
                return None;
            }
            channels.push(channel);
        }
    }
    let mut defaults = Vec::new();
    if let Some(values) = value.get("default_values") {
        let values = values.as_array()?;
        if values.len() < minimum as usize || values.len() > maximum as usize {
            return None;
        }
        for value in values {
            let entity_kind = match value.get("type")?.as_str()? {
                "user" if kind == 5 || kind == 7 => EntityKind::User,
                "role" if kind == 6 || kind == 7 => EntityKind::Role,
                "channel" if kind == 8 => EntityKind::Channel,
                _ => return None,
            };
            let text = value.get("id")?.as_str()?;
            let id = text
                .parse::<u64>()
                .ok()
                .filter(|id| *id != 0 && id.to_string() == text)?;
            let entity = EntityValue {
                kind: entity_kind,
                id: Snowflake(id),
            };
            if defaults.contains(&entity) {
                return None;
            }
            defaults.push(entity);
        }
    }
    Some((channels, defaults))
}

fn children(value: &Value, key: &str, depth: usize, left: &mut usize) -> Vec<Component> {
    let Some(items) = value.get(key).and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .map_while(|item| parse(item, depth, left))
        .collect()
}

fn parse(value: &Value, depth: usize, left: &mut usize) -> Option<Component> {
    if *left == 0 {
        return None;
    }
    *left -= 1;
    let kind = small(value, "type", 0);
    if depth > MAX_COMPONENT_DEPTH {
        return Some(Component::Unsupported(kind));
    }
    Some(match kind {
        1 => Component::Row(children(value, "components", depth + 1, left)),
        2 => {
            let style = small(value, "style", 1);
            let url = Some(cut(value, "url", MAX_COMPONENT_URL_CHARS)).filter(|u| !u.is_empty());
            Component::Button(Button {
                style,
                label: cut(value, "label", MAX_COMPONENT_LABEL_CHARS),
                custom_id: Some(cut(value, "custom_id", MAX_CUSTOM_ID_CHARS))
                    .filter(|id| !id.is_empty()),
                url: url.filter(|_| style == BUTTON_LINK),
                disabled: flag(value, "disabled"),
                premium: style == BUTTON_PREMIUM || value.get("sku_id").is_some(),
            })
        }
        3 | 5..=8 => {
            let (channel_types, default_values) = if kind == 3 {
                (Vec::new(), Vec::new())
            } else {
                match entity_constraints(value, kind) {
                    Some(constraints) => constraints,
                    None => return Some(Component::Unsupported(kind)),
                }
            };
            let options = value
                .get("options")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .take(MAX_SELECT_OPTIONS)
                        .map(|option| SelectOption {
                            label: cut(option, "label", MAX_COMPONENT_LABEL_CHARS),
                            value: cut(option, "value", MAX_CUSTOM_ID_CHARS),
                            description: cut(option, "description", MAX_COMPONENT_LABEL_CHARS),
                            default: flag(option, "default"),
                        })
                        .filter(|option| !option.value.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            Component::Select(Select {
                kind,
                custom_id: cut(value, "custom_id", MAX_CUSTOM_ID_CHARS),
                placeholder: cut(value, "placeholder", MAX_COMPONENT_LABEL_CHARS),
                min_values: small(value, "min_values", 1),
                max_values: small(value, "max_values", 1),
                disabled: flag(value, "disabled"),
                options,
                channel_types,
                default_values,
            })
        }
        10 => Component::Text(cut(value, "content", MAX_COMPONENT_TEXT_CHARS)),
        9 => Component::Section {
            body: children(value, "components", depth + 1, left),
            accessory: value
                .get("accessory")
                .and_then(|accessory| parse(accessory, depth + 1, left))
                .map(Box::new),
        },
        17 if !matches!(value.get("spoiler"), None | Some(Value::Bool(false))) => {
            Component::HiddenSpoiler
        }
        17 => Component::Container {
            body: children(value, "components", depth + 1, left),
            accent: value
                .get("accent_color")
                .and_then(Value::as_u64)
                .filter(|c| *c <= 0xFF_FFFF)
                .map(|c| c as u32),
        },
        14 => Component::Separator,
        other => Component::Unsupported(other),
    })
}

/// The components of a wire list, at most [`MAX_COMPONENTS`] in all.
pub fn parse_components(items: &[Value]) -> Vec<Component> {
    let mut left = MAX_COMPONENTS;
    items
        .iter()
        .map_while(|item| parse(item, 1, &mut left))
        .collect()
}

/// Deserializes a `components` list; anything malformed is "none".
pub(crate) fn lenient_components<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Vec<Component>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Array(items) => parse_components(&items),
        _ => Vec::new(),
    })
}

pub(crate) fn lenient_some_components<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<Component>>, D::Error> {
    lenient_components(d).map(Some)
}

fn list_bytes(list: &[Component]) -> usize {
    size_of_val(list) + list.iter().map(Component::heap_bytes).sum::<usize>()
}

impl Component {
    /// Heap bytes held, for the message store's byte budget.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Self::Row(items) | Self::Container { body: items, .. } => list_bytes(items),
            Self::Section { body, accessory } => {
                list_bytes(body)
                    + accessory
                        .as_deref()
                        .map_or(0, |a| size_of::<Component>() + a.heap_bytes())
            }
            Self::Button(button) => {
                button.label.capacity()
                    + button.custom_id.as_ref().map_or(0, String::capacity)
                    + button.url.as_ref().map_or(0, String::capacity)
            }
            Self::Select(select) => {
                select.custom_id.capacity()
                    + select.placeholder.capacity()
                    + select.channel_types.capacity()
                    + select.default_values.capacity() * size_of::<EntityValue>()
                    + select.options.capacity() * size_of::<SelectOption>()
                    + select
                        .options
                        .iter()
                        .map(|o| o.label.capacity() + o.value.capacity() + o.description.capacity())
                        .sum::<usize>()
            }
            Self::Text(text) => text.capacity(),
            Self::HiddenSpoiler | Self::Separator | Self::Unsupported(_) => 0,
        }
    }

    /// Releases spare capacity so [`heap_bytes`](Self::heap_bytes) is exact.
    pub fn compact(&mut self) {
        match self {
            Self::Row(items) | Self::Container { body: items, .. } => compact_list(items),
            Self::Section { body, accessory } => {
                compact_list(body);
                if let Some(accessory) = accessory {
                    accessory.compact();
                }
            }
            Self::Button(button) => {
                button.label.shrink_to_fit();
                if let Some(id) = &mut button.custom_id {
                    id.shrink_to_fit();
                }
                if let Some(url) = &mut button.url {
                    url.shrink_to_fit();
                }
            }
            Self::Select(select) => {
                select.custom_id.shrink_to_fit();
                select.placeholder.shrink_to_fit();
                select.options.shrink_to_fit();
                select.channel_types.shrink_to_fit();
                select.default_values.shrink_to_fit();
                for option in &mut select.options {
                    option.label.shrink_to_fit();
                    option.value.shrink_to_fit();
                    option.description.shrink_to_fit();
                }
            }
            Self::Text(text) => text.shrink_to_fit(),
            Self::HiddenSpoiler | Self::Separator | Self::Unsupported(_) => {}
        }
    }

    /// The button or select with this `custom_id`, anywhere in `list`.
    pub fn find<'a>(list: &'a [Component], custom_id: &str) -> Option<&'a Component> {
        list.iter().find_map(|component| match component {
            Self::Button(b) if b.custom_id.as_deref() == Some(custom_id) => Some(component),
            Self::Select(s) if s.custom_id == custom_id => Some(component),
            Self::Row(items) | Self::Container { body: items, .. } => Self::find(items, custom_id),
            Self::Section { body, accessory } => accessory
                .as_deref()
                .filter(|a| Self::find(std::slice::from_ref(a), custom_id).is_some())
                .or_else(|| Self::find(body, custom_id)),
            _ => None,
        })
    }
}

fn compact_list(list: &mut Vec<Component>) {
    list.shrink_to_fit();
    for component in list {
        component.compact();
    }
}

/// Shrinks and measures a whole list (see [`Component::heap_bytes`]).
pub fn compact_components(list: &mut Vec<Component>) {
    compact_list(list);
}

/// Heap bytes of a message's component list, including the list itself.
pub fn components_heap_bytes(list: &[Component]) -> usize {
    list_bytes(list)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parsed(value: Value) -> Vec<Component> {
        parse_components(value.as_array().unwrap())
    }

    #[test]
    fn rows_buttons_and_selects_parse_with_their_fields() {
        let list = parsed(json!([{"type": 1, "components": [
            {"type": 2, "style": 1, "label": "Yes", "custom_id": "yes"},
            {"type": 2, "style": 5, "label": "Docs", "url": "https://example.test/x"},
            {"type": 2, "style": 6, "sku_id": "9", "custom_id": "buy"},
            {"type": 2, "style": 1, "label": "Off", "custom_id": "off", "disabled": true}
        ]}, {"type": 1, "components": [{
            "type": 3, "custom_id": "pick", "placeholder": "Choose", "max_values": 2,
            "options": [
                {"label": "A", "value": "a", "default": true},
                {"label": "B", "value": "b", "description": "second"},
                {"label": "no value"}
            ]
        }]}, {"type": 1, "components": [{"type": 6, "custom_id": "roles"}]}]));
        assert_eq!(list.len(), 3);
        let Component::Row(buttons) = &list[0] else {
            panic!("not a row");
        };
        let Component::Button(yes) = &buttons[0] else {
            panic!("not a button");
        };
        assert_eq!(
            (yes.label.as_str(), yes.custom_id.as_deref()),
            ("Yes", Some("yes"))
        );
        let Component::Button(link) = &buttons[1] else {
            panic!();
        };
        assert_eq!(link.url.as_deref(), Some("https://example.test/x"));
        assert!(link.custom_id.is_none());
        let Component::Button(premium) = &buttons[2] else {
            panic!();
        };
        assert!(premium.premium);
        assert!(matches!(&buttons[3], Component::Button(b) if b.disabled));
        let Component::Row(selects) = &list[1] else {
            panic!();
        };
        let Component::Select(select) = &selects[0] else {
            panic!();
        };
        assert_eq!((select.max_values, select.min_values), (2, 1));
        assert_eq!(
            select.options.len(),
            2,
            "an option without a value is dropped"
        );
        assert!(select.options[0].default);
        assert!(matches!(
            Component::find(&list, "pick"),
            Some(Component::Select(_))
        ));
        assert!(matches!(
            Component::find(&list, "roles"),
            Some(Component::Select(s)) if s.kind == 6
        ));
        assert!(Component::find(&list, "missing").is_none());
    }

    #[test]
    fn container_spoilers_discard_descendants_and_invalid_flags_fail_closed() {
        for spoiler in [
            json!(true),
            json!(null),
            json!("false"),
            json!(0),
            json!({}),
        ] {
            let mut list = parsed(json!([{"type":17,"spoiler":spoiler,"components":[
                {"type":10,"content":"private text"},
                {"type":17,"components":[{"type":2,"style":1,"custom_id":"hidden","label":"private button"}]},
                {"type":6,"custom_id":"hidden-select"}
            ]}]));
            assert!(matches!(list.as_slice(), [Component::HiddenSpoiler]));
            assert_eq!(
                list[0].heap_bytes(),
                0,
                "no descendant strings or actions retained"
            );
            assert!(Component::find(&list, "hidden").is_none());
            assert!(Component::find(&list, "hidden-select").is_none());
            compact_components(&mut list);
            assert_eq!(list[0].heap_bytes(), 0);
        }
        let list = parsed(json!([{"type":17,"components":[
            {"type":17,"spoiler":true,"components":[{"type":10,"content":"secret"}]},
            {"type":17,"spoiler":false,"components":[{"type":2,"style":1,"custom_id":"visible","label":"Visible"}]}
        ]}]));
        assert!(Component::find(&list, "visible").is_some());
        let Component::Container { body, .. } = &list[0] else {
            panic!()
        };
        assert!(matches!(body[0], Component::HiddenSpoiler));
        assert!(matches!(&body[1], Component::Container { body, .. } if body.len()==1));
        let list = parsed(json!(
            (0..MAX_COMPONENTS + 8)
                .map(|_| json!({"type":17,"spoiler":true}))
                .collect::<Vec<_>>()
        ));
        assert_eq!(list.len(), MAX_COMPONENTS);
    }

    #[test]
    fn container_accents_accept_only_optional_24_bit_rgb() {
        for (wire, expected) in [
            (json!(0), Some(0)),
            (json!(0xFFFFFF), Some(0xFFFFFF)),
            (json!(0x123456), Some(0x123456)),
            (json!(null), None),
            (json!(-1), None),
            (json!(0x1000000u64), None),
            (json!(4294967296u64), None),
            (json!("255"), None),
            (json!(1.5), None),
        ] {
            let list = parsed(json!([{"type":17,"accent_color":wire,"components":[]} ]));
            assert!(matches!(&list[0], Component::Container { accent, .. } if *accent == expected));
        }
        assert!(matches!(
            &parsed(json!([{"type":17}]))[0],
            Component::Container { accent: None, .. }
        ));
    }

    #[test]
    fn components_v2_layout_parses_and_unknown_types_are_read_only() {
        let list = parsed(json!([{"type": 17, "accent_color": 255, "components": [
            {"type": 10, "content": "hello"},
            {"type": 14},
            {"type": 9, "components": [{"type": 10, "content": "side"}],
             "accessory": {"type": 2, "style": 1, "label": "Go", "custom_id": "go"}},
            {"type": 12, "items": []},
            {"type": 99}
        ]}]));
        let Component::Container { body, accent } = &list[0] else {
            panic!();
        };
        assert_eq!(*accent, Some(255));
        assert!(matches!(&body[0], Component::Text(t) if t == "hello"));
        assert!(matches!(body[1], Component::Separator));
        assert!(matches!(
            &body[2],
            Component::Section {
                accessory: Some(_),
                ..
            }
        ));
        assert!(matches!(body[3], Component::Unsupported(12)));
        assert!(matches!(body[4], Component::Unsupported(99)));
        assert!(matches!(
            Component::find(&list, "go"),
            Some(Component::Button(_))
        ));
    }

    #[test]
    fn the_total_depth_and_text_are_bounded() {
        let many: Vec<Value> = (0..30)
            .map(|n| {
                json!({"type": 1, "components": [
                    {"type": 2, "label": "x".repeat(500), "custom_id": format!("b{n}")},
                    {"type": 2, "label": "y", "custom_id": "c"}
                ]})
            })
            .collect();
        let list = parse_components(&many);
        fn count(list: &[Component]) -> usize {
            list.iter()
                .map(|c| match c {
                    Component::Row(items) => 1 + count(items),
                    _ => 1,
                })
                .sum()
        }
        assert!(count(&list) <= MAX_COMPONENTS);
        let Component::Row(first) = &list[0] else {
            panic!();
        };
        let Component::Button(button) = &first[0] else {
            panic!();
        };
        assert_eq!(button.label.chars().count(), MAX_COMPONENT_LABEL_CHARS);
        let deep = parsed(
            json!([{"type": 17, "components": [{"type": 17, "components": [
            {"type": 17, "components": [{"type": 10, "content": "deep"}]}]}]}]),
        );
        let Component::Container { body, .. } = &deep[0] else {
            panic!();
        };
        let Component::Container { body, .. } = &body[0] else {
            panic!();
        };
        assert!(
            matches!(&body[0], Component::Container { body, .. } if matches!(body[0], Component::Unsupported(10)))
        );
    }

    #[test]
    fn a_malformed_list_is_no_components_and_the_budget_counts_heap() {
        #[derive(Deserialize)]
        struct M {
            #[serde(default, deserialize_with = "lenient_components")]
            components: Vec<Component>,
        }
        let m: M = serde_json::from_value(json!({"components": "x"})).unwrap();
        assert!(m.components.is_empty());
        let mut list = parsed(json!([{"type": 1, "components": [
            {"type": 2, "label": "Yes", "custom_id": "yes"}]}]));
        compact_components(&mut list);
        assert!(components_heap_bytes(&list) >= 2 * size_of::<Component>() + 6);
    }
}

#[cfg(test)]
mod entity_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn typed_defaults_and_channel_filters_are_bounded_and_fail_closed() {
        let valid = json!({"type":8,"custom_id":"channel","min_values":0,"max_values":2,
            "channel_types":[0,12],"default_values":[{"id":"9","type":"channel"}]});
        let parsed = parse_components(std::slice::from_ref(&valid));
        let Component::Select(select) = &parsed[0] else {
            panic!("valid channel select");
        };
        assert_eq!(
            select.default_values,
            [EntityValue {
                kind: EntityKind::Channel,
                id: Snowflake(9)
            }]
        );
        assert!(select.accepts(select.default_values[0], Some(12)));
        assert!(!select.accepts(select.default_values[0], Some(2)));
        for (key, bad) in [
            ("channel_types", json!([99])),
            ("channel_types", json!([0, 0])),
            ("channel_types", json!(["0"])),
            ("min_values", json!(3)),
            ("max_values", json!(26)),
            ("disabled", json!("false")),
            ("required", json!(false)),
            ("default_values", json!([{"id":"9","type":"user"}])),
            ("default_values", json!([{"id":"0","type":"channel"}])),
            ("default_values", json!([{"id":"09","type":"channel"}])),
            (
                "default_values",
                json!([{"id":"9","type":"channel"},{"id":"9","type":"channel"}]),
            ),
        ] {
            let mut wire = valid.clone();
            wire[key] = bad;
            assert!(
                matches!(parse_components(&[wire])[0], Component::Unsupported(8)),
                "{key}"
            );
        }
        assert!(matches!(
            parse_components(&[
                json!({"type":5,"custom_id":"user","min_values":1,"default_values":[]})
            ])[0],
            Component::Unsupported(5)
        ));
        for (kind, accepted) in [(5, "user"), (6, "role"), (7, "user"), (7, "role")] {
            let parsed = parse_components(&[
                json!({"type":kind,"custom_id":"entity","default_values":[{"id":"9","type":accepted}]}),
            ]);
            assert!(
                matches!(&parsed[0],Component::Select(select) if select.default_values.len()==1)
            );
        }
    }
}
