//! Server "AutoMod" panel (SPEC §16, issue #96): the rules of Discord's own
//! AutoMod listed, created, edited, switched on and off, and deleted by a
//! person with Manage Server. This configures Discord's service; nothing here
//! scans messages or moderates on its own.
//!
//! The list is read when the panel opens and kept only while it is open.
//! Discord's limits are checked in `fastcord-discord` before anything is sent
//! (name, 1000 keywords of 60 characters, 10 patterns of 260, mention limit 50,
//! timeouts up to four weeks, 6 keyword rules and one of each other kind), and
//! Discord's own refusal text is shown as it came. Every write is one explicit
//! click, re-checked against the current read model (Manage Server) when it is
//! sent, at most one is in flight, and none is retried. A rule shows or
//! changes when Discord's answer or its `AUTO_MODERATION_RULE_*` Gateway event
//! arrives; an ambiguous failure says the change may have been applied.
//! Deleting a rule needs an explicit Confirm. Exemptions come from the server's
//! roles and channels in the reducer's `GuildAdmin` read model.

use crate::i18n::{tr, trf, trn};
use crate::panel_kit::{Confirm, Gate, auth_rejected};
use crate::ui::{button, primary_button};
use fastcord_discord::state::navigation::{AdminKind, GuildAdmin, NavigationSnapshot};
use fastcord_discord::{
    AutoModAction, AutoModChange, AutoModFeed, AutoModRequestError, AutoModRule, CREATABLE,
    MAX_ALLOW_KEYWORD, MAX_ALLOW_PRESET, MAX_BLOCK_MESSAGE_CHARS, MAX_KEYWORD_CHARS, MAX_KEYWORDS,
    MAX_MENTION_LIMIT, MAX_REGEX, MAX_REGEX_CHARS, MAX_TIMEOUT_SECONDS, PRESETS, RestClient,
    RestError, TIMEOUTS, TRIGGER_KEYWORD, TRIGGER_MENTION, TRIGGER_PRESET, TRIGGER_PROFILE,
    has_keywords, trigger_name,
};
use fastcord_model::Snowflake;
use iced::widget::{
    checkbox, column, container, pick_list, row, scrollable, space, text, text_input,
};
use iced::{Element, Length, Task};
use std::fmt;

/// Rule text held at once.
const MAX_RULES_BYTES: usize = 1024 * 1024;
/// Entries of a keyword list drawn at once; the rest are counted.
const SHOWN_ENTRIES: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Keyword,
    Regex,
    Allow,
    BlockMessage,
    Mention,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum List {
    Keywords,
    Regex,
    Allow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Save,
    Toggle(Snowflake),
    Delete(Snowflake),
}

#[derive(Clone, Debug)]
pub enum Event {
    Open(Snowflake),
    Close,
    Reload,
    /// The rule list (serial, answer).
    Loaded(u64, Result<Vec<AutoModRule>, RestError>),
    New(u8),
    Edit(Snowflake),
    CancelEdit,
    Toggle(Snowflake),
    AskDelete(Snowflake),
    CancelDelete,
    ConfirmDelete,
    Input(Field, String),
    Add(List),
    Remove(List, usize),
    Enabled(bool),
    Preset(u8, bool),
    Raid(bool),
    Block(bool),
    Alert(bool),
    AlertChannel(Option<Snowflake>),
    Timeout(Option<u32>),
    ExemptRole(Snowflake, bool),
    ExemptChannel(Snowflake, bool),
    Save,
    /// The answer to a create, edit or switch (serial, what it was).
    Done(u64, Op, Result<Option<AutoModRule>, AutoModRequestError>),
    Deleted(u64, Snowflake, Result<(), RestError>),
}

/// Fixed, secret-free text for a failed request; Discord's own refusal text
/// is passed through as it came.
pub fn failure(error: AutoModRequestError) -> String {
    match error {
        AutoModRequestError::Invalid(error) => tr(&error.to_string()).to_owned(),
        AutoModRequestError::Rest(error) => crate::moderation::failure(error),
    }
}

#[derive(Debug)]
struct Editor {
    /// The rule being changed; `None` for a new one.
    id: Option<Snowflake>,
    draft: AutoModRule,
    keyword: String,
    regex: String,
    allow: String,
    mention: String,
}

#[derive(Debug)]
struct Panel {
    guild: Snowflake,
    rules: Vec<AutoModRule>,
    bytes: usize,
    loaded: bool,
    loading: bool,
    error: Option<String>,
    confirm: Confirm<Snowflake>,
    editor: Option<Editor>,
}

#[derive(Default)]
pub struct State {
    panel: Option<Panel>,
    /// Serial of the loads; busy is the write in flight.
    gate: Gate<Op>,
    /// The newest Gateway rule change already applied.
    seen: u64,
}

fn may_manage(admin: Option<&GuildAdmin>, guild: Snowflake) -> bool {
    admin.is_some_and(|a| a.id == guild && a.can_manage_guild)
}

const NEED: &str = "You need Manage Server for AutoMod.";

impl Panel {
    fn upsert(&mut self, rule: AutoModRule) {
        if let Some(at) = self.rules.iter().position(|r| r.id == rule.id) {
            self.bytes = self.bytes + rule.bytes() - self.rules[at].bytes();
            self.rules[at] = rule;
        } else if self.rules.len() < fastcord_discord::MAX_RULES
            && self.bytes + rule.bytes() <= MAX_RULES_BYTES
        {
            self.bytes += rule.bytes();
            self.rules.push(rule);
        }
    }

    fn remove(&mut self, id: Snowflake) {
        if let Some(at) = self.rules.iter().position(|r| r.id == id) {
            self.bytes -= self.rules.remove(at).bytes();
        }
        if self.confirm.is(id) {
            self.confirm.cancel();
        }
    }

    /// Applies one Gateway change; the notice to show, if the open editor's
    /// rule was deleted.
    fn apply(&mut self, change: &AutoModChange) -> Option<&'static str> {
        match change {
            AutoModChange::Upsert(rule) => self.upsert((**rule).clone()),
            AutoModChange::Delete { id, .. } => {
                self.remove(*id);
                if self.editor.as_ref().is_some_and(|e| e.id == Some(*id)) {
                    self.editor = None;
                    return Some("That rule was deleted.");
                }
            }
        }
        None
    }
}

impl Editor {
    fn new(guild: Snowflake, trigger: u8) -> Self {
        let draft = AutoModRule::draft(guild, trigger);
        Self {
            id: None,
            mention: draft.mention_limit.to_string(),
            draft,
            keyword: String::new(),
            regex: String::new(),
            allow: String::new(),
        }
    }

    fn of(rule: &AutoModRule) -> Self {
        Self {
            id: Some(rule.id),
            draft: rule.clone(),
            keyword: String::new(),
            regex: String::new(),
            allow: String::new(),
            mention: rule.mention_limit.to_string(),
        }
    }

    fn list(&mut self, list: List) -> (&mut Vec<String>, usize, usize) {
        let preset = self.draft.trigger == TRIGGER_PRESET;
        match list {
            List::Keywords => (&mut self.draft.keywords, MAX_KEYWORDS, MAX_KEYWORD_CHARS),
            List::Regex => (&mut self.draft.regex, MAX_REGEX, MAX_REGEX_CHARS),
            List::Allow => (
                &mut self.draft.allow,
                if preset {
                    MAX_ALLOW_PRESET
                } else {
                    MAX_ALLOW_KEYWORD
                },
                MAX_KEYWORD_CHARS,
            ),
        }
    }

    /// Moves the typed text into its list: keywords and allowed phrases may be
    /// pasted comma or line separated, a pattern is taken whole. Anything
    /// over Discord's limits is refused with a sentence and not added.
    fn add(&mut self, list: List) -> Result<(), String> {
        let typed = match list {
            List::Keywords => std::mem::take(&mut self.keyword),
            List::Regex => std::mem::take(&mut self.regex),
            List::Allow => std::mem::take(&mut self.allow),
        };
        let entries: Vec<String> = if list == List::Regex {
            vec![typed.trim().to_owned()]
        } else {
            typed
                .split([',', '\n'])
                .map(|e| e.trim().to_owned())
                .collect()
        };
        let (held, max, max_chars) = self.list(list);
        let mut refused = None;
        for entry in entries.into_iter().filter(|e| !e.is_empty()) {
            if entry.chars().count() > max_chars {
                refused = Some(trf("An entry has at most {} characters.", &[&max_chars]));
            } else if held.len() >= max {
                refused = Some(trf("At most {} entries.", &[&max]));
            } else if !held.contains(&entry) {
                held.push(entry);
            }
        }
        refused.map_or(Ok(()), Err)
    }

    fn set_action(&mut self, keep: impl Fn(&AutoModAction) -> bool, add: Option<AutoModAction>) {
        self.draft.actions.retain(|a| keep(a));
        self.draft.actions.extend(add);
    }

    fn block(&self) -> Option<&Option<String>> {
        self.draft.actions.iter().find_map(|a| match a {
            AutoModAction::BlockMessage { message } => Some(message),
            _ => None,
        })
    }

    fn alert(&self) -> Option<Snowflake> {
        self.draft.actions.iter().find_map(|a| match a {
            AutoModAction::Alert { channel } => Some(*channel),
            _ => None,
        })
    }

    fn timeout(&self) -> Option<u32> {
        self.draft.actions.iter().find_map(|a| match a {
            AutoModAction::Timeout { seconds } => Some(*seconds),
            _ => None,
        })
    }
}

fn toggle(list: &mut Vec<Snowflake>, id: Snowflake, on: bool) {
    list.retain(|held| *held != id);
    if on {
        list.push(id);
    }
}

impl State {
    /// The server the panel is open for.
    pub fn page(&self) -> Option<Snowflake> {
        self.panel.as_ref().map(|panel| panel.guild)
    }

    pub fn clear(&mut self) {
        self.panel = None;
        self.gate.reset();
    }

    /// The server the panel needs the roles and channels read model of.
    pub fn wanted(&self) -> Option<Snowflake> {
        self.panel.as_ref().map(|p| p.guild)
    }

    pub fn is_rejected(event: &Event) -> bool {
        match event {
            Event::Loaded(_, r) => auth_rejected(r),
            Event::Deleted(_, _, r) => auth_rejected(r),
            Event::Done(_, _, r) => matches!(
                r,
                Err(AutoModRequestError::Rest(RestError::AuthenticationRequired))
            ),
            _ => false,
        }
    }

    /// Applies the Gateway's rule changes the panel has not seen yet.
    pub fn observe(&mut self, feed: &AutoModFeed) {
        let Some(panel) = &mut self.panel else {
            return;
        };
        if panel.loaded {
            for (seq, change) in &feed.items {
                if *seq > self.seen
                    && change.guild() == panel.guild
                    && let Some(note) = panel.apply(change)
                {
                    self.gate.say(tr(note));
                }
            }
        }
        self.seen = feed.latest();
    }

    fn load(&mut self, client: &RestClient) -> Task<Event> {
        let Some(panel) = &mut self.panel else {
            return Task::none();
        };
        panel.loading = true;
        panel.error = None;
        let (serial, guild, client) = (self.gate.bump(), panel.guild, client.clone());
        Task::perform(
            async move { client.list_automod_rules(guild).await },
            move |result| Event::Loaded(serial, result),
        )
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        client: &RestClient,
    ) -> Task<Event> {
        let admin = navigation.admin.as_ref();
        match event {
            Event::Open(guild) => {
                if !may_manage(admin, guild) {
                    return Task::none();
                }
                self.panel = Some(Panel {
                    guild,
                    rules: Vec::new(),
                    bytes: 0,
                    loaded: false,
                    loading: false,
                    error: None,
                    confirm: Confirm::default(),
                    editor: None,
                });
                self.gate.reset();
                return self.load(client);
            }
            Event::Close => self.clear(),
            Event::Reload => {
                if self
                    .panel
                    .as_ref()
                    .is_some_and(|p| !p.loading && !self.gate.is_busy())
                {
                    return self.load(client);
                }
            }
            Event::Loaded(serial, result) if self.gate.is_current(serial) => {
                let Some(panel) = &mut self.panel else {
                    return Task::none();
                };
                panel.loading = false;
                match result {
                    Ok(rules) => {
                        panel.rules.clear();
                        panel.bytes = 0;
                        for rule in rules {
                            panel.upsert(rule);
                        }
                        panel.loaded = true;
                        self.seen = navigation.automod.latest();
                    }
                    Err(error) => {
                        panel.error = Some(failure(AutoModRequestError::Rest(error)));
                    }
                }
            }
            Event::New(trigger) => {
                if let Some(panel) = self.panel.as_mut().filter(|_| !self.gate.is_busy())
                    && CREATABLE.contains(&trigger)
                    && panel.loaded
                {
                    panel.editor = Some(Editor::new(panel.guild, trigger));
                    self.gate.notice = None;
                }
            }
            Event::Edit(id) => {
                if let Some(panel) = self.panel.as_mut().filter(|_| !self.gate.is_busy())
                    && let Some(rule) = panel.rules.iter().find(|r| r.id == id)
                {
                    panel.editor = Some(Editor::of(rule));
                    self.gate.notice = None;
                    panel.confirm.cancel();
                }
            }
            Event::CancelEdit => {
                if let Some(panel) = self.panel.as_mut().filter(|_| !self.gate.is_busy()) {
                    panel.editor = None;
                }
            }
            Event::AskDelete(id) => {
                if let Some(panel) = self.panel.as_mut().filter(|_| !self.gate.is_busy())
                    && panel.rules.iter().any(|r| r.id == id)
                {
                    panel.confirm.ask(id);
                    self.gate.notice = None;
                }
            }
            Event::CancelDelete => {
                if let Some(panel) = &mut self.panel {
                    panel.confirm.cancel();
                }
            }
            Event::ConfirmDelete => {
                let Some(panel) = &mut self.panel else {
                    return Task::none();
                };
                let Some(id) = panel.confirm.take() else {
                    return Task::none();
                };
                if self.gate.is_busy() || !panel.rules.iter().any(|r| r.id == id) {
                    return Task::none();
                }
                if !may_manage(admin, panel.guild) {
                    self.gate.say(tr(NEED));
                    return Task::none();
                }
                let (serial, guild, client) =
                    (self.gate.begin(Op::Delete(id)), panel.guild, client.clone());
                return Task::perform(
                    async move { client.delete_automod_rule(guild, id).await },
                    move |result| Event::Deleted(serial, id, result),
                );
            }
            Event::Toggle(id) => {
                let Some(panel) = &mut self.panel else {
                    return Task::none();
                };
                if self.gate.is_busy() || panel.loading {
                    return Task::none();
                }
                let Some(original) = panel.rules.iter().find(|r| r.id == id).cloned() else {
                    return Task::none();
                };
                let mut edited = original.clone();
                edited.enabled = !edited.enabled;
                return self.send(Op::Toggle(id), Some(original), edited, admin, client);
            }
            Event::Save => {
                let Some(panel) = &mut self.panel else {
                    return Task::none();
                };
                let Some(editor) = &panel.editor else {
                    return Task::none();
                };
                let edited = editor.draft.clone();
                let original = editor
                    .id
                    .and_then(|id| panel.rules.iter().find(|r| r.id == id).cloned());
                if editor.id.is_some() && original.is_none() {
                    self.gate.say(tr("That rule no longer exists."));
                    return Task::none();
                }
                return self.send(Op::Save, original, edited, admin, client);
            }
            Event::Done(serial, op, result) if self.gate.is_current(serial) => {
                let Some(panel) = &mut self.panel else {
                    return Task::none();
                };
                self.gate.accept(serial);
                let mut reload = false;
                match result {
                    Ok(rule) => {
                        if op == Op::Save {
                            panel.editor = None;
                        }
                        self.gate.say(tr("Done."));
                        match rule {
                            Some(rule) => panel.upsert(rule),
                            // Discord accepted it but the answer was unreadable.
                            None => reload = true,
                        }
                    }
                    Err(error) => self.gate.say(failure(error)),
                }
                if reload {
                    return self.load(client);
                }
            }
            Event::Deleted(serial, id, result) if self.gate.is_current(serial) => {
                let Some(panel) = &mut self.panel else {
                    return Task::none();
                };
                self.gate.accept(serial);
                match result {
                    Ok(()) => {
                        panel.remove(id);
                        self.gate.say(tr("Deleted."));
                    }
                    Err(RestError::ResourceGone) => {
                        panel.remove(id);
                        self.gate.say(tr("That rule was already gone."));
                    }
                    Err(error) => self.gate.say(failure(AutoModRequestError::Rest(error))),
                }
            }
            // Stale answers (a closed or replaced panel) change nothing.
            Event::Loaded(..) | Event::Done(..) | Event::Deleted(..) => {}
            other => self.edit(other),
        }
        Task::none()
    }

    /// One write, checked once more against the read model, sent once.
    fn send(
        &mut self,
        op: Op,
        original: Option<AutoModRule>,
        edited: AutoModRule,
        admin: Option<&GuildAdmin>,
        client: &RestClient,
    ) -> Task<Event> {
        let Some(panel) = &mut self.panel else {
            return Task::none();
        };
        if self.gate.is_busy() || !panel.loaded {
            return Task::none();
        }
        if !may_manage(admin, panel.guild) {
            self.gate.say(tr(NEED));
            return Task::none();
        }
        let (serial, guild, client) = (self.gate.begin(op), panel.guild, client.clone());
        let existing = panel.rules.clone();
        Task::perform(
            async move {
                match original {
                    Some(original) => client.edit_automod_rule(guild, &original, &edited).await,
                    None => client.create_automod_rule(guild, &edited, &existing).await,
                }
            },
            move |result| Event::Done(serial, op, result),
        )
    }

    /// The editor's local changes: nothing is sent.
    fn edit(&mut self, event: Event) {
        let Some(panel) = &mut self.panel else {
            return;
        };
        let Some(editor) = &mut panel.editor else {
            return;
        };
        if self.gate.is_busy() {
            return;
        }
        let trigger = editor.draft.trigger;
        match event {
            Event::Input(Field::Name, value) => editor.draft.name = value,
            Event::Input(Field::Keyword, value) => editor.keyword = value,
            Event::Input(Field::Regex, value) => editor.regex = value,
            Event::Input(Field::Allow, value) => editor.allow = value,
            Event::Input(Field::Mention, value) => {
                editor.mention = value.chars().filter(char::is_ascii_digit).take(3).collect();
                editor.draft.mention_limit = editor.mention.parse().unwrap_or(0);
            }
            Event::Input(Field::BlockMessage, value) => {
                if let Some(AutoModAction::BlockMessage { message }) = editor
                    .draft
                    .actions
                    .iter_mut()
                    .find(|a| matches!(a, AutoModAction::BlockMessage { .. }))
                {
                    *message = (!value.is_empty()).then_some(value);
                }
            }
            Event::Add(list) => {
                if let Err(reason) = editor.add(list) {
                    self.gate.say(reason);
                } else {
                    self.gate.notice = None;
                }
            }
            Event::Remove(list, at) => {
                let (held, ..) = editor.list(list);
                if at < held.len() {
                    held.remove(at);
                }
            }
            Event::Enabled(on) => editor.draft.enabled = on,
            Event::Preset(preset, on) => {
                editor.draft.presets.retain(|p| *p != preset);
                if on {
                    editor.draft.presets.push(preset);
                    editor.draft.presets.sort_unstable();
                }
            }
            Event::Raid(on) => editor.draft.raid_protection = on,
            Event::Block(on) => {
                let kept = editor.block().cloned().flatten();
                editor.set_action(
                    |a| !matches!(a, AutoModAction::BlockMessage { .. }),
                    on.then_some(AutoModAction::BlockMessage { message: kept }),
                );
            }
            Event::Alert(on) => {
                editor.set_action(
                    |a| !matches!(a, AutoModAction::Alert { .. }),
                    // A channel is chosen next; until then it is not valid to send.
                    on.then_some(AutoModAction::Alert {
                        channel: Snowflake(0),
                    }),
                );
            }
            Event::AlertChannel(Some(channel)) => {
                editor.set_action(
                    |a| !matches!(a, AutoModAction::Alert { .. }),
                    Some(AutoModAction::Alert { channel }),
                );
            }
            Event::AlertChannel(None) => {
                editor.set_action(|a| !matches!(a, AutoModAction::Alert { .. }), None);
            }
            Event::Timeout(seconds) => {
                if matches!(trigger, TRIGGER_KEYWORD | TRIGGER_MENTION) {
                    editor.set_action(
                        |a| !matches!(a, AutoModAction::Timeout { .. }),
                        seconds.map(|seconds| AutoModAction::Timeout { seconds }),
                    );
                }
            }
            Event::ExemptRole(id, on) => toggle(&mut editor.draft.exempt_roles, id, on),
            Event::ExemptChannel(id, on) => toggle(&mut editor.draft.exempt_channels, id, on),
            _ => {}
        }
    }
}

fn small<'a>(label: impl Into<String>) -> iced::widget::Text<'a> {
    text(label.into()).size(12).color(crate::style::muted())
}

#[derive(Clone, PartialEq, Eq)]
struct Item<T>(T, String);

impl<T> fmt::Display for Item<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.1)
    }
}

fn duration(seconds: u32) -> String {
    TIMEOUTS
        .iter()
        .find(|(s, _)| *s == seconds)
        .map(|(_, name)| tr(name).to_owned())
        .or_else(|| (seconds == MAX_TIMEOUT_SECONDS).then(|| tr("4 weeks").to_owned()))
        .unwrap_or_else(|| trn(u64::from(seconds), "{} second", "{} seconds"))
}

fn channel_name(admin: Option<&GuildAdmin>, id: Snowflake) -> String {
    admin
        .and_then(|a| a.channel(id))
        .map_or_else(|| id.to_string(), |c| format!("#{}", c.name))
}

fn role_name(admin: Option<&GuildAdmin>, id: Snowflake) -> String {
    admin
        .and_then(|a| a.role(id))
        .map_or_else(|| id.to_string(), |r| r.name.clone())
}

/// What a rule does, in one line.
fn actions_line(rule: &AutoModRule, admin: Option<&GuildAdmin>) -> String {
    let parts: Vec<String> = rule
        .actions
        .iter()
        .map(|action| match action {
            AutoModAction::BlockMessage { message: None } => tr("Block message").to_owned(),
            AutoModAction::BlockMessage { message: Some(_) } => {
                tr("Block message with a custom message").to_owned()
            }
            AutoModAction::Alert { channel } => {
                trf("Send an alert to {}", &[&channel_name(admin, *channel)])
            }
            AutoModAction::Timeout { seconds } => trf("Time out for {}", &[&duration(*seconds)]),
            AutoModAction::BlockInteraction => tr("Block member interaction").to_owned(),
            AutoModAction::Other(_) => tr("Another action").to_owned(),
        })
        .collect();
    parts.join(", ")
}

/// What a rule matches, in one line.
fn trigger_line(rule: &AutoModRule) -> String {
    let kind = tr(&trigger_name(rule.trigger)).to_owned();
    match rule.trigger {
        TRIGGER_KEYWORD | TRIGGER_PROFILE => trf(
            "{}: {} keywords, {} patterns, {} allowed",
            &[
                &kind,
                &rule.keywords.len(),
                &rule.regex.len(),
                &rule.allow.len(),
            ],
        ),
        TRIGGER_PRESET => {
            let names: Vec<&str> = PRESETS
                .iter()
                .filter(|(id, _)| rule.presets.contains(id))
                .map(|(_, name)| tr(name))
                .collect();
            format!("{kind}: {}", names.join(", "))
        }
        TRIGGER_MENTION => trf(
            "{}: more than {} mentions{}",
            &[
                &kind,
                &rule.mention_limit,
                &if rule.raid_protection {
                    tr(", with raid protection")
                } else {
                    ""
                },
            ],
        ),
        _ => kind,
    }
}

fn exemptions_line(rule: &AutoModRule, admin: Option<&GuildAdmin>) -> Option<String> {
    if rule.exempt_roles.is_empty() && rule.exempt_channels.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = rule
        .exempt_roles
        .iter()
        .map(|id| role_name(admin, *id))
        .collect();
    parts.extend(
        rule.exempt_channels
            .iter()
            .map(|id| channel_name(admin, *id)),
    );
    Some(trf("Exempt: {}", &[&parts.join(", ")]))
}

fn rule_row<'a>(
    panel: &'a Panel,
    gate: &Gate<Op>,
    rule: &'a AutoModRule,
    admin: Option<&GuildAdmin>,
) -> Element<'a, Event> {
    let idle = !gate.is_busy() && !panel.loading && panel.editor.is_none();
    let mut info = column![
        text(rule.name.clone()).size(13),
        small(trigger_line(rule)),
        small(actions_line(rule, admin)),
    ]
    .spacing(1);
    if let Some(line) = exemptions_line(rule, admin) {
        info = info.push(small(line));
    }
    let control: Element<'_, Event> = if panel.confirm.is(rule.id) {
        row![
            text(tr("Delete this rule?")).size(12).style(text::danger),
            button(text(tr("Confirm")).size(12))
                .on_press_maybe(idle.then_some(Event::ConfirmDelete))
                .style(button::danger),
            button(text(tr("Cancel")).size(12)).on_press(Event::CancelDelete),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into()
    } else {
        row![
            checkbox(rule.enabled)
                .label(if rule.enabled { tr("On") } else { tr("Off") })
                .on_toggle_maybe(idle.then_some(|_| Event::Toggle(rule.id))),
            button(text(tr("Edit")).size(12)).on_press_maybe(idle.then_some(Event::Edit(rule.id))),
            button(text(tr("Delete")).size(12))
                .on_press_maybe(idle.then_some(Event::AskDelete(rule.id))),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into()
    };
    row![info.width(Length::Fill), control]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()
}

fn entries<'a>(
    title: &str,
    list: List,
    held: &'a [String],
    max: usize,
    typed: &'a str,
    field: Field,
) -> Element<'a, Event> {
    let mut rows = column![].spacing(2);
    for (at, entry) in held.iter().enumerate().take(SHOWN_ENTRIES) {
        rows = rows.push(
            row![
                text(entry.clone()).size(12).width(Length::Fill),
                button(text("x").size(11)).on_press(Event::Remove(list, at)),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        );
    }
    if held.len() > SHOWN_ENTRIES {
        rows = rows.push(small(trf("and {} more", &[&(held.len() - SHOWN_ENTRIES)])));
    }
    column![
        small(trf("{} ({}/{})", &[&tr(title), &held.len(), &max])),
        rows,
        row![
            text_input(tr("Add"), typed)
                .on_input(move |v| Event::Input(field, v))
                .on_submit(Event::Add(list))
                .size(12),
            button(text(tr("Add")).size(12)).on_press(Event::Add(list)),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
    ]
    .spacing(3)
    .into()
}

fn editor_view<'a>(
    gate: &Gate<Op>,
    editor: &'a Editor,
    admin: Option<&'a GuildAdmin>,
) -> Element<'a, Event> {
    let draft = &editor.draft;
    let trigger = draft.trigger;
    let mut form = column![
        row![
            text(if editor.id.is_some() {
                tr("Edit rule")
            } else {
                tr("New rule")
            })
            .size(13),
            small(tr(&trigger_name(trigger))),
        ]
        .spacing(8),
        text_input(tr("Rule name"), &draft.name)
            .on_input(|v| Event::Input(Field::Name, v))
            .size(13),
        checkbox(draft.enabled)
            .label(tr("Enabled"))
            .on_toggle(Event::Enabled),
    ]
    .spacing(6);
    if has_keywords(trigger) {
        form = form
            .push(entries(
                tr("Keywords (* matches any text)"),
                List::Keywords,
                &draft.keywords,
                MAX_KEYWORDS,
                &editor.keyword,
                Field::Keyword,
            ))
            .push(entries(
                tr("Regex patterns"),
                List::Regex,
                &draft.regex,
                MAX_REGEX,
                &editor.regex,
                Field::Regex,
            ));
    }
    if has_keywords(trigger) || trigger == TRIGGER_PRESET {
        form = form.push(entries(
            tr("Allowed phrases"),
            List::Allow,
            &draft.allow,
            if trigger == TRIGGER_PRESET {
                MAX_ALLOW_PRESET
            } else {
                MAX_ALLOW_KEYWORD
            },
            &editor.allow,
            Field::Allow,
        ));
    }
    if trigger == TRIGGER_PRESET {
        let mut lists = row![small(tr("Word lists"))].spacing(10);
        for (id, name) in PRESETS {
            lists = lists.push(
                checkbox(draft.presets.contains(&id))
                    .label(tr(name))
                    .on_toggle(move |on| Event::Preset(id, on)),
            );
        }
        form = form.push(lists);
    }
    if trigger == TRIGGER_MENTION {
        form = form
            .push(
                row![
                    small(trf(
                        "Mentions in one message (1 to {})",
                        &[&MAX_MENTION_LIMIT]
                    )),
                    text_input("10", &editor.mention)
                        .on_input(|v| Event::Input(Field::Mention, v))
                        .size(12)
                        .width(60),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            )
            .push(
                checkbox(draft.raid_protection)
                    .label(tr(
                        "Raid protection: also count mentions in a burst of new members",
                    ))
                    .on_toggle(Event::Raid),
            );
    }
    form = form.push(small(tr("Actions")));
    if !matches!(trigger, TRIGGER_PROFILE) {
        let block = editor.block();
        form = form.push(
            checkbox(block.is_some())
                .label(tr("Block the message"))
                .on_toggle(Event::Block),
        );
        if let Some(message) = block {
            form = form.push(
                text_input(
                    &trf(
                        "Message shown to the author (up to {})",
                        &[&MAX_BLOCK_MESSAGE_CHARS],
                    ),
                    message.as_deref().unwrap_or(""),
                )
                .on_input(|v| Event::Input(Field::BlockMessage, v))
                .size(12),
            );
        }
    }
    let alert = editor.alert();
    let channels: Vec<Item<Snowflake>> = admin
        .map(|a| {
            a.channels
                .iter()
                .filter(|c| matches!(c.kind, AdminKind::Text | AdminKind::Announcement))
                .map(|c| Item(c.id, format!("#{}", c.name)))
                .collect()
        })
        .unwrap_or_default();
    form = form.push(
        row![
            checkbox(alert.is_some())
                .label(tr("Send an alert to"))
                .on_toggle(Event::Alert),
            pick_list(
                channels.clone(),
                alert
                    .filter(|c| *c != Snowflake(0))
                    .map(|c| Item(c, channel_name(admin, c))),
                |item: Item<Snowflake>| Event::AlertChannel(Some(item.0)),
            )
            .text_size(12)
            .placeholder(tr("Choose a channel")),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    );
    if matches!(trigger, TRIGGER_KEYWORD | TRIGGER_MENTION) {
        let mut options: Vec<Item<u32>> = TIMEOUTS
            .iter()
            .map(|(s, name)| Item(*s, tr(name).to_owned()))
            .collect();
        options.push(Item(MAX_TIMEOUT_SECONDS, tr("4 weeks").to_owned()));
        if let Some(current) = editor
            .timeout()
            .filter(|c| options.iter().all(|o| o.0 != *c))
        {
            options.push(Item(current, duration(current)));
        }
        let current = editor.timeout();
        form = form.push(
            row![
                checkbox(current.is_some())
                    .label(tr("Time the member out for"))
                    .on_toggle(|on| Event::Timeout(on.then_some(60))),
                pick_list(
                    options,
                    current.map(|s| Item(s, duration(s))),
                    |item: Item<u32>| Event::Timeout(Some(item.0)),
                )
                .text_size(12),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
    }
    if let Some(admin) = admin {
        let roles = column(admin.roles.iter().filter(|r| !r.everyone).map(|r| {
            checkbox(draft.exempt_roles.contains(&r.id))
                .label(r.name.clone())
                .on_toggle(|on| Event::ExemptRole(r.id, on))
                .text_size(12)
                .into()
        }))
        .spacing(2);
        let chans = column(
            admin
                .channels
                .iter()
                .filter(|c| c.kind != AdminKind::Category)
                .map(|c| {
                    checkbox(draft.exempt_channels.contains(&c.id))
                        .label(format!("#{}", c.name))
                        .on_toggle(|on| Event::ExemptChannel(c.id, on))
                        .text_size(12)
                        .into()
                }),
        )
        .spacing(2);
        form = form
            .push(small(trf(
                "Exempt roles ({}/{})",
                &[
                    &draft.exempt_roles.len(),
                    &fastcord_discord::MAX_EXEMPT_ROLES,
                ],
            )))
            .push(scrollable(roles).height(Length::Fixed(90.0)))
            .push(small(trf(
                "Exempt channels ({}/{})",
                &[
                    &draft.exempt_channels.len(),
                    &fastcord_discord::MAX_EXEMPT_CHANNELS,
                ],
            )))
            .push(scrollable(chans).height(Length::Fixed(90.0)));
    }
    form.push(
        row![
            primary_button(text(tr("Save rule")).size(13))
                .on_press_maybe((!gate.is_busy()).then_some(Event::Save)),
            button(text(tr("Cancel")).size(13))
                .on_press_maybe((!gate.is_busy()).then_some(Event::CancelEdit)),
        ]
        .spacing(8),
    )
    .into()
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
) -> Option<Element<'a, Event>> {
    let panel = state.panel.as_ref()?;
    let admin = navigation.admin.as_ref().filter(|a| a.id == panel.guild);
    let can = may_manage(admin, panel.guild);
    let idle = !state.gate.is_busy() && !panel.loading;
    let mut news = row![small(tr("New rule"))]
        .spacing(6)
        .align_y(iced::Alignment::Center);
    for trigger in CREATABLE {
        news = news.push(
            button(text(tr(&trigger_name(trigger)).to_owned()).size(12)).on_press_maybe(
                (can && idle && panel.loaded && panel.editor.is_none())
                    .then_some(Event::New(trigger)),
            ),
        );
    }
    let mut view = column![
        row![
            space::horizontal(),
            button(text(tr("Reload")).size(13)).on_press_maybe(idle.then_some(Event::Reload)),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
        news,
    ]
    .spacing(6);
    if !can {
        view = view.push(small(tr(NEED)));
    }
    if panel.loaded && panel.rules.is_empty() {
        view = view.push(small(tr("No AutoMod rules.")));
    }
    if let Some(editor) = &panel.editor {
        view = view.push(editor_view(&state.gate, editor, admin));
    } else {
        let rows = column(
            panel
                .rules
                .iter()
                .map(|rule| rule_row(panel, &state.gate, rule, admin)),
        )
        .spacing(6);
        view = view.push(scrollable(rows).height(Length::Shrink));
    }
    if panel.loading {
        view = view.push(small(tr("Loading\u{2026}")));
    }
    if state.gate.is_busy() {
        view = view.push(small(tr("Waiting for Discord\u{2026}")));
    }
    for line in [&panel.error, &state.gate.notice].into_iter().flatten() {
        view = view.push(text(line.clone()).size(12));
    }
    Some(container(view).padding([6, 0]).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::gateway::{ClientProperties, HostOs};
    use fastcord_discord::{
        NetworkFailure, Refusal, RetryableFailure, RuleError, StatusCode, TRIGGER_SPAM, UserToken,
    };

    const G: Snowflake = Snowflake(100);

    fn client() -> RestClient {
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-test-credential".to_owned()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn navigation(manage: bool) -> NavigationSnapshot {
        NavigationSnapshot {
            admin: Some(GuildAdmin {
                id: G,
                name: "Home".into(),
                has_icon: false,
                owner: false,
                community: false,
                description: String::new(),
                system_channel: None,
                afk_channel: None,
                afk_timeout: 0,
                notifications: 0,
                content_filter: 0,
                verification_level: 0,
                rules_channel: None,
                public_updates_channel: None,
                features: Vec::new(),
                mfa_required: false,
                safety_alerts_channel: None,
                raid_alerts_disabled: false,
                can_manage_guild: manage,
                can_manage_channels: false,
                channels: Vec::new(),
                people: Vec::new(),
                can_manage_roles: false,
                roles: Vec::new(),
                mine: fastcord_model::Permissions::NONE,
                grant_all: false,
                top_position: 0,
            }),
            ..NavigationSnapshot::default()
        }
    }

    fn rule(id: u64, trigger: u8) -> AutoModRule {
        let mut rule = AutoModRule::draft(G, trigger);
        rule.id = Snowflake(id);
        rule.name = format!("rule {id}");
        rule.keywords = vec!["spam".into()];
        rule.presets = vec![1];
        rule
    }

    /// An open, loaded panel holding rules 1 (keyword) and 2 (mention spam).
    fn loaded() -> (State, NavigationSnapshot) {
        let (mut state, nav) = (State::default(), navigation(true));
        let _ = state.update(Event::Open(G), &nav, &client());
        let serial = state.gate.serial();
        let rules = vec![rule(1, TRIGGER_KEYWORD), rule(2, TRIGGER_MENTION)];
        let _ = state.update(Event::Loaded(serial, Ok(rules)), &nav, &client());
        (state, nav)
    }

    fn panel(state: &State) -> &Panel {
        state.panel.as_ref().unwrap()
    }

    fn refused(status: u16, code: Option<u32>, message: &str) -> RestError {
        RestError::Refused(Refusal::new(
            StatusCode::from_u16(status).unwrap(),
            code,
            message,
        ))
    }

    #[tokio::test]
    async fn the_panel_needs_manage_server_and_is_checked_again_on_every_write() {
        let mut state = State::default();
        let _ = state.update(Event::Open(G), &navigation(false), &client());
        assert!(state.panel.is_none());
        let (mut state, nav) = loaded();
        assert_eq!(state.wanted(), Some(G));
        // The permission is lost while the panel is open: nothing is sent.
        let none = navigation(false);
        let _ = state.update(Event::Toggle(Snowflake(1)), &none, &client());
        assert!(!state.gate.is_busy() && state.gate.notice.is_some());
        let _ = state.update(Event::AskDelete(Snowflake(1)), &nav, &client());
        let _ = state.update(Event::ConfirmDelete, &none, &client());
        assert!(!state.gate.is_busy());
        assert_eq!(panel(&state).rules.len(), 2);
    }

    #[tokio::test]
    async fn a_delete_needs_confirm_is_sent_once_and_the_row_goes_only_on_the_answer() {
        let (mut state, nav) = loaded();
        let _ = state.update(Event::ConfirmDelete, &nav, &client());
        assert!(!state.gate.is_busy(), "no Confirm, no request");
        let _ = state.update(Event::AskDelete(Snowflake(9)), &nav, &client());
        assert!(
            panel(&state).confirm.pending().is_none(),
            "not a listed rule"
        );
        let _ = state.update(Event::AskDelete(Snowflake(1)), &nav, &client());
        let _ = state.update(Event::ConfirmDelete, &nav, &client());
        assert_eq!(state.gate.busy(), Some(Op::Delete(Snowflake(1))));
        // Nothing else is sent while it waits.
        let _ = state.update(Event::Toggle(Snowflake(2)), &nav, &client());
        let _ = state.update(Event::AskDelete(Snowflake(2)), &nav, &client());
        assert_eq!(state.gate.busy(), Some(Op::Delete(Snowflake(1))));
        assert!(panel(&state).confirm.pending().is_none());
        let serial = state.gate.serial();
        let timeout = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout));
        let _ = state.update(
            Event::Deleted(serial, Snowflake(1), Err(timeout)),
            &nav,
            &client(),
        );
        assert_eq!(panel(&state).rules.len(), 2);
        assert!(
            state
                .gate
                .notice
                .as_ref()
                .unwrap()
                .contains("may have been applied")
        );
        let _ = state.update(
            Event::Deleted(serial, Snowflake(1), Ok(())),
            &nav,
            &client(),
        );
        assert_eq!(panel(&state).rules.len(), 1);
        // Already gone counts as done.
        let _ = state.update(
            Event::Deleted(serial, Snowflake(2), Err(RestError::ResourceGone)),
            &nav,
            &client(),
        );
        assert!(panel(&state).rules.is_empty());
    }

    #[tokio::test]
    async fn a_new_rule_is_validated_sent_once_and_applied_from_the_answer() {
        let (mut state, nav) = loaded();
        let _ = state.update(Event::New(TRIGGER_SPAM), &nav, &client());
        assert!(panel(&state).editor.is_some());
        // An empty name is Discord's limit, said without a request.
        let _ = state.update(Event::Save, &nav, &client());
        assert_eq!(state.gate.busy(), Some(Op::Save));
        let serial = state.gate.serial();
        let _ = state.update(
            Event::Done(
                serial,
                Op::Save,
                Err(AutoModRequestError::Invalid(RuleError::BadName)),
            ),
            &nav,
            &client(),
        );
        assert_eq!(
            state.gate.notice.as_deref(),
            Some("A rule name has 1 to 100 characters.")
        );
        assert!(
            panel(&state).editor.is_some(),
            "the form keeps what was typed"
        );
        let _ = state.update(Event::Input(Field::Name, "Spam".into()), &nav, &client());
        let _ = state.update(Event::Save, &nav, &client());
        // A second click while it waits changes nothing.
        let _ = state.update(Event::Save, &nav, &client());
        assert_eq!(state.gate.busy(), Some(Op::Save));
        let mut made = rule(7, TRIGGER_SPAM);
        made.name = "Spam".into();
        let _ = state.update(
            Event::Done(serial, Op::Save, Ok(Some(made.clone()))),
            &nav,
            &client(),
        );
        assert!(!state.gate.is_busy() && panel(&state).editor.is_none());
        assert_eq!(panel(&state).rules.len(), 3);
        // The Gateway's echo of the same rule changes nothing.
        let mut feed = AutoModFeed::default();
        feed.push(AutoModChange::Upsert(Box::new(made)));
        state.observe(&feed);
        assert_eq!(panel(&state).rules.len(), 3);
    }

    #[tokio::test]
    async fn refusals_are_shown_as_discord_said_them() {
        let (mut state, nav) = loaded();
        let _ = state.update(Event::Toggle(Snowflake(1)), &nav, &client());
        let serial = state.gate.serial();
        let say = |state: &mut State, error: RestError| {
            let _ = state.update(
                Event::Done(
                    serial,
                    Op::Toggle(Snowflake(1)),
                    Err(AutoModRequestError::Rest(error)),
                ),
                &nav,
                &client(),
            );
            state.gate.notice.clone().unwrap()
        };
        let text = say(
            &mut state,
            refused(400, Some(50035), "Maximum number of AutoMod rules reached"),
        );
        assert!(text.contains("Maximum number of AutoMod rules reached"));
        assert!(say(&mut state, refused(403, None, "")).contains("lack the permission"));
        assert!(say(&mut state, refused(404, None, "")).contains("no longer exists"));
        assert!(say(&mut state, refused(429, None, "")).contains("rate limiting"));
        assert!(say(&mut state, refused(403, Some(60003), "")).contains("two-factor"));
        assert!(!state.gate.is_busy());
        assert_eq!(panel(&state).rules.len(), 2, "a failure changes nothing");
    }

    #[tokio::test]
    async fn the_editor_bounds_what_is_typed_as_discord_does() {
        let (mut state, nav) = loaded();
        let _ = state.update(Event::New(TRIGGER_KEYWORD), &nav, &client());
        let type_in = |state: &mut State, field, text: &str| {
            let _ = state.update(Event::Input(field, text.into()), &nav, &client());
        };
        type_in(&mut state, Field::Keyword, "one, two,\nthree, one");
        let _ = state.update(Event::Add(List::Keywords), &nav, &client());
        let held = |state: &State| panel(state).editor.as_ref().unwrap().draft.keywords.clone();
        assert_eq!(held(&state), ["one", "two", "three"]);
        type_in(&mut state, Field::Keyword, &"k".repeat(61));
        let _ = state.update(Event::Add(List::Keywords), &nav, &client());
        assert_eq!(held(&state).len(), 3);
        assert!(state.gate.notice.is_some());
        // Patterns are taken whole, at most ten.
        for n in 0..11 {
            type_in(&mut state, Field::Regex, &format!("a,{n}"));
            let _ = state.update(Event::Add(List::Regex), &nav, &client());
        }
        let regex = panel(&state).editor.as_ref().unwrap().draft.regex.len();
        assert_eq!(regex, MAX_REGEX);
        let _ = state.update(Event::Remove(List::Keywords, 0), &nav, &client());
        assert_eq!(held(&state), ["two", "three"]);
        // Timeouts exist only where Discord allows them.
        let _ = state.update(Event::Timeout(Some(3_600)), &nav, &client());
        assert_eq!(
            panel(&state).editor.as_ref().unwrap().timeout(),
            Some(3_600)
        );
        let _ = state.update(Event::Block(false), &nav, &client());
        assert!(panel(&state).editor.as_ref().unwrap().block().is_none());
        let _ = state.update(Event::CancelEdit, &nav, &client());
        let _ = state.update(Event::New(TRIGGER_SPAM), &nav, &client());
        let _ = state.update(Event::Timeout(Some(60)), &nav, &client());
        assert_eq!(panel(&state).editor.as_ref().unwrap().timeout(), None);
        // Member profile rules cannot be created here.
        let _ = state.update(Event::CancelEdit, &nav, &client());
        let _ = state.update(Event::New(TRIGGER_PROFILE), &nav, &client());
        assert!(panel(&state).editor.is_none());
    }

    #[tokio::test]
    async fn editing_a_rule_keeps_unknown_actions_and_exemptions_toggle() {
        let (mut state, nav) = loaded();
        let _ = state.update(Event::Edit(Snowflake(1)), &nav, &client());
        let other = AutoModAction::Other(serde_json::json!({"type": 99}));
        state
            .panel
            .as_mut()
            .unwrap()
            .editor
            .as_mut()
            .unwrap()
            .draft
            .actions
            .push(other.clone());
        let _ = state.update(Event::Block(false), &nav, &client());
        let _ = state.update(Event::AlertChannel(Some(Snowflake(5))), &nav, &client());
        let _ = state.update(Event::ExemptRole(Snowflake(8), true), &nav, &client());
        let _ = state.update(Event::ExemptRole(Snowflake(8), true), &nav, &client());
        let _ = state.update(Event::ExemptChannel(Snowflake(9), true), &nav, &client());
        let _ = state.update(Event::ExemptChannel(Snowflake(9), false), &nav, &client());
        let draft = &panel(&state).editor.as_ref().unwrap().draft;
        assert!(draft.actions.contains(&other));
        assert!(draft.actions.contains(&AutoModAction::Alert {
            channel: Snowflake(5)
        }));
        assert_eq!(draft.exempt_roles, [Snowflake(8)]);
        assert!(draft.exempt_channels.is_empty());
    }

    #[tokio::test]
    async fn gateway_changes_update_the_open_list_and_stale_or_foreign_ones_do_not() {
        let (mut state, _) = loaded();
        let mut feed = AutoModFeed::default();
        let mut changed = rule(1, TRIGGER_KEYWORD);
        changed.name = "renamed".into();
        feed.push(AutoModChange::Upsert(Box::new(changed)));
        feed.push(AutoModChange::Upsert(Box::new(rule(3, TRIGGER_SPAM))));
        feed.push(AutoModChange::Delete {
            guild: G,
            id: Snowflake(2),
        });
        let mut other_guild = rule(4, TRIGGER_SPAM);
        other_guild.guild = Snowflake(999);
        feed.push(AutoModChange::Upsert(Box::new(other_guild)));
        state.observe(&feed);
        let names: Vec<_> = panel(&state).rules.iter().map(|r| r.id.0).collect();
        assert_eq!(names, [1, 3]);
        assert_eq!(panel(&state).rules[0].name, "renamed");
        // A replayed feed changes nothing.
        state.observe(&feed);
        assert_eq!(panel(&state).rules.len(), 2);
        // A rule deleted elsewhere closes its editor.
        let nav = navigation(true);
        let _ = state.update(Event::Edit(Snowflake(3)), &nav, &client());
        let mut feed = feed.clone();
        feed.push(AutoModChange::Delete {
            guild: G,
            id: Snowflake(3),
        });
        state.observe(&feed);
        assert!(panel(&state).editor.is_none() && panel(&state).rules.len() == 1);
        // Before the list is loaded, changes are not applied.
        let mut fresh = State::default();
        let _ = fresh.update(Event::Open(G), &nav, &client());
        fresh.observe(&feed);
        assert!(panel(&fresh).rules.is_empty());
    }

    #[tokio::test]
    async fn stale_answers_and_a_rejected_login_are_handled() {
        let (mut state, nav) = loaded();
        let serial = state.gate.serial();
        let _ = state.update(Event::Loaded(serial - 1, Ok(Vec::new())), &nav, &client());
        assert_eq!(panel(&state).rules.len(), 2);
        let _ = state.update(Event::Reload, &nav, &client());
        assert!(panel(&state).loading);
        let _ = state.update(
            Event::Loaded(state.gate.serial(), Err(RestError::PermissionDenied)),
            &nav,
            &client(),
        );
        assert!(panel(&state).error.is_some() && panel(&state).rules.len() == 2);
        let _ = state.update(Event::Close, &nav, &client());
        let _ = state.update(
            Event::Deleted(serial, Snowflake(1), Ok(())),
            &nav,
            &client(),
        );
        assert!(state.panel.is_none());
        assert!(State::is_rejected(&Event::Loaded(
            1,
            Err(RestError::AuthenticationRequired)
        )));
        assert!(State::is_rejected(&Event::Done(
            1,
            Op::Save,
            Err(AutoModRequestError::Rest(RestError::AuthenticationRequired))
        )));
        assert!(!State::is_rejected(&Event::Deleted(
            1,
            Snowflake(1),
            Err(RestError::PermissionDenied)
        )));
    }

    #[test]
    fn rules_are_described_in_one_line_each() {
        let mut rule = rule(1, TRIGGER_KEYWORD);
        rule.actions = vec![
            AutoModAction::BlockMessage {
                message: Some("no".into()),
            },
            AutoModAction::Timeout { seconds: 3_600 },
            AutoModAction::Timeout {
                seconds: MAX_TIMEOUT_SECONDS,
            },
        ];
        assert_eq!(
            actions_line(&rule, None),
            "Block message with a custom message, Time out for 1 hour, Time out for 4 weeks"
        );
        assert_eq!(
            trigger_line(&rule),
            "Keywords: 1 keywords, 0 patterns, 0 allowed"
        );
        rule.exempt_roles = vec![Snowflake(5)];
        assert_eq!(exemptions_line(&rule, None).as_deref(), Some("Exempt: 5"));
    }
}
