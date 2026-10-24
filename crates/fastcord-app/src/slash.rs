//! Slash commands and modals of applications (SPEC §9): the command list a
//! leading `/` opens, the typed option form of the chosen command, and the
//! form an application asks the user to fill in.
//!
//! The command list comes from `application-command-index` (unverified, see
//! PROTOCOL.md) and is cached per scope: at most [`MAX_SCOPES`] indexes within
//! [`MAX_CACHE_BYTES`], each reused for [`INDEX_TTL`] (also unverified). A
//! failed fetch is not repeated until the user clears the `/`. Choosing a
//! command only opens its form; nothing is sent until the user presses Run,
//! and then the worker re-checks the channel, permissions and session and
//! sends one interaction (`crate::interactions`). Autocomplete, attachment
//! options and subcommand groups are not offered.

use std::sync::Arc;
use std::time::{Duration, Instant};

use fastcord_discord::{
    AppCommand, CommandArg, CommandIndex, CommandOption, IndexScope, InteractionModal,
    OPTION_BOOLEAN, OPTION_SUB_COMMAND, RestClient, RestError,
};
use fastcord_model::Snowflake;
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Task};

use crate::gateway::NavigationBridge;
use crate::history::InteractionView;
use crate::interactions::Action;

/// Command lists kept at once.
pub const MAX_SCOPES: usize = 4;
/// Heap bytes all kept command lists may hold together.
pub const MAX_CACHE_BYTES: usize = 1024 * 1024;
/// How long a fetched list is reused. The official client's refresh rule is
/// not known; this is a short guess.
pub const INDEX_TTL: Duration = Duration::from_secs(5 * 60);
/// Commands listed at once.
const MAX_LISTED: usize = 8;
const FAILED: &str = "The command list could not be loaded.";
const NOT_QUEUED: &str = "Not sent: the connection is busy. Try again.";
const REQUIRED: &str = "Fill in every option marked with *.";
const CHOOSE_SUB: &str = "Choose a subcommand first.";
const CLOSED: &str = "That command is no longer available here.";

#[derive(Clone, Debug)]
pub enum Event {
    /// The answer to a command list request.
    Index(u64, IndexScope, Result<Arc<CommandIndex>, RestError>),
    /// A command of the list was chosen: open its form.
    Pick(Snowflake),
    /// A subcommand tab of the form.
    Sub(usize),
    /// A value for the form's option at this position.
    Field(usize, String),
    Run,
    CloseForm,
    ModalField(Snowflake, usize, String),
    ModalSubmit,
    ModalCancel,
}

struct Cached {
    scope: IndexScope,
    index: Arc<CommandIndex>,
    at: Instant,
}

struct Loading {
    scope: IndexScope,
    serial: u64,
    /// Dropping this aborts the request.
    _handle: iced::task::Handle,
}

struct Form {
    channel: Snowflake,
    command: AppCommand,
    /// The chosen subcommand, as an index into `command.options`.
    sub: Option<usize>,
    values: Vec<String>,
    error: Option<&'static str>,
}

impl Form {
    fn fields(&self) -> &[CommandOption] {
        match self.sub {
            Some(at) => &self.command.options[at].options,
            None if self.has_subcommands() => &[],
            None => &self.command.options,
        }
    }

    fn has_subcommands(&self) -> bool {
        self.command
            .options
            .iter()
            .any(|option| option.kind == OPTION_SUB_COMMAND)
    }

    /// The subcommand name and typed options, if every required one is filled.
    fn arguments(&self) -> Result<(Option<String>, Vec<CommandArg>), &'static str> {
        if self.has_subcommands() && self.sub.is_none() {
            return Err(CHOOSE_SUB);
        }
        let mut args = Vec::new();
        for (option, value) in self.fields().iter().zip(&self.values) {
            if option.required && value.trim().is_empty() {
                return Err(REQUIRED);
            }
            args.push(CommandArg {
                name: option.name.clone(),
                kind: option.kind,
                text: value.clone(),
            });
        }
        let sub = self.sub.map(|at| self.command.options[at].name.clone());
        Ok((sub, args))
    }
}

/// The answers being typed into the open modal.
struct ModalForm {
    id: Snowflake,
    values: Vec<String>,
}

#[derive(Default)]
pub struct State {
    cache: Vec<Cached>,
    loading: Option<Loading>,
    /// A scope whose list could not be loaded, and why.
    failed: Option<(IndexScope, &'static str)>,
    form: Option<Form>,
    modal: Option<ModalForm>,
    serial: u64,
}

/// What the panel needs to know about the open conversation.
pub struct Env<'a> {
    pub channel: Snowflake,
    pub scope: IndexScope,
    pub can_send: bool,
    pub client: &'a RestClient,
    pub bridge: &'a NavigationBridge,
    pub interaction: &'a InteractionView,
    pub now: Instant,
}

impl State {
    /// Forgets lists, forms and answers (logout, account change).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Closes what belongs to another conversation.
    pub fn sync(&mut self, channel: Option<Snowflake>) {
        if self
            .form
            .as_ref()
            .is_some_and(|form| Some(form.channel) != channel)
        {
            self.form = None;
        }
    }

    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Index(_, _, Err(RestError::AuthenticationRequired))
        )
    }

    fn fresh(&self, scope: IndexScope, now: Instant) -> Option<&Arc<CommandIndex>> {
        self.cache
            .iter()
            .find(|cached| cached.scope == scope && now.duration_since(cached.at) < INDEX_TTL)
            .map(|cached| &cached.index)
    }

    /// Called as the draft changes: a leading `/` loads the scope's command
    /// list unless a fresh one is held or a request is already out. Clearing
    /// the `/` forgets a failure so typing it again tries again.
    pub fn typed(&mut self, slash: bool, env: &Env<'_>) -> Task<Event> {
        if !slash {
            self.failed = None;
            return Task::none();
        }
        if !env.can_send
            || self.fresh(env.scope, env.now).is_some()
            || self.loading.as_ref().is_some_and(|l| l.scope == env.scope)
            || self.failed.is_some_and(|(scope, _)| scope == env.scope)
        {
            return Task::none();
        }
        self.serial = self.serial.wrapping_add(1);
        let (client, scope, serial) = (env.client.clone(), env.scope, self.serial);
        let (task, handle) = Task::perform(
            async move { client.command_index(scope).await.map(Arc::new) },
            move |result| Event::Index(serial, scope, result),
        )
        .abortable();
        self.loading = Some(Loading {
            scope,
            serial,
            _handle: handle.abort_on_drop(),
        });
        task
    }

    fn keep(&mut self, scope: IndexScope, index: Arc<CommandIndex>, now: Instant) {
        self.cache.retain(|cached| cached.scope != scope);
        self.cache.push(Cached {
            scope,
            index,
            at: now,
        });
        while self.cache.len() > MAX_SCOPES
            || self
                .cache
                .iter()
                .map(|cached| cached.index.heap_bytes())
                .sum::<usize>()
                > MAX_CACHE_BYTES
        {
            if self.cache.len() <= 1 {
                break;
            }
            self.cache.remove(0);
        }
    }

    /// Applies an event. Returns the task to run and whether the composer's
    /// `/` text was used up (a command was chosen).
    pub fn update(&mut self, event: Event, env: &Env<'_>) -> (Task<Event>, bool) {
        match event {
            Event::Index(serial, scope, result) => {
                if self.loading.as_ref().is_none_or(|l| l.serial != serial) {
                    return (Task::none(), false);
                }
                self.loading = None;
                match result {
                    Ok(index) => {
                        self.failed = None;
                        self.keep(scope, index, env.now);
                    }
                    Err(_) => self.failed = Some((scope, FAILED)),
                }
            }
            Event::Pick(id) => {
                let command = self
                    .fresh(env.scope, env.now)
                    .and_then(|index| index.commands.iter().find(|c| c.id == id))
                    .cloned();
                if let Some(command) = command.filter(|_| env.can_send) {
                    let mut form = Form {
                        channel: env.channel,
                        command,
                        sub: None,
                        values: Vec::new(),
                        error: None,
                    };
                    form.values = vec![String::new(); form.fields().len()];
                    self.form = Some(form);
                    return (Task::none(), true);
                }
            }
            Event::Sub(at) => {
                if let Some(form) = &mut self.form
                    && form
                        .command
                        .options
                        .get(at)
                        .is_some_and(|option| option.kind == OPTION_SUB_COMMAND)
                {
                    form.sub = Some(at);
                    form.values = vec![String::new(); form.fields().len()];
                    form.error = None;
                }
            }
            Event::Field(at, value) => {
                if let Some(form) = &mut self.form
                    && let Some(slot) = form.values.get_mut(at)
                {
                    *slot = value;
                    form.error = None;
                }
            }
            Event::CloseForm => self.form = None,
            Event::Run => self.run(env),
            Event::ModalField(id, at, value) => {
                if let Some(modal) = self.modal.as_mut().filter(|m| m.id == id)
                    && let Some(slot) = modal.values.get_mut(at)
                {
                    *slot = value;
                }
            }
            Event::ModalSubmit => {
                if let Some(modal) = self.modal.as_ref()
                    && !env.interaction.modal_sending
                {
                    let values = modal.values.clone();
                    env.bridge.interact(Action::Modal { values });
                }
            }
            Event::ModalCancel => {
                self.modal = None;
                env.bridge.interact(Action::CloseModal);
            }
        }
        (Task::none(), false)
    }

    fn run(&mut self, env: &Env<'_>) {
        let Some(form) = &mut self.form else {
            return;
        };
        // Re-checked at the moment of sending, not only when the form opened.
        if !env.can_send || form.channel != env.channel {
            form.error = Some(CLOSED);
            return;
        }
        if env.interaction.waiting.is_some() {
            form.error = Some(crate::interactions::COMMAND_BUSY);
            return;
        }
        let (subcommand, args) = match form.arguments() {
            Ok(arguments) => arguments,
            Err(error) => {
                form.error = Some(error);
                return;
            }
        };
        let queued = env.bridge.interact(Action::Command {
            channel: form.channel,
            command: Box::new(form.command.clone()),
            subcommand,
            args,
        });
        if queued {
            self.form = None;
        } else {
            form.error = Some(NOT_QUEUED);
        }
    }

    /// Keeps the typed answers in step with the modal the worker holds: a new
    /// modal starts from its inputs' own values, a closed one is forgotten.
    pub fn modal_shown(&mut self, modal: Option<&Arc<InteractionModal>>) {
        match modal {
            None => self.modal = None,
            Some(modal) if self.modal.as_ref().is_none_or(|m| m.id != modal.id) => {
                self.modal = Some(ModalForm {
                    id: modal.id,
                    values: modal.inputs.iter().map(|i| i.value.clone()).collect(),
                });
            }
            Some(_) => {}
        }
    }
}

fn small(label: &'static str, event: Event) -> Element<'static, Event> {
    button(text(label).size(12))
        .on_press(event)
        .padding([2, 8])
        .style(button::secondary)
        .into()
}

/// The command list under a leading `/`, or the open command form, or the
/// modal an application asked for.
pub fn view<'a>(
    state: &'a State,
    query: Option<&str>,
    env_scope: IndexScope,
    interaction: &'a InteractionView,
    now: Instant,
) -> Option<Element<'a, Event>> {
    if let Some(modal) = interaction.modal.as_ref() {
        return Some(modal_view(state, modal, interaction.modal_sending));
    }
    if let Some(form) = &state.form {
        return Some(form_view(form, interaction.waiting.as_deref()));
    }
    if let Some(name) = interaction.waiting.as_deref() {
        return Some(
            text(format!("Waiting for /{name}\u{2026}"))
                .size(12)
                .style(text::secondary)
                .into(),
        );
    }
    let query = query?;
    let mut list =
        column![text("Commands (pick one, or send the text as it is)").size(12)].spacing(4);
    if let Some((scope, reason)) = state.failed
        && scope == env_scope
    {
        return Some(list.push(text(reason).size(12).style(text::danger)).into());
    }
    let Some(index) = state.fresh(env_scope, now) else {
        return Some(
            list.push(
                text("Loading commands\u{2026}")
                    .size(12)
                    .style(text::secondary),
            )
            .into(),
        );
    };
    let found = index.matching(query, MAX_LISTED);
    if found.is_empty() {
        return Some(
            list.push(text("No matching command.").size(12).style(text::secondary))
                .into(),
        );
    }
    for command in found {
        let label = if command.application.is_empty() {
            format!("/{}  {}", command.name, command.description)
        } else {
            format!(
                "/{}  {} ({})",
                command.name, command.description, command.application
            )
        };
        list = list.push(
            button(text(label).size(13).wrapping(text::Wrapping::WordOrGlyph))
                .width(Length::Fill)
                .padding([3, 8])
                .on_press(Event::Pick(command.id))
                .style(button::secondary),
        );
    }
    Some(
        container(list)
            .padding(8)
            .style(container::rounded_box)
            .into(),
    )
}

fn field_label(option: &CommandOption) -> String {
    if option.required {
        format!("{}*", option.name)
    } else {
        option.name.clone()
    }
}

fn form_view<'a>(form: &'a Form, waiting: Option<&str>) -> Element<'a, Event> {
    let mut panel = column![
        row![
            text(format!("/{}", form.command.name)).size(13),
            text(form.command.description.as_str())
                .size(12)
                .style(text::secondary)
                .width(Length::Fill),
            small("Close", Event::CloseForm),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
    ]
    .spacing(6);
    if form.has_subcommands() {
        let tabs = form
            .command
            .options
            .iter()
            .enumerate()
            .filter(|(_, option)| option.kind == OPTION_SUB_COMMAND)
            .fold(row![].spacing(4), |tabs, (at, option)| {
                tabs.push(
                    button(text(option.name.as_str()).size(12))
                        .on_press(Event::Sub(at))
                        .style(if form.sub == Some(at) {
                            button::primary
                        } else {
                            button::secondary
                        }),
                )
            });
        panel = panel.push(tabs.wrap());
    }
    for (at, (option, value)) in form.fields().iter().zip(&form.values).enumerate() {
        let mut line = column![text(field_label(option)).size(12)].spacing(2);
        if !option.description.is_empty() {
            line = line.push(
                text(option.description.as_str())
                    .size(11)
                    .style(text::secondary),
            );
        }
        let choices: Vec<(String, String)> = if option.kind == OPTION_BOOLEAN {
            vec![("Yes".into(), "true".into()), ("No".into(), "false".into())]
        } else {
            option
                .choices
                .iter()
                .map(|c| (c.name.clone(), c.value.clone()))
                .collect()
        };
        if choices.is_empty() {
            let hint = match option.kind {
                6..=9 => "ID or mention",
                _ => "",
            };
            line = line.push(
                text_input(hint, value)
                    .on_input(move |text| Event::Field(at, text))
                    .size(13),
            );
        } else {
            line = line.push(
                row(choices.into_iter().map(|(name, wire)| {
                    button(text(name).size(12))
                        .on_press(Event::Field(at, wire.clone()))
                        .style(if *value == wire {
                            button::primary
                        } else {
                            button::secondary
                        })
                        .into()
                }))
                .spacing(4)
                .wrap(),
            );
        }
        panel = panel.push(line);
    }
    panel = panel.push(
        button(text("Run").size(13))
            .on_press_maybe(waiting.is_none().then_some(Event::Run))
            .padding([4, 14]),
    );
    if let Some(error) = form.error {
        panel = panel.push(text(error).size(12).style(text::danger));
    }
    container(scrollable(panel).height(Length::Shrink))
        .padding(8)
        .style(container::rounded_box)
        .max_height(360)
        .into()
}

fn modal_view<'a>(
    state: &'a State,
    modal: &'a Arc<InteractionModal>,
    sending: bool,
) -> Element<'a, Event> {
    let id = modal.id;
    let mut panel = column![text(modal.title.as_str()).size(14)].spacing(6);
    let values = state.modal.as_ref().filter(|m| m.id == id);
    for (at, input) in modal.inputs.iter().enumerate() {
        let value = values
            .and_then(|m| m.values.get(at))
            .map_or(input.value.as_str(), String::as_str);
        let label = if input.required {
            format!("{}*", input.label)
        } else {
            input.label.clone()
        };
        panel = panel.push(
            column![
                text(label).size(12),
                text_input(&input.placeholder, value)
                    .on_input_maybe(
                        (!sending).then_some(move |text| Event::ModalField(id, at, text))
                    )
                    .size(13),
            ]
            .spacing(2),
        );
    }
    panel = panel.push(
        row![
            button(text(if sending { "Sending\u{2026}" } else { "Submit" }).size(13))
                .on_press_maybe((!sending).then_some(Event::ModalSubmit))
                .padding([4, 14]),
            button(text("Cancel").size(13))
                .on_press(Event::ModalCancel)
                .style(button::secondary)
                .padding([4, 14]),
        ]
        .spacing(8),
    );
    container(panel)
        .padding(8)
        .style(container::rounded_box)
        .into()
}

#[cfg(test)]
mod tests {
    use fastcord_discord::UserToken;
    use fastcord_discord::gateway::{ClientProperties, HostOs};

    use super::*;
    use crate::gateway::Command;

    const INDEX: &str = include_str!("../../../fixtures/rest/application-command-index.json");
    const SCOPE: IndexScope = IndexScope::Guild(Snowflake(100));
    const CHANNEL: Snowflake = Snowflake(500);

    fn client() -> RestClient {
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-slash-credential".into()),
            ClientProperties::web(HostOs::Linux, "en-US", 1),
        )
        .unwrap()
    }

    fn index() -> Arc<CommandIndex> {
        Arc::new(fastcord_discord::parse_command_index(INDEX.as_bytes()).unwrap())
    }

    /// Applies an event; the task a real run would get is never needed here.
    fn step(state: &mut State, event: Event, env: &Env<'_>) -> bool {
        state.update(event, env).1
    }

    struct Harness {
        client: RestClient,
        bridge: NavigationBridge,
        idle: InteractionView,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                client: client(),
                bridge: NavigationBridge::new(),
                idle: InteractionView::default(),
            }
        }

        fn env(&self, now: Instant) -> Env<'_> {
            Env {
                channel: CHANNEL,
                scope: SCOPE,
                can_send: true,
                client: &self.client,
                bridge: &self.bridge,
                interaction: &self.idle,
                now,
            }
        }
    }

    fn loaded(state: &mut State, harness: &Harness, now: Instant) {
        state.keep(SCOPE, index(), now);
        let _ = harness;
    }

    fn open(state: &mut State, harness: &Harness, name: &str, now: Instant) {
        let id = state
            .fresh(SCOPE, now)
            .unwrap()
            .commands
            .iter()
            .find(|c| c.name == name)
            .unwrap()
            .id;
        let used = step(state, Event::Pick(id), &harness.env(now));
        assert!(used, "choosing a command uses the typed slash up");
    }

    #[test]
    fn a_fresh_list_is_reused_a_stale_one_is_fetched_and_a_failure_waits_for_the_slash_to_clear() {
        let harness = Harness::new();
        let mut state = State::default();
        let now = Instant::now();
        loaded(&mut state, &harness, now);
        assert!(state.fresh(SCOPE, now + INDEX_TTL / 2).is_some());
        assert!(state.fresh(SCOPE, now + INDEX_TTL).is_none());
        let _ = state.typed(true, &harness.env(now + INDEX_TTL / 2));
        assert!(state.loading.is_none(), "no request while a list is fresh");
        // Stale: one request goes out, and typing on does not start another.
        // (The task is only built here, never run: no network.)
        let _ = state.typed(true, &harness.env(now + INDEX_TTL));
        let serial = state.loading.as_ref().unwrap().serial;
        let _ = state.typed(true, &harness.env(now + INDEX_TTL));
        assert_eq!(state.loading.as_ref().unwrap().serial, serial);
        // A stale answer (another serial) is dropped; the right one is kept.
        step(
            &mut state,
            Event::Index(serial + 9, SCOPE, Ok(index())),
            &harness.env(now + INDEX_TTL),
        );
        assert!(state.loading.is_some());
        step(
            &mut state,
            Event::Index(serial, SCOPE, Err(RestError::PermissionDenied)),
            &harness.env(now + INDEX_TTL),
        );
        assert_eq!(state.failed, Some((SCOPE, FAILED)));
        let _ = state.typed(true, &harness.env(now + INDEX_TTL));
        assert!(
            state.loading.is_none(),
            "a failure is not retried by itself"
        );
        let _ = state.typed(false, &harness.env(now + INDEX_TTL));
        assert!(state.failed.is_none());
        let _ = state.typed(true, &harness.env(now + INDEX_TTL));
        assert!(
            state.loading.is_some(),
            "typing the slash again tries again"
        );
    }

    #[test]
    fn the_cache_is_bounded_in_scopes_and_the_newest_survives() {
        let mut state = State::default();
        let now = Instant::now();
        for n in 0..(MAX_SCOPES as u64 + 3) {
            state.keep(IndexScope::Guild(Snowflake(n)), index(), now);
        }
        assert_eq!(state.cache.len(), MAX_SCOPES);
        assert!(
            state
                .cache
                .iter()
                .any(|c| c.scope == IndexScope::Guild(Snowflake(MAX_SCOPES as u64 + 2)))
        );
        state.keep(IndexScope::Guild(Snowflake(0)), index(), now);
        assert_eq!(state.cache.len(), MAX_SCOPES);
    }

    #[test]
    fn a_command_form_collects_typed_options_and_sends_one_interaction() {
        let harness = Harness::new();
        let mut commands = harness.bridge.take_commands().unwrap();
        let mut state = State::default();
        let now = Instant::now();
        loaded(&mut state, &harness, now);
        open(&mut state, &harness, "roll", now);
        // A required option left empty sends nothing.
        step(&mut state, Event::Run, &harness.env(now));
        assert_eq!(state.form.as_ref().unwrap().error, Some(REQUIRED));
        assert!(commands.try_recv().is_err());
        step(&mut state, Event::Field(0, "20".into()), &harness.env(now));
        step(&mut state, Event::Field(1, "d20".into()), &harness.env(now));
        step(&mut state, Event::Run, &harness.env(now));
        assert!(state.form.is_none(), "queued: the form closes");
        let Ok(Command::Interact(action)) = commands.try_recv() else {
            panic!("nothing queued");
        };
        let Action::Command {
            channel,
            command,
            subcommand,
            args,
        } = *action
        else {
            panic!("not a command");
        };
        assert_eq!(
            (channel, command.name.as_str(), subcommand),
            (CHANNEL, "roll", None)
        );
        assert_eq!(
            args.iter().map(|a| a.text.as_str()).collect::<Vec<_>>(),
            ["20", "d20"]
        );
        assert!(commands.try_recv().is_err(), "one press, one request");
    }

    #[test]
    fn a_subcommand_must_be_chosen_and_a_waiting_command_blocks_another() {
        let harness = Harness::new();
        let mut commands = harness.bridge.take_commands().unwrap();
        let mut state = State::default();
        let now = Instant::now();
        loaded(&mut state, &harness, now);
        open(&mut state, &harness, "poll", now);
        assert!(state.form.as_ref().unwrap().fields().is_empty());
        step(&mut state, Event::Run, &harness.env(now));
        assert_eq!(state.form.as_ref().unwrap().error, Some(CHOOSE_SUB));
        step(&mut state, Event::Sub(5), &harness.env(now));
        assert!(
            state.form.as_ref().unwrap().sub.is_none(),
            "not a subcommand"
        );
        step(&mut state, Event::Sub(0), &harness.env(now));
        step(
            &mut state,
            Event::Field(0, "Lunch?".into()),
            &harness.env(now),
        );
        let waiting = InteractionView {
            waiting: Some("poll".into()),
            ..InteractionView::default()
        };
        let busy = Env {
            interaction: &waiting,
            ..harness.env(now)
        };
        step(&mut state, Event::Run, &busy);
        assert!(commands.try_recv().is_err());
        assert!(state.form.is_some());
        step(&mut state, Event::Run, &harness.env(now));
        let Ok(Command::Interact(action)) = commands.try_recv() else {
            panic!("nothing queued");
        };
        assert!(matches!(
            *action,
            Action::Command { subcommand: Some(ref sub), .. } if sub == "create"
        ));
        // A form of another conversation closes with the selection.
        open(&mut state, &harness, "roll", now);
        state.sync(Some(Snowflake(501)));
        assert!(state.form.is_none());
    }

    #[test]
    fn without_permission_nothing_opens_or_is_fetched() {
        let harness = Harness::new();
        let mut state = State::default();
        let now = Instant::now();
        loaded(&mut state, &harness, now);
        let denied = Env {
            can_send: false,
            ..harness.env(now)
        };
        let id = state.fresh(SCOPE, now).unwrap().commands[0].id;
        let used = step(&mut state, Event::Pick(id), &denied);
        assert!(!used && state.form.is_none());
        let mut other = State::default();
        let _ = other.typed(true, &denied);
        assert!(other.loading.is_none());
    }

    #[test]
    fn a_modal_starts_from_its_values_survives_edits_and_submits_once() {
        let harness = Harness::new();
        let mut commands = harness.bridge.take_commands().unwrap();
        let mut state = State::default();
        let now = Instant::now();
        let modal = Arc::new(
            fastcord_discord::parse_interaction_modal(include_str!(
                "../../../fixtures/gateway/interaction_modal_create.json"
            ))
            .unwrap(),
        );
        state.modal_shown(Some(&modal));
        let id = modal.id;
        step(
            &mut state,
            Event::ModalField(id, 0, "Title".into()),
            &harness.env(now),
        );
        step(
            &mut state,
            Event::ModalField(Snowflake(1), 0, "other".into()),
            &harness.env(now),
        );
        state.modal_shown(Some(&modal));
        assert_eq!(state.modal.as_ref().unwrap().values[0], "Title");
        step(&mut state, Event::ModalSubmit, &harness.env(now));
        let Ok(Command::Interact(action)) = commands.try_recv() else {
            panic!("nothing queued");
        };
        assert_eq!(
            *action,
            Action::Modal {
                values: vec!["Title".into(), String::new()]
            }
        );
        let sending = InteractionView {
            modal: Some(modal.clone()),
            modal_sending: true,
            ..InteractionView::default()
        };
        let busy = Env {
            interaction: &sending,
            ..harness.env(now)
        };
        step(&mut state, Event::ModalSubmit, &busy);
        assert!(
            commands.try_recv().is_err(),
            "no second submit while sending"
        );
        step(&mut state, Event::ModalCancel, &harness.env(now));
        assert!(matches!(
            *match commands.try_recv() {
                Ok(Command::Interact(action)) => action,
                _ => panic!("no close"),
            },
            Action::CloseModal
        ));
        state.modal_shown(None);
        assert!(state.modal.is_none());
    }
}
