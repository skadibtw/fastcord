//! Sticker picker: the open server's own stickers, listed from
//! `GET /guilds/{id}/stickers`, sent as one sticker-only message.
//!
//! Only stickers of the server being viewed are offered (other servers' and
//! standard-pack stickers are a Nitro privilege) and only where the timeline
//! says sending is allowed; Discord stays authoritative and its refusal is
//! shown. Previews are first frames from the public CDN through the shared
//! credential-free cache, decoded off the UI thread, at most [`PREVIEWS`]
//! 64x64 handles (about 192 KiB). A send is made once and never retried: an
//! ambiguous failure is reported and left to the user.

use crate::i18n::{tr, trf};
use fastcord_discord::{
    GuildSticker, MentionPolicy, NonceGenerator, OutgoingMessage, RestClient, RestError, SendError,
};
use fastcord_model::Snowflake;
use iced::widget::{button, column, container, image, row, scrollable, text};
use iced::{Element, Length, Task};

use crate::attachments::{AttachmentCache, DecodedImage};
use crate::avatars::AvatarKey;

/// Stickers that get a picture; the rest are listed by name.
const PREVIEWS: usize = 48;

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    Pick(usize),
    Listed(u64, Result<Vec<GuildSticker>, RestError>),
    Thumb(u64, usize, Option<DecodedImage>),
    Sent(u64, Result<(), SendError>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Loading,
    Failed(&'static str),
    Done,
    Sending,
}

/// Where the panel is used: lists and sends belong to this channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    pub guild: Option<Snowflake>,
    pub channel: Snowflake,
    pub can_send: bool,
}

pub struct State {
    open: Option<Context>,
    serial: u64,
    status: Status,
    stickers: Vec<GuildSticker>,
    thumbs: Vec<Option<image::Handle>>,
    nonces: NonceGenerator,
    /// Dropping this aborts the list request, the previews and a send in flight.
    task: Option<iced::task::Handle>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            open: None,
            serial: 0,
            status: Status::Loading,
            stickers: Vec::new(),
            thumbs: Vec::new(),
            nonces: NonceGenerator,
            task: None,
        }
    }
}

fn describe(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied | RestError::AuthenticationRequired => {
            tr("Discord did not let this account list stickers here.")
        }
        RestError::ResourceGone => tr("This server's stickers are no longer available."),
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            tr("Discord is rate limiting this request. Try again shortly.")
        }
        RestError::Retryable(_) | RestError::Http(_) => {
            tr("Stickers could not be loaded. Try again.")
        }
        _ => tr("The sticker list could not be read."),
    }
}

fn describe_send(error: SendError) -> &'static str {
    if matches!(error.cause(), RestError::WriteNotVerified(_)) {
        tr(fastcord_discord::WRITE_NOT_VERIFIED)
    } else if error.is_ambiguous() {
        tr("The sticker may have been delivered. Check the conversation before sending again.")
    } else {
        tr(
            "Discord did not send the sticker: it may be unavailable, or you may lack permission here.",
        )
    }
}

impl State {
    /// Closes the panel unless it belongs to `channel`.
    pub fn sync(&mut self, channel: Option<Snowflake>) {
        if self.open.is_some_and(|open| Some(open.channel) != channel) {
            self.close();
        }
    }

    pub fn is_open_for(&self, channel: Snowflake) -> bool {
        self.open.is_some_and(|open| open.channel == channel)
    }

    /// Forgets everything and aborts the work in flight.
    pub fn close(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            nonces: std::mem::take(&mut self.nonces),
            ..Self::default()
        };
    }

    pub fn update(
        &mut self,
        event: Event,
        context: Context,
        client: &RestClient,
        cache: Option<&AttachmentCache>,
    ) -> Task<Event> {
        match event {
            Event::Toggle => {
                if self.open.is_some() {
                    self.close();
                    return Task::none();
                }
                if !context.can_send {
                    return Task::none();
                }
                self.close();
                self.open = Some(context);
                let Some(guild) = context.guild else {
                    self.status = Status::Done;
                    return Task::none();
                };
                let (client, serial) = (client.clone(), self.serial);
                let (task, handle) = Task::perform(
                    async move { client.guild_stickers(guild).await },
                    move |result| Event::Listed(serial, result),
                )
                .abortable();
                self.task = Some(handle.abort_on_drop());
                return task;
            }
            Event::Listed(serial, result) if serial == self.serial && self.open.is_some() => {
                match result {
                    Ok(stickers) => {
                        self.status = Status::Done;
                        self.thumbs = vec![None; stickers.len()];
                        let tasks: Vec<_> = cache
                            .into_iter()
                            .flat_map(|cache| {
                                stickers.iter().enumerate().take(PREVIEWS).filter_map(
                                    move |(index, sticker)| {
                                        let key =
                                            AvatarKey::sticker(sticker.id, sticker.format_type)?;
                                        let (cache, id) = (cache.clone(), sticker.id);
                                        Some(Task::perform(
                                            async move {
                                                cache
                                                    .avatar(6, id, "sticker", &key.url())
                                                    .await
                                                    .ok()
                                            },
                                            move |image| Event::Thumb(serial, index, image),
                                        ))
                                    },
                                )
                            })
                            .collect();
                        self.stickers = stickers;
                        let (task, handle) = Task::batch(tasks).abortable();
                        self.task = Some(handle.abort_on_drop());
                        return task;
                    }
                    Err(error) => self.status = Status::Failed(describe(error)),
                }
            }
            Event::Thumb(serial, index, Some(img)) if serial == self.serial => {
                if let Some(slot) = self.thumbs.get_mut(index) {
                    *slot = Some(image::Handle::from_rgba(img.width, img.height, img.rgba));
                }
            }
            Event::Pick(index) => {
                let Some(open) = self.open else {
                    return Task::none();
                };
                let Some(sticker) = self.stickers.get(index) else {
                    return Task::none();
                };
                // Re-checked at the moment of sending, not only when listed.
                if self.status == Status::Sending
                    || !sticker.available
                    || !context.can_send
                    || context.channel != open.channel
                {
                    return Task::none();
                }
                self.status = Status::Sending;
                let message = OutgoingMessage::with_sticker(
                    sticker.id,
                    self.nonces.next_now(),
                    MentionPolicy::default(),
                );
                let (client, serial, channel) = (client.clone(), self.serial, open.channel);
                let (task, handle) = Task::perform(
                    async move { client.create_message(channel, &message).await.map(|_| ()) },
                    move |result| Event::Sent(serial, result),
                )
                .abortable();
                self.task = Some(handle.abort_on_drop());
                return task;
            }
            Event::Sent(serial, result) if serial == self.serial => match result {
                Ok(()) => self.close(),
                Err(error) => self.status = Status::Failed(describe_send(error)),
            },
            // Answers of closed panels and failed previews.
            Event::Listed(..) | Event::Thumb(..) | Event::Sent(..) => {}
        }
        Task::none()
    }
}

pub fn view(state: &State) -> Option<Element<'_, Event>> {
    let open = state.open?;
    let controls = row![
        text(tr("Stickers of this server")).size(13),
        iced::widget::space::horizontal(),
        crate::icons::close(Some(Event::Toggle)),
    ]
    .align_y(iced::Alignment::Center);
    let note = |line: &'static str| text(tr(line)).size(12).style(text::secondary);
    let status: Element<'_, Event> = match state.status {
        Status::Loading => note("Loading…").into(),
        Status::Sending => note("Sending…").into(),
        Status::Failed(reason) => text(tr(reason)).size(12).style(text::danger).into(),
        Status::Done if open.guild.is_none() => {
            note("Stickers from other servers and packs need Nitro; open a server channel.").into()
        }
        Status::Done if state.stickers.is_empty() => note("This server has no stickers.").into(),
        Status::Done => note("Pick a sticker to send it.").into(),
    };
    let cells = state.stickers.iter().enumerate().map(|(index, sticker)| {
        let picture: Element<'_, Event> = match state.thumbs.get(index).cloned().flatten() {
            Some(handle) => image(handle).width(Length::Fixed(64.0)).into(),
            None if AvatarKey::sticker(sticker.id, sticker.format_type).is_none() => {
                text(tr("(not shown)")).size(11).into()
            }
            None => text("…").size(24).into(),
        };
        let label = if sticker.available {
            sticker.name.clone()
        } else {
            trf("{} (unavailable)", &[&sticker.name])
        };
        button(
            column![picture, text(label).size(11).wrapping(text::Wrapping::None)]
                .spacing(2)
                .width(Length::Fixed(96.0)),
        )
        .on_press_maybe(sticker.available.then_some(Event::Pick(index)))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> RestClient {
        use fastcord_discord::UserToken;
        use fastcord_discord::gateway::{ClientProperties, HostOs};
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-sticker-credential".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn context(guild: Option<u64>) -> Context {
        Context {
            guild: guild.map(Snowflake),
            channel: Snowflake(5),
            can_send: true,
        }
    }

    fn sticker(id: u64, available: bool) -> GuildSticker {
        GuildSticker {
            id: Snowflake(id),
            name: "s".into(),
            format_type: 1,
            available,
            description: String::new(),
            tags: String::new(),
            creator: None,
        }
    }

    #[test]
    fn the_panel_belongs_to_its_channel_and_forgets_on_leaving() {
        let mut state = State {
            open: Some(context(Some(1))),
            stickers: vec![sticker(9, true)],
            ..State::default()
        };
        assert!(state.is_open_for(Snowflake(5)));
        state.sync(Some(Snowflake(5)));
        assert!(state.open.is_some());
        state.sync(Some(Snowflake(6)));
        assert!(state.open.is_none() && state.stickers.is_empty() && state.thumbs.is_empty());
    }

    #[test]
    fn picks_are_rechecked_and_a_send_is_never_repeated_or_retried() {
        let client = client();
        let mut state = State {
            open: Some(context(Some(1))),
            status: Status::Done,
            stickers: vec![sticker(9, false), sticker(10, true)],
            ..State::default()
        };
        let mut other = context(Some(1));
        other.channel = Snowflake(6);
        let mut denied = context(Some(1));
        denied.can_send = false;
        for (index, ctx) in [(0, context(Some(1))), (1, other), (1, denied)] {
            let _ = state.update(Event::Pick(index), ctx, &client, None);
            assert_eq!(state.status, Status::Done, "unavailable or stale: no send");
        }
        let _ = state.update(Event::Pick(1), context(Some(1)), &client, None);
        assert_eq!(state.status, Status::Sending);
        let serial = state.serial;
        let ambiguous = SendError::Ambiguous(RestError::InvalidJson);
        let ctx = context(Some(1));
        let _ = state.update(Event::Sent(serial + 1, Err(ambiguous)), ctx, &client, None);
        assert_eq!(state.status, Status::Sending, "a stale answer is ignored");
        let _ = state.update(Event::Sent(serial, Err(ambiguous)), ctx, &client, None);
        assert_eq!(state.status, Status::Failed(describe_send(ambiguous)));
        assert!(describe_send(ambiguous).contains("may have been delivered"));
        let _ = state.update(Event::Sent(serial, Ok(())), ctx, &client, None);
        assert!(state.open.is_none(), "a confirmed send closes the panel");
    }

    #[test]
    fn direct_conversations_offer_no_stickers_and_no_request() {
        let mut state = State::default();
        let _ = state.update(Event::Toggle, context(None), &client(), None);
        assert!(state.open.is_some() && state.status == Status::Done);
        assert!(state.stickers.is_empty());
        let mut denied = context(Some(1));
        denied.can_send = false;
        let mut closed = State::default();
        let _ = closed.update(Event::Toggle, denied, &client(), None);
        assert!(closed.open.is_none(), "no sending right: no panel");
    }
}
