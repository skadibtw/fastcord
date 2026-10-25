//! Poll votes, endings and voter lists on the history worker (SPEC §9). The
//! rules live in [`crate::polls`]; this is the part that checks the store and
//! makes the requests.

use std::sync::Arc;

use fastcord_discord::gateway::{Dispatch, MessagePollVote};
use fastcord_discord::{RestClient, RestError, is_own_message};
use fastcord_model::{Emoji, Message, MessageUpdate, Snowflake};

use super::{History, Inspection, NOT_LOADED, NOT_OPEN};
use crate::i18n::tr;
use crate::polls::{self, Kind};
use crate::timeline::UsersState;

const BAD_ANSWER: &str = "That answer is not part of the poll.";

/// Discord's answer to a poll request of the user.
pub struct PollDone {
    pub channel: Snowflake,
    pub message: Snowflake,
    pub kind: Kind,
    pub result: Result<PollAnswered, RestError>,
}

pub enum PollAnswered {
    Voted,
    Ended(Box<Message>),
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl History {
    /// The held message with a poll the user acts on, checked against the
    /// store and never against what a possibly stale UI showed.
    fn poll_target(
        &self,
        channel: Snowflake,
        message: Snowflake,
    ) -> Result<Arc<Message>, &'static str> {
        if self.channel != Some(channel) {
            return Err(NOT_OPEN);
        }
        let held = self.store.get(channel, message).ok_or(NOT_LOADED)?;
        if held.poll.is_none() {
            return Err(polls::NO_POLL);
        }
        Ok(held)
    }

    /// The user's click on a poll answer: toggles it in their selection (a
    /// single-choice poll replaces it) and sets the whole selection once.
    pub fn vote_poll(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        answer: u32,
    ) {
        self.dirty = true;
        self.notice = None;
        let held = match self.poll_target(channel, message) {
            Ok(held) => held,
            Err(notice) => {
                self.notice = Some(tr(notice));
                return;
            }
        };
        let Some(poll) = held.poll.as_deref() else {
            return;
        };
        if poll.ended(now_ms()) {
            self.notice = Some(tr(polls::ENDED));
            return;
        }
        if !poll.answers.iter().any(|a| a.answer_id == answer) {
            self.notice = Some(tr(BAD_ANSWER));
            return;
        }
        let mut chosen = poll.my_votes();
        if let Some(at) = chosen.iter().position(|&id| id == answer) {
            chosen.remove(at);
        } else if poll.allow_multiselect {
            chosen.push(answer);
            chosen.sort_unstable();
        } else {
            chosen = vec![answer];
        }
        let kind = Kind::Vote(chosen.clone());
        if let Err(notice) = self.polls.begin(channel, message) {
            self.notice = Some(tr(notice));
            return;
        }
        let rest = rest.clone();
        self.poll_requests.push(Box::pin(async move {
            let result = rest
                .set_poll_votes(channel, message, &chosen)
                .await
                .map(|()| PollAnswered::Voted);
            PollDone {
                channel,
                message,
                kind,
                result,
            }
        }));
    }

    /// The user's request to end their own poll now.
    pub fn end_poll(&mut self, rest: &RestClient, channel: Snowflake, message: Snowflake) {
        self.dirty = true;
        self.notice = None;
        let held = match self.poll_target(channel, message) {
            Ok(held) => held,
            Err(notice) => {
                self.notice = Some(tr(notice));
                return;
            }
        };
        if !self.user.is_some_and(|user| is_own_message(&held, user)) {
            self.notice = Some(tr(polls::NOT_YOURS));
            return;
        }
        if held.poll.as_deref().is_some_and(|p| p.ended(now_ms())) {
            self.notice = Some(tr(polls::ENDED));
            return;
        }
        if let Err(notice) = self.polls.begin(channel, message) {
            self.notice = Some(tr(notice));
            return;
        }
        let rest = rest.clone();
        self.poll_requests.push(Box::pin(async move {
            let result = rest
                .end_poll(channel, message)
                .await
                .map(|answer| PollAnswered::Ended(Box::new(answer)));
            PollDone {
                channel,
                message,
                kind: Kind::End,
                result,
            }
        }));
    }

    /// Applies Discord's answer to a poll request. Returns whether Discord
    /// rejected the login. A success goes into the store through the same path
    /// as the Gateway's report, which makes whichever arrives second a no-op;
    /// a failure changes nothing and is said once.
    pub(super) fn finish_poll(&mut self, done: PollDone) -> bool {
        let PollDone {
            channel,
            message,
            kind,
            result,
        } = done;
        self.dirty = true;
        self.polls.finish(channel, message);
        match (result, &kind) {
            (Ok(PollAnswered::Voted), Kind::Vote(chosen)) => {
                self.apply_own_votes(channel, message, chosen);
            }
            (Ok(PollAnswered::Ended(answer)), _) => {
                let held = self.store.get(channel, message);
                let mut poll = answer
                    .poll
                    .map(|poll| *poll)
                    .or_else(|| held.and_then(|h| h.poll.as_deref().cloned()));
                if let Some(poll) = poll.as_mut() {
                    poll.finalize();
                }
                if let Some(poll) = poll {
                    self.apply_store(&Dispatch::MessageUpdate(Box::new(
                        MessageUpdate::poll_only(message, channel, poll),
                    )));
                }
            }
            (Ok(PollAnswered::Voted), Kind::End) => {}
            (Err(error), _) => {
                self.notice = Some(tr(polls::failure(&kind, error)));
                return error == RestError::AuthenticationRequired;
            }
        }
        false
    }

    /// Makes the held poll show exactly `chosen` as the account's votes, as
    /// the Gateway's events for them would.
    fn apply_own_votes(&mut self, channel: Snowflake, message: Snowflake, chosen: &[u32]) {
        let Some(user) = self.user else {
            return;
        };
        let Some(held) = self.store.get(channel, message) else {
            return;
        };
        let Some(poll) = held.poll.as_deref() else {
            return;
        };
        let events: Vec<Dispatch> = poll
            .answers
            .iter()
            .filter(|a| poll.count(a.answer_id).1 != chosen.contains(&a.answer_id))
            .map(|a| {
                let vote = MessagePollVote {
                    user_id: user,
                    channel_id: channel,
                    message_id: message,
                    guild_id: None,
                    answer_id: a.answer_id,
                };
                if chosen.contains(&a.answer_id) {
                    Dispatch::MessagePollVoteAdd(vote)
                } else {
                    Dispatch::MessagePollVoteRemove(vote)
                }
            })
            .collect();
        for event in &events {
            self.apply_store(event);
        }
    }

    /// The user's request to see who voted for an answer. Shares the one list
    /// slot with "who reacted".
    pub fn poll_voters(
        &mut self,
        rest: &RestClient,
        channel: Snowflake,
        message: Snowflake,
        answer: u32,
    ) {
        self.dirty = true;
        self.notice = None;
        let held = match self.poll_target(channel, message) {
            Ok(held) => held,
            Err(notice) => {
                self.notice = Some(tr(notice));
                return;
            }
        };
        let Some(text) = held.poll.as_deref().and_then(|poll| {
            poll.answers
                .iter()
                .find(|a| a.answer_id == answer)
                .map(|a| a.poll_media.text.clone())
        }) else {
            self.notice = Some(tr(BAD_ANSWER));
            return;
        };
        self.inspections = self.inspections.wrapping_add(1);
        let serial = self.inspections;
        self.inspection = Some(Inspection {
            serial,
            channel,
            message,
            emoji: Emoji {
                id: None,
                name: None,
                animated: false,
            },
            poll_answer: Some(text),
            state: UsersState::Loading,
            confirm: None,
            busy: false,
        });
        let rest = rest.clone();
        let limit = u8::try_from(polls::MAX_VOTERS + 1).unwrap_or(u8::MAX);
        self.users_pending = Some(Box::pin(async move {
            (
                serial,
                rest.poll_voters(channel, message, answer, limit).await,
            )
        }));
    }
}
