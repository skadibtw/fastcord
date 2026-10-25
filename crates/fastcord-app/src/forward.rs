//! Message forwarding: copy one message into up to [`MAX_DESTINATIONS`]
//! destinations the user ticks, with an optional note.
//!
//! Discord makes the snapshot (`message_reference.type = 1`); this client sends
//! only the source IDs and never reads the original's content again. Each
//! destination is its own request, made once: a destination that is confirmed
//! or ambiguous is never sent to again, only one Discord refused can be retried
//! by pressing Forward again. The optional note is a separate ordinary message
//! posted after its forward was confirmed, also once. Messages shown to the user
//! never name the source channel or author. Destinations are the sendable
//! channels and conversations in the navigation windows (at most 256 labels).

use fastcord_discord::{
    ForwardSource, MentionPolicy, NonceGenerator, OutgoingMessage, RestClient, RestError, SendError,
};
use fastcord_model::{REPLY_PREVIEW_CHARS, Snowflake};
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Task};

/// Most destinations one forward may be sent to at once.
pub const MAX_DESTINATIONS: usize = 5;
/// Most destinations offered; the navigation windows are smaller than this.
pub const MAX_CANDIDATES: usize = 256;
const MAX_LABEL_CHARS: usize = 64;

/// A place the account can post to, as the navigation windows show it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub id: Snowflake,
    pub label: String,
}

impl Candidate {
    pub fn new(id: Snowflake, label: &str) -> Self {
        Self {
            id,
            label: label.chars().take(MAX_LABEL_CHARS).collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Forwarded (and the note, if any, posted).
    Sent,
    /// Forwarded, but the note was not posted.
    NoteFailed(&'static str),
    /// Discord definitely did not create it; may be retried by the user.
    Refused(&'static str),
    /// It may exist; never repeated.
    Ambiguous,
}

impl Outcome {
    /// Nothing more may be sent to this destination.
    fn settled(&self) -> bool {
        !matches!(self, Self::Refused(_))
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Open {
        source: ForwardSource,
        preview: String,
    },
    Toggle(Snowflake),
    Input(String),
    Send,
    Done(u64, Snowflake, Outcome),
    Close,
}

struct Source {
    source: ForwardSource,
    preview: String,
}

#[derive(Default)]
pub struct State {
    open: Option<Source>,
    serial: u64,
    note: String,
    picked: Vec<Snowflake>,
    outcomes: Vec<(Snowflake, Outcome)>,
    in_flight: Vec<Snowflake>,
    notice: Option<&'static str>,
    nonces: NonceGenerator,
    /// Dropping this aborts the requests; it is only dropped when no send is in
    /// flight (see [`State::sync`]) or on logout.
    task: Option<iced::task::Handle>,
}

fn describe(error: SendError) -> Outcome {
    if error.is_ambiguous() {
        return Outcome::Ambiguous;
    }
    Outcome::Refused(match error.cause() {
        RestError::PermissionDenied => {
            "Discord did not allow this: you may lack permission there, or the message cannot be forwarded."
        }
        RestError::ResourceGone => {
            "The destination or the original message is no longer available."
        }
        RestError::AuthenticationRequired => "Sign-in is required again.",
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            "Discord is rate limiting this. Try again shortly."
        }
        RestError::WriteNotVerified(_) => fastcord_discord::WRITE_NOT_VERIFIED,
        _ => "Discord did not forward the message.",
    })
}

/// What one destination's task does: forward, then the note once the forward
/// was confirmed.
struct Plan {
    destination: Snowflake,
    nonce: fastcord_discord::Nonce,
    note: Option<OutgoingMessage>,
}

impl State {
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    fn busy(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// Closes the panel when the user leaves the conversation it was opened
    /// in, unless sends are in flight (their answers must still be shown).
    pub fn sync(&mut self, channel: Option<Snowflake>) {
        if !self.busy()
            && self
                .open
                .as_ref()
                .is_some_and(|open| Some(open.source.channel) != channel)
        {
            self.close();
        }
    }

    pub fn close(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            nonces: std::mem::take(&mut self.nonces),
            ..Self::default()
        };
    }

    fn outcome(&self, destination: Snowflake) -> Option<&Outcome> {
        self.outcomes
            .iter()
            .find(|(id, _)| *id == destination)
            .map(|(_, outcome)| outcome)
    }

    /// Chooses what to send: every ticked destination that is still offered
    /// and neither confirmed, ambiguous nor in flight. Marks them in flight.
    fn plan(&mut self, candidates: &[Candidate], note: Option<&str>) -> Vec<Plan> {
        let mut plans = Vec::new();
        let picked = std::mem::take(&mut self.picked);
        for destination in picked {
            if !candidates.iter().any(|c| c.id == destination) {
                continue;
            }
            self.picked.push(destination);
            if self.in_flight.contains(&destination)
                || self.outcome(destination).is_some_and(Outcome::settled)
            {
                continue;
            }
            let note = note.and_then(|text| {
                OutgoingMessage::new(text, self.nonces.next_now(), MentionPolicy::default()).ok()
            });
            plans.push(Plan {
                destination,
                nonce: self.nonces.next_now(),
                note,
            });
            self.in_flight.push(destination);
        }
        plans
    }

    pub fn update(
        &mut self,
        event: Event,
        candidates: &[Candidate],
        client: &RestClient,
    ) -> Task<Event> {
        match event {
            Event::Open { source, preview } => {
                if self.busy() {
                    return Task::none();
                }
                self.close();
                self.open = Some(Source {
                    source,
                    preview: preview.chars().take(REPLY_PREVIEW_CHARS).collect(),
                });
            }
            Event::Close if !self.busy() => self.close(),
            Event::Close => {}
            Event::Input(value) if self.open.is_some() && !self.busy() => {
                self.note = value;
                self.notice = None;
            }
            Event::Toggle(destination) if self.open.is_some() && !self.busy() => {
                self.notice = None;
                if let Some(at) = self.picked.iter().position(|id| *id == destination) {
                    self.picked.remove(at);
                } else if self.outcome(destination).is_some_and(Outcome::settled) {
                    self.notice = Some("Already forwarded there; it will not be sent twice.");
                } else if !candidates.iter().any(|c| c.id == destination) {
                    self.notice = Some("You cannot post there.");
                } else if self.picked.len() >= MAX_DESTINATIONS {
                    self.notice = Some("Forward to at most five places at once.");
                } else {
                    self.picked.push(destination);
                }
            }
            Event::Send if !self.busy() => {
                let Some(open) = self.open.as_ref().map(|open| open.source) else {
                    return Task::none();
                };
                let note = self.note.trim().to_owned();
                if !note.is_empty()
                    && OutgoingMessage::new(&note, self.nonces.next_now(), MentionPolicy::default())
                        .is_err()
                {
                    self.notice = Some("The note is too long.");
                    return Task::none();
                }
                self.notice = None;
                let plans = self.plan(candidates, (!note.is_empty()).then_some(note.as_str()));
                if plans.is_empty() {
                    self.notice = Some("Nothing to send: pick a place that has not received it.");
                    return Task::none();
                }
                let serial = self.serial;
                let tasks = plans.into_iter().map(|plan| {
                    let client = client.clone();
                    let destination = plan.destination;
                    Task::perform(
                        async move {
                            let forward = OutgoingMessage::forward(open, plan.nonce);
                            if let Err(error) = client.create_message(destination, &forward).await {
                                return describe(error);
                            }
                            match plan.note {
                                Some(note) => match client.create_message(destination, &note).await
                                {
                                    Ok(_) => Outcome::Sent,
                                    Err(error) if error.is_ambiguous() => Outcome::NoteFailed(
                                        "Forwarded, but the note may not have been sent.",
                                    ),
                                    Err(_) => {
                                        Outcome::NoteFailed("Forwarded, but the note was not sent.")
                                    }
                                },
                                None => Outcome::Sent,
                            }
                        },
                        move |outcome| Event::Done(serial, destination, outcome),
                    )
                });
                let (task, handle) = Task::batch(tasks).abortable();
                self.task = Some(handle.abort_on_drop());
                return task;
            }
            Event::Done(serial, destination, outcome) if serial == self.serial => {
                self.in_flight.retain(|id| *id != destination);
                self.outcomes.retain(|(id, _)| *id != destination);
                self.outcomes.push((destination, outcome));
                if !self.busy() {
                    self.task = None;
                }
            }
            _ => {}
        }
        Task::none()
    }
}

pub fn view<'a>(state: &'a State, candidates: &[Candidate]) -> Option<Element<'a, Event>> {
    let open = state.open.as_ref()?;
    let busy = state.busy();
    let controls = row![
        text("Forward message").size(13),
        iced::widget::space::horizontal(),
        crate::icons::close((!busy).then_some(Event::Close)),
    ]
    .align_y(iced::Alignment::Center);
    let preview = text(if open.preview.is_empty() {
        "(no text)"
    } else {
        open.preview.as_str()
    })
    .size(12)
    .style(text::secondary);
    let note = text_input("Add a message (optional)", &state.note)
        .on_input_maybe((!busy).then_some(Event::Input))
        .on_submit(Event::Send);
    let list = candidates.iter().map(|candidate| {
        let ticked = state.picked.contains(&candidate.id);
        let status = if state.in_flight.contains(&candidate.id) {
            " - sending".to_owned()
        } else {
            match state.outcome(candidate.id) {
                Some(Outcome::Sent) => " - forwarded".to_owned(),
                Some(Outcome::NoteFailed(reason)) => format!(" - {reason}"),
                Some(Outcome::Refused(reason)) => format!(" - not sent: {reason}"),
                Some(Outcome::Ambiguous) => {
                    " - may have been delivered; check before sending again".to_owned()
                }
                None => String::new(),
            }
        };
        button(
            text(format!(
                "{} {}{status}",
                if ticked { "[x]" } else { "[ ]" },
                candidate.label
            ))
            .size(13),
        )
        .on_press_maybe((!busy).then_some(Event::Toggle(candidate.id)))
        .style(button::secondary)
        .width(Length::Fill)
        .into()
    });
    let mut panel = column![
        controls,
        preview,
        note,
        scrollable(column(list).spacing(2)).height(Length::Fixed(180.0)),
        row![
            button(text("Forward").size(13))
                .on_press_maybe((!busy && !state.picked.is_empty()).then_some(Event::Send)),
            text(format!(
                "{} of {MAX_DESTINATIONS} places",
                state.picked.len()
            ))
            .size(12)
            .style(text::secondary),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    ]
    .spacing(6);
    if let Some(notice) = state.notice {
        panel = panel.push(text(notice).size(12).style(text::danger));
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
            UserToken::new("offline-forward-credential".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn places(ids: &[u64]) -> Vec<Candidate> {
        ids.iter()
            .map(|id| Candidate::new(Snowflake(*id), "place"))
            .collect()
    }

    fn opened() -> State {
        let mut state = State::default();
        let source = ForwardSource {
            channel: Snowflake(5),
            message: Snowflake(6),
            guild: Some(Snowflake(7)),
        };
        let _ = state.update(
            Event::Open {
                source,
                preview: "x".repeat(500),
            },
            &[],
            &client(),
        );
        state
    }

    #[test]
    fn at_most_five_offered_places_can_be_ticked_and_untick_works() {
        let mut state = opened();
        let all = places(&[1, 2, 3, 4, 5, 6, 8]);
        for id in 1..=6 {
            let _ = state.update(Event::Toggle(Snowflake(id)), &all, &client());
        }
        assert_eq!(state.picked.len(), MAX_DESTINATIONS);
        assert!(state.notice.is_some(), "the sixth is refused with a reason");
        let _ = state.update(Event::Toggle(Snowflake(3)), &all, &client());
        assert!(!state.picked.contains(&Snowflake(3)));
        let _ = state.update(Event::Toggle(Snowflake(99)), &all, &client());
        assert!(
            !state.picked.contains(&Snowflake(99)),
            "not an offered place"
        );
        assert_eq!(state.open.as_ref().unwrap().preview.chars().count(), 200);
    }

    #[test]
    fn sending_is_once_per_place_and_only_refusals_can_be_retried() {
        let mut state = opened();
        let all = places(&[1, 2, 3, 4]);
        for id in 1..=4 {
            let _ = state.update(Event::Toggle(Snowflake(id)), &all, &client());
        }
        let _ = state.update(Event::Send, &all, &client());
        assert_eq!(state.in_flight.len(), 4);
        // While requests are in flight nothing can be sent, ticked or closed.
        let _ = state.update(Event::Send, &all, &client());
        let _ = state.update(Event::Close, &all, &client());
        assert!(state.is_open() && state.in_flight.len() == 4);
        let serial = state.serial;
        let done = |state: &mut State, serial: u64, id: u64, outcome: Outcome| {
            let _ = state.update(Event::Done(serial, Snowflake(id), outcome), &all, &client());
        };
        done(&mut state, serial + 1, 1, Outcome::Sent);
        assert_eq!(state.in_flight.len(), 4, "a stale answer is ignored");
        done(&mut state, serial, 1, Outcome::Sent);
        done(&mut state, serial, 2, Outcome::Ambiguous);
        done(&mut state, serial, 3, Outcome::Refused("no"));
        done(&mut state, serial, 4, Outcome::NoteFailed("no note"));
        assert!(!state.busy());
        // Pressing Forward again plans only the refused place.
        let plans = state.plan(&all, None);
        assert_eq!(
            plans.iter().map(|p| p.destination).collect::<Vec<_>>(),
            vec![Snowflake(3)]
        );
        // A confirmed or ambiguous place cannot be ticked again.
        let _ = state.update(Event::Toggle(Snowflake(1)), &all, &client());
        let _ = state.update(Event::Toggle(Snowflake(2)), &all, &client());
        assert_eq!(state.picked.iter().filter(|id| id.0 <= 2).count(), 2);
    }

    #[test]
    fn a_place_that_is_no_longer_offered_is_dropped_before_sending() {
        let mut state = opened();
        let _ = state.update(Event::Toggle(Snowflake(1)), &places(&[1, 2]), &client());
        let _ = state.update(Event::Toggle(Snowflake(2)), &places(&[1, 2]), &client());
        assert!(state.plan(&places(&[2]), None).len() == 1);
        assert_eq!(state.picked, vec![Snowflake(2)]);
    }

    #[test]
    fn a_note_is_validated_before_anything_is_sent() {
        let mut state = opened();
        let all = places(&[1]);
        let _ = state.update(Event::Toggle(Snowflake(1)), &all, &client());
        state.note = "n".repeat(fastcord_discord::MAX_CONTENT_CHARS + 1);
        let _ = state.update(Event::Send, &all, &client());
        assert!(state.in_flight.is_empty() && state.notice == Some("The note is too long."));
        state.note = " hi ".into();
        let plans = state.plan(&all, Some("hi"));
        assert!(plans[0].note.is_some());
    }

    #[test]
    fn refusals_do_not_name_the_source_and_ambiguity_is_kept_apart() {
        for error in [
            SendError::NotSent(RestError::PermissionDenied),
            SendError::NotSent(RestError::ResourceGone),
            SendError::NotSent(RestError::AuthenticationRequired),
            SendError::NotSent(RestError::InvalidRateLimit),
            SendError::NotSent(RestError::InvalidJson),
        ] {
            let Outcome::Refused(reason) = describe(error) else {
                panic!("a refusal");
            };
            assert!(!reason.chars().any(|c| c.is_ascii_digit()), "{reason}");
        }
        assert_eq!(
            describe(SendError::Ambiguous(RestError::InvalidJson)),
            Outcome::Ambiguous
        );
    }

    #[test]
    fn leaving_the_conversation_closes_an_idle_panel_but_not_a_busy_one() {
        let mut state = opened();
        state.sync(Some(Snowflake(5)));
        assert!(state.is_open());
        state.in_flight.push(Snowflake(1));
        state.sync(Some(Snowflake(9)));
        assert!(
            state.is_open(),
            "answers of sends in flight must stay visible"
        );
        state.in_flight.clear();
        state.sync(Some(Snowflake(9)));
        assert!(!state.is_open());
    }

    #[test]
    fn the_panel_builds() {
        let state = opened();
        assert!(view(&state, &places(&[1])).is_some());
        assert!(view(&State::default(), &[]).is_none());
    }
}
