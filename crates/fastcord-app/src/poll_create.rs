//! Poll creation panel: a question, two to ten answers, a duration and
//! whether several answers may be chosen, sent as one poll message.
//!
//! The limits are Discord's (question 300 characters, answers 55, ten answers,
//! up to 768 hours) and checked before anything is sent; the server stays
//! authoritative about everything else, including whether this account may
//! create polls in the channel, and its refusal is shown. A send is made once
//! and never repeated by the program. After an ambiguous failure (the poll may
//! exist) the user may press Create again; the same poll then goes out with the
//! same nonce, which makes Discord return the poll it already made instead of
//! a second one, while a changed poll is a new, deliberate send.

use fastcord_discord::{
    Nonce, NonceGenerator, OutgoingMessage, PollDraft, PollError, RestClient, SendError,
};
use fastcord_model::{MAX_POLL_ANSWERS, Snowflake};
use iced::widget::{button, checkbox, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Task};

/// Offered durations in hours (Discord accepts 1 to 768).
pub const DURATIONS: [(u32, &str); 7] = [
    (1, "1 hour"),
    (4, "4 hours"),
    (8, "8 hours"),
    (24, "24 hours"),
    (72, "3 days"),
    (168, "1 week"),
    (336, "2 weeks"),
];
const DEFAULT_DURATION: u32 = 24;

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    Question(String),
    Answer(usize, String),
    AddAnswer,
    RemoveAnswer(usize),
    Duration(u32),
    Multiselect(bool),
    Create,
    Sent(u64, Result<(), SendError>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Editing,
    Invalid(PollError),
    Sending,
    Failed(&'static str),
}

/// Where the panel is used: the poll is posted to this channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    pub channel: Snowflake,
    pub can_send: bool,
}

pub struct State {
    open: Option<Context>,
    serial: u64,
    status: Status,
    question: String,
    answers: Vec<String>,
    hours: u32,
    multiselect: bool,
    nonces: NonceGenerator,
    /// The poll of an ambiguous send and its nonce.
    attempt: Option<(PollDraft, Nonce)>,
    /// Dropping this aborts a send in flight.
    task: Option<iced::task::Handle>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            open: None,
            serial: 0,
            status: Status::Editing,
            question: String::new(),
            answers: vec![String::new(), String::new()],
            hours: DEFAULT_DURATION,
            multiselect: false,
            nonces: NonceGenerator::default(),
            attempt: None,
            task: None,
        }
    }
}

fn describe_send(error: SendError) -> &'static str {
    if matches!(
        error.cause(),
        fastcord_discord::RestError::WriteNotVerified(_)
    ) {
        fastcord_discord::WRITE_NOT_VERIFIED
    } else if error.is_ambiguous() {
        "The poll may have been created. Check the conversation; pressing Create again will not make a second copy of the same poll."
    } else {
        "Discord did not create the poll. You may lack the permission to create polls here."
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

    /// Forgets everything and aborts a send in flight.
    pub fn close(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            nonces: std::mem::take(&mut self.nonces),
            ..Self::default()
        };
    }

    fn edited(&mut self) {
        if self.status != Status::Sending {
            self.status = Status::Editing;
        }
    }

    pub fn update(&mut self, event: Event, context: Context, client: &RestClient) -> Task<Event> {
        match event {
            Event::Toggle => {
                if self.open.is_some() {
                    self.close();
                } else if context.can_send {
                    self.close();
                    self.open = Some(context);
                }
            }
            Event::Question(text) if self.open.is_some() => {
                self.question = text;
                self.edited();
            }
            Event::Answer(at, text) if self.open.is_some() => {
                if let Some(slot) = self.answers.get_mut(at) {
                    *slot = text;
                    self.edited();
                }
            }
            Event::AddAnswer if self.open.is_some() && self.answers.len() < MAX_POLL_ANSWERS => {
                self.answers.push(String::new());
            }
            Event::RemoveAnswer(at) if self.open.is_some() && self.answers.len() > 2 => {
                if at < self.answers.len() {
                    self.answers.remove(at);
                    self.edited();
                }
            }
            Event::Duration(hours) if self.open.is_some() => {
                if DURATIONS.iter().any(|&(offered, _)| offered == hours) {
                    self.hours = hours;
                    self.edited();
                }
            }
            Event::Multiselect(on) if self.open.is_some() => {
                self.multiselect = on;
                self.edited();
            }
            Event::Create => {
                let Some(open) = self.open else {
                    return Task::none();
                };
                // Re-checked at the moment of sending, not only when opened.
                if self.status == Status::Sending
                    || !context.can_send
                    || context.channel != open.channel
                {
                    return Task::none();
                }
                let draft = match PollDraft::new(
                    &self.question,
                    &self.answers,
                    self.hours,
                    self.multiselect,
                ) {
                    Ok(draft) => draft,
                    Err(error) => {
                        self.status = Status::Invalid(error);
                        return Task::none();
                    }
                };
                let nonce = match &self.attempt {
                    Some((last, nonce)) if *last == draft => *nonce,
                    _ => self.nonces.next_now(),
                };
                self.attempt = Some((draft.clone(), nonce));
                self.status = Status::Sending;
                let message = OutgoingMessage::with_poll(draft, nonce);
                let (client, serial, channel) = (client.clone(), self.serial, open.channel);
                let (task, handle) = Task::perform(
                    async move { client.create_message(channel, &message).await.map(|_| ()) },
                    move |result| Event::Sent(serial, result),
                )
                .abortable();
                self.task = Some(handle.abort_on_drop());
                return task;
            }
            Event::Sent(serial, result) if serial == self.serial && self.open.is_some() => {
                match result {
                    Ok(()) => self.close(),
                    Err(error) => {
                        // A refusal means nothing was created: a fresh nonce
                        // is right for whatever the user sends next.
                        if !error.is_ambiguous() {
                            self.attempt = None;
                        }
                        self.status = Status::Failed(describe_send(error));
                    }
                }
            }
            // Edits of a closed panel and answers of closed sends.
            _ => {}
        }
        Task::none()
    }
}

pub fn view(state: &State) -> Option<Element<'_, Event>> {
    state.open?;
    let sending = state.status == Status::Sending;
    let controls = row![
        text("Create a poll").size(13),
        iced::widget::space::horizontal(),
        crate::icons::close(Some(Event::Toggle)),
    ]
    .align_y(iced::Alignment::Center);
    let question = text_input("Question", &state.question)
        .on_input_maybe((!sending).then_some(Event::Question))
        .size(13);
    let mut answers = column![].spacing(4);
    for (at, answer) in state.answers.iter().enumerate() {
        let field = text_input(&format!("Answer {}", at + 1), answer)
            .on_input_maybe((!sending).then_some(move |text| Event::Answer(at, text)))
            .size(13)
            .width(Length::Fill);
        answers = answers.push(
            row![
                field,
                button(text("Remove").size(12))
                    .on_press_maybe(
                        (!sending && state.answers.len() > 2).then_some(Event::RemoveAnswer(at))
                    )
                    .style(button::secondary),
            ]
            .spacing(6),
        );
    }
    let durations = DURATIONS.iter().fold(
        row![text("Open for").size(12)]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        |line, &(hours, label)| {
            line.push(
                button(text(label).size(12))
                    .on_press_maybe((!sending).then_some(Event::Duration(hours)))
                    .style(if hours == state.hours {
                        button::primary
                    } else {
                        button::secondary
                    }),
            )
        },
    );
    let mut panel = column![
        controls,
        question,
        answers,
        row![
            button(text("Add answer").size(12))
                .on_press_maybe(
                    (!sending && state.answers.len() < MAX_POLL_ANSWERS)
                        .then_some(Event::AddAnswer)
                )
                .style(button::secondary),
            checkbox(state.multiselect)
                .label("Allow several answers")
                .size(14)
                .text_size(12)
                .on_toggle_maybe((!sending).then_some(Event::Multiselect)),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center),
        scrollable(durations).direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::default()
        )),
        button(text("Create").size(13)).on_press_maybe((!sending).then_some(Event::Create)),
    ]
    .spacing(6);
    match state.status {
        Status::Editing => {}
        Status::Sending => panel = panel.push(text("Sending…").size(12).style(text::secondary)),
        Status::Invalid(error) => {
            panel = panel.push(text(error.to_string()).size(12).style(text::danger));
        }
        Status::Failed(reason) => panel = panel.push(text(reason).size(12).style(text::danger)),
    }
    Some(container(panel).padding(8).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> RestClient {
        use fastcord_discord::UserToken;
        use fastcord_discord::gateway::{ClientProperties, HostOs};
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-poll-credential".into()),
            ClientProperties::web(HostOs::Linux, "en-US", 1),
        )
        .unwrap()
    }

    fn context() -> Context {
        Context {
            channel: Snowflake(5),
            can_send: true,
        }
    }

    fn filled() -> State {
        let mut state = State::default();
        let ctx = context();
        for event in [
            Event::Toggle,
            Event::Question("Lunch?".into()),
            Event::Answer(0, "Pizza".into()),
            Event::Answer(1, "Soup".into()),
        ] {
            let _ = state.update(event, ctx, &client());
        }
        state
    }

    #[test]
    fn the_panel_needs_the_right_to_send_and_belongs_to_its_channel() {
        let mut denied = State::default();
        let none = Context {
            can_send: false,
            ..context()
        };
        let _ = denied.update(Event::Toggle, none, &client());
        assert!(denied.open.is_none());
        let mut state = filled();
        assert!(state.is_open_for(Snowflake(5)));
        state.sync(Some(Snowflake(5)));
        assert!(state.open.is_some());
        state.sync(Some(Snowflake(6)));
        assert!(state.open.is_none() && state.question.is_empty());
    }

    #[test]
    fn answers_are_bounded_at_two_to_ten_and_durations_are_the_offered_ones() {
        let mut state = filled();
        for _ in 0..20 {
            let _ = state.update(Event::AddAnswer, context(), &client());
        }
        assert_eq!(state.answers.len(), MAX_POLL_ANSWERS);
        for _ in 0..20 {
            let _ = state.update(Event::RemoveAnswer(0), context(), &client());
        }
        assert_eq!(state.answers.len(), 2);
        let _ = state.update(Event::Duration(5), context(), &client());
        assert_eq!(state.hours, DEFAULT_DURATION, "5 hours is not offered");
        let _ = state.update(Event::Duration(168), context(), &client());
        assert_eq!(state.hours, 168);
    }

    #[test]
    fn an_invalid_poll_sends_nothing() {
        let mut state = State::default();
        let _ = state.update(Event::Toggle, context(), &client());
        let _ = state.update(Event::Create, context(), &client());
        assert_eq!(state.status, Status::Invalid(PollError::NoQuestion));
        assert!(state.attempt.is_none());
        let _ = state.update(Event::Question("q".into()), context(), &client());
        let _ = state.update(Event::Create, context(), &client());
        assert_eq!(state.status, Status::Invalid(PollError::TooFewAnswers));
    }

    #[test]
    fn a_send_is_made_once_and_an_ambiguous_one_is_repeated_only_with_its_nonce() {
        let mut state = filled();
        let _ = state.update(Event::Create, context(), &client());
        assert_eq!(state.status, Status::Sending);
        let first = state.attempt.clone().unwrap();
        // A second press while sending is ignored.
        let _ = state.update(Event::Create, context(), &client());
        assert_eq!(state.attempt.clone().unwrap().1, first.1);
        let serial = state.serial;
        let ambiguous = SendError::Ambiguous(fastcord_discord::RestError::InvalidJson);
        let _ = state.update(
            Event::Sent(serial + 1, Err(ambiguous)),
            context(),
            &client(),
        );
        assert_eq!(state.status, Status::Sending, "a stale answer is ignored");
        let _ = state.update(Event::Sent(serial, Err(ambiguous)), context(), &client());
        assert_eq!(state.status, Status::Failed(describe_send(ambiguous)));
        // The same poll goes out again under the same nonce...
        let _ = state.update(Event::Create, context(), &client());
        assert_eq!(state.attempt.clone().unwrap().1, first.1);
        let _ = state.update(Event::Sent(serial, Err(ambiguous)), context(), &client());
        // ...a changed poll is a new send...
        let _ = state.update(Event::Answer(1, "Salad".into()), context(), &client());
        let _ = state.update(Event::Create, context(), &client());
        assert_ne!(state.attempt.clone().unwrap().1, first.1);
        // ...and a refusal forgets the attempt.
        let refused = SendError::NotSent(fastcord_discord::RestError::PermissionDenied);
        let _ = state.update(Event::Sent(serial, Err(refused)), context(), &client());
        assert!(state.attempt.is_none());
        assert!(describe_send(refused).contains("permission"));
        let _ = state.update(Event::Create, context(), &client());
        let _ = state.update(Event::Sent(serial, Ok(())), context(), &client());
        assert!(state.open.is_none(), "a confirmed send closes the panel");
    }

    #[test]
    fn a_stale_context_cannot_send() {
        let mut state = filled();
        let mut other = context();
        other.channel = Snowflake(6);
        let mut denied = context();
        denied.can_send = false;
        for ctx in [other, denied] {
            let _ = state.update(Event::Create, ctx, &client());
            assert_eq!(state.status, Status::Editing);
        }
    }
}
