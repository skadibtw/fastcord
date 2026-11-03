//! Local, bounded Text Display identities. No media decoding or network work.
use std::collections::HashSet;

use crate::timeline::{Row, Snapshot};
use fastcord_model::{Component, Snowflake, markup};

pub const MAX_REVEALED: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub account: Snowflake,
    pub generation: u64,
    pub session: u64,
    pub channel: Snowflake,
    pub message: Snowflake,
    pub revision: u64,
    pub text: u16,
}

impl Target {
    pub fn for_row(row: &Row, owner: (Snowflake, u64, u64), text: u16) -> Self {
        Self {
            account: owner.0,
            generation: owner.1,
            session: owner.2,
            channel: row.message.channel_id,
            message: row.message.id,
            revision: row.revision,
            text,
        }
    }

    fn content(self, owner: Option<(Snowflake, u64, u64)>, snapshot: &Snapshot) -> Option<&str> {
        if owner != Some((self.account, self.generation, self.session))
            || snapshot.channel_id != Some(self.channel)
            || snapshot.interaction_session != self.session
        {
            return None;
        }
        let row = snapshot.rows.iter().find(|row| {
            row.message.id == self.message
                && row.message.channel_id == self.channel
                && row.revision == self.revision
                && row.hidden.is_none()
        })?;
        text_at(&row.message.components, usize::from(self.text))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Spoiler(Target, u16),
    Link(Target, String),
}

#[derive(Default)]
pub struct State {
    revealed: HashSet<(Target, u16)>,
}

impl State {
    pub fn hidden(&self, target: Target, spoiler: u16) -> bool {
        crate::chat_prefs::current().spoiler_hidden(self.revealed.contains(&(target, spoiler)))
    }

    /// Only a currently rendered, uncovered safe link can leave this boundary.
    pub fn update(
        &mut self,
        event: Event,
        owner: Option<(Snowflake, u64, u64)>,
        snapshot: &Snapshot,
    ) -> Option<String> {
        let target = match &event {
            Event::Spoiler(target, _) | Event::Link(target, _) => *target,
        };
        let parsed = parse(target.content(owner, snapshot)?)?;
        match event {
            Event::Spoiler(_, id) => {
                if !parsed
                    .blocks
                    .iter()
                    .flat_map(|block| &block.spans)
                    .any(|part| part.spoiler == Some(id))
                {
                    return None;
                }
                if !self.revealed.remove(&(target, id)) {
                    if self.revealed.len() >= MAX_REVEALED {
                        self.revealed.clear();
                    }
                    self.revealed.insert((target, id));
                }
                None
            }
            Event::Link(_, url) => (url.len() <= 2048
                && crate::rich::safe_url(&url)
                && parsed
                    .blocks
                    .iter()
                    .flat_map(|block| &block.spans)
                    .any(|part| {
                        matches!(part.kind, markup::Kind::Link(link) if link == url)
                            && part.spoiler.is_none_or(|id| !self.hidden(target, id))
                    }))
            .then_some(url),
        }
    }

    pub fn reconcile(&mut self, owner: Option<(Snowflake, u64, u64)>, snapshot: &Snapshot) {
        self.revealed
            .retain(|(target, _)| target.content(owner, snapshot).is_some());
    }
}

/// The traversal matches component_view's Text Display ordinals. Hidden
/// containers have no children and cannot contribute text or asset demand.
pub fn text_at(items: &[Component], mut index: usize) -> Option<&str> {
    fn walk<'a>(items: &'a [Component], index: &mut usize) -> Option<&'a str> {
        for item in items {
            match item {
                Component::Text(text) => {
                    if *index == 0 {
                        return Some(text);
                    }
                    *index -= 1;
                }
                Component::Row(body) | Component::Container { body, .. } => {
                    if let Some(text) = walk(body, index) {
                        return Some(text);
                    }
                }
                Component::Section { body, accessory } => {
                    if let Some(text) = walk(body, index) {
                        return Some(text);
                    }
                    if let Some(accessory) = accessory
                        && let Some(text) = walk(std::slice::from_ref(accessory.as_ref()), index)
                    {
                        return Some(text);
                    }
                }
                _ => {}
            }
        }
        None
    }
    walk(items, &mut index)
}

/// Parser limits/malformed nesting may leave delimiters as plain text. Do not
/// expose potentially hidden text through that fallback. Escaped pipes are
/// separate spans, and code deliberately keeps literal delimiters.
pub fn parse(source: &str) -> Option<markup::Parsed<'_>> {
    let parsed = markup::parse(source);
    let unsupported_spoiler = parsed.blocks.iter().any(|block| {
        !matches!(block.kind, markup::BlockKind::Code(_))
            && block.spans.iter().any(|part| {
                part.spoiler.is_none()
                    && !part.style.has(markup::Style::CODE)
                    && part.text.contains("||")
            })
    });
    (!unsupported_spoiler).then_some(parsed)
}

#[derive(Clone, Copy)]
pub struct View<'a> {
    pub rich: crate::rich::Cx<'a>,
    pub state: &'a State,
}

/// Fake scripts exercise only local reveals, never link opening or writes.
#[cfg(feature = "fake-discord")]
pub fn scripted(gateway: &crate::gateway::GatewayPanel, command: &str) -> Option<Event> {
    let mut words = command.split_whitespace();
    if words.next()? != "reveal" {
        return None;
    }
    let message: Snowflake = words.next()?.parse().ok()?;
    let text: u16 = words.next()?.parse().ok()?;
    let spoiler: u16 = words.next()?.parse().ok()?;
    if words.next().is_some() {
        return None;
    }
    let row = gateway
        .timeline
        .rows
        .iter()
        .find(|row| row.message.id == message)?;
    text_at(&row.message.components, usize::from(text))?;
    Some(Event::Spoiler(
        Target::for_row(
            row,
            (
                gateway.navigation.current_user.as_ref()?.id,
                gateway.id,
                gateway.timeline.interaction_session,
            ),
            text,
        ),
        spoiler,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{rich, timeline, variable_list::harness::Ui};
    use std::{collections::HashMap, sync::Arc};
    const OWNER: Option<(Snowflake, u64, u64)> = Some((Snowflake(1), 7, 9));

    fn fixture() -> (Snapshot, Target, Target) {
        let message = serde_json::from_value(serde_json::json!({
            "id":"10","channel_id":"20","author":{"id":"3","username":"app","bot":true},
            "content":"||body||","timestamp":"2026-11-01T00:00:00Z",
            "components":[
                {"type":10,"content":"||first <:fox:123>|| [public](https://example.test) ||[private](https://secret.test)||"},
                {"type":17,"components":[{"type":10,"content":"||second||"}]},
                {"type":17,"spoiler":true,"components":[{"type":10,"content":"||never||"}]}
            ]
        })).unwrap();
        let row = Row::new(Arc::new(message), 4);
        let a = Target::for_row(&row, OWNER.unwrap(), 0);
        let b = Target::for_row(&row, OWNER.unwrap(), 1);
        (
            Snapshot {
                channel_id: Some(Snowflake(20)),
                interaction_session: 9,
                rows: vec![row],
                ..Default::default()
            },
            a,
            b,
        )
    }

    #[test]
    fn component_text_scopes_reveal_and_links_to_current_account_session_revision_and_text() {
        let (mut snapshot, a, b) = fixture();
        let mut state = State::default();
        assert_eq!(
            text_at(&snapshot.rows[0].message.components, 2),
            None,
            "hidden container contributes no ordinal"
        );
        state.update(Event::Spoiler(a, 0), OWNER, &snapshot);
        assert!(state.revealed.contains(&(a, 0)));
        assert!(
            !state.revealed.contains(&(b, 0)),
            "another text's spoiler zero stays separate"
        );
        assert_eq!(
            state
                .update(
                    Event::Link(a, "https://example.test".into()),
                    OWNER,
                    &snapshot
                )
                .as_deref(),
            Some("https://example.test")
        );
        assert!(
            state
                .update(
                    Event::Link(a, "https://injected.test".into()),
                    OWNER,
                    &snapshot
                )
                .is_none()
        );
        assert!(
            state
                .update(
                    Event::Link(a, "javascript:alert(1)".into()),
                    OWNER,
                    &snapshot
                )
                .is_none()
        );
        state.update(Event::Spoiler(a, 999), OWNER, &snapshot);
        assert_eq!(state.revealed.len(), 1, "invented spoiler index refused");
        for owner in [
            None,
            Some((Snowflake(2), 7, 9)),
            Some((Snowflake(1), 8, 9)),
            Some((Snowflake(1), 7, 10)),
        ] {
            assert!(
                state
                    .update(
                        Event::Link(a, "https://example.test".into()),
                        owner,
                        &snapshot
                    )
                    .is_none()
            );
            state.update(Event::Spoiler(b, 0), owner, &snapshot);
            assert!(!state.revealed.contains(&(b, 0)));
        }
        let mut other_channel = snapshot.clone();
        other_channel.channel_id = Some(Snowflake(99));
        assert!(
            state
                .update(
                    Event::Link(a, "https://example.test".into()),
                    OWNER,
                    &other_channel
                )
                .is_none()
        );
        let mut other_session = snapshot.clone();
        other_session.interaction_session += 1;
        assert!(
            state
                .update(
                    Event::Link(a, "https://example.test".into()),
                    OWNER,
                    &other_session
                )
                .is_none()
        );
        snapshot.rows[0].revision += 1;
        state.reconcile(OWNER, &snapshot);
        assert!(state.revealed.is_empty());
        assert!(
            state
                .update(
                    Event::Link(a, "https://example.test".into()),
                    OWNER,
                    &snapshot
                )
                .is_none()
        );
        let current = Target::for_row(&snapshot.rows[0], OWNER.unwrap(), 0);
        state.update(Event::Spoiler(current, 0), OWNER, &snapshot);
        Arc::make_mut(&mut snapshot.rows[0].message)
            .components
            .clear();
        state.reconcile(OWNER, &snapshot);
        assert!(state.revealed.is_empty());
        let (snapshot, a, _) = fixture();
        state.update(Event::Spoiler(a, 0), OWNER, &snapshot);
        state.reconcile(None, &snapshot);
        assert!(state.revealed.is_empty(), "account teardown drops state");
    }

    #[test]
    fn component_text_reveal_budget_retains_no_source_and_resets_on_removal() {
        let (mut snapshot, a, _) = fixture();
        let mut state = State::default();
        for revision in 0..MAX_REVEALED as u64 + 20 {
            snapshot.rows[0].revision = revision;
            let target = Target { revision, ..a };
            state.update(Event::Spoiler(target, 0), OWNER, &snapshot);
            assert!(state.revealed.len() <= MAX_REVEALED);
        }
        assert!(
            std::mem::size_of::<(Target, u16)>() <= 80,
            "fixed byte cost per key"
        );
        snapshot.rows.clear();
        state.reconcile(OWNER, &snapshot);
        assert!(state.revealed.is_empty());
    }

    #[test]
    fn component_text_parser_keeps_emoji_adjacent_spoilers_and_fails_closed_at_limits() {
        for source in [
            "||<:fox:123>||",
            "<:fox:123>||secret||<a:fox:123>",
            "||🦊||",
            "||[secret](https://example.test)||",
        ] {
            let parsed = parse(source).expect("valid delimiters must not be swallowed by emoji");
            assert!(
                parsed
                    .blocks
                    .iter()
                    .flat_map(|b| &b.spans)
                    .any(|p| p.spoiler == Some(0)),
                "{source}"
            );
        }
        for source in [
            format!("{}||secret||", "🦊".repeat(1100)),
            format!("{}||secret||", "x\n".repeat(201)),
            "||unclosed".into(),
        ] {
            assert!(
                parse(&source).is_none(),
                "unhandled delimiter cannot become exposed fallback"
            );
        }
        for source in [
            r"\|\|literal\|\|",
            "`||code||`",
            "```\n||code||\n```",
            "**bold** *italic* __under__ ~~strike~~",
        ] {
            assert!(
                parse(source).is_some(),
                "safe literal/formatting retained: {source}"
            );
        }
    }

    fn clicks(element: iced::Element<'_, timeline::Event>) -> Vec<timeline::Event> {
        let mut ui = Ui::new(600.0, 180.0, element);
        let mut events = ui.redraw();
        ui.draw();
        for y in (4..180).step_by(8) {
            for x in (4..600).step_by(8) {
                for event in ui.click(x as f32, y as f32) {
                    if !events.contains(&event) {
                        events.push(event);
                    }
                }
            }
        }
        events
    }

    #[test]
    fn component_text_actual_widgets_namespace_spoilers_links_and_never_emit_emoji_interests() {
        let (snapshot, a, b) = fixture();
        let row = &snapshot.rows[0];
        let names = rich::Names::default();
        let body_revealed = HashSet::from([(a.message, 0)]);
        let emojis = HashMap::new();
        let pictures = Default::default();
        let cx = rich::Cx {
            names: &names,
            revealed: &body_revealed,
            emojis: &emojis,
            pictures: &pictures,
            now: 0,
        };
        let mut state = State::default();
        let source = text_at(&row.message.components, 0).unwrap();
        let tree_events = clicks(crate::component_view::view(
            row,
            a.channel,
            None,
            OWNER,
            Some(View {
                rich: cx,
                state: &state,
            }),
        ));
        assert!(tree_events.contains(&timeline::Event::ComponentText(Event::Spoiler(a, 0))));
        assert!(
            tree_events.contains(&timeline::Event::ComponentText(Event::Spoiler(b, 0))),
            "text ordinal follows nested container traversal"
        );
        assert!(!tree_events.iter().any(|event| matches!(event,timeline::Event::ComponentText(Event::Spoiler(target,_)) if target.text > 1)),"hidden container publishes no text identity");
        let events = clicks(rich::component(
            source,
            &row.message,
            a,
            View {
                rich: cx,
                state: &state,
            },
        ));
        assert!(
            events.contains(&timeline::Event::ComponentText(Event::Spoiler(a, 0))),
            "body reveal must not reveal component zero"
        );
        assert!(events.contains(&timeline::Event::ComponentText(Event::Link(
            a,
            "https://example.test".into()
        ))));
        assert!(!events.iter().any(|event| matches!(
            event,
            timeline::Event::EmojiVisibility(..)
                | timeline::Event::ToggleSpoiler { .. }
                | timeline::Event::OpenLink(_)
        )));
        assert!(
            state
                .update(
                    Event::Link(a, "https://secret.test".into()),
                    OWNER,
                    &snapshot
                )
                .is_none(),
            "hidden link cannot open"
        );
        state.update(Event::Spoiler(a, 0), OWNER, &snapshot);
        let events = clicks(rich::component(
            source,
            &row.message,
            a,
            View {
                rich: cx,
                state: &state,
            },
        ));
        assert!(!events.contains(&timeline::Event::ComponentText(Event::Spoiler(a, 0))));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, timeline::Event::EmojiVisibility(..)))
        );
        let other = clicks(rich::component(
            text_at(&row.message.components, 1).unwrap(),
            &row.message,
            b,
            View {
                rich: cx,
                state: &state,
            },
        ));
        assert!(other.contains(&timeline::Event::ComponentText(Event::Spoiler(b, 0))));
        state.update(Event::Spoiler(a, 1), OWNER, &snapshot);
        assert_eq!(
            state
                .update(
                    Event::Link(a, "https://secret.test".into()),
                    OWNER,
                    &snapshot
                )
                .as_deref(),
            Some("https://secret.test")
        );
        let body = clicks(
            rich::body(
                "||body||",
                row.message.as_ref(),
                a.channel,
                a.revision,
                rich::Cx {
                    revealed: &HashSet::new(),
                    ..cx
                },
            )
            .unwrap(),
        );
        assert!(
            body.contains(&timeline::Event::ToggleSpoiler {
                message_id: a.message,
                spoiler: 0
            }),
            "component reveal must not reveal body zero"
        );
    }
}
