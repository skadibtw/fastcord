//! The unified inbox (issue #77): one pane over every unread conversation and
//! recent mention.
//!
//! "Unread" is the reducer's own read table (`navigation.inbox`), so it always
//! agrees with the channel badges and with other clients' acknowledgements;
//! nothing here caches it. "Mentions" is Discord's mention list
//! (`GET /users/@me/mentions`), read only when the user opens that tab,
//! refreshes or asks for more, at most [`MAX_MENTIONS`] previews and
//! [`MAX_MENTION_BYTES`] of text, dropped when the pane closes or the account
//! changes. Neither list ever reaches the notification policy, so reading the
//! history raises no toast. Mark-as-read goes through the history
//! acknowledgement path; muting is one explicit `PATCH`. Jumping opens the
//! conversation and then jumps in the usual `history::Intent` way; the wait
//! for the conversation gives up after [`JUMP_PATIENCE`].

use std::time::{Duration, Instant};

use fastcord_discord::state::navigation::{Inbox, InboxRow};
use fastcord_discord::{MentionFilter, MentionsPage, RestClient, RestError};
use fastcord_model::{Message, Snowflake};
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Color, Element, Length, Task};

pub const MAX_MENTIONS: usize = 100;
pub const MAX_MENTION_BYTES: usize = 64 * 1024;
const PREVIEW_CHARS: usize = 200;
const AUTHOR_CHARS: usize = 64;
const JUMP_PATIENCE: Duration = Duration::from_secs(10);

const PANEL: Color = crate::style::SIDEBAR;
const ROW: Color = crate::style::CHAT;
const MUTED: Color = crate::style::SUBTLE;
const WARN: Color = crate::style::RED;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Unread,
    Mentions,
}

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    Tab(Tab),
    /// Open the unread conversation at this row of the snapshot.
    OpenRow(usize),
    /// Jump to the mention at this index.
    JumpMention(usize),
    MarkRead(usize),
    MarkAllRead,
    ToggleMute(usize),
    Refresh,
    More,
    ToggleRoles,
    ToggleEveryone,
    Loaded(u64, Result<MentionsPage, RestError>),
    Muted(Snowflake, Result<(), RestError>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Loading,
    Failed(&'static str),
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention {
    pub channel: Snowflake,
    pub message: Snowflake,
    pub author_id: Snowflake,
    pub author: String,
    pub date: String,
    pub preview: String,
}

impl Mention {
    fn new(message: &Message) -> Self {
        let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
        Self {
            channel: message.channel_id,
            message: message.id,
            author_id: message.author.id,
            author: clip(message.author.display_name(), AUTHOR_CHARS),
            date: clip(&message.timestamp, 10),
            preview: clip(&message.content, PREVIEW_CHARS),
        }
    }

    fn bytes(&self) -> usize {
        64 + self.author.len() + self.date.len() + self.preview.len()
    }
}

/// Where a jump lands once its conversation is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The first message after the read cursor.
    Unread(Option<Snowflake>),
    Message(Snowflake),
}

/// What an action of the pane asks the application to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Open(Snowflake, Target),
}

pub struct State {
    open: bool,
    tab: Tab,
    filter: MentionFilter,
    /// Identifies the current mention request; older answers find another.
    serial: u64,
    status: Status,
    mentions: Vec<Mention>,
    bytes: usize,
    /// The ID of the last mention held: the `before` cursor of the next page.
    cursor: Option<Snowflake>,
    has_more: bool,
    /// Whether the mention list was ever asked for since the pane opened.
    asked: bool,
    /// Dropping this aborts the request in flight.
    task: Option<iced::task::Handle>,
    /// Conversations with a mute request in flight.
    muting: Vec<Snowflake>,
    notice: Option<&'static str>,
    pending: Option<(Snowflake, Target, Instant)>,
    expired: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            open: false,
            tab: Tab::Unread,
            filter: MentionFilter::default(),
            serial: 0,
            status: Status::Idle,
            mentions: Vec::new(),
            bytes: 0,
            cursor: None,
            has_more: false,
            asked: false,
            task: None,
            muting: Vec::new(),
            notice: None,
            pending: None,
            expired: false,
        }
    }
}

pub fn describe(error: RestError) -> &'static str {
    match error {
        RestError::NotificationWritesPaused => {
            "Notification changes are paused pending validation. Use the official Discord client."
        }
        RestError::PermissionDenied => "Discord does not allow this.",
        RestError::ResourceGone => "That conversation is gone.",
        RestError::AuthenticationRequired => "Discord no longer accepts this login.",
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            "Discord is rate limiting this. Try again shortly."
        }
        RestError::Retryable(_) | RestError::Http(_) => "Discord failed. Try again.",
        RestError::WriteNotVerified(_) => fastcord_discord::WRITE_NOT_VERIFIED,
        _ => "The inbox request failed.",
    }
}

impl State {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// How many inbox rows the reducer snapshot should carry.
    pub fn rows_wanted(&self) -> usize {
        if self.open && self.tab == Tab::Unread {
            fastcord_discord::state::navigation::MAX_INBOX_ROWS
        } else {
            0
        }
    }

    fn reset_mentions(&mut self) {
        self.serial = self.serial.wrapping_add(1);
        self.task = None;
        self.mentions.clear();
        self.bytes = 0;
        self.cursor = None;
        self.has_more = false;
        self.status = Status::Idle;
        self.asked = false;
    }

    fn load(&mut self, client: &RestClient) -> Task<Event> {
        let client = client.clone();
        let (before, filter, serial) = (self.cursor, self.filter, self.serial);
        self.status = Status::Loading;
        self.asked = true;
        let (task, handle) = Task::perform(
            async move { client.recent_mentions(before, filter).await },
            move |result| Event::Loaded(serial, result),
        )
        .abortable();
        self.task = Some(handle.abort_on_drop());
        task
    }

    /// The jump waiting for its conversation, once that is the open one.
    pub fn take_pending(&mut self, open_channel: Option<Snowflake>) -> Option<(Snowflake, Target)> {
        let (channel, target, since) = self.pending?;
        if since.elapsed() > JUMP_PATIENCE {
            self.pending = None;
            self.expired = true;
        } else if open_channel == Some(channel) {
            self.pending = None;
            return Some((channel, target));
        }
        None
    }

    pub fn take_expired(&mut self) -> bool {
        std::mem::take(&mut self.expired)
    }

    pub fn set_pending(&mut self, channel: Snowflake, target: Target) {
        self.pending = Some((channel, target, Instant::now()));
    }

    /// Closes the pane and forgets everything (logout, new account).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// `rows` is the reducer's current inbox; `client` is `None` when no
    /// request may be sent (the account was stopped).
    pub fn update(
        &mut self,
        event: Event,
        rows: &Inbox,
        client: &RestClient,
    ) -> (Task<Event>, Action) {
        let mut action = Action::None;
        let mut task = Task::none();
        match event {
            Event::Toggle => {
                if self.open {
                    self.open = false;
                    self.reset_mentions();
                    self.notice = None;
                } else {
                    self.open = true;
                    self.tab = Tab::Unread;
                }
            }
            Event::Tab(tab) if self.open => {
                self.tab = tab;
                self.notice = None;
                if tab == Tab::Mentions && !self.asked {
                    task = self.load(client);
                }
            }
            Event::Refresh if self.open && self.tab == Tab::Mentions => {
                self.reset_mentions();
                task = self.load(client);
            }
            Event::More
                if self.open
                    && self.status == Status::Done
                    && self.has_more
                    && self.cursor.is_some() =>
            {
                self.serial = self.serial.wrapping_add(1);
                task = self.load(client);
            }
            Event::ToggleRoles | Event::ToggleEveryone if self.open => {
                if matches!(event, Event::ToggleRoles) {
                    self.filter.roles = !self.filter.roles;
                } else {
                    self.filter.everyone = !self.filter.everyone;
                }
                self.reset_mentions();
                task = self.load(client);
            }
            Event::OpenRow(index) => {
                if let Some(row) = rows.rows.get(index) {
                    self.open = false;
                    self.reset_mentions();
                    action = Action::Open(row.channel, Target::Unread(row.acked));
                }
            }
            Event::JumpMention(index) => {
                if let Some(mention) = self
                    .mentions
                    .get(index)
                    .filter(|mention| !rows.suppressed.contains_key(&mention.author_id))
                {
                    action = Action::Open(mention.channel, Target::Message(mention.message));
                    self.open = false;
                    self.reset_mentions();
                }
            }
            Event::ToggleMute(index) => {
                if let Some(row) = rows.rows.get(index)
                    && !self.muting.contains(&row.channel)
                    && self.muting.len() < fastcord_discord::state::navigation::MAX_INBOX_ROWS
                {
                    self.muting.push(row.channel);
                    self.notice = None;
                    let (client, guild, channel, muted) =
                        (client.clone(), row.guild, row.channel, !row.channel_muted);
                    task = Task::perform(
                        async move { client.set_channel_muted(guild, channel, muted).await },
                        move |result| Event::Muted(channel, result),
                    );
                }
            }
            Event::Muted(channel, result) => {
                self.muting.retain(|c| *c != channel);
                if let Err(error) = result {
                    self.notice = Some(describe(error));
                }
            }
            Event::Loaded(serial, result) if serial == self.serial => {
                self.task = None;
                match result {
                    Ok(page) => {
                        let last = page.items.last().map(|m| m.id);
                        let mut full = false;
                        for item in &page.items {
                            // Ignored and blocked authors are never kept, so
                            // neither the list nor its count can show them.
                            if rows.suppressed.contains_key(&item.author.id) {
                                continue;
                            }
                            let mention = Mention::new(item);
                            if self.mentions.iter().any(|m| m.message == mention.message) {
                                continue;
                            }
                            if self.mentions.len() >= MAX_MENTIONS
                                || self.bytes + mention.bytes() > MAX_MENTION_BYTES
                            {
                                full = true;
                                break;
                            }
                            self.bytes += mention.bytes();
                            self.mentions.push(mention);
                        }
                        self.cursor = last;
                        self.has_more = page.has_more && !full && last.is_some();
                        self.status = Status::Done;
                    }
                    Err(error) => self.status = Status::Failed(describe(error)),
                }
            }
            // Replaced requests, and events while the pane is closed.
            _ => {}
        }
        (task, action)
    }
}

fn unread_row<'a>(index: usize, item: &'a InboxRow) -> Element<'a, Event> {
    let place = if item.guild.is_some() {
        format!("{} · #{}", item.guild_name, item.name)
    } else {
        item.name.clone()
    };
    let mut badges = String::new();
    if item.mentions > 0 {
        badges.push_str(&format!("{} mention(s)", item.mentions));
    } else if item.unread {
        badges.push_str("unread");
    }
    if item.muted {
        badges.push_str(" · muted");
    }
    let mute_label = if item.channel_muted { "Unmute" } else { "Mute" };
    row![
        button(column![text(place).size(14), text(badges).size(12).color(MUTED)].spacing(2),)
            .width(Length::Fill)
            .style(|_, _| button::Style {
                background: Some(ROW.into()),
                text_color: Color::WHITE,
                ..Default::default()
            })
            .on_press(Event::OpenRow(index)),
        button(text("Mark read").size(12)).on_press(Event::MarkRead(index)),
        button(text(mute_label).size(12)).on_press(Event::ToggleMute(index)),
    ]
    .spacing(4)
    .into()
}

pub fn view<'a>(state: &'a State, inbox: &'a Inbox) -> Element<'a, Event> {
    let tab = |label: &'static str, tab: Tab| -> Element<'a, Event> {
        let selected = state.tab == tab;
        button(text(label).size(13))
            .on_press_maybe((!selected).then_some(Event::Tab(tab)))
            .padding([4, 10])
            .style(crate::style::secondary_button(selected))
            .into()
    };
    let mut body = column![crate::icons::panel_title(
        "Inbox",
        vec![tab("Unread", Tab::Unread), tab("Mentions", Tab::Mentions)],
        Event::Toggle,
    )]
    .spacing(8);
    if let Some(notice) = state.notice {
        body = body.push(text(notice).size(12).color(WARN));
    }
    match state.tab {
        Tab::Unread => {
            let mut list = column![].spacing(4);
            for (index, item) in inbox.rows.iter().enumerate() {
                list = list.push(unread_row(index, item));
            }
            let summary = if inbox.total == 0 {
                "Nothing unread.".to_owned()
            } else if inbox.total > inbox.rows.len() {
                format!(
                    "{} unread conversations, {} mentions. Showing {}.",
                    inbox.total,
                    inbox.mentions,
                    inbox.rows.len()
                )
            } else {
                format!(
                    "{} unread conversations, {} mentions.",
                    inbox.total, inbox.mentions
                )
            };
            body = body.push(text(summary).size(12).color(MUTED));
            if !inbox.rows.is_empty() {
                let label = if inbox.total > inbox.rows.len() {
                    "Mark these as read"
                } else {
                    "Mark all as read"
                };
                body = body
                    .push(button(text(label).size(13)).on_press(Event::MarkAllRead))
                    .push(scrollable(list).height(Length::Fill));
            }
        }
        Tab::Mentions => {
            let flag = |label: &'static str, on: bool, event: Event| {
                button(text(format!("{label}: {}", if on { "on" } else { "off" })).size(12))
                    .on_press(event)
            };
            body = body.push(
                row![
                    flag("Roles", state.filter.roles, Event::ToggleRoles),
                    flag("@everyone", state.filter.everyone, Event::ToggleEveryone),
                    button(text("Refresh").size(12)).on_press(Event::Refresh),
                ]
                .spacing(6),
            );
            // A user ignored or blocked after the page loaded disappears too.
            let visible = |mention: &Mention| !inbox.suppressed.contains_key(&mention.author_id);
            let shown = state.mentions.iter().filter(|m| visible(m)).count();
            body = body.push(match &state.status {
                Status::Idle => text("").size(12),
                Status::Loading => text("Loading mentions…").size(12).color(MUTED),
                Status::Failed(reason) => text(*reason).size(12).color(WARN),
                Status::Done if shown == 0 => text("No recent mentions.").size(12).color(MUTED),
                Status::Done => text(format!("{shown} mentions")).size(12).color(MUTED),
            });
            let mut list = column![].spacing(4);
            for (index, mention) in state.mentions.iter().enumerate() {
                if !visible(mention) {
                    continue;
                }
                list = list.push(
                    button(
                        column![
                            text(format!("{} · {}", mention.author, mention.date))
                                .size(12)
                                .color(MUTED),
                            text(&mention.preview).size(14),
                        ]
                        .spacing(2),
                    )
                    .width(Length::Fill)
                    .style(|_, _| button::Style {
                        background: Some(ROW.into()),
                        text_color: Color::WHITE,
                        ..Default::default()
                    })
                    .on_press(Event::JumpMention(index)),
                );
            }
            body = body.push(scrollable(list).height(Length::Fill));
            if state.status == Status::Done && state.has_more {
                body = body.push(button(text("Load more").size(13)).on_press(Event::More));
            }
        }
    }
    container(body)
        .padding(10)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(PANEL.into()),
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::MENTIONS_PAGE_SIZE;
    use fastcord_discord::state::navigation::InboxRow;

    fn client() -> RestClient {
        use fastcord_discord::UserToken;
        use fastcord_discord::gateway::{ClientProperties, HostOs};
        RestClient::new(
            UserToken::new("offline-inbox-credential".into()),
            ClientProperties::web(HostOs::Linux, "en-US", 1),
        )
        .unwrap()
    }

    fn mention_message(id: u64, text: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "channel_id": "500", "content": text,
            "timestamp": "2026-10-07T12:00:00.000000+00:00",
            "author": {"id": "3", "username": "other"},
        }))
        .unwrap()
    }

    fn page(ids: &[u64], has_more: bool) -> MentionsPage {
        MentionsPage {
            items: ids.iter().map(|id| mention_message(*id, "hi")).collect(),
            has_more,
        }
    }

    fn rows(n: usize) -> Inbox {
        Inbox {
            suppressed: Default::default(),
            total: n,
            mentions: 0,
            rows: (0..n as u64)
                .map(|i| InboxRow {
                    channel: Snowflake(100 + i),
                    guild: (i % 2 == 0).then_some(Snowflake(7)),
                    guild_name: "g".into(),
                    name: "c".into(),
                    unread: true,
                    mentions: 0,
                    muted: false,
                    channel_muted: i == 1,
                    last_message: Some(Snowflake(50)),
                    acked: Some(Snowflake(40)),
                })
                .collect(),
        }
    }

    fn step(state: &mut State, event: Event, inbox: &Inbox) -> Action {
        state.update(event, inbox, &client()).1
    }

    #[test]
    fn the_unread_tab_loads_nothing_and_the_mentions_tab_loads_once_on_request() {
        let mut state = State::default();
        let inbox = rows(0);
        step(&mut state, Event::Toggle, &inbox);
        assert!(state.open && state.status == Status::Idle && !state.asked);
        assert_eq!(state.rows_wanted(), 64);
        step(&mut state, Event::Tab(Tab::Mentions), &inbox);
        assert_eq!(state.status, Status::Loading);
        assert_eq!(state.rows_wanted(), 0);
        let serial = state.serial;
        // Switching back and forth does not ask again.
        step(&mut state, Event::Tab(Tab::Unread), &inbox);
        step(&mut state, Event::Tab(Tab::Mentions), &inbox);
        assert_eq!(state.serial, serial);
    }

    #[test]
    fn pages_append_without_duplicates_and_are_byte_and_count_bounded() {
        let mut state = State::default();
        let inbox = rows(0);
        step(&mut state, Event::Toggle, &inbox);
        step(&mut state, Event::Tab(Tab::Mentions), &inbox);
        let serial = state.serial;
        step(
            &mut state,
            Event::Loaded(serial, Ok(page(&[30, 20], true))),
            &inbox,
        );
        assert_eq!(
            (state.mentions.len(), state.cursor, state.has_more),
            (2, Some(Snowflake(20)), true)
        );
        step(&mut state, Event::More, &inbox);
        let serial = state.serial;
        step(
            &mut state,
            Event::Loaded(serial, Ok(page(&[20, 10], false))),
            &inbox,
        );
        assert_eq!(state.mentions.len(), 3, "the repeated mention is dropped");
        assert!(!state.has_more);
        // The cap holds and stops further paging.
        step(&mut state, Event::Refresh, &inbox);
        let serial = state.serial;
        let many: Vec<u64> = (1..=MENTIONS_PAGE_SIZE as u64).collect();
        step(
            &mut state,
            Event::Loaded(serial, Ok(page(&many, true))),
            &inbox,
        );
        for round in 0..6u64 {
            if !state.has_more {
                break;
            }
            step(&mut state, Event::More, &inbox);
            let serial = state.serial;
            let ids: Vec<u64> = (0..MENTIONS_PAGE_SIZE as u64)
                .map(|i| 1_000 + round * 100 + i)
                .collect();
            step(
                &mut state,
                Event::Loaded(serial, Ok(page(&ids, true))),
                &inbox,
            );
        }
        assert!(state.mentions.len() <= MAX_MENTIONS && state.bytes <= MAX_MENTION_BYTES);
        assert!(!state.has_more, "the cap ends paging");
    }

    #[test]
    fn mentions_by_ignored_or_blocked_users_are_never_kept_or_listed() {
        use fastcord_discord::state::Suppression;
        let mut inbox = rows(0);
        inbox.suppressed = std::sync::Arc::new(std::collections::BTreeMap::from([(
            Snowflake(3),
            Suppression::Ignored,
        )]));
        let mut state = State::default();
        step(&mut state, Event::Toggle, &inbox);
        step(&mut state, Event::Tab(Tab::Mentions), &inbox);
        let serial = state.serial;
        let mut mixed = page(&[30, 20], false);
        mixed.items[1].author.id = Snowflake(4);
        step(&mut state, Event::Loaded(serial, Ok(mixed)), &inbox);
        // The hidden author's mention is dropped; the cursor still advances.
        assert_eq!(state.mentions.len(), 1);
        assert_eq!(state.mentions[0].message, Snowflake(20));
        assert_eq!(state.cursor, Some(Snowflake(20)));
        // Ignoring someone after the page loaded hides it and refuses a jump.
        let later = rows(0);
        assert_eq!(
            step(&mut state, Event::JumpMention(0), &later),
            Action::Open(Snowflake(500), Target::Message(Snowflake(20)))
        );
        let mut state = State::default();
        step(&mut state, Event::Toggle, &later);
        step(&mut state, Event::Tab(Tab::Mentions), &later);
        let serial = state.serial;
        step(
            &mut state,
            Event::Loaded(serial, Ok(page(&[30], false))),
            &later,
        );
        assert_eq!(state.mentions.len(), 1);
        assert_eq!(
            step(&mut state, Event::JumpMention(0), &inbox),
            Action::None
        );
        let _ = view(&state, &inbox);
    }

    #[test]
    fn answers_of_replaced_requests_and_a_closed_pane_are_ignored() {
        let mut state = State::default();
        let inbox = rows(0);
        step(&mut state, Event::Toggle, &inbox);
        step(&mut state, Event::Tab(Tab::Mentions), &inbox);
        let old = state.serial;
        step(&mut state, Event::Refresh, &inbox);
        step(
            &mut state,
            Event::Loaded(old, Ok(page(&[9], false))),
            &inbox,
        );
        assert!(
            state.mentions.is_empty(),
            "an older answer finds another serial"
        );
        let in_flight = state.serial;
        step(&mut state, Event::Toggle, &inbox);
        step(
            &mut state,
            Event::Loaded(in_flight, Ok(page(&[9], false))),
            &inbox,
        );
        // Closed: the list was dropped, so a late answer is stale.
        assert!(!state.open && state.mentions.is_empty());
    }

    #[test]
    fn failures_are_shown_once_and_never_retried() {
        let mut state = State::default();
        let inbox = rows(0);
        step(&mut state, Event::Toggle, &inbox);
        step(&mut state, Event::Tab(Tab::Mentions), &inbox);
        let serial = state.serial;
        for error in [
            RestError::PermissionDenied,
            RestError::ResourceGone,
            RestError::InvalidRateLimit,
            RestError::AuthenticationRequired,
        ] {
            state.serial = serial;
            step(&mut state, Event::Loaded(serial, Err(error)), &inbox);
            assert!(matches!(state.status, Status::Failed(_)));
            assert!(state.task.is_none(), "no automatic retry");
        }
    }

    #[test]
    fn a_row_or_mention_opens_the_conversation_and_closes_the_pane() {
        let mut state = State::default();
        let inbox = rows(3);
        step(&mut state, Event::Toggle, &inbox);
        assert_eq!(
            step(&mut state, Event::OpenRow(2), &inbox),
            Action::Open(Snowflake(102), Target::Unread(Some(Snowflake(40))))
        );
        assert!(!state.open);
        assert_eq!(step(&mut state, Event::OpenRow(9), &inbox), Action::None);
        step(&mut state, Event::Toggle, &inbox);
        step(&mut state, Event::Tab(Tab::Mentions), &inbox);
        let serial = state.serial;
        step(
            &mut state,
            Event::Loaded(serial, Ok(page(&[30], false))),
            &inbox,
        );
        assert_eq!(
            step(&mut state, Event::JumpMention(0), &inbox),
            Action::Open(Snowflake(500), Target::Message(Snowflake(30)))
        );
    }

    #[test]
    fn muting_is_one_request_per_conversation_until_it_answers() {
        let mut state = State::default();
        let inbox = rows(2);
        state.open = true;
        step(&mut state, Event::ToggleMute(0), &inbox);
        step(&mut state, Event::ToggleMute(0), &inbox);
        step(&mut state, Event::ToggleMute(1), &inbox);
        assert_eq!(state.muting, [Snowflake(100), Snowflake(101)]);
        step(
            &mut state,
            Event::Muted(Snowflake(100), Err(RestError::PermissionDenied)),
            &inbox,
        );
        assert!(state.notice.is_some() && state.muting == [Snowflake(101)]);
    }

    #[test]
    fn a_pending_jump_waits_for_its_channel_fires_once_and_expires() {
        let mut state = State::default();
        state.set_pending(Snowflake(600), Target::Unread(Some(Snowflake(1))));
        assert_eq!(state.take_pending(Some(Snowflake(500))), None);
        assert_eq!(
            state.take_pending(Some(Snowflake(600))),
            Some((Snowflake(600), Target::Unread(Some(Snowflake(1)))))
        );
        assert_eq!(state.take_pending(Some(Snowflake(600))), None);
        assert!(!state.take_expired());
        state.pending = Some((
            Snowflake(1),
            Target::Unread(None),
            Instant::now() - JUMP_PATIENCE * 2,
        ));
        assert_eq!(state.take_pending(None), None);
        assert!(state.take_expired() && !state.take_expired());
    }
}
