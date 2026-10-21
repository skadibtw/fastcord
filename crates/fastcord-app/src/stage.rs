//! The Stage side of a call: the account's own seat (request to speak, become a
//! speaker, move to the audience), the live session's topic and, for
//! moderators, start/edit/end and the raised hands. Every write is one click;
//! the reducer rechecks it against the current state before sending, and a
//! refusal is shown with Discord's own reason.
use crate::gateway::{StageControls, VoiceControls};
use crate::{Message, friends, style};
use fastcord_discord::state::navigation::StageSeat;
use fastcord_discord::{MAX_TOPIC_CHARS, RestError, StageAction};
use iced::widget::{button, column, container, row, space, text, text_input};
use iced::{Element, Font, Length};

const BOLD: Font = Font {
    weight: iced::font::Weight::Bold,
    ..Font::DEFAULT
};

/// Most raised hands listed at once; the rest are a count.
const MAX_HANDS: usize = 8;

#[derive(Clone, Debug)]
pub enum Event {
    /// One explicit action on the Stage (a button or a member-menu entry).
    Do(StageAction),
    Done(u64, Result<(), RestError>),
    /// Opens the topic field for a new session (`false`) or to edit (`true`).
    OpenTopic(bool),
    Topic(String),
    CloseTopic,
    SubmitTopic,
}

#[derive(Debug, Default)]
pub struct State {
    pub pending: Option<u64>,
    pub serial: u64,
    pub notice: Option<String>,
    /// The topic field: (editing a live session, text).
    pub topic: Option<(bool, String)>,
}

impl State {
    /// A new account, a left call or another Stage: nothing carries over, and a
    /// late answer to an older request is ignored.
    pub fn reset(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            ..Self::default()
        };
    }

    /// The action a topic submission stands for.
    pub fn submission(&self) -> Option<StageAction> {
        self.topic.as_ref().map(|(editing, text)| {
            if *editing {
                StageAction::SetTopic(text.clone())
            } else {
                StageAction::Start(text.clone())
            }
        })
    }

    /// Begins one write; `None` while another is still unanswered.
    pub fn begin(&mut self) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        self.serial = self.serial.wrapping_add(1);
        self.pending = Some(self.serial);
        self.notice = None;
        Some(self.serial)
    }

    pub fn finish(&mut self, serial: u64, result: Result<(), RestError>) {
        if self.pending != Some(serial) {
            return;
        }
        self.pending = None;
        match result {
            Ok(()) => self.topic = None,
            Err(error) => self.notice = Some(failure(error)),
        }
    }
}

/// What a refused or failed Stage request tells the user.
pub fn failure(error: RestError) -> String {
    match error {
        RestError::PermissionDenied => {
            "That is not allowed right now. The Stage or your role may have changed.".to_owned()
        }
        RestError::ResourceGone => "The Stage session no longer exists.".to_owned(),
        other => friends::failure(other),
    }
}

fn act(
    label: &'static str,
    action: StageAction,
    enabled: bool,
    danger: bool,
) -> Element<'static, Message> {
    button(text(label).size(12))
        .on_press_maybe(enabled.then_some(Message::Stage(Event::Do(action))))
        .style(style::tool_button(false, danger))
        .into()
}

/// The account's own seat, above the voice panel: shown only in a Stage call.
pub fn own<'a>(
    state: &State,
    voice: &VoiceControls,
    restricted: bool,
) -> Option<Element<'a, Message>> {
    let stage = &voice.stage;
    if !stage.active {
        return None;
    }
    let free = !restricted && state.pending.is_none();
    let permissions = stage.permissions;
    let (role, buttons): (&str, Element<'a, Message>) = if !stage.suppressed {
        (
            "You are a speaker",
            act("Move to audience", StageAction::MoveToAudience, free, false),
        )
    } else if stage.requested {
        (
            "Hand raised: waiting for a moderator",
            row![
                act("Cancel request", StageAction::CancelRequest, free, false),
                act("Become speaker", StageAction::BecomeSpeaker, free, false),
            ]
            .spacing(4)
            .into(),
        )
    } else {
        (
            "You are in the audience",
            row![
                act(
                    "Request to speak",
                    StageAction::RequestToSpeak,
                    free && permissions.request,
                    false
                ),
                act(
                    "Become speaker",
                    StageAction::BecomeSpeaker,
                    free && permissions.moderate,
                    false
                ),
            ]
            .spacing(4)
            .into(),
        )
    };
    let mut content = column![text(role).size(12).font(BOLD), buttons].spacing(4);
    if suppressed_without_right(stage) {
        content = content.push(
            text("You cannot ask to speak in this Stage.")
                .size(11)
                .style(text::secondary),
        );
    }
    if let Some(notice) = &state.notice {
        content = content.push(text(notice.clone()).size(11).style(text::danger));
    }
    Some(
        container(content)
            .padding([6, 8])
            .width(Length::Fill)
            .style(style::footer)
            .into(),
    )
}

fn suppressed_without_right(stage: &StageControls) -> bool {
    stage.suppressed
        && !stage.requested
        && !stage.permissions.request
        && !stage.permissions.moderate
}

/// The Stage header above the conversation: the topic, the moderator's
/// Start/Edit/End and the raised hands waiting for approval.
pub fn banner<'a>(
    state: &'a State,
    voice: &VoiceControls,
    restricted: bool,
) -> Option<Element<'a, Message>> {
    let stage = &voice.stage;
    if !stage.active {
        return None;
    }
    let free = !restricted && state.pending.is_none();
    let manage = stage.permissions.manage;
    let live = stage.topic.is_some();
    let heading = match &stage.topic {
        Some(topic) => text(topic.clone()).size(15).font(BOLD),
        None => text("No live Stage session")
            .size(14)
            .style(text::secondary),
    };
    let mut header = row![container(heading).width(Length::Fill)]
        .spacing(6)
        .align_y(iced::Alignment::Center);
    if manage && state.topic.is_none() {
        if live {
            header = header
                .push(
                    button(text("Edit topic").size(12))
                        .on_press_maybe(free.then_some(Message::Stage(Event::OpenTopic(true))))
                        .style(style::tool_button(false, false)),
                )
                .push(act("End Stage", StageAction::End, free, true));
        } else {
            header = header.push(
                button(text("Start Stage").size(12))
                    .on_press_maybe(free.then_some(Message::Stage(Event::OpenTopic(false))))
                    .style(style::tool_button(false, false)),
            );
        }
    }
    let mut content = column![header].spacing(6);
    if let Some((editing, draft)) = &state.topic {
        let valid = fastcord_discord::stage_topic(draft).is_some();
        content = content.push(
            row![
                text_input("Topic", draft)
                    .on_input(|value| Message::Stage(Event::Topic(value)))
                    .on_submit_maybe((free && valid).then_some(Message::Stage(Event::SubmitTopic)))
                    .size(13),
                button(text(if *editing { "Save" } else { "Start" }).size(12))
                    .on_press_maybe((free && valid).then_some(Message::Stage(Event::SubmitTopic)))
                    .style(style::tool_button(false, false)),
                button(text("Cancel").size(12))
                    .on_press(Message::Stage(Event::CloseTopic))
                    .style(style::tool_button(false, false)),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );
        content = content.push(
            text(format!(
                "{}/{MAX_TOPIC_CHARS} characters",
                draft.chars().count()
            ))
            .size(11)
            .style(text::secondary),
        );
    }
    let hands: Vec<_> = stage
        .seats
        .iter()
        .filter(|(_, seat)| *seat == StageSeat::Hand)
        .collect();
    if stage.permissions.moderate && !hands.is_empty() {
        content = content.push(
            text(format!("RAISED HANDS ({})", hands.len()))
                .size(11)
                .font(BOLD)
                .style(text::secondary),
        );
        for (user, _) in hands.iter().take(MAX_HANDS) {
            content = content.push(
                row![
                    container(text(voice.display_name(*user)).size(13)).width(Length::Fill),
                    act("Approve", StageAction::Invite(*user), free, false),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center),
            );
        }
        if hands.len() > MAX_HANDS {
            content = content.push(
                text(format!("+{} more", hands.len() - MAX_HANDS))
                    .size(11)
                    .style(text::secondary),
            );
        }
    }
    if let Some(notice) = &state.notice {
        content = content.push(text(notice.as_str()).size(12).style(text::danger));
    }
    content = content.push(space::vertical().height(0));
    Some(
        container(content)
            .padding([8, 12])
            .width(Length::Fill)
            .style(style::footer)
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_write_at_a_time_and_a_stale_answer_is_ignored() {
        let mut state = State::default();
        let first = state.begin().unwrap();
        assert!(state.begin().is_none(), "the first is still unanswered");
        state.finish(first + 1, Ok(()));
        assert_eq!(state.pending, Some(first), "an answer to another request");
        state.finish(first, Err(RestError::PermissionDenied));
        assert!(state.pending.is_none());
        assert!(state.notice.as_deref().unwrap().contains("not allowed"));
        // A new account or Stage forgets the request, so its late answer is dropped.
        let second = state.begin().unwrap();
        state.reset();
        state.finish(second, Err(RestError::PermissionDenied));
        assert!(state.notice.is_none() && state.pending.is_none());
    }

    #[test]
    fn a_topic_submission_starts_or_edits() {
        let mut state = State::default();
        assert_eq!(state.submission(), None);
        state.topic = Some((false, "Hello".into()));
        assert_eq!(state.submission(), Some(StageAction::Start("Hello".into())));
        state.topic = Some((true, "Bye".into()));
        assert_eq!(
            state.submission(),
            Some(StageAction::SetTopic("Bye".into()))
        );
        state.finish(0, Ok(()));
        assert!(state.topic.is_some(), "an unrelated answer keeps the draft");
        let serial = state.begin().unwrap();
        state.finish(serial, Ok(()));
        assert!(state.topic.is_none(), "success closes the field");
    }

    #[test]
    fn own_controls_follow_the_seat_and_the_permissions() {
        let mut voice = VoiceControls::default();
        let state = State::default();
        assert!(own(&state, &voice, false).is_none(), "not in a Stage");
        voice.stage.active = true;
        voice.stage.suppressed = true;
        assert!(own(&state, &voice, false).is_some());
        assert!(banner(&state, &voice, false).is_some());
    }

    #[test]
    fn a_suppressed_account_never_transmits_and_a_speaker_follows_the_other_gates() {
        let mut voice = VoiceControls::default();
        assert!(voice.capture_enabled_with(true));
        voice.stage.suppressed = true;
        assert!(!voice.capture_enabled_with(true), "audience is silent");
        assert!(!voice.capture_enabled_with(false));
        voice.stage.suppressed = false;
        assert!(voice.capture_enabled_with(true));
        voice.self_muted = true;
        assert!(
            !voice.capture_enabled_with(true),
            "a speaker can still mute"
        );
    }
}
