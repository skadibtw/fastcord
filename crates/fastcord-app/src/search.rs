//! Server-backed message search: the header search box, its filters, and the
//! bounded results list. Discord does the searching; this keeps at most
//! [`MAX_HITS`] previews (and [`MAX_RESULT_BYTES`] of text), never message
//! bodies, and nothing is stored on disk. Opening a hit is a normal
//! `history::Intent::JumpTo`, which loads the exact message context.

use crate::i18n::{tr, trf, trn};
use std::time::{Duration, Instant};

use crate::ui::button;
use fastcord_discord::{
    RestClient, RestError, SEARCH_PAGE_SIZE, SearchHas, SearchOutcome, SearchQuery, SearchScope,
};
use fastcord_model::{Message, Snowflake};
use iced::widget::{checkbox, column, container, row, scrollable, text, text_input};
use iced::{Color, Element, Length, Task};

pub const MAX_HITS: usize = 100;
pub const MAX_RESULT_BYTES: usize = 128 * 1024;
const PREVIEW_CHARS: usize = 200;
const AUTHOR_CHARS: usize = 64;
/// Index-not-ready answers followed before giving up.
const MAX_INDEX_WAITS: u8 = 5;
/// A jump into another channel waits this long for that channel to open.
const JUMP_PATIENCE: Duration = Duration::from_secs(10);

// Discord-style dark palette.
const PANEL: Color = crate::style::SIDEBAR;
const ROW: Color = crate::style::CHAT;
const MUTED: Color = crate::style::SUBTLE;
const WARN: Color = crate::style::RED;

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    Input(String),
    Submit,
    More,
    WholeServer(bool),
    CycleHas,
    FromMe(bool),
    Clear,
    Jump(usize),
    Done(u64, Result<SearchOutcome, RestError>),
    Retry(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Loading,
    /// Discord is still indexing; the next ask comes after its delay.
    Indexing(u8),
    Failed(&'static str),
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub guild: Option<Snowflake>,
    pub channel: Snowflake,
    pub message: Snowflake,
    pub author: String,
    pub date: String,
    pub preview: String,
    pub attachments: usize,
}

impl Hit {
    fn new(scope: SearchScope, message: &Message) -> Self {
        let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
        Self {
            guild: match scope {
                SearchScope::Guild(id) => Some(id),
                SearchScope::Channel(_) => None,
            },
            channel: message.channel_id,
            message: message.id,
            author: clip(message.author.display_name(), AUTHOR_CHARS),
            date: clip(&message.timestamp, 10),
            preview: clip(&message.content, PREVIEW_CHARS),
            attachments: message.attachments.len(),
        }
    }

    fn bytes(&self) -> usize {
        64 + self.author.len() + self.date.len() + self.preview.len()
    }
}

/// What the app knows about the open conversation.
pub struct Context<'a> {
    pub client: &'a RestClient,
    pub account: Snowflake,
    pub guild: Option<Snowflake>,
    pub channel: Snowflake,
}

pub struct State {
    pub open: bool,
    input: String,
    whole_server: bool,
    has: Option<SearchHas>,
    from_me: bool,
    /// Identifies the current query; answers of older ones find another serial.
    serial: u64,
    query: Option<SearchQuery>,
    status: Status,
    hits: Vec<Hit>,
    total: u32,
    bytes: usize,
    /// Dropping this aborts the request in flight (cancellation).
    task: Option<iced::task::Handle>,
    pending_jump: Option<(Snowflake, Snowflake, Instant)>,
    /// A pending jump gave up waiting for its channel (never opened).
    jump_expired: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            open: false,
            input: String::new(),
            whole_server: false,
            has: None,
            from_me: false,
            serial: 0,
            query: None,
            status: Status::Idle,
            hits: Vec::new(),
            total: 0,
            bytes: 0,
            task: None,
            pending_jump: None,
            jump_expired: false,
        }
    }
}

const HAS_CYCLE: [Option<SearchHas>; 4] = [
    None,
    Some(SearchHas::Image),
    Some(SearchHas::File),
    Some(SearchHas::Link),
];

fn has_label(has: Option<SearchHas>) -> &'static str {
    match has {
        None => tr("Any content"),
        Some(SearchHas::Image) => tr("Has image"),
        Some(SearchHas::File) => tr("Has file"),
        Some(SearchHas::Link) => tr("Has link"),
        Some(_) => tr("Has other"),
    }
}

fn describe(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied => tr("You cannot search here."),
        RestError::ResourceGone => tr("This conversation is no longer available."),
        RestError::AuthenticationRequired => tr("Discord no longer accepts this login."),
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            tr("Discord is rate limiting searches. Try again shortly.")
        }
        RestError::Retryable(_) | RestError::Http(_) => {
            tr("Search failed on Discord's side. Try again.")
        }
        _ => tr("Search results could not be read."),
    }
}

impl State {
    /// Empties the panel for another account; the serial moves on, so an
    /// answer to a request of the previous one is never taken (#169).
    pub fn clear(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            ..Self::default()
        };
    }

    pub fn hit(&self, index: usize) -> Option<&Hit> {
        self.hits.get(index)
    }

    pub fn set_pending_jump(&mut self, channel: Snowflake, message: Snowflake) {
        self.pending_jump = Some((channel, message, Instant::now()));
    }

    /// The jump waiting for its channel, once `open_channel` is it.
    pub fn take_pending_jump(&mut self, open_channel: Option<Snowflake>) -> Option<Snowflake> {
        let (channel, message, since) = self.pending_jump?;
        if since.elapsed() > JUMP_PATIENCE {
            self.pending_jump = None;
            self.jump_expired = true;
        } else if open_channel == Some(channel) {
            self.pending_jump = None;
            return Some(message);
        }
        None
    }

    /// Whether a jump gave up since this was last asked (said once).
    pub fn take_jump_expired(&mut self) -> bool {
        std::mem::take(&mut self.jump_expired)
    }

    fn cancel(&mut self) {
        self.serial = self.serial.wrapping_add(1);
        self.task = None;
    }

    fn page_task(&mut self, client: &RestClient) -> Task<Event> {
        let Some(query) = self.query.clone() else {
            return Task::none();
        };
        let client = client.clone();
        let serial = self.serial;
        self.status = Status::Loading;
        let (task, handle) = Task::perform(
            async move { client.search_messages(&query).await },
            move |result| Event::Done(serial, result),
        )
        .abortable();
        self.task = Some(handle.abort_on_drop());
        task
    }

    fn start(&mut self, context: &Context<'_>) -> Task<Event> {
        self.cancel();
        self.hits.clear();
        self.bytes = 0;
        self.total = 0;
        let scope = match context.guild {
            Some(guild) => SearchScope::Guild(guild),
            None => SearchScope::Channel(context.channel),
        };
        let mut query = SearchQuery::new(scope, self.input.clone());
        query.has = self.has;
        query.author = self.from_me.then_some(context.account);
        if context.guild.is_some() && !self.whole_server {
            query.channel = Some(context.channel);
        }
        if !query.is_sendable() {
            self.query = None;
            self.status = Status::Failed("Type something to search for.");
            return Task::none();
        }
        self.query = Some(query);
        self.page_task(context.client)
    }

    pub fn update(&mut self, event: Event, context: &Context<'_>) -> Task<Event> {
        match event {
            Event::Toggle => {
                self.open = !self.open;
                if !self.open {
                    self.cancel();
                    self.status = Status::Idle;
                }
            }
            Event::Input(value) => {
                self.input = value
                    .chars()
                    .take(fastcord_discord::MAX_SEARCH_CHARS)
                    .collect();
            }
            Event::WholeServer(on) => self.whole_server = on,
            Event::FromMe(on) => self.from_me = on,
            Event::CycleHas => {
                let at = HAS_CYCLE.iter().position(|h| *h == self.has).unwrap_or(0);
                self.has = HAS_CYCLE[(at + 1) % HAS_CYCLE.len()];
            }
            Event::Submit => return self.start(context),
            Event::Clear => {
                self.cancel();
                *self = Self {
                    open: true,
                    serial: self.serial,
                    ..Self::default()
                };
            }
            Event::More => {
                if matches!(self.status, Status::Done)
                    && let Some(query) = &mut self.query
                    && self.hits.len() < MAX_HITS
                    && self.bytes < MAX_RESULT_BYTES
                    && (self.hits.len() as u32) < self.total
                {
                    // Discord pages by match count, not by what survived filtering.
                    query.offset = query.offset.saturating_add(SEARCH_PAGE_SIZE as u32);
                    self.cancel();
                    return self.page_task(context.client);
                }
            }
            Event::Done(serial, result) if serial == self.serial => {
                self.task = None;
                match result {
                    Ok(SearchOutcome::Page(page)) => {
                        let scope = self.query.as_ref().map(|q| q.scope);
                        for message in &page.hits {
                            let Some(scope) = scope else { break };
                            let hit = Hit::new(scope, message);
                            if self.hits.len() >= MAX_HITS
                                || self.bytes + hit.bytes() > MAX_RESULT_BYTES
                            {
                                break;
                            }
                            self.bytes += hit.bytes();
                            self.hits.push(hit);
                        }
                        self.total = page.total;
                        self.status = Status::Done;
                    }
                    Ok(SearchOutcome::IndexNotReady { retry_after }) => {
                        let waits = match self.status {
                            Status::Indexing(n) => n + 1,
                            _ => 1,
                        };
                        if waits > MAX_INDEX_WAITS {
                            self.status =
                                Status::Failed("Discord is still indexing this conversation.");
                        } else {
                            self.status = Status::Indexing(waits);
                            let serial = self.serial;
                            let (task, handle) = Task::perform(
                                async move { tokio::time::sleep(retry_after).await },
                                move |()| Event::Retry(serial),
                            )
                            .abortable();
                            self.task = Some(handle.abort_on_drop());
                            return task;
                        }
                    }
                    Err(error) => self.status = Status::Failed(describe(error)),
                }
            }
            Event::Retry(serial) if serial == self.serial => {
                // Keep the wait counter: the request is the same query again.
                let waits = self.status.clone();
                let task = self.page_task(context.client);
                if let Status::Indexing(_) = waits {
                    self.status = waits;
                }
                return task;
            }
            // Answers of replaced or cancelled queries.
            Event::Done(..) | Event::Retry(_) => {}
            Event::Jump(_) => {}
        }
        Task::none()
    }
}

fn panel(content: impl Into<Element<'static, Event>>) -> Element<'static, Event> {
    container(content)
        .padding(10)
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(PANEL.into()),
            border: iced::Border {
                radius: 6.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

/// The header search control; `guild` is whether the conversation is a server.
pub fn view(state: &State, in_guild: bool) -> Element<'_, Event> {
    if !state.open {
        return button(text(tr("Search")).size(13))
            .on_press(Event::Toggle)
            .into();
    }
    let mut controls = row![
        text_input(tr("Search messages"), &state.input)
            .on_input(Event::Input)
            .on_submit(Event::Submit)
            .width(Length::Fill),
        button(text(tr("Search")).size(13)).on_press(Event::Submit),
        button(text(has_label(state.has)).size(13))
            .on_press(Event::CycleHas)
            .padding([4, 10])
            .style(crate::style::secondary_button(false)),
        checkbox(state.from_me)
            .label(tr("From me"))
            .on_toggle(Event::FromMe),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);
    if in_guild {
        controls = controls.push(
            checkbox(state.whole_server)
                .label(tr("Whole server"))
                .on_toggle(Event::WholeServer),
        );
    }
    controls = controls
        .push(crate::icons::secondary(tr("Clear"), Event::Clear))
        .push(crate::icons::tool(
            crate::icons::Icon::Close,
            tr("Close search"),
            Event::Toggle,
        ));
    let status: Element<'_, Event> = match &state.status {
        Status::Idle => text("").size(12).into(),
        Status::Loading => text(tr("Searching…")).size(12).color(MUTED).into(),
        Status::Indexing(n) => text(trf(
            "Discord is indexing this conversation; retrying ({}/{})…",
            &[&n, &MAX_INDEX_WAITS],
        ))
        .size(12)
        .color(MUTED)
        .into(),
        Status::Failed(reason) => text(tr(reason)).size(12).color(WARN).into(),
        Status::Done if state.hits.is_empty() => {
            text(tr("No messages found.")).size(12).color(MUTED).into()
        }
        Status::Done => text(trf("{} of {} results", &[&state.hits.len(), &state.total]))
            .size(12)
            .color(MUTED)
            .into(),
    };
    let mut list = column![].spacing(4);
    for (index, hit) in state.hits.iter().enumerate() {
        let attachments = if hit.attachments > 0 {
            trn(
                hit.attachments as u64,
                " [{} attachment]",
                " [{} attachments]",
            )
        } else {
            String::new()
        };
        list = list.push(
            button(
                column![
                    text(format!("{} · {}", hit.author, hit.date))
                        .size(12)
                        .color(MUTED),
                    text(format!("{}{attachments}", hit.preview)).size(14),
                ]
                .spacing(2),
            )
            .width(Length::Fill)
            .style(|_, _| button::Style {
                background: Some(ROW.into()),
                text_color: Color::WHITE,
                ..Default::default()
            })
            .on_press(Event::Jump(index)),
        );
    }
    let mut body = column![controls, status].spacing(6);
    if !state.hits.is_empty() {
        body = body.push(scrollable(list).height(Length::Fixed(220.0)));
        if matches!(state.status, Status::Done)
            && state.hits.len() < MAX_HITS
            && state.bytes < MAX_RESULT_BYTES
            && (state.hits.len() as u32) < state.total
        {
            body = body.push(button(text(tr("Load more")).size(13)).on_press(Event::More));
        }
    }
    panel(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> RestClient {
        use fastcord_discord::UserToken;
        use fastcord_discord::gateway::{ClientProperties, HostOs};
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-search-credential".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn message(id: u64, text: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "channel_id": "500", "content": text,
            "timestamp": "2026-10-07T12:00:00.000000+00:00",
            "author": {"id": "2", "username": "alt"},
        }))
        .unwrap()
    }

    fn typed(text: &str) -> State {
        State {
            input: text.into(),
            ..State::default()
        }
    }

    fn page(total: u32, messages: Vec<Message>) -> Result<SearchOutcome, RestError> {
        Ok(SearchOutcome::Page(fastcord_discord::SearchPage {
            total,
            hits: messages,
        }))
    }

    fn ctx(client: &RestClient) -> Context<'_> {
        Context {
            client,
            account: Snowflake(2),
            guild: Some(Snowflake(7)),
            channel: Snowflake(500),
        }
    }

    #[test]
    fn answers_of_replaced_queries_are_ignored() {
        let client = client();
        let mut state = typed("needle");
        let _ = state.start(&ctx(&client));
        let old = state.serial;
        let _ = state.start(&ctx(&client));
        assert_ne!(old, state.serial);
        let _ = state.update(
            Event::Done(old, page(1, vec![message(1, "x")])),
            &ctx(&client),
        );
        assert!(state.hits.is_empty());
        assert_eq!(state.status, Status::Loading);
        let _ = state.update(
            Event::Done(state.serial, page(1, vec![message(1, "x")])),
            &ctx(&client),
        );
        assert_eq!(state.hits.len(), 1);
        assert_eq!(state.status, Status::Done);
    }

    #[test]
    fn previews_and_totals_are_bounded() {
        let client = client();
        let mut state = typed("x");
        let _ = state.start(&ctx(&client));
        let long = "é".repeat(5_000);
        let many: Vec<Message> = (1..=25).map(|i| message(i, &long)).collect();
        for _ in 0..8 {
            let serial = state.serial;
            let _ = state.update(
                Event::Done(serial, page(9_999, many.clone())),
                &ctx(&client),
            );
            state.status = Status::Done;
        }
        assert!(state.hits.len() <= MAX_HITS);
        assert!(state.bytes <= MAX_RESULT_BYTES);
        assert!(
            state
                .hits
                .iter()
                .all(|h| h.preview.chars().count() <= PREVIEW_CHARS)
        );
    }

    #[test]
    fn indexing_waits_then_gives_up_visibly() {
        let client = client();
        let mut state = typed("x");
        let _ = state.start(&ctx(&client));
        let not_ready = || {
            Ok(SearchOutcome::IndexNotReady {
                retry_after: Duration::from_secs(2),
            })
        };
        for n in 1..=MAX_INDEX_WAITS {
            let _ = state.update(Event::Done(state.serial, not_ready()), &ctx(&client));
            assert_eq!(state.status, Status::Indexing(n));
            assert!(state.task.is_some());
        }
        let _ = state.update(Event::Done(state.serial, not_ready()), &ctx(&client));
        assert!(matches!(state.status, Status::Failed(_)));
    }

    #[test]
    fn errors_are_visible_and_empty_queries_are_not_sent() {
        let client = client();
        let mut state = State::default();
        let _ = state.update(Event::Submit, &ctx(&client));
        assert!(matches!(state.status, Status::Failed(_)));
        state.input = "x".into();
        let _ = state.start(&ctx(&client));
        let _ = state.update(
            Event::Done(state.serial, Err(RestError::PermissionDenied)),
            &ctx(&client),
        );
        assert_eq!(state.status, Status::Failed("You cannot search here."));
    }

    #[test]
    fn a_jump_waits_for_its_channel_and_then_fires_once() {
        let mut state = State::default();
        state.set_pending_jump(Snowflake(600), Snowflake(9));
        assert_eq!(state.take_pending_jump(Some(Snowflake(500))), None);
        assert_eq!(
            state.take_pending_jump(Some(Snowflake(600))),
            Some(Snowflake(9))
        );
        assert_eq!(state.take_pending_jump(Some(Snowflake(600))), None);
        assert!(!state.take_jump_expired(), "it fired, it did not expire");
    }

    #[test]
    fn a_jump_whose_channel_never_opens_expires_once() {
        let mut state = State::default();
        let Some(long_ago) = Instant::now().checked_sub(JUMP_PATIENCE + Duration::from_secs(1))
        else {
            return; // a clock too young to subtract from
        };
        state.pending_jump = Some((Snowflake(600), Snowflake(9), long_ago));
        assert_eq!(state.take_pending_jump(Some(Snowflake(500))), None);
        assert!(state.take_jump_expired());
        assert!(!state.take_jump_expired(), "said once");
        assert_eq!(state.take_pending_jump(Some(Snowflake(600))), None);
    }

    #[test]
    fn clearing_for_another_account_never_reuses_a_serial() {
        let mut state = State::default();
        state.cancel();
        let old = state.serial;
        state.clear();
        assert_ne!(state.serial, old);
        assert_ne!(state.serial, 0, "an answer for serial 0 could still arrive");
    }
}
