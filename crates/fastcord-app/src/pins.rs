//! The channel's pinned-message panel: a paged list of bounded previews read
//! from Discord when the panel opens or is refreshed (nothing is stored on
//! disk), and pin/unpin of a message. At most [`MAX_PINS`] previews and
//! [`MAX_PIN_BYTES`] of text are kept; closing the panel, leaving the channel
//! or logging out drops them. Opening a pin is a normal
//! `history::Intent::JumpTo` in the same channel.

use crate::i18n::{tr, trf, trn};
use fastcord_discord::{PinsPage, RestClient, RestError};
use fastcord_model::{Message, Snowflake};
use iced::widget::{button, column, container, scrollable, text};
use iced::{Color, Element, Length, Task};

/// Discord allows 250 pins per channel.
pub const MAX_PINS: usize = 250;
pub const MAX_PIN_BYTES: usize = 128 * 1024;
const PREVIEW_CHARS: usize = 200;
const AUTHOR_CHARS: usize = 64;

const PANEL: Color = crate::style::SIDEBAR;
const ROW: Color = crate::style::CHAT;
const MUTED: Color = crate::style::SUBTLE;
const WARN: Color = crate::style::RED;

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    Refresh,
    More,
    Jump(usize),
    /// Pin or unpin a message of the open channel.
    SetPinned(Snowflake, Snowflake, bool),
    Done(u64, Result<PinsPage, RestError>),
    Changed(Snowflake, Result<(), RestError>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Idle,
    Loading,
    Failed(&'static str),
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub message: Snowflake,
    pub author: String,
    pub date: String,
    pub preview: String,
    pub attachments: usize,
}

impl Pin {
    fn new(message: &Message) -> Self {
        let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
        Self {
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

pub struct State {
    open: bool,
    /// The channel the list belongs to.
    channel: Option<Snowflake>,
    /// Identifies the current request; answers of older ones find another.
    serial: u64,
    status: Status,
    pins: Vec<Pin>,
    bytes: usize,
    /// `pinned_at` of the last item held: the `before` cursor of the next page.
    cursor: Option<String>,
    has_more: bool,
    /// Dropping this aborts the request in flight.
    task: Option<iced::task::Handle>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            open: false,
            channel: None,
            serial: 0,
            status: Status::Idle,
            pins: Vec::new(),
            bytes: 0,
            cursor: None,
            has_more: false,
            task: None,
        }
    }
}

fn describe(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied => tr("You cannot see or change pins here."),
        RestError::ResourceGone => tr("That message or conversation is gone."),
        RestError::AuthenticationRequired => tr("Discord no longer accepts this login."),
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            tr("Discord is rate limiting this. Try again shortly.")
        }
        RestError::Retryable(_) | RestError::Http(_) => tr("Discord failed. Try again."),
        RestError::WriteNotVerified(_) => tr(fastcord_discord::WRITE_NOT_VERIFIED),
        _ => tr("Pinned messages could not be read."),
    }
}

impl State {
    /// Whether the panel is open on `channel`.
    pub fn shows(&self, channel: Snowflake) -> bool {
        self.open && self.channel == Some(channel)
    }

    pub fn pin(&self, index: usize) -> Option<&Pin> {
        self.pins.get(index)
    }

    fn reset(&mut self, channel: Option<Snowflake>) {
        self.serial = self.serial.wrapping_add(1);
        self.task = None;
        self.pins.clear();
        self.bytes = 0;
        self.cursor = None;
        self.has_more = false;
        self.channel = channel;
        self.status = Status::Idle;
    }

    fn load(&mut self, client: &RestClient) -> Task<Event> {
        let Some(channel) = self.channel else {
            return Task::none();
        };
        let client = client.clone();
        let before = self.cursor.clone();
        let serial = self.serial;
        self.status = Status::Loading;
        let (task, handle) = Task::perform(
            async move { client.pinned_messages(channel, before.as_deref()).await },
            move |result| Event::Done(serial, result),
        )
        .abortable();
        self.task = Some(handle.abort_on_drop());
        task
    }

    /// `channel` is the open conversation.
    pub fn update(&mut self, event: Event, client: &RestClient, channel: Snowflake) -> Task<Event> {
        match event {
            Event::Toggle => {
                if self.shows(channel) {
                    self.open = false;
                    self.reset(None);
                } else {
                    self.open = true;
                    self.reset(Some(channel));
                    return self.load(client);
                }
            }
            Event::Refresh if self.shows(channel) => {
                self.reset(Some(channel));
                return self.load(client);
            }
            Event::More
                if self.shows(channel)
                    && self.status == Status::Done
                    && self.has_more
                    && self.cursor.is_some() =>
            {
                self.serial = self.serial.wrapping_add(1);
                return self.load(client);
            }
            Event::SetPinned(target, message, pinned) if target == channel => {
                let client = client.clone();
                return Task::perform(
                    async move { client.set_pinned(target, message, pinned).await },
                    move |result| Event::Changed(target, result),
                );
            }
            Event::Changed(target, result) if target == channel => match result {
                // Pins changed here: read the list again rather than guess it.
                Ok(()) if self.shows(channel) => {
                    self.reset(Some(channel));
                    return self.load(client);
                }
                Ok(()) => {}
                Err(error) => {
                    self.open = true;
                    if self.channel != Some(channel) {
                        self.reset(Some(channel));
                    }
                    self.status = Status::Failed(describe(error));
                }
            },
            Event::Done(serial, result) if serial == self.serial => {
                self.task = None;
                match result {
                    Ok(page) => {
                        let mut full = false;
                        for item in &page.items {
                            let pin = Pin::new(&item.message);
                            if self.pins.iter().any(|p| p.message == pin.message) {
                                continue;
                            }
                            if self.pins.len() >= MAX_PINS
                                || self.bytes + pin.bytes() > MAX_PIN_BYTES
                            {
                                full = true;
                                break;
                            }
                            self.bytes += pin.bytes();
                            self.pins.push(pin);
                        }
                        self.cursor = page.items.last().and_then(|i| i.pinned_at.clone());
                        self.has_more = page.has_more && !full && self.cursor.is_some();
                        self.status = Status::Done;
                    }
                    Err(error) => self.status = Status::Failed(describe(error)),
                }
            }
            // Answers of replaced requests, and events for another channel.
            _ => {}
        }
        Task::none()
    }
}

/// The panel under the header.
pub fn view(state: &State) -> Element<'_, Event> {
    let status: Element<'_, Event> = match &state.status {
        Status::Idle => text("").size(12).into(),
        Status::Loading => text(tr("Loading pinned messages…"))
            .size(12)
            .color(MUTED)
            .into(),
        Status::Failed(reason) => text(tr(reason)).size(12).color(WARN).into(),
        Status::Done if state.pins.is_empty() => {
            text(tr("No pinned messages.")).size(12).color(MUTED).into()
        }
        Status::Done => text(trf("{} pinned", &[&state.pins.len()]))
            .size(12)
            .color(MUTED)
            .into(),
    };
    let mut list = column![].spacing(4);
    for (index, pin) in state.pins.iter().enumerate() {
        let attachments = if pin.attachments > 0 {
            trn(
                pin.attachments as u64,
                " [{} attachment]",
                " [{} attachments]",
            )
        } else {
            String::new()
        };
        list = list.push(
            button(
                column![
                    text(format!("{} · {}", pin.author, pin.date))
                        .size(12)
                        .color(MUTED),
                    text(format!("{}{attachments}", pin.preview)).size(14),
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
    let mut body = column![
        crate::icons::panel_title(
            tr("Pinned messages"),
            vec![crate::icons::secondary(tr("Refresh"), Event::Refresh)],
            Event::Toggle,
        ),
        status
    ]
    .spacing(6);
    if !state.pins.is_empty() {
        body = body.push(scrollable(list).height(Length::Fixed(220.0)));
    }
    if state.status == Status::Done && state.has_more {
        body = body.push(button(text(tr("Load more")).size(13)).on_press(Event::More));
    }
    container(body)
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

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::PinnedMessage;

    const CHANNEL: Snowflake = Snowflake(500);

    fn client() -> RestClient {
        use fastcord_discord::UserToken;
        use fastcord_discord::gateway::{ClientProperties, HostOs};
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-pins-credential".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn item(id: u64, text: &str) -> PinnedMessage {
        PinnedMessage {
            pinned_at: Some(format!("2026-10-07T12:{:02}:00.000000+00:00", id % 60)),
            message: serde_json::from_value(serde_json::json!({
                "id": id.to_string(), "channel_id": "500", "content": text,
                "timestamp": "2026-10-07T12:00:00.000000+00:00",
                "author": {"id": "2", "username": "alt"},
            }))
            .unwrap(),
        }
    }

    fn page(items: Vec<PinnedMessage>, has_more: bool) -> Result<PinsPage, RestError> {
        Ok(PinsPage { items, has_more })
    }

    fn open_state(client: &RestClient) -> State {
        let mut state = State::default();
        let _ = state.update(Event::Toggle, client, CHANNEL);
        state
    }

    #[test]
    fn answers_of_replaced_requests_and_other_channels_are_ignored() {
        let client = client();
        let mut state = open_state(&client);
        let old = state.serial;
        let _ = state.update(Event::Refresh, &client, CHANNEL);
        let _ = state.update(
            Event::Done(old, page(vec![item(1, "a")], false)),
            &client,
            CHANNEL,
        );
        assert!(state.pins.is_empty(), "stale list request");
        assert_eq!(state.status, Status::Loading);
        assert!(state.shows(CHANNEL) && !state.shows(Snowflake(501)));
        // A refresh for another channel does nothing.
        let serial = state.serial;
        let _ = state.update(Event::Refresh, &client, Snowflake(501));
        assert_eq!(state.serial, serial);
    }

    #[test]
    fn pages_append_without_duplicates_and_follow_the_cursor() {
        let client = client();
        let mut state = open_state(&client);
        let serial = state.serial;
        let _ = state.update(
            Event::Done(serial, page(vec![item(1, "a"), item(2, "b")], true)),
            &client,
            CHANNEL,
        );
        assert!(state.has_more && state.cursor.is_some());
        let _ = state.update(Event::More, &client, CHANNEL);
        let serial = state.serial;
        let _ = state.update(
            Event::Done(serial, page(vec![item(2, "b"), item(3, "c")], false)),
            &client,
            CHANNEL,
        );
        let ids: Vec<u64> = state.pins.iter().map(|p| p.message.0).collect();
        assert_eq!(ids, [1, 2, 3]);
        assert!(!state.has_more);
        assert_eq!(state.pin(2).map(|p| p.preview.as_str()), Some("c"));
    }

    #[test]
    fn previews_are_clipped_and_the_list_is_byte_bounded() {
        let client = client();
        let mut state = open_state(&client);
        let serial = state.serial;
        let long = "x".repeat(10_000);
        let items: Vec<_> = (1..=50).map(|i| item(i, &long)).collect();
        let _ = state.update(Event::Done(serial, page(items, true)), &client, CHANNEL);
        assert!(state.pins.iter().all(|p| p.preview.chars().count() == 200));
        assert!(state.bytes <= MAX_PIN_BYTES && state.pins.len() <= MAX_PINS);
    }

    #[test]
    fn a_deleted_pin_error_is_visible_and_closing_or_leaving_releases_the_list() {
        let client = client();
        let mut state = open_state(&client);
        let serial = state.serial;
        let _ = state.update(
            Event::Done(serial, page(vec![item(1, "a")], false)),
            &client,
            CHANNEL,
        );
        let _ = state.update(
            Event::Changed(CHANNEL, Err(RestError::PermissionDenied)),
            &client,
            CHANNEL,
        );
        assert!(matches!(state.status, Status::Failed(m) if m.contains("cannot")));
        let _ = state.update(Event::Toggle, &client, CHANNEL);
        assert!(!state.open && state.pins.is_empty() && state.channel.is_none());
        // Opening, then the user moves to another channel: the panel is not shown.
        let _ = state.update(Event::Toggle, &client, CHANNEL);
        assert!(!state.shows(Snowflake(501)));
        let _ = state.update(Event::Toggle, &client, Snowflake(501));
        assert!(state.shows(Snowflake(501)) && state.pins.is_empty());
    }
}
