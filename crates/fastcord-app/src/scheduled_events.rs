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
//! typed using date-specific native timezone conversion on workers; ambiguous
//! or nonexistent DST times are refused. Unsupported platforms explicitly use UTC.

use crate::ui::{button, primary_button};
use fastcord_discord::state::navigation::{ChannelKind, NavigationSnapshot};
use fastcord_discord::{
    DraftError, EventAction, EventDraft, EventRecurrence, EventRecurrenceChange, RestClient,
    RestError,
};
use fastcord_model::markup::TimeStyle;
use fastcord_model::{
    EventKind, EventStatus, MAX_DESCRIPTION_CHARS, MAX_LOCATION_CHARS, MAX_NAME_CHARS,
    ScheduledEvent, Snowflake,
};
use iced::widget::{column, container, pick_list, row, scrollable, text, text_input};
use iced::{Element, Length, Task};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

use crate::gateway::NavigationBridge;
use crate::i18n::{tr, trf};
use crate::localtime;
use crate::panel_kit::auth_rejected;

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
    ShowUsers(Snowflake),
    NextUsers,
    FirstUsers,
    UsersLoaded(
        u64,
        Snowflake,
        Result<fastcord_discord::EventUsers, RestError>,
    ),
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
    Repeat(EventRecurrenceChange),
    Channel(Choice),
    Save,
    Prepared(u64, Snowflake, Result<EventAction, String>),
    Done(u64, Result<(), RestError>),
    /// Hides the banner of an event.
    Hide(Snowflake),
}

#[derive(Clone, Debug)]
struct Form {
    /// The event being edited and the start it had.
    editing: Option<(Snowflake, u64)>,
    original_end: Option<u64>,
    original_civil: Option<(String, String)>,
    recurrence: EventRecurrenceChange,
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
    Users(Snowflake),
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
    /// Unique ownership of the current list request, also advanced on close/reset.
    generation: u64,
    list_task: Option<iced::task::Handle>,
    notice: Option<String>,
    confirm: Option<(Confirm, Snowflake)>,
    hidden: Vec<Snowflake>,
    users: Option<fastcord_discord::EventUsers>,
    users_epoch: u64,
    users_loading: bool,
    users_task: Option<iced::task::Handle>,
    users_error: Option<String>,
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
            list_task: None,
            notice: None,
            confirm: None,
            hidden: Vec::new(),
            users: None,
            users_epoch: 0,
            users_loading: false,
            users_task: None,
            users_error: None,
        }
    }
}

/// Unix milliseconds of `YYYY-MM-DD HH:MM` in the given offset.
fn parse_civil(raw: &str) -> Option<PrimitiveDateTime> {
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
    Some(PrimitiveDateTime::new(date, clock))
}

#[cfg(test)]
fn parse_local(raw: &str, offset: UtcOffset) -> Option<u64> {
    let seconds = parse_civil(raw)?.assume_offset(offset).unix_timestamp();
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

// At most two timestamps per retained scheduled event (100 per guild).
type TimeOffsets = Vec<(u64, Option<i32>)>;
#[derive(Default)]
struct PublishedOffsets {
    owner: u64,
    entries: TimeOffsets,
}
static OFFSETS: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<PublishedOffsets>>> =
    std::sync::OnceLock::new();
fn offsets() -> &'static std::sync::Arc<std::sync::Mutex<PublishedOffsets>> {
    OFFSETS.get_or_init(Default::default)
}
/// Account-worker owned schedule; the UI only reads the published offsets.
pub(crate) struct TimeCache {
    entries: TimeOffsets,
    deadline: Option<std::time::Instant>,
    owner: u64,
    published: std::sync::Arc<std::sync::Mutex<PublishedOffsets>>,
}
impl TimeCache {
    #[cfg(test)]
    pub(crate) fn isolated() -> Self {
        Self::with_published(Default::default())
    }

    pub(crate) fn new() -> Self {
        Self::with_published(std::sync::Arc::clone(offsets()))
    }

    fn with_published(published: std::sync::Arc<std::sync::Mutex<PublishedOffsets>>) -> Self {
        let owner = {
            let mut current = published.lock().unwrap_or_else(|p| p.into_inner());
            current.owner = current.owner.wrapping_add(1);
            current.entries.clear();
            current.owner
        };
        Self {
            entries: Vec::new(),
            deadline: None,
            owner,
            published,
        }
    }

    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

    pub(crate) fn wait(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    pub(crate) fn prepare(&mut self, events: &[ScheduledEvent]) {
        let times = events
            .iter()
            .take(100)
            .flat_map(|e| std::iter::once(e.start_ms).chain(e.end_ms))
            .collect();
        self.update(times, std::time::Instant::now());
    }

    pub(crate) fn refresh_due(&mut self, now: std::time::Instant) -> bool {
        if self.deadline.is_none_or(|deadline| now < deadline) {
            return false;
        }
        self.update(self.entries.iter().map(|(ms, _)| *ms).collect(), now)
    }

    fn update(&mut self, times: Vec<u64>, now: std::time::Instant) -> bool {
        self.update_with(times, now, fastcord_platform::timezone::refresh, |ms| {
            i64::try_from(ms / 1000)
                .ok()
                .and_then(fastcord_platform::timezone::offset_at)
        })
    }

    fn update_with(
        &mut self,
        times: Vec<u64>,
        now: std::time::Instant,
        mut refresh: impl FnMut(),
        mut resolve: impl FnMut(u64) -> Option<i32>,
    ) -> bool {
        let due = self.deadline.is_none_or(|deadline| now >= deadline);
        let mut next = Vec::with_capacity(times.len().min(200));
        let mut refreshed = false;
        for ms in times.into_iter().take(200) {
            if next.iter().any(|(saved, _)| *saved == ms) {
                continue;
            }
            let saved = (!due)
                .then(|| self.entries.iter().find(|(saved, _)| *saved == ms))
                .flatten();
            let offset = if let Some((_, offset)) = saved {
                *offset
            } else {
                if !refreshed {
                    refresh();
                    refreshed = true;
                }
                resolve(ms)
            };
            next.push((ms, offset));
        }
        self.deadline = if next.is_empty() {
            None
        } else if due {
            Some(now + Self::INTERVAL)
        } else {
            self.deadline
        };
        let changed = next != self.entries;
        self.entries = next;
        if changed {
            let mut published = self.published.lock().unwrap_or_else(|p| p.into_inner());
            if published.owner == self.owner {
                published.entries = self.entries.clone();
            }
        }
        changed
    }
}
fn offset_for(ms: u64) -> Option<UtcOffset> {
    offsets()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entries
        .iter()
        .find(|(saved, _)| *saved == ms)
        .and_then(|(_, offset)| *offset)
        .and_then(|offset| UtcOffset::from_whole_seconds(offset).ok())
}
fn system_time(raw: &str) -> Result<u64, String> {
    use fastcord_platform::timezone::{Civil, LocalResult};
    let civil =
        parse_civil(raw).ok_or_else(|| tr("Write the time as YYYY-MM-DD HH:MM.").to_owned())?;
    let seconds = match fastcord_platform::timezone::resolve(Civil {
        year: civil.year(),
        month: civil.month() as u8,
        day: civil.day(),
        hour: civil.hour(),
        minute: civil.minute(),
    }) {
        LocalResult::Unique(seconds) => seconds,
        LocalResult::Unsupported => civil.assume_utc().unix_timestamp(),
        LocalResult::Ambiguous => {
            return Err(tr(
                "This local time occurs twice during the clock change. Choose an unambiguous time.",
            )
            .to_owned());
        }
        LocalResult::Missing => return Err(tr(
            "This local time does not exist or its timezone is unavailable. Choose another time.",
        )
        .to_owned()),
    };
    u64::try_from(seconds)
        .ok()
        .and_then(|s| s.checked_mul(1000))
        .ok_or_else(|| tr("The event time is out of range.").to_owned())
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
    // This function runs on the form-validation worker. Keep known instants
    // when their displayed civil text is unchanged, even if the OS timezone
    // changes while the form is open (or the text lies in a DST fold).
    fastcord_platform::timezone::refresh();
    let original = form.editing.filter(|_| {
        form.original_civil
            .as_ref()
            .is_some_and(|(start, _)| start == form.start.trim())
    });
    let start = match original {
        Some((_, before)) => before,
        None => system_time(&form.start)?,
    };
    let end = match form.end.trim() {
        "" => None,
        raw => Some(
            match form.original_end.filter(|_| {
                form.original_civil
                    .as_ref()
                    .is_some_and(|(_, end)| end == raw)
            }) {
                Some(before) => before,
                None => system_time(raw)?,
            },
        ),
    };
    let draft = EventDraft {
        name: form.name.clone(),
        description: form.description.clone(),
        kind: form.kind,
        channel: form.channel,
        location: form.location.clone(),
        start_ms: start,
        end_ms: end,
        recurrence: form.recurrence,
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
    /// The events of `selected`, the open server, are shown.
    pub fn is_open_for(&self, selected: Option<Snowflake>) -> bool {
        self.guild.is_some() && self.guild == selected
    }

    /// Logout or account change.
    pub fn clear(&mut self) {
        self.clear_list();
        self.clear_users();
        *self = Self {
            serial: self.serial.wrapping_add(1),
            users_epoch: self.users_epoch.wrapping_add(1),
            generation: self.generation,
            ..Self::default()
        };
    }

    /// Only an answer owned by this panel/account may reject its login. Check
    /// before update consumes the ownership marker, just like applying results.
    pub fn is_rejected(&self, event: &Event, navigation: &NavigationSnapshot) -> bool {
        match event {
            Event::Loaded(generation, guild, result) => {
                self.owns_list(*generation, *guild) && auth_rejected(result)
            }
            Event::Done(serial, result) => self.pending == Some(*serial) && auth_rejected(result),
            Event::UsersLoaded(epoch, event, result) => {
                self.owns_users(*epoch, *event, navigation) && auth_rejected(result)
            }
            _ => false,
        }
    }

    fn owns_list(&self, generation: u64, guild: Snowflake) -> bool {
        generation == self.generation
            && self.guild == Some(guild)
            && matches!(self.load, Load::Loading)
    }

    fn clear_list(&mut self) {
        if let Some(task) = self.list_task.take() {
            task.abort();
        }
        self.generation = self.generation.wrapping_add(1);
        self.load = Load::Idle;
    }

    fn owns_users(&self, epoch: u64, event: Snowflake, navigation: &NavigationSnapshot) -> bool {
        epoch == self.users_epoch
            && self.users_loading
            && matches!(self.screen, Screen::Users(open) if open == event)
            && self.users_visible(navigation, event)
    }

    fn clear_users(&mut self) {
        if let Some(task) = self.users_task.take() {
            task.abort();
        }
        self.users_epoch = self.users_epoch.wrapping_add(1);
        self.users = None;
        self.users_loading = false;
        self.users_error = None;
    }

    fn users_visible(&self, navigation: &NavigationSnapshot, event: Snowflake) -> bool {
        self.guild == navigation.selected_guild
            && find(navigation, event).is_some()
            && navigation.readable_events.contains(&event)
    }

    pub fn sync_users(&mut self, navigation: &NavigationSnapshot) {
        if let Screen::Users(event) = self.screen
            && !self.users_visible(navigation, event)
        {
            self.clear_users();
        }
    }

    fn fetch_users(
        &mut self,
        navigation: &NavigationSnapshot,
        client: &RestClient,
        event: Snowflake,
        after: Option<Snowflake>,
    ) -> Task<Event> {
        if self.users_loading || !self.users_visible(navigation, event) {
            return Task::none();
        }
        let Some(guild) = self.guild else {
            return Task::none();
        };
        self.clear_users();
        self.screen = Screen::Users(event);
        self.users_loading = true;
        let epoch = self.users_epoch;
        let client = client.clone();
        let (task, handle) = Task::perform(
            async move { client.scheduled_event_users(guild, event, after).await },
            move |result| Event::UsersLoaded(epoch, event, result),
        )
        .abortable();
        self.users_task = Some(handle.abort_on_drop());
        task
    }

    fn fetch(&mut self, guild: Snowflake, client: &RestClient) -> Task<Event> {
        self.clear_list();
        self.load = Load::Loading;
        let (client, generation) = (client.clone(), self.generation);
        let (task, handle) = Task::perform(
            async move { client.scheduled_events(guild).await },
            move |result| Event::Loaded(generation, guild, result),
        )
        .abortable();
        self.list_task = Some(handle.abort_on_drop());
        task
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
                self.clear_list();
                self.clear_users();
                // A write in flight survives a reopen: dropping it would let a
                // second Create through and lose the first one's answer.
                *self = Self {
                    guild: Some(guild),
                    hidden: std::mem::take(&mut self.hidden),
                    serial: self.serial,
                    generation: self.generation,
                    users_epoch: self.users_epoch.wrapping_add(1),
                    pending: self.pending,
                    ..Self::default()
                };
                self.fetch(guild, client)
            }
            Event::Close => {
                self.clear_list();
                self.clear_users();
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
                if !self.owns_list(answered, guild) {
                    return Task::none();
                }
                self.list_task = None;
                match result {
                    Ok(events) => {
                        self.load = Load::Idle;
                        bridge.set_scheduled_events(guild, events);
                    }
                    Err(error) => self.load = Load::Failed(failure(error, "Nothing")),
                }
                Task::none()
            }
            Event::ShowUsers(id) => self.fetch_users(navigation, client, id, None),
            Event::FirstUsers => match self.screen {
                Screen::Users(id) => self.fetch_users(navigation, client, id, None),
                _ => Task::none(),
            },
            Event::NextUsers => {
                if let Screen::Users(id) = self.screen
                    && let Some(next) = self.users.as_ref().and_then(|page| page.next)
                {
                    self.fetch_users(navigation, client, id, Some(next))
                } else {
                    Task::none()
                }
            }
            Event::UsersLoaded(epoch, id, result) => {
                if epoch != self.users_epoch
                    || !self.users_loading
                    || !matches!(self.screen, Screen::Users(open) if open == id)
                {
                    return Task::none();
                }
                if !self.users_visible(navigation, id) {
                    self.clear_users();
                    return Task::none();
                }
                self.users_task = None;
                self.users_loading = false;
                match result {
                    Ok(page) => self.users = Some(page),
                    Err(error) => self.users_error = Some(failure(error, "Nothing")),
                }
                Task::none()
            }
            Event::Show(id) => {
                self.clear_users();
                self.screen = Screen::Detail(id);
                self.confirm = None;
                self.notice = None;
                Task::none()
            }
            Event::List => {
                self.clear_users();
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
                    self.clear_users();
                    self.screen = Screen::Form(Box::new(Form {
                        editing: None,
                        original_end: None,
                        original_civil: None,
                        recurrence: EventRecurrenceChange::Clear,
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
                    if fastcord_platform::timezone::supported()
                        && (offset_for(event.start_ms).is_none()
                            || event.end_ms.is_some_and(|end| offset_for(end).is_none()))
                    {
                        self.notice = Some(tr("The event timezone is unavailable; editing is disabled until it can be resolved.").to_owned());
                        return Task::none();
                    }
                    self.clear_users();
                    let start = format_local(
                        event.start_ms,
                        offset_for(event.start_ms).unwrap_or(UtcOffset::UTC),
                    );
                    let end = event.end_ms.map_or_else(String::new, |e| {
                        format_local(e, offset_for(e).unwrap_or(UtcOffset::UTC))
                    });
                    self.screen = Screen::Form(Box::new(Form {
                        editing: Some((id, event.start_ms)),
                        original_end: event.end_ms,
                        original_civil: Some((start.clone(), end.clone())),
                        recurrence: EventRecurrenceChange::Keep,
                        name: event.name.clone(),
                        description: event.description.clone(),
                        kind: event.kind,
                        channel: event.channel_id,
                        location: event.location.clone(),
                        start,
                        end,
                        error: None,
                    }));
                    self.notice = None;
                }
                Task::none()
            }
            Event::Input(field, value) => {
                if let Screen::Form(form) = &mut self.screen
                    && self.pending.is_none()
                {
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
            Event::Repeat(recurrence) => {
                if let Screen::Form(form) = &mut self.screen
                    && self.pending.is_none()
                    && self.guild == navigation.selected_guild
                    && recurrence_editable(form, navigation)
                {
                    form.recurrence = recurrence;
                    form.error = None;
                }
                Task::none()
            }
            Event::Kind(kind) => {
                if let Screen::Form(form) = &mut self.screen
                    && self.pending.is_none()
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
                if let Screen::Form(form) = &mut self.screen
                    && self.pending.is_none()
                {
                    form.channel = Some(choice.id);
                    form.error = None;
                }
                Task::none()
            }
            Event::Save => {
                let Screen::Form(form) = &self.screen else {
                    return Task::none();
                };
                let Some(guild) = self.guild else {
                    return Task::none();
                };
                if self.pending.is_some() {
                    return Task::none();
                }
                let form = (**form).clone();
                self.serial = self.serial.wrapping_add(1);
                let serial = self.serial;
                self.pending = Some(serial);
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            draft_of(&form).map(|draft| match form.editing {
                                Some((id, _)) => EventAction::Edit(id, draft),
                                None => EventAction::Create(draft),
                            })
                        })
                        .await
                        .unwrap_or_else(|_| Err("The timezone worker stopped.".to_owned()))
                    },
                    move |result| Event::Prepared(serial, guild, result),
                )
            }
            Event::Prepared(serial, guild, result) => {
                if self.pending != Some(serial) {
                    return Task::none();
                }
                self.pending = None;
                if self.guild != Some(guild) || !matches!(self.screen, Screen::Form(_)) {
                    return Task::none();
                }
                match result {
                    Ok(action) => self.write(guild, action, bridge),
                    Err(error) => {
                        if let Screen::Form(form) = &mut self.screen {
                            form.error = Some(error);
                        }
                        Task::none()
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
                            self.clear_users();
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
    let offset = offset_for(ms);
    let mut at = localtime::markup(unix, TimeStyle::LongDateTime, offset, now).unwrap_or_default();
    if offset.is_none() {
        at.push_str(" UTC");
    }
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
        Screen::Users(id) => users_body(state, navigation, *id),
        Screen::Form(form) => form_body(form, navigation, busy),
    };
    Some(
        container(column![head, body].spacing(6).width(Length::Fill))
            .width(Length::Fill)
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
        actions = actions
            .push(primary_button(text(tr("Join")).size(13)).on_press(Event::Join(guild, channel)));
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
            primary_button(text(tr("New event")).size(13))
                .on_press_maybe((!busy).then_some(Event::New)),
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
                    .color(crate::style::muted()),
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
        actions = actions
            .push(primary_button(text(tr("Join")).size(13)).on_press(Event::Join(guild, channel)));
    }
    if state.users_visible(navigation, id) {
        actions = actions
            .push(button(text(tr("Interested members")).size(13)).on_press(Event::ShowUsers(id)));
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

fn users_body<'a>(
    state: &'a State,
    navigation: &NavigationSnapshot,
    id: Snowflake,
) -> Element<'a, Event> {
    let mut out = column![
        button(text(tr("Back to event"))).on_press(Event::Show(id)),
        text(tr("Interested members"))
    ]
    .spacing(6)
    .width(Length::Fill);
    if !state.users_visible(navigation, id) {
        return out
            .push(text(tr("This event is no longer accessible.")))
            .into();
    }
    let mut members = column![].spacing(6).width(Length::Fill);
    if state.users_loading {
        members = members.push(text(tr("Loading…")));
    }
    if let Some(error) = &state.users_error {
        members = members.push(text(error.as_str()).style(text::danger));
    }
    if let Some(page) = &state.users {
        if page.rows.is_empty() {
            members = members.push(text(tr("No interested members on this page.")));
        }
        for user in &page.rows {
            members = members.push(
                text(format!("{} · {}", user.name, user.id))
                    .size(13)
                    .width(Length::Fill),
            );
        }
    }
    out = out.push(
        scrollable(members)
            .height(Length::Fixed(260.0))
            .width(Length::Fill),
    );
    // Paging stays reachable even with fifty rows; it does not scroll away
    // with the names, and replacing a page starts its member list at the top.
    out.push(
        row![
            button(text(tr("First page / refresh")))
                .on_press_maybe((!state.users_loading).then_some(Event::FirstUsers)),
            button(text(tr("Next"))).on_press_maybe(
                (!state.users_loading && state.users.as_ref().is_some_and(|p| p.next.is_some()))
                    .then_some(Event::NextUsers)
            ),
        ]
        .spacing(8),
    )
    .into()
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
            if fastcord_platform::timezone::supported() {
                "Starts: YYYY-MM-DD HH:MM (local time)"
            } else {
                "Starts: YYYY-MM-DD HH:MM (UTC)"
            },
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
    out = out.push(recurrence_controls(form, navigation, busy));
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

/// Recurrence controls change the whole series, never an individual occurrence.
fn recurrence_editable(form: &Form, navigation: &NavigationSnapshot) -> bool {
    match form.editing {
        Some((id, _)) => find(navigation, id).is_some_and(|event| {
            navigation.selected_guild == Some(event.guild_id)
                && !event.status.is_over()
                && manages(navigation, event)
        }),
        None => navigation.event_permissions.manage || navigation.event_permissions.create,
    }
}

fn recurrence_controls<'a>(
    form: &'a Form,
    navigation: &'a NavigationSnapshot,
    busy: bool,
) -> Element<'a, Event> {
    let editable = !busy && recurrence_editable(form, navigation);
    let choice = |label: &'static str, recurrence| {
        button(text(tr(label)).size(12))
            .style(if form.recurrence == recurrence {
                button::primary
            } else {
                button::secondary
            })
            .on_press_maybe(editable.then_some(Event::Repeat(recurrence)))
    };
    let mut out = column![].spacing(6);
    if let Some((id, _)) = form.editing {
        out = out.push(text(tr("Repeat changes apply to the whole series.")).size(12));
        if let Some(event) = find(navigation, id) {
            let rule = event.recurrence.as_deref().unwrap_or_else(|| tr("Once"));
            out = out.push(text(trf("Current repeat: {}", &[&rule])).size(12));
        }
        out = out.push(choice("Keep current repeat", EventRecurrenceChange::Keep));
    }
    out = out.push(
        row![
            choice("Once", EventRecurrenceChange::Clear),
            choice(
                "Weekly (UTC)",
                EventRecurrenceChange::Replace(EventRecurrence::Weekly)
            ),
            choice(
                "Every other week (UTC)",
                EventRecurrenceChange::Replace(EventRecurrence::EveryOtherWeek)
            ),
        ]
        .spacing(6),
    );
    if matches!(form.recurrence, EventRecurrenceChange::Replace(_)) {
        out = out.push(text(tr("Repeats at the same UTC time and weekday as the first occurrence. Local times may shift when daylight saving time changes.")).size(12));
    }
    if form.editing.is_some() && form.recurrence != EventRecurrenceChange::Keep {
        out = out.push(
            text(tr(
                "This replaces the current repeat rule for the entire event series.",
            ))
            .size(12),
        );
    }
    out.into()
}

#[cfg(test)]
mod tests;
