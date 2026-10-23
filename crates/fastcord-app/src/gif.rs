//! GIF picker: trending or searched results from Discord's GIF route, shown as
//! at most [`fastcord_discord::GIF_RESULTS`] small previews. Previews come
//! from the GIF provider through the credential-free fetcher (never with the
//! Discord token) and are decoded off the UI thread to static first frames.
//! Picking a result hands its URL to the composer; nothing is stored on disk
//! beyond the shared byte-bounded attachment cache.

use fastcord_discord::{GifResult, MAX_GIF_QUERY_CHARS, RestClient, RestError};
use fastcord_model::Snowflake;
use iced::widget::{button, column, container, image, row, scrollable, text, text_input};
use iced::{Element, Length, Task};

use crate::attachments::{AttachmentCache, DecodedImage};

/// Cache identity for GIF previews: they share the credential-free path.
const PREVIEW_KIND: u8 = 5;

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    Input(String),
    Submit,
    /// Use this result: the app inserts its URL into the composer.
    Pick(usize),
    Done(u64, Result<Vec<GifResult>, RestError>),
    Thumb(u64, usize, Option<DecodedImage>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Idle,
    Loading,
    Failed(&'static str),
    Done,
}

pub struct State {
    pub open: bool,
    input: String,
    serial: u64,
    status: Status,
    results: Vec<GifResult>,
    thumbs: Vec<Option<image::Handle>>,
    /// Dropping this aborts the request and preview downloads in flight.
    task: Option<iced::task::Handle>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            open: false,
            input: String::new(),
            serial: 0,
            status: Status::Idle,
            results: Vec::new(),
            thumbs: Vec::new(),
            task: None,
        }
    }
}

fn describe(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied | RestError::AuthenticationRequired => {
            "Discord refused the GIF search."
        }
        RestError::ResourceGone => "Discord has no GIF search for this account.",
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            "Discord is rate limiting GIF searches. Try again shortly."
        }
        RestError::Retryable(_) | RestError::Http(_) => "GIF search failed. Try again.",
        _ => "GIF results could not be read.",
    }
}

impl State {
    /// The URL of result `index`, if it is still listed.
    pub fn url(&self, index: usize) -> Option<&str> {
        self.results.get(index).map(|r| r.url.as_str())
    }

    pub fn close(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            ..Self::default()
        };
    }

    fn search(&mut self, client: &RestClient) -> Task<Event> {
        self.serial = self.serial.wrapping_add(1);
        self.results.clear();
        self.thumbs.clear();
        self.status = Status::Loading;
        let query = self.input.trim().to_owned();
        let client = client.clone();
        let serial = self.serial;
        let (task, handle) = Task::perform(
            async move {
                client
                    .gifs((!query.is_empty()).then_some(query.as_str()))
                    .await
            },
            move |result| Event::Done(serial, result),
        )
        .abortable();
        self.task = Some(handle.abort_on_drop());
        task
    }

    pub fn update(
        &mut self,
        event: Event,
        client: &RestClient,
        cache: Option<&AttachmentCache>,
    ) -> Task<Event> {
        match event {
            Event::Toggle => {
                if self.open {
                    self.close();
                } else {
                    self.open = true;
                    return self.search(client);
                }
            }
            Event::Input(value) => {
                self.input = value.chars().take(MAX_GIF_QUERY_CHARS).collect();
            }
            Event::Submit => return self.search(client),
            Event::Done(serial, result) if serial == self.serial => match result {
                Ok(results) => {
                    self.status = Status::Done;
                    self.thumbs = vec![None; results.len()];
                    let Some(cache) = cache else {
                        self.results = results;
                        return Task::none();
                    };
                    let tasks = results.iter().enumerate().map(|(index, gif)| {
                        let cache = cache.clone();
                        let url = gif.preview.clone();
                        Task::perform(
                            async move {
                                cache
                                    .avatar(PREVIEW_KIND, Snowflake(1), &url, &url)
                                    .await
                                    .ok()
                            },
                            move |image| Event::Thumb(serial, index, image),
                        )
                    });
                    let tasks: Vec<_> = tasks.collect();
                    self.results = results;
                    let (task, handle) = Task::batch(tasks).abortable();
                    self.task = Some(handle.abort_on_drop());
                    return task;
                }
                Err(error) => self.status = Status::Failed(describe(error)),
            },
            Event::Thumb(serial, index, Some(img)) if serial == self.serial => {
                if let Some(slot) = self.thumbs.get_mut(index) {
                    *slot = Some(image::Handle::from_rgba(img.width, img.height, img.rgba));
                }
            }
            // Answers of replaced searches, failed previews and picks (the app's).
            Event::Done(..) | Event::Thumb(..) | Event::Pick(_) => {}
        }
        Task::none()
    }
}

pub fn view(state: &State) -> Option<Element<'_, Event>> {
    if !state.open {
        return None;
    }
    let controls = row![
        text_input("Search GIFs (empty for trending)", &state.input)
            .on_input(Event::Input)
            .on_submit(Event::Submit)
            .width(Length::Fill),
        button(text("Search").size(13)).on_press(Event::Submit),
        button(text("Close").size(13)).on_press(Event::Toggle),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);
    let status: Element<'_, Event> = match &state.status {
        Status::Idle => text("").size(12).into(),
        Status::Loading => text("Loading…").size(12).style(text::secondary).into(),
        Status::Failed(reason) => text(*reason).size(12).style(text::danger).into(),
        Status::Done if state.results.is_empty() => text("No GIFs found.")
            .size(12)
            .style(text::secondary)
            .into(),
        Status::Done => text("Pick a GIF to add its link to your message.")
            .size(12)
            .style(text::secondary)
            .into(),
    };
    let cells = state.results.iter().enumerate().map(|(index, gif)| {
        let picture: Element<'_, Event> = match state.thumbs.get(index).cloned().flatten() {
            Some(handle) => image(handle).width(Length::Fixed(64.0)).into(),
            None => text("…").size(24).into(),
        };
        button(
            column![
                picture,
                text(&gif.title).size(11).wrapping(text::Wrapping::None)
            ]
            .spacing(2)
            .width(Length::Fixed(96.0)),
        )
        .on_press(Event::Pick(index))
        .padding(4)
        .style(button::secondary)
        .into()
    });
    Some(
        container(
            column![
                controls,
                status,
                scrollable(row(cells).spacing(6).wrap()).height(Length::Fixed(220.0)),
            ]
            .spacing(6),
        )
        .padding(8)
        .into(),
    )
}
