//! Communication app access (SPEC §16, issue #101): the apps the account has
//! authorized (user settings), a server's integrations and bots (server
//! settings, Manage Server) and a channel's incoming webhooks (channel
//! settings, Manage Webhooks).
//!
//! Every write is one explicit click and one request, checked again against the
//! current read model right before it is sent, never retried, with at most one
//! request in flight. Removing an authorization, an integration or a webhook
//! needs a Confirm. Adding an app never authorizes anything here: it opens
//! Discord's own consent page in the browser for a client id the user typed.
//! A webhook token lives only in the open panel's list, is never logged, is
//! copied (as the webhook URL) only by its own click, and is dropped (and
//! zeroized) when the panel closes or the account changes. Nothing is ever
//! posted through a webhook.

use crate::ui::{button, primary_button};
use fastcord_discord::state::navigation::{AdminKind, NavigationSnapshot};
use fastcord_discord::{
    AuthorizedApp, Integration, MAX_WEBHOOK_NAME, RestClient, RestError, Webhook, authorize_url,
    parse_client_id,
};
use fastcord_model::{Permissions, Snowflake};
use iced::widget::{column, container, row, scrollable, space, text, text_input};
use iced::{Element, Length, Task};

use crate::announce::{can_manage_webhooks, describe};
use crate::i18n::{tr, trf};
use crate::panel_kit::{Confirm, Gate, auth_rejected};

/// What one confirmed removal deletes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    App(Snowflake),
    Integration(Snowflake),
    Webhook(Snowflake),
}

#[derive(Clone, Debug)]
pub enum Event {
    /// "Authorized apps" of the account panel.
    OpenAuthorized,
    /// "Apps and integrations" of a server's settings.
    OpenGuild(Snowflake),
    /// "Webhooks" of a text channel's settings: server and channel.
    OpenWebhooks(Snowflake, Snowflake),
    Close,
    ClientInput(String),
    /// "Add app via the browser".
    AddInBrowser,
    Authorized(u64, Result<Vec<AuthorizedApp>, RestError>),
    Integrations(u64, Snowflake, Result<Vec<Integration>, RestError>),
    Webhooks(u64, Snowflake, Result<Vec<Webhook>, RestError>),
    Ask(Target),
    Cancel,
    Confirm,
    Removed(u64, Target, Result<(), RestError>),
    NameInput(String),
    Create,
    Created(u64, Snowflake, Result<Webhook, RestError>),
    /// Starts renaming or moving one webhook.
    Edit(Snowflake),
    SaveName,
    MoveTo(Snowflake),
    Edited(u64, Snowflake, Result<Webhook, RestError>),
    CopyUrl(Snowflake),
}

/// What `main.rs` does for the panel: it owns the browser and the clipboard.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Open(String),
    Copy(String),
}

#[derive(Debug)]
enum Panel {
    Authorized(Option<Vec<AuthorizedApp>>),
    Guild(Snowflake, Option<Vec<Integration>>),
    Webhooks {
        guild: Snowflake,
        channel: Snowflake,
        list: Option<Vec<Webhook>>,
        editing: Option<Snowflake>,
    },
}

#[derive(Default)]
pub struct State {
    panel: Option<Panel>,
    client_input: String,
    name_input: String,
    confirm: Confirm<Target>,
    life: Gate,
}

fn tidy(mut name: String) -> String {
    name.truncate(
        name.char_indices()
            .nth(MAX_WEBHOOK_NAME + 8)
            .map_or(name.len(), |(i, _)| i),
    );
    name
}

impl State {
    /// The server settings page this panel is, when it is one.
    pub fn page(&self) -> Option<(Snowflake, crate::server_settings::Section)> {
        use crate::server_settings::Section;
        match self.panel.as_ref()? {
            Panel::Guild(guild, _) => Some((*guild, Section::Integrations)),
            Panel::Webhooks { guild, channel, .. } => Some((*guild, Section::Webhooks(*channel))),
            Panel::Authorized(_) => None,
        }
    }

    pub fn clear(&mut self) {
        let mut life = std::mem::take(&mut self.life);
        life.reset();
        *self = Self {
            life,
            ..Self::default()
        };
    }

    pub fn is_rejected(event: &Event) -> bool {
        match event {
            Event::Authorized(_, r) => auth_rejected(r),
            Event::Integrations(_, _, r) => auth_rejected(r),
            Event::Webhooks(_, _, r) => auth_rejected(r),
            Event::Removed(_, _, r) => auth_rejected(r),
            Event::Created(_, _, r) => auth_rejected(r),
            Event::Edited(_, _, r) => auth_rejected(r),
            _ => false,
        }
    }

    /// The server whose permissions the open panel needs from the reducer.
    pub fn wanted_guild(&self) -> Option<Snowflake> {
        match self.panel {
            Some(Panel::Guild(guild, _)) | Some(Panel::Webhooks { guild, .. }) => Some(guild),
            _ => None,
        }
    }

    fn open(&mut self, panel: Panel) {
        // The old panel (and any webhook token in it) is dropped here.
        self.panel = Some(panel);
        self.confirm.cancel();
        self.life.notice = None;
        self.name_input.clear();
    }

    fn start<F, Fut>(&mut self, run: F) -> Task<Event>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Event> + Send + 'static,
    {
        self.life.begin(());
        Task::perform(run(), |event| event)
    }

    fn webhooks_mut(&mut self, channel: Snowflake) -> Option<&mut Vec<Webhook>> {
        match &mut self.panel {
            Some(Panel::Webhooks {
                channel: open,
                list: Some(list),
                ..
            }) if *open == channel => Some(list),
            _ => None,
        }
    }

    fn guild_blocked(navigation: &NavigationSnapshot, guild: Snowflake) -> bool {
        !navigation
            .admin
            .as_ref()
            .is_some_and(|a| a.id == guild && a.can_manage_guild)
    }

    /// Whether the panel's own scope still allows a write.
    fn blocked(&self, navigation: &NavigationSnapshot) -> Option<&'static str> {
        match &self.panel {
            Some(Panel::Guild(guild, _)) if Self::guild_blocked(navigation, *guild) => {
                Some(tr("You need Manage Server for this."))
            }
            Some(Panel::Webhooks { guild, channel, .. })
                if !can_manage_webhooks(navigation, *guild, *channel) =>
            {
                Some(tr("You need Manage Webhooks in this channel."))
            }
            _ => None,
        }
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        client: &RestClient,
    ) -> (Task<Event>, Option<Action>) {
        let serial = self.life.serial();
        let client = client.clone();
        match event {
            Event::Authorized(s, result) if self.life.accept(s) => {
                match (&mut self.panel, result) {
                    (Some(Panel::Authorized(list)), Ok(found)) => *list = Some(found),
                    (Some(Panel::Authorized(_)), Err(error)) => {
                        self.life.notice = Some(describe(
                            error,
                            "Discord refused to list your authorized apps.",
                            "Discord has no such list (unverified route).",
                        ));
                    }
                    _ => {}
                }
            }
            Event::Integrations(s, guild, result) if self.life.accept(s) => {
                match (&mut self.panel, result) {
                    (Some(Panel::Guild(open, list)), Ok(found)) if *open == guild => {
                        *list = Some(found);
                    }
                    (Some(Panel::Guild(open, _)), Err(error)) if *open == guild => {
                        self.life.notice = Some(describe(
                            error,
                            "Discord refused: you need Manage Server to see integrations.",
                            "That server no longer exists.",
                        ));
                    }
                    _ => {}
                }
            }
            Event::Webhooks(s, channel, result) if self.life.accept(s) => {
                if let Some(Panel::Webhooks {
                    channel: open,
                    list,
                    ..
                }) = &mut self.panel
                    && *open == channel
                {
                    match result {
                        Ok(found) => *list = Some(found),
                        Err(error) => {
                            self.life.notice = Some(describe(
                                error,
                                "Discord refused: you need Manage Webhooks in this channel.",
                                "That channel no longer exists.",
                            ));
                        }
                    }
                }
            }
            Event::Removed(s, target, result) if self.life.accept(s) => {
                let gone = matches!(result, Err(RestError::ResourceGone));
                match result {
                    Ok(()) | Err(RestError::ResourceGone) => {
                        self.confirm.cancel();
                        match (&mut self.panel, target) {
                            (Some(Panel::Authorized(Some(list))), Target::App(id)) => {
                                list.retain(|a| a.id != id);
                            }
                            (Some(Panel::Guild(_, Some(list))), Target::Integration(id)) => {
                                list.retain(|i| i.id != id);
                            }
                            (
                                Some(Panel::Webhooks {
                                    list: Some(list), ..
                                }),
                                Target::Webhook(id),
                            ) => {
                                list.retain(|w| w.id != id);
                            }
                            _ => {}
                        }
                        self.life.notice = Some(
                            if gone {
                                tr("It was already removed.")
                            } else {
                                tr("Removed.")
                            }
                            .to_owned(),
                        );
                    }
                    Err(error) => {
                        self.life.notice = Some(describe(
                            error,
                            "Discord refused: you lack the permission, or this cannot be removed.",
                            "That no longer exists.",
                        ));
                    }
                }
            }
            Event::Created(s, channel, result) if self.life.accept(s) => match result {
                Ok(webhook) => {
                    if let Some(list) = self.webhooks_mut(channel) {
                        list.push(webhook);
                        self.name_input.clear();
                        self.life.notice = Some(tr("Webhook created.").to_owned());
                    }
                }
                Err(error) => {
                    self.life.notice = Some(describe(
                        error,
                        "Discord refused: you need Manage Webhooks in this channel.",
                        "That channel no longer exists.",
                    ));
                }
            },
            Event::Edited(s, channel, result) if self.life.accept(s) => match result {
                Ok(webhook) => {
                    let moved = webhook.channel != channel;
                    let mut kept = false;
                    if let Some(list) = self.webhooks_mut(channel) {
                        if moved {
                            list.retain(|w| w.id != webhook.id);
                        } else if let Some(old) = list.iter_mut().find(|w| w.id == webhook.id) {
                            let token = webhook.token.or(old.token.take());
                            *old = Webhook { token, ..webhook };
                        }
                        kept = true;
                    }
                    if kept {
                        if moved && let Some(Panel::Webhooks { editing, .. }) = &mut self.panel {
                            *editing = None;
                        }
                        self.life.notice =
                            Some(if moved { tr("Moved.") } else { tr("Renamed.") }.to_owned());
                    }
                }
                Err(error) => {
                    self.life.notice = Some(describe(
                        error,
                        "Discord refused: you need Manage Webhooks in that channel.",
                        "That webhook no longer exists.",
                    ));
                }
            },
            Event::Authorized(..)
            | Event::Integrations(..)
            | Event::Webhooks(..)
            | Event::Removed(..)
            | Event::Created(..)
            | Event::Edited(..) => {}
            Event::Cancel => self.confirm.cancel(),
            Event::ClientInput(v) => self.client_input = v.chars().take(24).collect(),
            Event::NameInput(v) => self.name_input = tidy(v),
            Event::Close if !self.life.is_busy() => {
                self.panel = None;
                self.confirm.cancel();
                self.life.notice = None;
                self.name_input.clear();
                self.client_input.clear();
            }
            Event::Close => {}
            Event::AddInBrowser => match parse_client_id(&self.client_input) {
                Some(id) => {
                    self.life.notice = Some(
                        tr("Opened Discord's page in your browser. Consent happens there.")
                            .to_owned(),
                    );
                    return (Task::none(), Some(Action::Open(authorize_url(id))));
                }
                None => {
                    self.life.notice =
                        Some(tr("Enter the app's client id: digits only.").to_owned());
                }
            },
            Event::CopyUrl(id) => {
                let url = match &self.panel {
                    Some(Panel::Webhooks {
                        list: Some(list), ..
                    }) => list
                        .iter()
                        .find(|w| w.id == id)
                        .and_then(|w| w.token.as_ref().map(|t| t.url(id))),
                    _ => None,
                };
                match url {
                    Some(url) => {
                        self.life.notice =
                            Some(tr("Webhook URL copied. Treat it as a password.").to_owned());
                        return (Task::none(), Some(Action::Copy(url)));
                    }
                    None => {
                        self.life.notice =
                            Some(tr("Discord sent no token for this webhook.").to_owned());
                    }
                }
            }
            _ if self.life.refuse_if_busy() => {}
            Event::OpenAuthorized => {
                self.open(Panel::Authorized(None));
                return (
                    self.start(move || async move {
                        Event::Authorized(serial, client.authorized_apps().await)
                    }),
                    None,
                );
            }
            Event::OpenGuild(guild) => {
                if Self::guild_blocked(navigation, guild) {
                    self.life.notice = Some(tr("You need Manage Server for this.").to_owned());
                    return (Task::none(), None);
                }
                self.open(Panel::Guild(guild, None));
                return (
                    self.start(move || async move {
                        Event::Integrations(serial, guild, client.integrations(guild).await)
                    }),
                    None,
                );
            }
            Event::OpenWebhooks(guild, channel) => {
                if !can_manage_webhooks(navigation, guild, channel) {
                    self.life.notice =
                        Some(tr("You need Manage Webhooks in a text channel for this.").to_owned());
                    return (Task::none(), None);
                }
                self.open(Panel::Webhooks {
                    guild,
                    channel,
                    list: None,
                    editing: None,
                });
                return (
                    self.start(move || async move {
                        Event::Webhooks(serial, channel, client.channel_webhooks(channel).await)
                    }),
                    None,
                );
            }
            Event::Ask(target) => {
                let listed = match (&self.panel, target) {
                    (Some(Panel::Authorized(Some(l))), Target::App(id)) => {
                        l.iter().any(|a| a.id == id)
                    }
                    (Some(Panel::Guild(_, Some(l))), Target::Integration(id)) => {
                        l.iter().any(|i| i.id == id)
                    }
                    (Some(Panel::Webhooks { list: Some(l), .. }), Target::Webhook(id)) => {
                        l.iter().any(|w| w.id == id)
                    }
                    _ => false,
                };
                if listed {
                    self.confirm.ask(target);
                }
            }
            Event::Confirm => {
                let Some(target) = self.confirm.pending() else {
                    return (Task::none(), None);
                };
                if let Some(reason) = self.blocked(navigation) {
                    self.life.notice = Some(reason.to_owned());
                    return (Task::none(), None);
                }
                let guild = match &self.panel {
                    Some(Panel::Guild(guild, _)) => *guild,
                    _ => Snowflake(0),
                };
                return (
                    self.start(move || async move {
                        let result = match target {
                            Target::App(id) => client.deauthorize_app(id).await,
                            Target::Integration(id) => client.delete_integration(guild, id).await,
                            Target::Webhook(id) => client.delete_webhook(id).await,
                        };
                        Event::Removed(serial, target, result)
                    }),
                    None,
                );
            }
            Event::Create => {
                let Some(Panel::Webhooks { guild, channel, .. }) = &self.panel else {
                    return (Task::none(), None);
                };
                let (guild, channel) = (*guild, *channel);
                let name = self.name_input.trim().to_owned();
                if name.is_empty() || name.chars().count() > MAX_WEBHOOK_NAME {
                    self.life.notice = Some(trf(
                        "Name the webhook (1 to {} characters).",
                        &[&MAX_WEBHOOK_NAME],
                    ));
                    return (Task::none(), None);
                }
                if !can_manage_webhooks(navigation, guild, channel) {
                    self.life.notice =
                        Some(tr("You need Manage Webhooks in this channel.").to_owned());
                    return (Task::none(), None);
                }
                return (
                    self.start(move || async move {
                        Event::Created(serial, channel, client.create_webhook(channel, &name).await)
                    }),
                    None,
                );
            }
            Event::Edit(id) => {
                if let Some(Panel::Webhooks {
                    list: Some(list),
                    editing,
                    ..
                }) = &mut self.panel
                    && let Some(w) = list.iter().find(|w| w.id == id)
                {
                    self.name_input = w.name.clone();
                    *editing = Some(id);
                    self.confirm.cancel();
                }
            }
            Event::SaveName | Event::MoveTo(_) => {
                let Some(Panel::Webhooks {
                    guild,
                    channel,
                    list: Some(list),
                    editing: Some(id),
                }) = &self.panel
                else {
                    return (Task::none(), None);
                };
                let (guild, channel, id) = (*guild, *channel, *id);
                let Some(current) = list.iter().find(|w| w.id == id) else {
                    return (Task::none(), None);
                };
                if !can_manage_webhooks(navigation, guild, channel) {
                    self.life.notice =
                        Some(tr("You need Manage Webhooks in this channel.").to_owned());
                    return (Task::none(), None);
                }
                let (name, to) = match event {
                    Event::MoveTo(target) => {
                        if target == channel || !can_manage_webhooks(navigation, guild, target) {
                            self.life.notice =
                                Some(tr("You cannot move it to that channel.").to_owned());
                            return (Task::none(), None);
                        }
                        (None, Some(target))
                    }
                    _ => {
                        let name = self.name_input.trim().to_owned();
                        if name.is_empty()
                            || name.chars().count() > MAX_WEBHOOK_NAME
                            || name == current.name
                        {
                            self.life.notice = Some(tr("Enter a new name first.").to_owned());
                            return (Task::none(), None);
                        }
                        (Some(name), None)
                    }
                };
                return (
                    self.start(move || async move {
                        Event::Edited(
                            serial,
                            channel,
                            client.edit_webhook(id, name.as_deref(), to).await,
                        )
                    }),
                    None,
                );
            }
        }
        (Task::none(), None)
    }
}

fn scope_line(scopes: &[String]) -> String {
    if scopes.is_empty() {
        tr("no scopes listed").to_owned()
    } else {
        scopes.join(", ")
    }
}

fn when(text: &Option<String>) -> String {
    text.as_deref().map_or_else(String::new, |t| {
        trf(" \u{b7} since {}", &[&t.get(..10).unwrap_or(t)])
    })
}

fn removal<'a>(state: &State, target: Target, idle: bool, what: &str) -> Element<'a, Event> {
    if state.confirm.is(target) {
        row![
            text(trf("Remove {}?", &[&tr(what)]))
                .size(12)
                .style(text::danger),
            button(text(tr("Remove")).size(12))
                .style(button::danger)
                .on_press_maybe(idle.then_some(Event::Confirm)),
            button(text(tr("Cancel")).size(12)).on_press(Event::Cancel),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into()
    } else {
        button(text(tr("Remove")).size(12))
            .on_press_maybe(idle.then_some(Event::Ask(target)))
            .into()
    }
}

fn add_app<'a>(state: &'a State, idle: bool) -> Element<'a, Event> {
    column![
        text(tr("Add an app")).size(13),
        row![
            text_input(tr("App client id"), &state.client_input)
                .on_input(Event::ClientInput)
                .size(13),
            primary_button(text(tr("Add app via the browser")).size(12))
                .on_press_maybe(idle.then_some(Event::AddInBrowser)),
        ]
        .spacing(6),
        text(tr(
            "Consent happens on Discord's own page; fastcord never authorizes an app itself."
        ))
        .size(12)
        .color(crate::style::muted()),
    ]
    .spacing(4)
    .into()
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
) -> Option<Element<'a, Event>> {
    let panel = state.panel.as_ref()?;
    let idle = !state.life.is_busy();
    let title = match panel {
        Panel::Authorized(_) => tr("Authorized apps"),
        Panel::Guild(..) => tr("Apps and integrations"),
        Panel::Webhooks { .. } => tr("Webhooks"),
    };
    // Server and channel pages sit in the settings screen, which has the
    // title and the cross; the account's list keeps its own.
    let mut body = column![].spacing(6);
    if matches!(panel, Panel::Authorized(_)) {
        body = body.push(
            row![
                text(title).size(13),
                space::horizontal(),
                crate::icons::close(idle.then_some(Event::Close)),
            ]
            .align_y(iced::Alignment::Center),
        );
    }
    let loading = |loaded: bool| {
        (!loaded).then(|| text(tr("Loading...")).size(12).color(crate::style::muted()))
    };
    match panel {
        Panel::Authorized(list) => {
            body = body.push(
                text(tr("Apps you authorized with your account. This list uses an unverified Discord route."))
                    .size(12)
                    .color(crate::style::muted()),
            );
            body = body.extend(loading(list.is_some()).map(iced::Element::from));
            let rows = list.iter().flatten().map(|app| {
                column![
                    text(if app.name.is_empty() {
                        tr("Unnamed app")
                    } else {
                        &app.name
                    })
                    .size(13),
                    text(format!(
                        "{}{}",
                        scope_line(&app.scopes),
                        when(&app.authorized_at)
                    ))
                    .size(12)
                    .color(crate::style::muted()),
                    removal(state, Target::App(app.id), idle, "this authorization"),
                ]
                .spacing(2)
                .into()
            });
            body = body
                .push(scrollable(column(rows).spacing(8)).height(Length::Fixed(200.0)))
                .push(add_app(state, idle));
        }
        Panel::Guild(_, list) => {
            body = body.extend(loading(list.is_some()).map(iced::Element::from));
            if list.as_ref().is_some_and(Vec::is_empty) {
                body = body.push(
                    text(tr("No integrations or bots."))
                        .size(12)
                        .color(crate::style::muted()),
                );
            }
            let rows = list.iter().flatten().map(|i| {
                let mut line = format!("{} ({})", i.name, i.kind);
                if !i.account.is_empty() {
                    line.push_str(&format!(" \u{b7} {}", i.account));
                }
                if let Some(bot) = &i.bot {
                    line.push_str(&trf(" \u{b7} bot {}", &[bot]));
                }
                row![
                    text(line).size(13).width(Length::Fill),
                    removal(state, Target::Integration(i.id), idle, &i.name),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .into()
            });
            body = body
                .push(scrollable(column(rows).spacing(4)).height(Length::Fixed(200.0)))
                .push(
                    text(tr(
                        "Command permissions of each app are managed in the official client.",
                    ))
                    .size(12)
                    .color(crate::style::muted()),
                )
                .push(add_app(state, idle));
        }
        Panel::Webhooks {
            guild,
            channel,
            list,
            editing,
        } => {
            body = body.push(
                text(tr("Incoming webhooks of this channel. The URL is a secret: copying it is your click only, and fastcord never posts through a webhook."))
                    .size(12)
                    .color(crate::style::muted()),
            );
            body = body.extend(loading(list.is_some()).map(iced::Element::from));
            if list.as_ref().is_some_and(Vec::is_empty) {
                body = body.push(
                    text(tr("No incoming webhooks."))
                        .size(12)
                        .color(crate::style::muted()),
                );
            }
            let targets: Vec<_> = navigation
                .admin
                .iter()
                .filter(|a| a.id == *guild)
                .flat_map(|a| a.channels.iter())
                .filter(|c| {
                    c.kind == AdminKind::Text
                        && c.id != *channel
                        && c.mine.contains(Permissions::MANAGE_WEBHOOKS)
                })
                .collect();
            let mut rows = column![].spacing(6);
            for w in list.iter().flatten() {
                let mut line = row![
                    text(&w.name).size(13).width(Length::Fill),
                    button(text(tr("Copy webhook URL")).size(12)).on_press_maybe(
                        (idle && w.token.is_some()).then_some(Event::CopyUrl(w.id))
                    ),
                    button(text(tr("Edit")).size(12))
                        .on_press_maybe(idle.then_some(Event::Edit(w.id))),
                    removal(state, Target::Webhook(w.id), idle, "this webhook"),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center);
                if *editing != Some(w.id) {
                    line = line.spacing(6);
                }
                rows = rows.push(line);
                if *editing == Some(w.id) {
                    let mut edit = column![
                        row![
                            text_input(tr("Webhook name"), &state.name_input)
                                .on_input(Event::NameInput)
                                .size(13),
                            button(text(tr("Rename")).size(12))
                                .on_press_maybe(idle.then_some(Event::SaveName)),
                        ]
                        .spacing(6),
                        text(tr("Move to:")).size(12).color(crate::style::muted()),
                    ]
                    .spacing(4);
                    if targets.is_empty() {
                        edit = edit.push(
                            text(tr("No other text channel where you have Manage Webhooks."))
                                .size(12)
                                .color(crate::style::muted()),
                        );
                    }
                    for t in &targets {
                        edit = edit.push(
                            button(text(format!("#{}", t.name)).size(12))
                                .style(crate::style::secondary_button(false))
                                .on_press_maybe(idle.then_some(Event::MoveTo(t.id))),
                        );
                    }
                    rows = rows.push(edit);
                }
            }
            body = body
                .push(scrollable(rows).height(Length::Fixed(200.0)))
                .push(
                    row![
                        text_input(tr("New webhook name"), &state.name_input)
                            .on_input(Event::NameInput)
                            .size(13),
                        primary_button(text(tr("Create")).size(12))
                            .on_press_maybe(idle.then_some(Event::Create)),
                    ]
                    .spacing(6),
                );
        }
    }
    if let Some(notice) = &state.life.notice {
        body = body.push(text(notice.as_str()).size(12));
    }
    Some(container(body).padding(8).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::state::navigation::{AdminChannel, GuildAdmin};

    const GUILD: Snowflake = Snowflake(1);
    const HERE: Snowflake = Snowflake(10);
    const THERE: Snowflake = Snowflake(11);

    fn client() -> RestClient {
        use fastcord_discord::UserToken;
        use fastcord_discord::gateway::{ClientProperties, HostOs};
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-apps-credential".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn channel(id: Snowflake, hooks: bool) -> AdminChannel {
        AdminChannel {
            id,
            name: format!("c{}", id.0),
            kind: AdminKind::Text,
            parent_id: None,
            position: 0,
            topic: String::new(),
            nsfw: false,
            slowmode: 0,
            user_limit: 0,
            bitrate: 0,
            rtc_region: None,
            video_quality: 1,
            can_manage: false,
            overwrites: Vec::new(),
            can_manage_permissions: false,
            mine: if hooks {
                Permissions::MANAGE_WEBHOOKS
            } else {
                Permissions::NONE
            },
            synced: None,
            everyone_view: false,
        }
    }

    fn nav(manage_guild: bool, there: bool) -> NavigationSnapshot {
        NavigationSnapshot {
            admin: Some(GuildAdmin {
                id: GUILD,
                name: "Mine".into(),
                has_icon: false,
                owner: false,
                community: true,
                description: String::new(),
                system_channel: None,
                afk_channel: None,
                afk_timeout: 300,
                notifications: 0,
                content_filter: 0,
                can_manage_guild: manage_guild,
                can_manage_channels: false,
                channels: vec![channel(HERE, true), channel(THERE, there)],
                people: Vec::new(),
                can_manage_roles: false,
                roles: Vec::new(),
                mine: Permissions::NONE,
                grant_all: false,
                top_position: 0,
                rules_channel: None,
                public_updates_channel: None,
                verification_level: 0,
                mfa_required: false,
                safety_alerts_channel: None,
                raid_alerts_disabled: false,
                features: Vec::new(),
            }),
            ..NavigationSnapshot::default()
        }
    }

    fn hook(id: u64, channel: Snowflake, secret: bool) -> Webhook {
        let body = serde_json::json!([{
            "id": id.to_string(), "type": 1, "name": format!("hook{id}"),
            "channel_id": channel.to_string(),
            "token": secret.then_some("fake-token-zzzz"),
        }]);
        fastcord_discord::parse_webhooks(body.to_string().as_bytes())
            .unwrap()
            .remove(0)
    }

    fn run(state: &mut State, event: Event, nav: &NavigationSnapshot) -> Option<Action> {
        state.update(event, nav, &client()).1
    }

    fn open_webhooks(state: &mut State, n: &NavigationSnapshot) {
        run(state, Event::OpenWebhooks(GUILD, HERE), n);
        assert!(state.life.is_busy());
        run(
            state,
            Event::Webhooks(0, HERE, Ok(vec![hook(7, HERE, true), hook(8, HERE, false)])),
            n,
        );
        assert!(!state.life.is_busy());
    }

    #[test]
    fn webhooks_need_manage_webhooks_and_load_once() {
        let mut state = State::default();
        run(
            &mut state,
            Event::OpenWebhooks(GUILD, THERE),
            &nav(true, false),
        );
        assert!(state.panel.is_none() && state.life.notice.is_some());
        open_webhooks(&mut state, &nav(true, true));
        run(
            &mut state,
            Event::OpenWebhooks(GUILD, HERE),
            &nav(true, true),
        );
        assert_eq!(state.wanted_guild(), Some(GUILD));
    }

    #[test]
    fn the_url_is_copied_only_on_its_click_and_dropped_on_close() {
        let n = nav(true, true);
        let mut state = State::default();
        open_webhooks(&mut state, &n);
        assert_eq!(
            run(&mut state, Event::CopyUrl(Snowflake(7)), &n),
            Some(Action::Copy(
                "https://discord.com/api/webhooks/7/fake-token-zzzz".to_owned()
            ))
        );
        assert_eq!(run(&mut state, Event::CopyUrl(Snowflake(8)), &n), None);
        assert!(!state.life.notice.as_deref().unwrap().contains("fake-token"));
        assert!(!format!("{:?}", state.panel).contains("fake-token"));
        run(&mut state, Event::Close, &n);
        assert!(state.panel.is_none());
        assert_eq!(run(&mut state, Event::CopyUrl(Snowflake(7)), &n), None);
    }

    #[test]
    fn removing_asks_first_checks_again_and_sends_once() {
        let n = nav(true, true);
        let mut state = State::default();
        open_webhooks(&mut state, &n);
        run(&mut state, Event::Ask(Target::Webhook(Snowflake(99))), &n);
        assert_eq!(state.confirm.pending(), None);
        run(&mut state, Event::Confirm, &n);
        assert!(!state.life.is_busy(), "nothing to confirm");
        run(&mut state, Event::Ask(Target::Webhook(Snowflake(7))), &n);
        let lost = NavigationSnapshot {
            admin: None,
            ..NavigationSnapshot::default()
        };
        run(&mut state, Event::Confirm, &lost);
        assert!(!state.life.is_busy() && state.life.notice.is_some());
        run(&mut state, Event::Confirm, &n);
        assert!(state.life.is_busy());
        run(&mut state, Event::Confirm, &n);
        run(&mut state, Event::Ask(Target::Webhook(Snowflake(8))), &n);
        run(
            &mut state,
            Event::Removed(1, Target::Webhook(Snowflake(7)), Ok(())),
            &n,
        );
        assert!(state.life.is_busy(), "a stale answer is ignored");
        run(
            &mut state,
            Event::Removed(0, Target::Webhook(Snowflake(7)), Ok(())),
            &n,
        );
        assert!(!state.life.is_busy());
        let Some(Panel::Webhooks {
            list: Some(list), ..
        }) = &state.panel
        else {
            panic!("panel");
        };
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn failures_say_what_happened() {
        let n = nav(true, true);
        for (error, expect) in [
            (RestError::PermissionDenied, "Discord refused"),
            (RestError::ResourceGone, "no longer exists"),
            (RestError::InvalidRateLimit, "rate limiting"),
            (
                RestError::Retryable(fastcord_discord::RetryableFailure::Server(
                    reqwest::StatusCode::BAD_GATEWAY,
                )),
                "may have been applied",
            ),
        ] {
            let mut state = State::default();
            open_webhooks(&mut state, &n);
            run(&mut state, Event::Ask(Target::Webhook(Snowflake(7))), &n);
            run(&mut state, Event::Confirm, &n);
            run(
                &mut state,
                Event::Removed(0, Target::Webhook(Snowflake(7)), Err(error)),
                &n,
            );
            let notice = state.life.notice.unwrap();
            // A gone webhook is simply removed; the rest keep the panel as it was.
            assert!(
                notice.contains(expect) || notice == "It was already removed.",
                "{notice}"
            );
        }
        assert!(State::is_rejected(&Event::Removed(
            0,
            Target::App(Snowflake(1)),
            Err(RestError::AuthenticationRequired)
        )));
    }

    #[test]
    fn create_rename_and_move_are_gated_and_update_the_list() {
        let n = nav(true, true);
        let mut state = State::default();
        open_webhooks(&mut state, &n);
        run(&mut state, Event::Create, &n);
        assert!(!state.life.is_busy(), "an empty name is not sent");
        run(&mut state, Event::NameInput("new".into()), &n);
        run(&mut state, Event::Create, &n);
        assert!(state.life.is_busy());
        run(
            &mut state,
            Event::Created(0, HERE, Ok(hook(9, HERE, true))),
            &n,
        );
        assert_eq!(state.name_input, "");
        run(&mut state, Event::Edit(Snowflake(9)), &n);
        run(&mut state, Event::SaveName, &n);
        assert!(!state.life.is_busy(), "an unchanged name is not sent");
        run(&mut state, Event::MoveTo(HERE), &n);
        assert!(
            !state.life.is_busy(),
            "moving to the same channel is not sent"
        );
        run(&mut state, Event::MoveTo(THERE), &nav(true, false));
        assert!(!state.life.is_busy(), "no Manage Webhooks in the target");
        run(&mut state, Event::MoveTo(THERE), &n);
        assert!(state.life.is_busy());
        run(
            &mut state,
            Event::Edited(0, HERE, Ok(hook(9, THERE, true))),
            &n,
        );
        let Some(Panel::Webhooks {
            list: Some(list),
            editing,
            ..
        }) = &state.panel
        else {
            panic!("panel");
        };
        assert_eq!(list.len(), 2, "the moved webhook left this channel");
        assert_eq!(*editing, None);
    }

    #[test]
    fn rename_keeps_the_token_when_discord_omits_it() {
        let n = nav(true, true);
        let mut state = State::default();
        open_webhooks(&mut state, &n);
        run(&mut state, Event::Edit(Snowflake(7)), &n);
        run(&mut state, Event::NameInput("renamed".into()), &n);
        run(&mut state, Event::SaveName, &n);
        assert!(state.life.is_busy());
        run(
            &mut state,
            Event::Edited(0, HERE, Ok(hook(7, HERE, false))),
            &n,
        );
        assert!(matches!(
            run(&mut state, Event::CopyUrl(Snowflake(7)), &n),
            Some(Action::Copy(_))
        ));
    }

    #[test]
    fn integrations_need_manage_server_and_the_stale_answer_is_ignored() {
        let mut state = State::default();
        run(&mut state, Event::OpenGuild(GUILD), &nav(false, true));
        assert!(state.panel.is_none());
        let n = nav(true, true);
        run(&mut state, Event::OpenGuild(GUILD), &n);
        run(
            &mut state,
            Event::Integrations(5, GUILD, Ok(Vec::new())),
            &n,
        );
        assert!(state.life.is_busy());
        run(
            &mut state,
            Event::Integrations(0, GUILD, Ok(Vec::new())),
            &n,
        );
        assert!(!state.life.is_busy());
        run(
            &mut state,
            Event::Ask(Target::Integration(Snowflake(1))),
            &n,
        );
        assert_eq!(state.confirm.pending(), None, "not listed");
        state.clear();
        assert!(state.panel.is_none());
        run(
            &mut state,
            Event::Integrations(0, GUILD, Ok(Vec::new())),
            &n,
        );
        assert!(state.panel.is_none(), "an answer after clear is dropped");
    }

    #[test]
    fn adding_an_app_only_opens_the_browser_for_a_numeric_client_id() {
        let n = nav(true, true);
        let mut state = State::default();
        run(&mut state, Event::OpenAuthorized, &n);
        run(&mut state, Event::Authorized(0, Ok(Vec::new())), &n);
        run(&mut state, Event::ClientInput("12a".into()), &n);
        assert_eq!(run(&mut state, Event::AddInBrowser, &n), None);
        run(&mut state, Event::ClientInput(" 123 ".into()), &n);
        assert_eq!(
            run(&mut state, Event::AddInBrowser, &n),
            Some(Action::Open(
                "https://discord.com/oauth2/authorize?client_id=123&scope=bot%20applications.commands"
                    .to_owned()
            ))
        );
        assert!(!state.life.is_busy(), "no request is sent");
    }
}
