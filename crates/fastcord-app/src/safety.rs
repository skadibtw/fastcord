//! Message requests, Ignore and communication privacy (SPEC §16, issue #80).
//!
//! One panel with three tabs. **Requests** lists the direct conversations
//! Discord holds as message requests (spam ones marked), with a read-only
//! preview of the newest few messages and an explicit Accept or Decline;
//! previewing sends no accept and no acknowledgement, and a request is never
//! selectable as an ordinary conversation, so nothing marks it read. **Ignored**
//! lists the users the account ignores (a flag apart from blocking) with Stop
//! ignoring. **Privacy** shows who may send friend requests, whether members of
//! servers may message the account, the selected server's override and the
//! explicit-content scan, read only when the user asks and saved only for the
//! fields the user changed.
//!
//! Every change is one explicit click and one request; at most one is in
//! flight, none is ever retried, and what is shown changes only from Discord's
//! own events (`CHANNEL_*`, `RELATIONSHIP_*`, `USER_SETTINGS_UPDATE`), which is
//! also how another client's change appears. A late answer never restores
//! something an event already changed.

use std::collections::BTreeMap;

use fastcord_discord::state::Suppression;
use fastcord_discord::state::navigation::{MAX_REQUEST_ROWS, NavigationSnapshot};
use fastcord_discord::{
    FriendSources, NetworkFailure, PrivacyEdit, PrivacyRequestError, PrivacySettings,
    RelationshipAction, RestClient, RestError, RetryableFailure,
};
use fastcord_model::{Message, Snowflake};
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Element, Length, Task};

use crate::i18n::{tr, trf};
use crate::style;

/// Requests shown per page.
pub const PAGE_SIZE: usize = 8;
/// Ignored users listed.
const MAX_IGNORED_SHOWN: usize = 100;
const PREVIEW_AUTHOR_CHARS: usize = 32;
const PREVIEW_TEXT_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Requests,
    Ignored,
    Privacy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Accept(Snowflake),
    Decline(Snowflake),
    Unignore(Snowflake),
    LoadPrivacy,
    SavePrivacy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Everyone,
    FriendsOfFriends,
    ServerMembers,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Done,
    Settings(PrivacySettings),
}

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    Close,
    Tab(Tab),
    PageNext,
    PagePrev,
    Preview(Snowflake),
    ClosePreview,
    PreviewLoaded(u64, Snowflake, Result<Vec<Message>, RestError>),
    Accept(Snowflake),
    AskDecline(Snowflake),
    ConfirmDecline,
    CancelDecline,
    Unignore(Snowflake),
    LoadPrivacy,
    ToggleSource(Source),
    ToggleDefaultRestricted,
    ToggleServer,
    SetFilter(u8),
    SavePrivacy,
    Done(u64, Action, Result<Outcome, PrivacyRequestError>),
}

/// One shown line of a request preview (clipped; never an attachment or link).
#[derive(Clone, Debug, PartialEq, Eq)]
struct PreviewLine {
    author: String,
    text: String,
}

enum PreviewState {
    Loading,
    Loaded(Vec<PreviewLine>),
    Failed(String),
}

struct Preview {
    channel: Snowflake,
    serial: u64,
    state: PreviewState,
}

/// The settings as read, and what the user is changing them to.
struct Form {
    loaded: PrivacySettings,
    draft: PrivacySettings,
    /// Events up to this stamp are already in `loaded`.
    since: u64,
}

#[derive(Default)]
pub struct State {
    open: bool,
    tab: Option<Tab>,
    page: usize,
    preview: Option<Preview>,
    confirm: Option<Snowflake>,
    form: Option<Form>,
    /// Event stamp when the request in flight was sent.
    requested_at: u64,
    busy: Option<Action>,
    notice: Option<String>,
    serial: u64,
    /// Numbers previews apart from each other and from the writes.
    preview_serial: u64,
    /// Dropped (aborting the request) on logout, account change or close.
    task: Option<iced::task::Handle>,
    preview_task: Option<iced::task::Handle>,
}

fn describe(error: PrivacyRequestError, action: Action) -> String {
    let error = match error {
        PrivacyRequestError::Invalid(_) => {
            return tr("That change is not one this client can send.").to_owned();
        }
        PrivacyRequestError::Rest(error) => error,
    };
    match error {
        RestError::AuthenticationRequired => tr("Your session has expired. Log in again.").to_owned(),
        RestError::PermissionDenied => tr("Discord refused this for this account.").to_owned(),
        RestError::ResourceGone => match action {
            Action::Accept(_) | Action::Decline(_) => {
                tr("That request is already gone; the list follows Discord.").to_owned()
            }
            _ => tr("Discord could not find that.").to_owned(),
        },
        RestError::InvalidRateLimit | RestError::RateLimited | RestError::SchedulerCapacityExceeded => {
            tr("Discord is rate limiting requests. Wait a moment and try again.").to_owned()
        }
        RestError::Http(status) if status.as_u16() == 429 => {
            tr("Discord is rate limiting requests. Wait a moment and try again.").to_owned()
        }
        RestError::Http(status) if status.as_u16() == 400 => {
            tr("Discord rejected the change, or it needs verification only the official client can complete.")
                .to_owned()
        }
        RestError::InvalidJson if action == Action::LoadPrivacy => {
            tr("Discord answered with something unexpected, so these settings cannot be edited here.")
                .to_owned()
        }
        RestError::InvalidJson => {
            tr("Discord answered with something unexpected. The change may have been applied; reload to check.")
                .to_owned()
        }
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection))
        | RestError::Retryable(_) => match action {
            Action::LoadPrivacy => tr("Could not reach Discord. Try Load again.").to_owned(),
            _ => tr("Discord did not confirm. The change may have been applied; reload or check the list before trying again.")
                .to_owned(),
        },
        RestError::WriteNotVerified(_) => tr(fastcord_discord::WRITE_NOT_VERIFIED).to_owned(),
        other => trf("The request failed: {}.", &[&tr(&other.to_string())]),
    }
}

fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Lines of a preview, newest last. A suppressed author's text is withheld.
fn preview_lines(
    messages: Vec<Message>,
    suppressed: &BTreeMap<Snowflake, Suppression>,
) -> Vec<PreviewLine> {
    let mut lines: Vec<_> = messages
        .into_iter()
        .take(fastcord_discord::PREVIEW_MESSAGES)
        .map(|message| {
            if suppressed.contains_key(&message.author.id) {
                return PreviewLine {
                    author: String::new(),
                    text: tr("Hidden message").to_owned(),
                };
            }
            let text = if message.content.trim().is_empty() {
                if message.attachments.is_empty() {
                    tr("(no text)").to_owned()
                } else {
                    tr("(attachment)").to_owned()
                }
            } else {
                clip(message.content.trim(), PREVIEW_TEXT_CHARS)
            };
            PreviewLine {
                author: clip(message.author.display_name(), PREVIEW_AUTHOR_CHARS),
                text,
            }
        })
        .collect();
    lines.reverse();
    lines
}

fn sources_of(sources: FriendSources, which: Source) -> bool {
    match which {
        Source::Everyone => sources.everyone,
        Source::FriendsOfFriends => sources.mutual_friends,
        Source::ServerMembers => sources.mutual_guilds,
    }
}

fn sorted(list: &Option<Vec<Snowflake>>) -> Option<Vec<Snowflake>> {
    list.clone().map(|mut list| {
        list.sort();
        list
    })
}

impl Form {
    /// Only the known fields the user changed.
    fn edit(&self) -> PrivacyEdit {
        let (loaded, draft) = (&self.loaded, &self.draft);
        PrivacyEdit {
            friend_sources: draft.friend_sources.filter(|_| {
                loaded.friend_sources.is_some() && draft.friend_sources != loaded.friend_sources
            }),
            default_guilds_restricted: draft.default_guilds_restricted.filter(|_| {
                loaded.default_guilds_restricted.is_some()
                    && draft.default_guilds_restricted != loaded.default_guilds_restricted
            }),
            restricted_guilds: draft.restricted_guilds.clone().filter(|_| {
                loaded.restricted_guilds.is_some()
                    && sorted(&draft.restricted_guilds) != sorted(&loaded.restricted_guilds)
            }),
            explicit_filter: draft.explicit_filter.filter(|_| {
                loaded.explicit_filter.is_some() && draft.explicit_filter != loaded.explicit_filter
            }),
        }
    }
}

impl State {
    pub fn is_open(&self) -> bool {
        self.open
    }

    fn tab(&self) -> Tab {
        self.tab.unwrap_or(Tab::Requests)
    }

    /// Logout or account change: drops the panel, previews, drafts and any
    /// request in flight.
    pub fn clear(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            preview_serial: self.preview_serial,
            ..Self::default()
        };
    }

    /// Follows the reducer: a request that was accepted, declined or removed
    /// (here or elsewhere) leaves its preview and confirmation behind, and a
    /// settings change from another client replaces the shown value.
    pub fn sync(&mut self, navigation: &NavigationSnapshot) {
        if !self.open {
            return;
        }
        let pending = |channel: Snowflake| {
            navigation
                .requests
                .rows
                .iter()
                .any(|row| row.channel == channel)
        };
        if self.preview.as_ref().is_some_and(|p| !pending(p.channel)) {
            self.preview = None;
            self.preview_task = None;
        }
        if self.confirm.is_some_and(|channel| !pending(channel)) {
            self.confirm = None;
        }
        let pages = navigation.requests.rows.len().div_ceil(PAGE_SIZE).max(1);
        self.page = self.page.min(pages - 1);
        if let Some(form) = &mut self.form
            && navigation.privacy.latest > form.since
        {
            let before = form.loaded.clone();
            navigation.privacy.newer_than(form.since, &mut form.loaded);
            form.since = navigation.privacy.latest;
            // A field another client changed replaces the user's draft of it.
            let (loaded, draft) = (&form.loaded, &mut form.draft);
            if loaded.friend_sources != before.friend_sources {
                draft.friend_sources = loaded.friend_sources;
            }
            if loaded.default_guilds_restricted != before.default_guilds_restricted {
                draft.default_guilds_restricted = loaded.default_guilds_restricted;
            }
            if loaded.restricted_guilds != before.restricted_guilds {
                draft
                    .restricted_guilds
                    .clone_from(&loaded.restricted_guilds);
            }
            if loaded.explicit_filter != before.explicit_filter {
                draft.explicit_filter = loaded.explicit_filter;
            }
        }
    }

    fn send<F, Fut>(&mut self, action: Action, client: &RestClient, run: F) -> Task<Event>
    where
        F: FnOnce(RestClient) -> Fut,
        Fut: Future<Output = Result<Outcome, PrivacyRequestError>> + Send + 'static,
    {
        self.busy = Some(action);
        self.notice = None;
        self.confirm = None;
        let serial = self.serial;
        let client = client.clone();
        let (task, handle) = Task::perform(run(client), move |result| {
            Event::Done(serial, action, result)
        })
        .abortable();
        self.task = Some(handle.abort_on_drop());
        task
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        client: &RestClient,
    ) -> Task<Event> {
        match event {
            Event::Toggle if self.open => {
                if self.busy.is_none() {
                    self.clear();
                }
            }
            Event::Toggle => {
                self.open = true;
                self.sync(navigation);
            }
            Event::Close if self.busy.is_none() => self.clear(),
            Event::Close => {}
            Event::Done(serial, action, result) if serial == self.serial => {
                self.busy = None;
                self.task = None;
                match result {
                    Ok(outcome) => self.finished(outcome, action, navigation),
                    Err(error) => self.notice = Some(describe(error, action)),
                }
            }
            Event::PreviewLoaded(serial, channel, result) => {
                if let Some(preview) = self
                    .preview
                    .as_mut()
                    .filter(|p| p.serial == serial && p.channel == channel)
                {
                    self.preview_task = None;
                    preview.state = match result {
                        Ok(messages) => PreviewState::Loaded(preview_lines(
                            messages,
                            navigation.suppressed.as_ref(),
                        )),
                        Err(error) => PreviewState::Failed(describe(
                            PrivacyRequestError::Rest(error),
                            Action::LoadPrivacy,
                        )),
                    };
                }
            }
            _ if !self.open => {}
            Event::Tab(tab) => {
                self.tab = Some(tab);
                self.notice = None;
                self.confirm = None;
            }
            Event::PageNext => {
                let pages = navigation.requests.rows.len().div_ceil(PAGE_SIZE).max(1);
                self.page = (self.page + 1).min(pages - 1);
            }
            Event::PagePrev => self.page = self.page.saturating_sub(1),
            Event::ClosePreview => {
                self.preview = None;
                self.preview_task = None;
            }
            Event::Preview(channel) => {
                if navigation
                    .requests
                    .rows
                    .iter()
                    .any(|r| r.channel == channel)
                {
                    self.preview_serial = self.preview_serial.wrapping_add(1);
                    let serial = self.preview_serial;
                    self.preview = Some(Preview {
                        channel,
                        serial,
                        state: PreviewState::Loading,
                    });
                    let client = client.clone();
                    let (task, handle) = Task::perform(
                        async move { client.message_request_preview(channel).await },
                        move |result| Event::PreviewLoaded(serial, channel, result),
                    )
                    .abortable();
                    self.preview_task = Some(handle.abort_on_drop());
                    return task;
                }
            }
            Event::CancelDecline => self.confirm = None,
            _ if self.busy.is_some() => {
                // One request at a time; the answer of the first is awaited.
                if !matches!(event, Event::Done(..)) {
                    self.notice = Some(tr("Wait for the previous change to finish.").to_owned());
                }
            }
            Event::Accept(channel) => {
                if navigation
                    .requests
                    .rows
                    .iter()
                    .any(|r| r.channel == channel)
                {
                    return self.send(Action::Accept(channel), client, move |client| async move {
                        client
                            .accept_message_request(channel)
                            .await
                            .map(|()| Outcome::Done)
                            .map_err(PrivacyRequestError::Rest)
                    });
                }
                self.notice = Some(tr("That request is no longer pending.").to_owned());
            }
            Event::AskDecline(channel) => {
                if navigation
                    .requests
                    .rows
                    .iter()
                    .any(|r| r.channel == channel)
                {
                    self.confirm = Some(channel);
                    self.notice = None;
                }
            }
            Event::ConfirmDecline => {
                let Some(channel) = self.confirm.take() else {
                    return Task::none();
                };
                if navigation
                    .requests
                    .rows
                    .iter()
                    .any(|r| r.channel == channel)
                {
                    return self.send(Action::Decline(channel), client, move |client| async move {
                        client
                            .decline_message_request(channel)
                            .await
                            .map(|()| Outcome::Done)
                            .map_err(PrivacyRequestError::Rest)
                    });
                }
                self.notice = Some(tr("That request is no longer pending.").to_owned());
            }
            Event::Unignore(user) => {
                if navigation.suppressed.get(&user) == Some(&Suppression::Ignored) {
                    return self.send(Action::Unignore(user), client, move |client| async move {
                        client
                            .relationship_action(RelationshipAction::Unignore, user)
                            .await
                            .map(|()| Outcome::Done)
                            .map_err(PrivacyRequestError::Rest)
                    });
                }
            }
            Event::LoadPrivacy => {
                self.requested_at = navigation.privacy.latest;
                return self.send(Action::LoadPrivacy, client, |client| async move {
                    client
                        .privacy_settings()
                        .await
                        .map(Outcome::Settings)
                        .map_err(PrivacyRequestError::Rest)
                });
            }
            Event::ToggleSource(which) => {
                if let Some(form) = &mut self.form
                    && let Some(sources) = &mut form.draft.friend_sources
                {
                    match which {
                        Source::Everyone => sources.everyone = !sources.everyone,
                        Source::FriendsOfFriends => {
                            sources.mutual_friends = !sources.mutual_friends
                        }
                        Source::ServerMembers => sources.mutual_guilds = !sources.mutual_guilds,
                    }
                    self.notice = None;
                }
            }
            Event::ToggleDefaultRestricted => {
                if let Some(form) = &mut self.form
                    && let Some(restricted) = &mut form.draft.default_guilds_restricted
                {
                    *restricted = !*restricted;
                    self.notice = None;
                }
            }
            Event::ToggleServer => {
                if let (Some(form), Some(guild)) = (&mut self.form, navigation.selected_guild)
                    && let Some(list) = &mut form.draft.restricted_guilds
                {
                    if let Some(at) = list.iter().position(|id| *id == guild) {
                        list.remove(at);
                    } else if list.len() < fastcord_discord::MAX_RESTRICTED_GUILDS {
                        list.push(guild);
                    }
                    self.notice = None;
                }
            }
            Event::SetFilter(level) => {
                if let Some(form) = &mut self.form
                    && level <= 2
                    && form.draft.explicit_filter.is_some()
                {
                    form.draft.explicit_filter = Some(level);
                    self.notice = None;
                }
            }
            Event::SavePrivacy => {
                let Some(form) = &self.form else {
                    return Task::none();
                };
                let edit = form.edit();
                if edit == PrivacyEdit::default() {
                    self.notice = Some(tr("Nothing changed.").to_owned());
                    return Task::none();
                }
                self.requested_at = navigation.privacy.latest;
                return self.send(Action::SavePrivacy, client, move |client| async move {
                    client.edit_privacy(&edit).await.map(Outcome::Settings)
                });
            }
            // A stale answer of an earlier session.
            Event::Done(..) => {}
        }
        Task::none()
    }

    fn finished(&mut self, outcome: Outcome, action: Action, navigation: &NavigationSnapshot) {
        match outcome {
            Outcome::Settings(mut settings) => {
                // Events newer than the request were not in the answer.
                navigation
                    .privacy
                    .newer_than(self.requested_at, &mut settings);
                let mut loaded = self.form.take().map(|f| f.loaded).unwrap_or_default();
                loaded.overlay(&settings);
                self.form = Some(Form {
                    draft: loaded.clone(),
                    loaded,
                    since: navigation.privacy.latest,
                });
                self.notice = (action == Action::SavePrivacy).then(|| tr("Saved.").to_owned());
            }
            Outcome::Done => {
                self.notice = Some(match action {
                    Action::Accept(_) => {
                        tr("Accepted. The conversation moves to your messages when Discord confirms.")
                    }
                    Action::Decline(_) => tr("Declined. It leaves the list when Discord confirms."),
                    _ => tr("Sent. The list changes when Discord confirms."),
                }
                .to_owned());
            }
        }
    }

    /// A 401 from this panel's request: the caller stops the account.
    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Done(
                _,
                _,
                Err(PrivacyRequestError::Rest(RestError::AuthenticationRequired))
            ) | Event::PreviewLoaded(_, _, Err(RestError::AuthenticationRequired))
        )
    }

    #[cfg(test)]
    fn busy(&self) -> Option<Action> {
        self.busy
    }
}

fn small<'a>(label: &'static str, event: Option<Event>) -> Element<'a, Event> {
    button(text(tr(label)).size(12))
        .on_press_maybe(event)
        .style(style::tool_button(true, false))
        .into()
}

fn toggle<'a>(label: String, on: bool, event: Option<Event>) -> Element<'a, Event> {
    button(
        text(format!(
            "{label}: {}",
            if on { tr("On") } else { tr("Off") }
        ))
        .size(12),
    )
    .on_press_maybe(event)
    .style(style::tool_button(on, false))
    .into()
}

fn name_of(navigation: &NavigationSnapshot, user: Snowflake) -> String {
    navigation
        .relationships
        .iter()
        .find(|r| r.user.id == user)
        .map(|r| clip(r.user.display_name(), PREVIEW_AUTHOR_CHARS))
        .unwrap_or_else(|| trf("User {}", &[&user]))
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
) -> Option<Element<'a, Event>> {
    if !state.open {
        return None;
    }
    let idle = state.busy.is_none();
    let tab_button = |label: &'static str, tab: Tab| {
        button(text(tr(label)).size(12))
            .on_press(Event::Tab(tab))
            .style(style::tool_button(state.tab() == tab, false))
    };
    let mut panel = column![
        row![
            text(tr("Safety")).size(13),
            tab_button("Requests", Tab::Requests),
            tab_button("Ignored", Tab::Ignored),
            tab_button("Privacy", Tab::Privacy),
            space::horizontal(),
            crate::icons::close(idle.then_some(Event::Close)),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
    ]
    .spacing(6);
    panel = match state.tab() {
        Tab::Requests => panel.push(requests_view(state, navigation, idle)),
        Tab::Ignored => panel.push(ignored_view(navigation, idle)),
        Tab::Privacy => panel.push(privacy_view(state, navigation, idle)),
    };
    if state.busy.is_some() {
        panel = panel.push(
            text(tr("Waiting for Discord…"))
                .size(12)
                .style(text::secondary),
        );
    }
    if let Some(notice) = &state.notice {
        panel = panel.push(text(notice.as_str()).size(12).style(text::secondary));
    }
    Some(
        container(panel)
            .padding(8)
            .width(Length::Fill)
            .style(style::card)
            .into(),
    )
}

fn requests_view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
    idle: bool,
) -> Element<'a, Event> {
    let requests = &navigation.requests;
    let mut body = column![
        text(tr("Previewing a request does not accept it or mark it read. Only Accept lets this person message you normally."))
            .size(11)
            .style(text::secondary)
    ]
    .spacing(4);
    if requests.rows.is_empty() {
        return body.push(text(tr("No message requests.")).size(12)).into();
    }
    let start = state.page * PAGE_SIZE;
    for request in requests.rows.iter().skip(start).take(PAGE_SIZE) {
        let channel = request.channel;
        let confirming = state.confirm == Some(channel);
        let mut line = row![
            text(if request.spam {
                trf("{} (spam)", &[&request.label])
            } else {
                request.label.clone()
            })
            .size(12)
            .width(Length::Fill),
            small("Show preview", Some(Event::Preview(channel))),
            small("Accept", idle.then_some(Event::Accept(channel))),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        line = if confirming {
            line.push(small("Yes, decline", idle.then_some(Event::ConfirmDecline)))
                .push(small("Keep", Some(Event::CancelDecline)))
        } else {
            line.push(small("Decline", idle.then_some(Event::AskDecline(channel))))
        };
        body = body.push(line);
        if let Some(preview) = state.preview.as_ref().filter(|p| p.channel == channel) {
            let mut lines = column![].spacing(2);
            match &preview.state {
                PreviewState::Loading => {
                    lines =
                        lines.push(text(tr("Loading preview…")).size(11).style(text::secondary));
                }
                PreviewState::Failed(reason) => {
                    lines = lines.push(text(reason.as_str()).size(11).style(text::danger));
                }
                PreviewState::Loaded(items) if items.is_empty() => {
                    lines = lines.push(text(tr("No messages.")).size(11).style(text::secondary));
                }
                PreviewState::Loaded(items) => {
                    for item in items {
                        lines = lines.push(
                            text(if item.author.is_empty() {
                                item.text.clone()
                            } else {
                                format!("{}: {}", item.author, item.text)
                            })
                            .size(11),
                        );
                    }
                }
            }
            body = body.push(lines.push(small("Hide preview", Some(Event::ClosePreview))));
        }
    }
    let pages = requests.rows.len().div_ceil(PAGE_SIZE).max(1);
    let more = if requests.total > MAX_REQUEST_ROWS {
        trf(" ({} not listed)", &[&(requests.total - MAX_REQUEST_ROWS)])
    } else {
        String::new()
    };
    body.push(
        row![
            small("Previous", (state.page > 0).then_some(Event::PagePrev)),
            text(trf(
                "Page {} of {}, {} requests{}",
                &[&(state.page + 1), &pages, &requests.total, &more],
            ))
            .size(11),
            small("Next", (state.page + 1 < pages).then_some(Event::PageNext)),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
    )
    .into()
}

fn ignored_view<'a>(navigation: &'a NavigationSnapshot, idle: bool) -> Element<'a, Event> {
    let mut list = column![].spacing(2);
    let mut count = 0;
    for user in navigation
        .suppressed
        .iter()
        .filter(|(_, kind)| **kind == Suppression::Ignored)
        .map(|(id, _)| *id)
    {
        count += 1;
        if count > MAX_IGNORED_SHOWN {
            continue;
        }
        list = list.push(
            row![
                text(name_of(navigation, user)).size(12).width(Length::Fill),
                small("Stop ignoring", idle.then_some(Event::Unignore(user))),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );
    }
    let summary = if count == 0 {
        tr("You are not ignoring anyone. Use Ignore on a profile card. Ignoring hides messages, notifications and calls without blocking.").to_owned()
    } else if count > MAX_IGNORED_SHOWN {
        trf(
            "{} ignored users. Showing {}.",
            &[&count, &MAX_IGNORED_SHOWN],
        )
    } else {
        trf("{} ignored.", &[&count])
    };
    column![
        text(summary).size(11).style(text::secondary),
        scrollable(list).height(Length::Fixed(140.0)),
    ]
    .spacing(4)
    .into()
}

fn privacy_view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
    idle: bool,
) -> Element<'a, Event> {
    let mut body = column![].spacing(4);
    let loading = state.busy == Some(Action::LoadPrivacy);
    body = body.push(
        row![
            text(tr("Settings are read only when you ask."))
                .size(11)
                .style(text::secondary),
            space::horizontal(),
            small(
                if state.form.is_some() {
                    "Reload"
                } else {
                    "Load"
                },
                (idle && !loading).then_some(Event::LoadPrivacy)
            ),
        ]
        .align_y(iced::Alignment::Center),
    );
    let Some(form) = &state.form else {
        return body.into();
    };
    let draft = &form.draft;
    let unknown = || {
        text(tr("Not available for this account."))
            .size(11)
            .style(text::secondary)
    };
    body = body.push(text(tr("Who can send you a friend request")).size(12));
    body = match draft.friend_sources {
        Some(sources) => body.push(
            row![
                toggle(
                    tr("Everyone").into(),
                    sources_of(sources, Source::Everyone),
                    idle.then_some(Event::ToggleSource(Source::Everyone))
                ),
                toggle(
                    tr("Friends of friends").into(),
                    sources_of(sources, Source::FriendsOfFriends),
                    idle.then_some(Event::ToggleSource(Source::FriendsOfFriends))
                ),
                toggle(
                    tr("Server members").into(),
                    sources_of(sources, Source::ServerMembers),
                    idle.then_some(Event::ToggleSource(Source::ServerMembers))
                ),
            ]
            .spacing(6),
        ),
        None => body.push(unknown()),
    };
    body = body.push(text(tr("Direct messages from server members")).size(12));
    body = match draft.default_guilds_restricted {
        Some(restricted) => body.push(toggle(
            tr("Allowed in new servers").into(),
            !restricted,
            idle.then_some(Event::ToggleDefaultRestricted),
        )),
        None => body.push(unknown()),
    };
    body = match (navigation.selected_guild, &draft.restricted_guilds) {
        (Some(guild), Some(list)) => body.push(toggle(
            trf("Allowed from {}", &[&navigation.selected_guild_name]),
            !list.contains(&guild),
            idle.then_some(Event::ToggleServer),
        )),
        (None, Some(_)) => body.push(
            text(tr("Open a server to change its override."))
                .size(11)
                .style(text::secondary),
        ),
        _ => body,
    };
    body = body.push(text(tr("Scan direct messages for explicit images")).size(12));
    body = match draft.explicit_filter {
        Some(level) => body.push(
            row![
                toggle(
                    tr("Off").into(),
                    level == 0,
                    idle.then_some(Event::SetFilter(0))
                ),
                toggle(
                    tr("From non-friends").into(),
                    level == 1,
                    idle.then_some(Event::SetFilter(1))
                ),
                toggle(
                    tr("From everyone").into(),
                    level == 2,
                    idle.then_some(Event::SetFilter(2))
                ),
            ]
            .spacing(6),
        ),
        None => body.push(unknown()),
    };
    body.push(
        row![
            small(
                "Save changes",
                (idle && form.edit() != PrivacyEdit::default()).then_some(Event::SavePrivacy)
            ),
            text(tr("Only the fields you changed are sent."))
                .size(11)
                .style(text::secondary),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::gateway::{ClientProperties, HostOs};
    use fastcord_discord::state::navigation::{MessageRequests, RequestRow};
    use fastcord_discord::{PrivacyLive, StatusCode};
    use std::sync::Arc;

    fn rest() -> RestClient {
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            fastcord_discord::UserToken::new("dummy".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn requests(ids: &[u64]) -> NavigationSnapshot {
        NavigationSnapshot {
            requests: MessageRequests {
                total: ids.len(),
                rows: ids
                    .iter()
                    .map(|id| RequestRow {
                        channel: Snowflake(*id),
                        label: format!("user{id}"),
                        spam: *id % 2 == 0,
                    })
                    .collect(),
            },
            ..NavigationSnapshot::default()
        }
    }

    fn opened(navigation: &NavigationSnapshot) -> State {
        let mut state = State::default();
        drop(state.update(Event::Toggle, navigation, &rest()));
        state
    }

    fn settings(restricted: bool) -> PrivacySettings {
        PrivacySettings {
            friend_sources: Some(FriendSources {
                everyone: false,
                mutual_friends: true,
                mutual_guilds: false,
            }),
            default_guilds_restricted: Some(restricted),
            restricted_guilds: Some(vec![Snowflake(7)]),
            explicit_filter: Some(1),
        }
    }

    fn loaded(state: &mut State, navigation: &NavigationSnapshot, value: PrivacySettings) {
        drop(state.update(Event::LoadPrivacy, navigation, &rest()));
        drop(state.update(
            Event::Done(
                state.serial,
                Action::LoadPrivacy,
                Ok(Outcome::Settings(value)),
            ),
            navigation,
            &rest(),
        ));
    }

    #[test]
    fn previewing_sends_nothing_that_accepts_or_acknowledges() {
        let navigation = requests(&[10, 11]);
        let mut state = opened(&navigation);
        drop(state.update(Event::Preview(Snowflake(10)), &navigation, &rest()));
        // A preview is a read: no write is in flight and the request stays.
        assert_eq!(state.busy(), None);
        assert!(state.preview.is_some());
        // A preview of something that is not a pending request is not started.
        let mut other = opened(&navigation);
        drop(other.update(Event::Preview(Snowflake(99)), &navigation, &rest()));
        assert!(other.preview.is_none());
        // The answer shows clipped lines, oldest first, nothing else.
        let message = |id: u64, text: &str| -> Message {
            serde_json::from_value(serde_json::json!({
                "id": id.to_string(), "channel_id": "10", "content": text,
                "timestamp": "2026-10-07T12:00:00.000000+00:00",
                "author": {"id": "3", "username": "stranger"},
            }))
            .unwrap()
        };
        let long = "x".repeat(1000);
        drop(state.update(
            Event::PreviewLoaded(
                state.preview.as_ref().unwrap().serial,
                Snowflake(10),
                Ok(vec![message(2, &long), message(1, "hello")]),
            ),
            &navigation,
            &rest(),
        ));
        let Some(Preview {
            state: PreviewState::Loaded(lines),
            ..
        }) = &state.preview
        else {
            panic!("loaded");
        };
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[1].text.chars().count(), PREVIEW_TEXT_CHARS);
    }

    #[test]
    fn a_suppressed_authors_text_is_withheld_from_a_preview() {
        let message: Message = serde_json::from_value(serde_json::json!({
            "id": "1", "channel_id": "10", "content": "secret",
            "timestamp": "2026-10-07T12:00:00.000000+00:00",
            "author": {"id": "3", "username": "blocked"},
        }))
        .unwrap();
        let mut suppressed = BTreeMap::new();
        suppressed.insert(Snowflake(3), Suppression::Blocked);
        let lines = preview_lines(vec![message], &suppressed);
        assert_eq!(lines[0].text, "Hidden message");
        assert!(lines[0].author.is_empty());
    }

    #[test]
    fn accept_and_decline_act_only_on_a_pending_request_and_decline_asks_first() {
        let navigation = requests(&[10]);
        let mut state = opened(&navigation);
        // Not pending: nothing is sent.
        drop(state.update(Event::Accept(Snowflake(99)), &navigation, &rest()));
        assert_eq!(state.busy(), None);
        // Decline needs the second click and is validated again at that click.
        drop(state.update(Event::AskDecline(Snowflake(10)), &navigation, &rest()));
        assert_eq!(state.busy(), None);
        assert_eq!(state.confirm, Some(Snowflake(10)));
        drop(state.update(Event::ConfirmDecline, &requests(&[]), &rest()));
        assert_eq!(state.busy(), None, "the request was handled elsewhere");
        drop(state.update(Event::AskDecline(Snowflake(10)), &navigation, &rest()));
        drop(state.update(Event::ConfirmDecline, &navigation, &rest()));
        assert_eq!(state.busy(), Some(Action::Decline(Snowflake(10))));
        // One request at a time.
        drop(state.update(Event::Accept(Snowflake(10)), &navigation, &rest()));
        assert_eq!(state.busy(), Some(Action::Decline(Snowflake(10))));
        // The answer only reports; the list changes with Discord's event.
        drop(state.update(
            Event::Done(
                state.serial,
                Action::Decline(Snowflake(10)),
                Ok(Outcome::Done),
            ),
            &navigation,
            &rest(),
        ));
        assert_eq!(state.busy(), None);
        assert!(state.notice.as_deref().unwrap().contains("confirms"));
    }

    #[test]
    fn a_request_handled_by_another_client_clears_its_preview_and_confirmation() {
        let navigation = requests(&[10]);
        let mut state = opened(&navigation);
        drop(state.update(Event::Preview(Snowflake(10)), &navigation, &rest()));
        drop(state.update(Event::AskDecline(Snowflake(10)), &navigation, &rest()));
        state.sync(&requests(&[]));
        assert!(state.preview.is_none() && state.confirm.is_none());
        // An answer after the event is harmless.
        drop(state.update(
            Event::PreviewLoaded(1, Snowflake(10), Ok(Vec::new())),
            &requests(&[]),
            &rest(),
        ));
        assert!(state.preview.is_none());
    }

    #[test]
    fn requests_are_paged_and_the_page_follows_the_list() {
        let ids: Vec<u64> = (1..=20).collect();
        let navigation = requests(&ids);
        let mut state = opened(&navigation);
        for _ in 0..10 {
            drop(state.update(Event::PageNext, &navigation, &rest()));
        }
        assert_eq!(state.page, 2);
        state.sync(&requests(&[1, 2, 3]));
        assert_eq!(state.page, 0);
    }

    #[test]
    fn privacy_sends_only_changed_known_fields_and_unknown_ones_stay_untouched() {
        let navigation = NavigationSnapshot::default();
        let mut state = opened(&navigation);
        // Nothing is read until the user asks.
        assert!(state.form.is_none() && state.busy().is_none());
        loaded(
            &mut state,
            &navigation,
            PrivacySettings {
                default_guilds_restricted: None,
                ..settings(false)
            },
        );
        let form = state.form.as_ref().unwrap();
        assert_eq!(form.edit(), PrivacyEdit::default());
        // An unknown field cannot be toggled, so it can never be written.
        drop(state.update(Event::ToggleDefaultRestricted, &navigation, &rest()));
        assert_eq!(state.form.as_ref().unwrap().edit(), PrivacyEdit::default());
        drop(state.update(Event::SetFilter(2), &navigation, &rest()));
        drop(state.update(Event::ToggleSource(Source::Everyone), &navigation, &rest()));
        let edit = state.form.as_ref().unwrap().edit();
        assert_eq!(edit.explicit_filter, Some(2));
        assert_eq!(
            edit.friend_sources,
            Some(FriendSources {
                everyone: true,
                mutual_friends: true,
                mutual_guilds: false
            })
        );
        assert_eq!(edit.default_guilds_restricted, None);
        assert_eq!(edit.restricted_guilds, None);
    }

    #[test]
    fn the_server_override_edits_the_whole_list_and_only_for_the_selected_server() {
        let navigation = NavigationSnapshot {
            selected_guild: Some(Snowflake(8)),
            ..NavigationSnapshot::default()
        };
        let mut state = opened(&navigation);
        loaded(&mut state, &navigation, settings(false));
        drop(state.update(Event::ToggleServer, &navigation, &rest()));
        let edit = state.form.as_ref().unwrap().edit();
        assert_eq!(
            edit.restricted_guilds,
            Some(vec![Snowflake(7), Snowflake(8)]),
            "the existing override is kept, not dropped"
        );
        // Toggling back leaves nothing to send.
        drop(state.update(Event::ToggleServer, &navigation, &rest()));
        assert_eq!(state.form.as_ref().unwrap().edit(), PrivacyEdit::default());
        // No server selected: no override can be edited.
        let none = NavigationSnapshot::default();
        drop(state.update(Event::ToggleServer, &none, &rest()));
        assert_eq!(state.form.as_ref().unwrap().edit(), PrivacyEdit::default());
    }

    #[test]
    fn a_change_from_another_client_replaces_the_value_and_a_late_answer_cannot_undo_it() {
        let mut live = PrivacyLive::default();
        let navigation = |live: &PrivacyLive| NavigationSnapshot {
            privacy: Arc::new(live.clone()),
            ..NavigationSnapshot::default()
        };
        let mut state = opened(&navigation(&live));
        // The read is sent while nothing has changed...
        drop(state.update(Event::LoadPrivacy, &navigation(&live), &rest()));
        // ...another client flips the setting before the answer arrives...
        assert!(live.apply(&PrivacySettings {
            default_guilds_restricted: Some(true),
            ..PrivacySettings::default()
        }));
        // ...and the answer still carries the older value.
        drop(state.update(
            Event::Done(
                state.serial,
                Action::LoadPrivacy,
                Ok(Outcome::Settings(settings(false))),
            ),
            &navigation(&live),
            &rest(),
        ));
        let form = state.form.as_ref().unwrap();
        assert_eq!(form.loaded.default_guilds_restricted, Some(true));
        assert_eq!(form.draft.default_guilds_restricted, Some(true));
        // A later remote change replaces both the shown value and a local draft.
        drop(state.update(Event::SetFilter(2), &navigation(&live), &rest()));
        assert!(live.apply(&PrivacySettings {
            explicit_filter: Some(0),
            ..PrivacySettings::default()
        }));
        state.sync(&navigation(&live));
        let form = state.form.as_ref().unwrap();
        assert_eq!(form.draft.explicit_filter, Some(0));
        assert_eq!(form.edit(), PrivacyEdit::default());
    }

    #[test]
    fn a_failed_save_keeps_the_draft_says_it_may_have_applied_and_is_not_repeated() {
        let navigation = NavigationSnapshot::default();
        let mut state = opened(&navigation);
        loaded(&mut state, &navigation, settings(false));
        drop(state.update(Event::SetFilter(2), &navigation, &rest()));
        drop(state.update(Event::SavePrivacy, &navigation, &rest()));
        assert_eq!(state.busy(), Some(Action::SavePrivacy));
        drop(state.update(Event::SavePrivacy, &navigation, &rest()));
        assert!(state.notice.as_deref().unwrap().contains("Wait"));
        drop(state.update(
            Event::Done(
                state.serial,
                Action::SavePrivacy,
                Err(PrivacyRequestError::Rest(RestError::Retryable(
                    RetryableFailure::Network(NetworkFailure::Connection),
                ))),
            ),
            &navigation,
            &rest(),
        ));
        assert_eq!(state.busy(), None, "nothing is resent by itself");
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("may have been applied")
        );
        assert_eq!(state.form.as_ref().unwrap().draft.explicit_filter, Some(2));
        assert_eq!(state.form.as_ref().unwrap().loaded.explicit_filter, Some(1));
    }

    #[test]
    fn unignore_is_offered_only_for_an_ignored_user_and_errors_are_worded() {
        let mut navigation = NavigationSnapshot::default();
        let mut map = BTreeMap::new();
        map.insert(Snowflake(5), Suppression::Ignored);
        map.insert(Snowflake(6), Suppression::Blocked);
        navigation.suppressed = Arc::new(map);
        let mut state = opened(&navigation);
        drop(state.update(Event::Unignore(Snowflake(6)), &navigation, &rest()));
        assert_eq!(state.busy(), None, "a block is not an ignore");
        drop(state.update(Event::Unignore(Snowflake(5)), &navigation, &rest()));
        assert_eq!(state.busy(), Some(Action::Unignore(Snowflake(5))));
        for (error, wanted) in [
            (RestError::AuthenticationRequired, "expired"),
            (RestError::PermissionDenied, "refused"),
            (RestError::ResourceGone, "find"),
            (
                RestError::Http(StatusCode::TOO_MANY_REQUESTS),
                "rate limiting",
            ),
            (RestError::Http(StatusCode::BAD_REQUEST), "verification"),
        ] {
            let said = describe(
                PrivacyRequestError::Rest(error),
                Action::Unignore(Snowflake(5)),
            );
            assert!(said.contains(wanted), "{said}");
        }
    }

    #[test]
    fn a_401_stops_the_account_and_logout_drops_everything() {
        let rejected = Event::Done(
            0,
            Action::Accept(Snowflake(1)),
            Err(PrivacyRequestError::Rest(RestError::AuthenticationRequired)),
        );
        assert!(State::is_rejected(&rejected));
        assert!(State::is_rejected(&Event::PreviewLoaded(
            0,
            Snowflake(1),
            Err(RestError::AuthenticationRequired)
        )));
        let navigation = requests(&[10]);
        let mut state = opened(&navigation);
        drop(state.update(Event::Preview(Snowflake(10)), &navigation, &rest()));
        drop(state.update(Event::AskDecline(Snowflake(10)), &navigation, &rest()));
        drop(state.update(Event::ConfirmDecline, &navigation, &rest()));
        let stale = state.serial;
        state.clear();
        assert!(!state.is_open() && state.preview.is_none() && state.busy().is_none());
        // The answer of the request that was in flight finds a new serial.
        drop(state.update(
            Event::Done(stale, Action::Decline(Snowflake(10)), Ok(Outcome::Done)),
            &navigation,
            &rest(),
        ));
        assert!(state.notice.is_none());
    }

    #[test]
    fn the_views_build_for_every_tab() {
        let mut navigation = requests(&[10, 11]);
        let mut map = BTreeMap::new();
        map.insert(Snowflake(5), Suppression::Ignored);
        navigation.suppressed = Arc::new(map);
        let mut state = opened(&navigation);
        loaded(&mut state, &navigation, settings(true));
        for tab in [Tab::Requests, Tab::Ignored, Tab::Privacy] {
            drop(state.update(Event::Tab(tab), &navigation, &rest()));
            assert!(view(&state, &navigation).is_some());
        }
        assert!(view(&State::default(), &navigation).is_none());
    }
}
