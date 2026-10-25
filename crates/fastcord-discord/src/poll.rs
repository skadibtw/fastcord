//! Polls (SPEC §9, docs/PROTOCOL.md): creating one with a message, voting,
//! ending one's own poll and listing who voted for an answer.
//!
//! * Create: a `poll` object in `POST /channels/{id}/messages`
//!   ([`PollDraft`], sent through [`crate::OutgoingMessage::with_poll`]).
//! * Vote: `PUT /channels/{id}/polls/{message}/answers/@me` with the complete
//!   set of answers the account wants (`answer_ids`, empty to withdraw). The
//!   request sets a state rather than adding to one, so repeating it after an
//!   ambiguous failure cannot vote twice.
//! * End: `POST /channels/{id}/polls/{message}/expire`; Discord answers with
//!   the message.
//! * Voters: `GET /channels/{id}/polls/{message}/answers/{answer}?limit=n`,
//!   only when the user asks.
//!
//! Each operation has one route and is sent once; the caller owns any retry.

use std::fmt;

use fastcord_model::{
    MAX_POLL_ANSWER_CHARS, MAX_POLL_ANSWERS, MAX_POLL_QUESTION_CHARS, Message, Snowflake,
};
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserializer, Serialize};

use crate::reaction::UserList;
use crate::{
    Clock, Method, Priority, ReactionUser, RestClient, RestError, RestRequest, Route,
    edit::parse_edited,
};

/// Longest poll Discord accepts: 32 days.
pub const POLL_MAX_HOURS: u32 = 768;
/// Most voters one list holds.
const MAX_VOTERS: u8 = 100;

/// Why a poll cannot be created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollError {
    NoQuestion,
    QuestionTooLong,
    /// Fewer than two usable answers.
    TooFewAnswers,
    TooManyAnswers,
    AnswerTooLong,
    /// Zero hours or more than [`POLL_MAX_HOURS`].
    BadDuration,
}

impl fmt::Display for PollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoQuestion => "a poll needs a question",
            Self::QuestionTooLong => "the question is too long",
            Self::TooFewAnswers => "a poll needs at least two answers",
            Self::TooManyAnswers => "a poll has at most ten answers",
            Self::AnswerTooLong => "an answer is too long",
            Self::BadDuration => "the poll's duration is not allowed",
        })
    }
}

impl std::error::Error for PollError {}

#[derive(Serialize, Clone, PartialEq, Eq)]
struct Media {
    text: String,
}

#[derive(Serialize, Clone, PartialEq, Eq)]
struct Answer {
    poll_media: Media,
}

/// A validated poll ready to be sent with a message. `Debug` never prints text.
#[derive(Serialize, Clone, PartialEq, Eq)]
pub struct PollDraft {
    question: Media,
    answers: Vec<Answer>,
    /// Hours the poll stays open.
    duration: u32,
    allow_multiselect: bool,
    /// `1` is the default layout.
    layout_type: u8,
}

impl PollDraft {
    /// Surrounding whitespace is trimmed; blank answers are dropped (the
    /// composer leaves unused fields empty). The server stays authoritative
    /// about everything else.
    pub fn new(
        question: &str,
        answers: &[String],
        duration_hours: u32,
        allow_multiselect: bool,
    ) -> Result<Self, PollError> {
        let question = question.trim();
        if question.is_empty() {
            return Err(PollError::NoQuestion);
        }
        if question.chars().count() > MAX_POLL_QUESTION_CHARS {
            return Err(PollError::QuestionTooLong);
        }
        let answers: Vec<&str> = answers
            .iter()
            .map(|answer| answer.trim())
            .filter(|answer| !answer.is_empty())
            .collect();
        if answers.len() < 2 {
            return Err(PollError::TooFewAnswers);
        }
        if answers.len() > MAX_POLL_ANSWERS {
            return Err(PollError::TooManyAnswers);
        }
        if answers
            .iter()
            .any(|answer| answer.chars().count() > MAX_POLL_ANSWER_CHARS)
        {
            return Err(PollError::AnswerTooLong);
        }
        if duration_hours == 0 || duration_hours > POLL_MAX_HOURS {
            return Err(PollError::BadDuration);
        }
        Ok(Self {
            question: Media {
                text: question.to_owned(),
            },
            answers: answers
                .into_iter()
                .map(|text| Answer {
                    poll_media: Media {
                        text: text.to_owned(),
                    },
                })
                .collect(),
            duration: duration_hours,
            allow_multiselect,
            layout_type: 1,
        })
    }
}

impl fmt::Debug for PollDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PollDraft")
            .field("answers", &self.answers.len())
            .field("duration", &self.duration)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct VoteBody {
    answer_ids: Vec<String>,
}

fn vote_route(channel: Snowflake, message: Snowflake) -> Result<Route, RestError> {
    Route::new(
        Method::PUT,
        &format!("/channels/{channel}/polls/{message}/answers/@me"),
    )
}

fn vote_body(answers: &[u32]) -> VoteBody {
    VoteBody {
        answer_ids: answers.iter().map(u32::to_string).collect(),
    }
}

fn expire_route(channel: Snowflake, message: Snowflake) -> Result<Route, RestError> {
    Route::new(
        Method::POST,
        &format!("/channels/{channel}/polls/{message}/expire"),
    )
}

fn voters_route(
    channel: Snowflake,
    message: Snowflake,
    answer: u32,
    limit: u8,
) -> Result<Route, RestError> {
    Route::new(
        Method::GET,
        &format!("/channels/{channel}/polls/{message}/answers/{answer}?limit={limit}"),
    )
}

/// `{"users": [...]}`; everything else is skipped, and the list never holds
/// more than `limit` users whatever Discord sends.
struct Voters {
    limit: usize,
}

impl<'de> Visitor<'de> for Voters {
    type Value = Vec<ReactionUser>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object with a list of users")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut users = Vec::new();
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            if key == "users" {
                users = map.next_value_seed(UserList { limit: self.limit })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(users)
    }
}

fn parse_voters(body: &[u8], limit: usize) -> Result<Vec<ReactionUser>, RestError> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let users = deserializer
        .deserialize_map(Voters { limit })
        .map_err(|_| RestError::InvalidJson)?;
    deserializer.end().map_err(|_| RestError::InvalidJson)?;
    Ok(users)
}

impl<C: Clock> RestClient<C> {
    /// Sets the account's votes in a poll to exactly `answers` (empty
    /// withdraws them). Sent once at user-write priority.
    pub async fn set_poll_votes(
        &self,
        channel: Snowflake,
        message: Snowflake,
        answers: &[u32],
    ) -> Result<(), RestError> {
        let request = RestRequest::new(vote_route(channel, message)?, Priority::UserWrite)
            .json(&vote_body(answers))?;
        self.execute(request).await.map(drop)
    }

    /// Ends the account's own poll now and returns the message as Discord has
    /// it. Sent once.
    pub async fn end_poll(
        &self,
        channel: Snowflake,
        message: Snowflake,
    ) -> Result<Message, RestError> {
        let request = RestRequest::new(expire_route(channel, message)?, Priority::UserWrite);
        let response = self.execute(request).await?;
        parse_edited(response.body(), channel, message)
    }

    /// The first voters of one answer, at most `limit` (clamped to 1 to 100).
    pub async fn poll_voters(
        &self,
        channel: Snowflake,
        message: Snowflake,
        answer: u32,
        limit: u8,
    ) -> Result<Vec<ReactionUser>, RestError> {
        let limit = limit.clamp(1, MAX_VOTERS);
        let request = RestRequest::new(
            voters_route(channel, message, answer, limit)?,
            Priority::UserRead,
        );
        let response = self.execute(request).await?;
        parse_voters(response.body(), usize::from(limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNEL: Snowflake = Snowflake(500);
    const MESSAGE: Snowflake = Snowflake(1_290_000_000_000_000_010);

    fn texts(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_create_body_matches_the_documented_poll_object() {
        let draft = PollDraft::new(" Lunch? ", &texts(&["Pizza", " ", "Soup "]), 24, true).unwrap();
        let sent = serde_json::to_value(&draft).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({
                "question": {"text": "Lunch?"},
                "answers": [
                    {"poll_media": {"text": "Pizza"}},
                    {"poll_media": {"text": "Soup"}}
                ],
                "duration": 24,
                "allow_multiselect": true,
                "layout_type": 1
            })
        );
        assert!(!format!("{draft:?}").contains("Pizza"));
    }

    #[test]
    fn drafts_obey_the_documented_limits() {
        let ok = texts(&["a", "b"]);
        let many: Vec<String> = (0..=MAX_POLL_ANSWERS).map(|n| n.to_string()).collect();
        let long = texts(&["a", &"x".repeat(MAX_POLL_ANSWER_CHARS + 1)]);
        let cases = [
            (PollDraft::new("  ", &ok, 24, false), PollError::NoQuestion),
            (
                PollDraft::new(&"q".repeat(MAX_POLL_QUESTION_CHARS + 1), &ok, 24, false),
                PollError::QuestionTooLong,
            ),
            (
                PollDraft::new("q", &texts(&["a", "  "]), 24, false),
                PollError::TooFewAnswers,
            ),
            (
                PollDraft::new("q", &many, 24, false),
                PollError::TooManyAnswers,
            ),
            (
                PollDraft::new("q", &long, 24, false),
                PollError::AnswerTooLong,
            ),
            (PollDraft::new("q", &ok, 0, false), PollError::BadDuration),
            (
                PollDraft::new("q", &ok, POLL_MAX_HOURS + 1, false),
                PollError::BadDuration,
            ),
        ];
        for (result, error) in cases {
            assert_eq!(result.err(), Some(error));
        }
        assert!(PollDraft::new("q", &ok, POLL_MAX_HOURS, false).is_ok());
    }

    #[test]
    fn each_operation_has_one_route_and_a_vote_sets_the_whole_selection() {
        let vote = vote_route(CHANNEL, MESSAGE).unwrap();
        assert_eq!(vote.key().method().as_str(), "PUT");
        assert_eq!(
            vote.url.as_str(),
            "https://discord.com/api/v9/channels/500/polls/1290000000000000010/answers/@me"
        );
        assert_eq!(
            serde_json::to_value(vote_body(&[1, 3])).unwrap(),
            serde_json::json!({"answer_ids": ["1", "3"]})
        );
        assert_eq!(
            serde_json::to_value(vote_body(&[])).unwrap(),
            serde_json::json!({"answer_ids": []})
        );
        let end = expire_route(CHANNEL, MESSAGE).unwrap();
        assert_eq!(end.key().method().as_str(), "POST");
        assert!(
            end.url
                .as_str()
                .ends_with("/polls/1290000000000000010/expire")
        );
        let voters = voters_route(CHANNEL, MESSAGE, 2, 25).unwrap();
        assert_eq!(voters.key().method().as_str(), "GET");
        assert!(voters.url.as_str().ends_with("/answers/2?limit=25"));
    }

    #[test]
    fn voters_are_bounded_and_unexpected_bodies_are_invalid() {
        let body = br#"{"users":[{"id":"1","username":"a","global_name":"Ann","avatar":"x"},
            {"id":"2","username":"b"},{"id":"3","username":"c"}],"other":[1,2]}"#;
        let voters = parse_voters(body, 2).unwrap();
        assert_eq!(voters.len(), 2);
        assert_eq!(voters[0].name, "Ann");
        assert_eq!(voters[1].name, "b");
        assert!(parse_voters(br#"{}"#, 5).unwrap().is_empty());
        let recorded = parse_voters(
            include_bytes!("../../../fixtures/rest/poll-voters.json"),
            100,
        )
        .unwrap();
        assert_eq!(
            recorded.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(),
            ["Alt Account", "other_fixture"]
        );
        for bad in [&b"[]"[..], b"{\"users\":3}", b"{\"users\":[]} x", b""] {
            assert_eq!(parse_voters(bad, 5), Err(RestError::InvalidJson));
        }
    }
}
