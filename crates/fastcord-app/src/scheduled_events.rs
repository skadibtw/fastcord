//! Scheduled community events (SPEC §16, issue #92): the list and details of a
//! server's events, interest, the event link, joining the voice or Stage
//! channel, creating, editing, starting, ending, cancelling and deleting, and
//! a banner for an event that is about to start.
//!
//! The events live in the reducer (`NavigationSnapshot::events`); this panel
//! only loads a fresh list into it and shows what the Gateway reports. Every
//! write is one explicit click, refused while another is in flight, rechecked
//! by the reducer against current permissions and the event's state just
//! before sending, and never retried; cancelling and deleting ask first, and
//! an ambiguous failure reads "may have been applied". Times are shown and
//! typed in the local time zone as it was at start-up (no DST history).

use fastcord_discord::state::navigation::{ChannelKind, NavigationSnapshot};
use fastcord_discord::{DraftError, EventAction, EventDraft, RestClient, RestError};
use fastcord_model::markup::TimeStyle;
use fastcord_model::{
    EventKind, EventStatus, MAX_DESCRIPTION_CHARS, MAX_LOCATION_CHARS, MAX_NAME_CHARS,
    ScheduledEvent, Snowflake,
};
use iced::widget::{button, column, container, pick_list, row, scrollable, text, text_input};
use iced::{Element, Length, Task};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

use crate::gateway::NavigationBridge;
use crate::i18n::{tr, trf};
use crate::localtime;

/// An event this close to starting (or running) shows the banner.
const SOON_MS: u64 = 15 * 60 * 1000;
/// A scheduled event that is late by more than this no longer shows it.
const LATE_MS: u64 = 60 * 60 * 1000;
/// Banners the user hid, remembered for the session.
const MAX_HIDDEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    Cancel,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Description,
    Location,
    Start,
    End,
}

/// A voice or Stage channel to pick for an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    id: Snowflake,
    name: String,
}

impl std::fmt::Display for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Open(Snowflake),
    Close,
    Reload,
    Loaded(u64, Snowflake, Result<Vec<ScheduledEvent>, RestError>),
    Show(Snowflake),
    List,
    Do(EventAction),
    Ask(Confirm, Snowflake),
    Keep,
    CopyLink(Snowflake),
    /// Join an event's channel; handled by the application.
    Join(Snowflake, Snowflake),
    New,
    Edit(Snowflake),
    Input(Field, String),
    Kind(EventKind),
    Channel(Choice),
    Save,
    Done(u64, Result<(), RestError>),
    /// Hides the banner of an event.
    Hide(Snowflake),
}

#[derive(Debug)]
struct Form {
    /// The event being edited and the start it had.
    editing: Option<(Snowflake, u64)>,
    name: String,
    description: String,
    kind: EventKind,
    channel: Option<Snowflake>,
    location: String,
    start: String,
    end: String,
    error: Option<String>,
}

#[derive(Debug)]
enum Screen {
    List,
    Detail(Snowflake),
    Form(Box<Form>),
}

#[derive(Debug, Default)]
enum Load {
    #[default]
    Idle,
    Loading,
    Failed(String),
}

#[derive(Debug)]
pub struct State {
    /// The server whose events the panel shows; `None` while closed.
    guild: Option<Snowflake>,
    screen: Screen,
    load: Load,
    /// The serial of the write in flight.
    pending: Option<u64>,
    serial: u64,
    generation: u64,
    notice: Option<String>,
    confirm: Option<(Confirm, Snowflake)>,
    hidden: Vec<Snowflake>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            guild: None,
            screen: Screen::List,
            load: Load::Idle,
            pending: None,
            serial: 0,
            generation: 0,
            notice: None,
            confirm: None,
            hidden: Vec::new(),
        }
    }
}

/// Unix milliseconds of `YYYY-MM-DD HH:MM` in the given offset.
fn parse_local(raw: &str, offset: UtcOffset) -> Option<u64> {
    let (date, clock) = raw.trim().split_once([' ', 'T'])?;
    let mut date = date.split('-');
    let (year, month, day) = (date.next()?, date.next()?, date.next()?);
    let mut clock = clock.trim().split(':');
    let (hour, minute) = (clock.next()?, clock.next()?);
    if date.next().is_some() || clock.next().is_some() {
        return None;
    }
    let date = Date::from_calendar_date(
        year.parse().ok()?,
        Month::try_from(month.parse::<u8>().ok()?).ok()?,
        day.parse().ok()?,
    )
    .ok()?;
    let clock = Time::from_hms(hour.parse().ok()?, minute.parse().ok()?, 0).ok()?;
    let seconds = PrimitiveDateTime::new(date, clock)
        .assume_offset(offset)
        .unix_timestamp();
    u64::try_from(seconds).ok().map(|s| s * 1_000)
}

fn format_local(ms: u64, offset: UtcOffset) -> String {
    let Ok(at) = time::OffsetDateTime::from_unix_timestamp((ms / 1_000) as i64) else {
        return String::new();
    };
    let Some(at) = at.checked_to_offset(offset) else {
        return String::new();
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        at.year(),
        at.month() as u8,
        at.day(),
        at.hour(),
        at.minute()
    )
}

fn offset() -> UtcOffset {
    localtime::offset().unwrap_or(UtcOffset::UTC)
}

fn now_ms() -> u64 {
    u64::try_from(localtime::now()).unwrap_or(0) * 1_000
}

fn find(navigation: &NavigationSnapshot, id: Snowflake) -> Option<&ScheduledEvent> {
    navigation.events.iter().find(|event| event.id == id)
}

/// Whether the account may change this event (the reducer rechecks).
fn manages(navigation: &NavigationSnapshot, event: &ScheduledEvent) -> bool {
    let permissions = navigation.event_permissions;
    permissions.manage
        || (permissions.create
            && event.creator_id.is_some()
            && event.creator_id == navigation.current_user.as_ref().map(|u| u.id))
}

/// Voice and Stage channels the account sees, for an event of `kind`.
fn channels(navigation: &NavigationSnapshot, kind: EventKind) -> Vec<Choice> {
    navigation
        .channels
        .rows
        .iter()
        .filter(|row| {
            row.kind == ChannelKind::Voice
                && row.occupant.is_none()
                && row.enabled
                && row.stage == (kind == EventKind::Stage)
        })
        .map(|row| Choice {
            id: row.id,
            name: row.name.clone(),
        })
        .collect()
}

fn channel_name(navigation: &NavigationSnapshot, id: Snowflake) -> String {
    navigation
        .channels
        .rows
        .iter()
        .find(|row| row.id == id && row.occupant.is_none())
        .map_or_else(
            || tr("a channel you cannot see").to_owned(),
            |r| r.name.clone(),
        )
}

/// Whether joining `channel` for an event of the selected server is offered:
/// the event is held and not over, and the channel is one the account may
/// connect to. Checked again when the click arrives.
pub fn joinable(navigation: &NavigationSnapshot, guild: Snowflake, channel: Snowflake) -> bool {
    navigation.selected_guild == Some(guild)
        && navigation
            .events
            .iter()
            .any(|event| event.joinable_channel() == Some(channel))
        && navigation.channels.rows.iter().any(|row| {
            row.id == channel
                && row.occupant.is_none()
                && row.kind == ChannelKind::Voice
                && row.enabled
                && row.permissions.connect
        })
}

/// What a failed request tells the user.
fn failure(error: RestError, what: &str) -> String {
    crate::onboarding::failure(error, what)
}

fn draft_of(form: &Form) -> Result<EventDraft, String> {
    let start = parse_local(&form.start, offset()).ok_or(tr(
        "Write the start as YYYY-MM-DD HH:MM in your local time.",
    ))?;
    let end = match form.end.trim() {
        "" => None,
        raw => Some(parse_local(raw, offset()).ok_or(tr("Write the end as YYYY-MM-DD HH:MM."))?),
    };
    let draft = EventDraft {
        name: form.name.clone(),
        description: form.description.clone(),
        kind: form.kind,
        channel: form.channel,
        location: form.location.clone(),
        start_ms: start,
        end_ms: end,
    };
    // A new event, or an edit that moves the start, must start in the future.
    // (The form shows minutes only.)
    let future = form
        .editing
        .is_none_or(|(_, before)| before / 60_000 != start / 60_000);
    draft
        .check(now_ms(), future)
        .map_err(|e: DraftError| tr(e.message()).to_owned())?;
    Ok(draft)
}

impl State {
    /// Logout or account change.
    pub fn clear(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            generation: self.generation.wrapping_add(1),
            ..Self::default()
        };
    }

    /// A 401 stops the account like every other request.
    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Loaded(_, _, Err(RestError::AuthenticationRequired))
                | Event::Done(_, Err(RestError::AuthenticationRequired))
        )
    }

    fn fetch(&mut self, guild: Snowflake, client: &RestClient) -> Task<Event> {
        self.load = Load::Loading;
        let (client, generation) = (client.clone(), self.generation);
        Task::perform(
            async move { client.scheduled_events(guild).await },
            move |result| Event::Loaded(generation, guild, result),
        )
    }

    fn write(
        &mut self,
        guild: Snowflake,
        action: EventAction,
        bridge: &NavigationBridge,
    ) -> Task<Event> {
        if self.pending.is_some() {
            return Task::none();
        }
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        self.pending = Some(serial);
        self.notice = None;
        self.confirm = None;
        let bridge = bridge.clone();
        Task::perform(
            async move { bridge.scheduled_event(guild, action).await },
            move |result| Event::Done(serial, result),
        )
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        client: &RestClient,
        bridge: &NavigationBridge,
    ) -> Task<Event> {
        match event {
            Event::Open(guild) => {
                if self.guild == Some(guild) && !matches!(self.load, Load::Failed(_)) {
                    return Task::none();
                }
                // A write in flight survives a reopen: dropping it would let a
                // second Create through and lose the first one's answer.
                *self = Self {
                    guild: Some(guild),
                    hidden: std::mem::take(&mut self.hidden),
                    serial: self.serial,
                    generation: self.generation,
                    pending: self.pending,
                    ..Self::default()
                };
                self.fetch(guild, client)
            }
            Event::Close => {
                // A write in flight still ends in `Done`; the notice is dropped.
                self.guild = None;
                self.screen = Screen::List;
                Task::none()
            }
            Event::Reload => match self.guild {
                Some(guild) if !matches!(self.load, Load::Loading) => self.fetch(guild, client),
                _ => Task::none(),
            },
            Event::Loaded(answered, guild, result) => {
                if answered != self.generation
                    || self.guild != Some(guild)
                    || !matches!(self.load, Load::Loading)
                {
                    return Task::none();
                }
                match result {
                    Ok(events) => {
                        self.load = Load::Idle;
                        bridge.set_scheduled_events(guild, events);
                    }
                    Err(error) => self.load = Load::Failed(failure(error, "Nothing")),
                }
                Task::none()
            }
            Event::Show(id) => {
                self.screen = Screen::Detail(id);
                self.confirm = None;
                self.notice = None;
                Task::none()
            }
            Event::List => {
                self.screen = Screen::List;
                self.confirm = None;
                Task::none()
            }
            Event::Do(action) => match self.guild {
                Some(guild) => self.write(guild, action, bridge),
                None => Task::none(),
            },
            Event::Ask(what, id) => {
                if self.pending.is_none() {
                    self.confirm = Some((what, id));
                }
                Task::none()
            }
            Event::Keep => {
                self.confirm = None;
                Task::none()
            }
            Event::CopyLink(id) => match find(navigation, id) {
                Some(event) => iced::clipboard::write(event.link()),
                None => Task::none(),
            },
            // Handled by the application (the voice join path).
            Event::Join(..) => Task::none(),
            Event::New => {
                if self.guild.is_some() && self.pending.is_none() {
                    self.screen = Screen::Form(Box::new(Form {
                        editing: None,
                        name: String::new(),
                        description: String::new(),
                        kind: EventKind::Voice,
                        channel: None,
                        location: String::new(),
                        start: String::new(),
                        end: String::new(),
                        error: None,
                    }));
                    self.notice = None;
                }
                Task::none()
            }
            Event::Edit(id) => {
                if let Some(event) = find(navigation, id).filter(|e| !e.status.is_over())
                    && self.guild.is_some()
                    && self.pending.is_none()
                {
                    let offset = offset();
                    self.screen = Screen::Form(Box::new(Form {
                        editing: Some((id, event.start_ms)),
                        name: event.name.clone(),
                        description: event.description.clone(),
                        kind: event.kind,
                        channel: event.channel_id,
                        location: event.location.clone(),
                        start: format_local(event.start_ms, offset),
                        end: event
                            .end_ms
                            .map_or_else(String::new, |e| format_local(e, offset)),
                        error: None,
                    }));
                    self.notice = None;
                }
                Task::none()
            }
            Event::Input(field, value) => {
                if let Screen::Form(form) = &mut self.screen {
                    let (target, max) = match field {
                        Field::Name => (&mut form.name, MAX_NAME_CHARS),
                        Field::Description => (&mut form.description, MAX_DESCRIPTION_CHARS),
                        Field::Location => (&mut form.location, MAX_LOCATION_CHARS),
                        Field::Start => (&mut form.start, 16),
                        Field::End => (&mut form.end, 16),
                    };
                    *target = value.chars().take(max).collect();
                    form.error = None;
                }
                Task::none()
            }
            Event::Kind(kind) => {
                if let Screen::Form(form) = &mut self.screen
                    && form.kind != kind
                {
                    form.kind = kind;
                    // A voice channel is not a Stage and the other way round.
                    form.channel = None;
                    form.error = None;
                }
                Task::none()
            }
            Event::Channel(choice) => {
                if let Screen::Form(form) = &mut self.screen {
                    form.channel = Some(choice.id);
                    form.error = None;
                }
                Task::none()
            }
            Event::Save => {
                let Screen::Form(form) = &mut self.screen else {
                    return Task::none();
                };
                if self.pending.is_some() {
                    return Task::none();
                }
                match draft_of(form) {
                    Err(message) => {
                        form.error = Some(message);
                        Task::none()
                    }
                    Ok(draft) => {
                        let action = match form.editing {
                            Some((id, _)) => EventAction::Edit(id, draft),
                            None => EventAction::Create(draft),
                        };
                        match self.guild {
                            Some(guild) => self.write(guild, action, bridge),
                            None => Task::none(),
                        }
                    }
                }
            }
            Event::Done(serial, result) => {
                if self.pending != Some(serial) {
                    return Task::none();
                }
                self.pending = None;
                match result {
                    Ok(()) => {
                        if matches!(self.screen, Screen::Form(_)) {
                            self.screen = Screen::List;
                        }
                        self.notice =
                            Some(tr("Sent. It changes here when Discord confirms it.").to_owned());
                    }
                    Err(error) => {
                        let message = failure(error, "The change");
                        match &mut self.screen {
                            Screen::Form(form) => form.error = Some(message),
                            _ => self.notice = Some(message),
                        }
                    }
                }
                Task::none()
            }
            Event::Hide(id) => {
                if self.hidden.len() >= MAX_HIDDEN {
                    self.hidden.remove(0);
                }
                if !self.hidden.contains(&id) {
                    self.hidden.push(id);
                }
                Task::none()
            }
        }
    }
}

fn status_label(event: &ScheduledEvent) -> &'static str {
    match event.status {
        EventStatus::Scheduled => tr("Scheduled"),
        EventStatus::Active => tr("Live now"),
        EventStatus::Completed => tr("Ended"),
        EventStatus::Canceled => tr("Canceled"),
    }
}

fn kind_label(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Stage => tr("Stage"),
        EventKind::Voice => tr("Voice"),
        EventKind::External => tr("Elsewhere"),
    }
}

fn when(ms: u64, now: i64) -> String {
    let unix = (ms / 1_000) as i64;
    let at = localtime::markup(unix, TimeStyle::LongDateTime, localtime::offset(), now)
        .unwrap_or_default();
    let relative = localtime::markup(unix, TimeStyle::Relative, None, now).unwrap_or_default();
    format!("{at} ({relative})")
}

fn count(event: &ScheduledEvent) -> String {
    event.user_count.map_or_else(String::new, |n| {
        trf(" \u{b7} {} interested", &[&crate::invite::count(n)])
    })
}

/// The event about to start (or running) that the banner announces: a voice or
/// Stage event, live or due within [`SOON_MS`], that was not hidden.
fn soon<'a>(
    state: &State,
    navigation: &'a NavigationSnapshot,
    now_ms: u64,
) -> Option<&'a ScheduledEvent> {
    navigation.events.iter().find(|event| {
        event.joinable_channel().is_some()
            && !state.hidden.contains(&event.id)
            && match event.status {
                EventStatus::Active => true,
                EventStatus::Scheduled => {
                    event.start_ms <= now_ms + SOON_MS && now_ms <= event.start_ms + LATE_MS
                }
                _ => false,
            }
    })
}

/// The open panel, or the banner of an event about to start.
pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
) -> Option<Element<'a, Event>> {
    let Some(guild) = state.guild else {
        return banner(state, navigation);
    };
    if navigation.selected_guild != Some(guild) {
        return banner(state, navigation);
    }
    let busy = state.pending.is_some();
    let head = row![
        text(tr("Events")).size(13).width(Length::Fill),
        crate::icons::close((!busy).then_some(Event::Close)),
    ]
    .spacing(6);
    let body = match &state.screen {
        Screen::List => list_body(state, navigation, busy),
        Screen::Detail(id) => detail_body(state, navigation, *id, busy),
        Screen::Form(form) => form_body(form, navigation, busy),
    };
    Some(
        container(column![head, body].spacing(6))
            .max_width(720)
            .into(),
    )
}

fn banner<'a>(state: &'a State, navigation: &'a NavigationSnapshot) -> Option<Element<'a, Event>> {
    let guild = navigation.selected_guild?;
    let now = localtime::now();
    let event = soon(state, navigation, now_ms())?;
    let line = if event.status == EventStatus::Active {
        trf("Live now: \"{}\"", &[&event.name])
    } else {
        let relative = localtime::markup(
            (event.start_ms / 1_000) as i64,
            TimeStyle::Relative,
            None,
            now,
        )
        .unwrap_or_default();
        trf("Starting soon: \"{}\" {}", &[&event.name, &relative])
    };
    let mut actions = row![text(line).size(13).width(Length::Fill)]
        .spacing(8)
        .align_y(iced::Alignment::Center);
    if let Some(channel) = event.joinable_channel()
        && joinable(navigation, guild, channel)
    {
        actions =
            actions.push(button(text(tr("Join")).size(13)).on_press(Event::Join(guild, channel)));
    }
    actions = actions
        .push(button(text(tr("Details")).size(13)).on_press(Event::Open(guild)))
        .push(button(text(tr("Hide")).size(13)).on_press(Event::Hide(event.id)));
    Some(
        container(actions)
            .padding([4, 8])
            .style(container::rounded_box)
            .into(),
    )
}

fn list_body<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
    busy: bool,
) -> Element<'a, Event> {
    let permissions = navigation.event_permissions;
    let mut tools = row![].spacing(6);
    if permissions.manage || permissions.create {
        tools = tools.push(
            button(text(tr("New event")).size(13)).on_press_maybe((!busy).then_some(Event::New)),
        );
    }
    tools = tools.push(
        button(text(tr("Refresh")).size(13))
            .on_press_maybe((!matches!(state.load, Load::Loading)).then_some(Event::Reload)),
    );
    let mut list = column![].spacing(4);
    let now = localtime::now();
    for event in &navigation.events {
        list = list.push(
            button(
                column![
                    text(format!("{} \u{b7} {}", event.name, status_label(event))).size(13),
                    text(format!(
                        "{} \u{b7} {}{}",
                        when(event.start_ms, now),
                        kind_label(event.kind),
                        count(event)
                    ))
                    .size(11)
                    .style(text::secondary),
                ]
                .spacing(2),
            )
            .on_press(Event::Show(event.id))
            .width(Length::Fill),
        );
    }
    let mut out = column![tools].spacing(6);
    match &state.load {
        Load::Loading => out = out.push(text(tr("Loading\u{2026}")).size(12)),
        Load::Failed(error) => out = out.push(text(error.as_str()).size(12).style(text::danger)),
        Load::Idle => {}
    }
    if let Some(notice) = &state.notice {
        out = out.push(text(notice.as_str()).size(12));
    }
    if navigation.events.is_empty() && matches!(state.load, Load::Idle) {
        out = out.push(text(tr("No upcoming events.")).size(12));
    }
    out.push(scrollable(list).height(Length::Fixed(260.0)))
        .into()
}

fn detail_body<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
    id: Snowflake,
    busy: bool,
) -> Element<'a, Event> {
    let back = button(text(tr("All events")).size(13)).on_press(Event::List);
    let Some(event) = find(navigation, id) else {
        return column![text(tr("This event no longer exists.")).size(12), back,]
            .spacing(6)
            .into();
    };
    let now = localtime::now();
    let place = match (event.kind, event.channel_id) {
        (EventKind::External, _) => trf("At {}", &[&event.location]),
        (EventKind::Stage, Some(channel)) => {
            trf("Stage channel {}", &[&channel_name(navigation, channel)])
        }
        (_, Some(channel)) => trf("Voice channel {}", &[&channel_name(navigation, channel)]),
        (kind, None) => kind_label(kind).to_owned(),
    };
    let mut info = column![
        text(event.name.as_str()).size(15),
        text(format!("{}{}", status_label(event), count(event))).size(12),
        text(trf("Starts {}", &[&when(event.start_ms, now)])).size(12),
    ]
    .spacing(3);
    if let Some(end) = event.end_ms {
        info = info.push(text(trf("Ends {}", &[&when(end, now)])).size(12));
    }
    info = info.push(text(place).size(12));
    if let Some(repeat) = &event.recurrence {
        info = info.push(text(trf("Repeats: {}", &[repeat])).size(12));
    }
    if !event.creator.is_empty() {
        info = info.push(text(trf("By {}", &[&event.creator])).size(12));
    }
    if !event.description.is_empty() {
        info = info.push(text(event.description.as_str()).size(13));
    }

    let act = |label: &'static str, event: Event| {
        button(text(tr(label)).size(13)).on_press_maybe((!busy).then_some(event))
    };
    let mut actions = row![].spacing(6);
    if !event.status.is_over() && event.interested != Some(true) {
        actions = actions.push(act("Interested", Event::Do(EventAction::Interested(id))));
    }
    if event.interested != Some(false) {
        actions = actions.push(act(
            "Not interested",
            Event::Do(EventAction::NotInterested(id)),
        ));
    }
    actions = actions.push(button(text(tr("Copy link")).size(13)).on_press(Event::CopyLink(id)));
    if let (Some(channel), Some(guild)) = (event.joinable_channel(), state.guild)
        && joinable(navigation, guild, channel)
    {
        actions =
            actions.push(button(text(tr("Join")).size(13)).on_press(Event::Join(guild, channel)));
    }
    let mut out = column![back, info, actions].spacing(8);
    if manages(navigation, event) {
        let mut manage = row![].spacing(6);
        if !event.status.is_over() {
            manage = manage.push(act("Edit", Event::Edit(id)));
        }
        if event.status == EventStatus::Scheduled {
            manage = manage.push(act("Start now", Event::Do(EventAction::Start(id))));
            manage = manage.push(act("Cancel event", Event::Ask(Confirm::Cancel, id)));
        }
        if event.status == EventStatus::Active {
            manage = manage.push(act("End event", Event::Do(EventAction::End(id))));
        }
        manage = manage.push(act("Delete", Event::Ask(Confirm::Delete, id)));
        out = out.push(manage);
        if let Some((what, _)) = state.confirm.filter(|(_, target)| *target == id) {
            let (question, label, action) = match what {
                Confirm::Cancel => (
                    "Cancel this event? Interested members are told it was canceled.",
                    "Yes, cancel it",
                    EventAction::Cancel(id),
                ),
                Confirm::Delete => (
                    "Delete this event for good?",
                    "Yes, delete it",
                    EventAction::Delete(id),
                ),
            };
            out = out.push(
                row![
                    text(tr(question)).size(12).width(Length::Fill),
                    act(label, Event::Do(action)),
                    button(text(tr("Keep")).size(13)).on_press(Event::Keep),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center),
            );
        }
    }
    if busy {
        out = out.push(text(tr("Sending\u{2026}")).size(12));
    }
    if let Some(notice) = &state.notice {
        out = out.push(text(notice.as_str()).size(12));
    }
    scrollable(out).height(Length::Fixed(300.0)).into()
}

fn form_body<'a>(
    form: &'a Form,
    navigation: &'a NavigationSnapshot,
    busy: bool,
) -> Element<'a, Event> {
    let pick = |label: &'static str, kind: EventKind| {
        button(text(tr(label)).size(13))
            .on_press_maybe((!busy && form.editing.is_none()).then_some(Event::Kind(kind)))
            .style(if form.kind == kind {
                button::primary
            } else {
                button::secondary
            })
    };
    let input = |hint: &'static str, value: &'a str, field: Field| {
        text_input(tr(hint), value)
            .on_input(move |v| Event::Input(field, v))
            .size(13)
    };
    let mut out = column![
        text(if form.editing.is_some() {
            tr("Edit event")
        } else {
            tr("New event")
        })
        .size(13),
        input("Name", &form.name, Field::Name),
        input(
            "Description (optional)",
            &form.description,
            Field::Description
        ),
        row![
            pick("Voice", EventKind::Voice),
            pick("Stage", EventKind::Stage),
            pick("Somewhere else", EventKind::External),
        ]
        .spacing(6),
    ]
    .spacing(6);
    if form.kind == EventKind::External {
        out = out.push(input("Location", &form.location, Field::Location));
    } else {
        let options = channels(navigation, form.kind);
        let selected = options.iter().find(|c| Some(c.id) == form.channel).cloned();
        out = out.push(
            pick_list(options, selected, Event::Channel)
                .placeholder(tr("Channel"))
                .width(Length::Fill),
        );
    }
    out = out
        .push(input(
            "Starts: YYYY-MM-DD HH:MM (local time)",
            &form.start,
            Field::Start,
        ))
        .push(input(
            if form.kind == EventKind::External {
                "Ends: YYYY-MM-DD HH:MM (required)"
            } else {
                "Ends: YYYY-MM-DD HH:MM (optional)"
            },
            &form.end,
            Field::End,
        ));
    if let Some(error) = &form.error {
        out = out.push(text(error.as_str()).size(12).style(text::danger));
    }
    out.push(
        row![
            button(
                text(if busy {
                    tr("Sending\u{2026}")
                } else {
                    tr("Save")
                })
                .size(13)
            )
            .on_press_maybe((!busy).then_some(Event::Save)),
            button(text(tr("Back")).size(13)).on_press_maybe((!busy).then_some(Event::List)),
        ]
        .spacing(6),
    )
    .into()
}

#[cfg(test)]
mod tests;
