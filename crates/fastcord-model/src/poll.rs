//! Polls: the `poll` object of a message (question, answers, expiry and the
//! running results), bounded on decoding, and the pure vote arithmetic the
//! message store applies for `MESSAGE_POLL_VOTE_ADD`/`REMOVE`.

use std::fmt;
use std::mem::size_of;

use serde::{Deserialize, Deserializer};

use crate::Emoji;

/// Most answers one poll keeps (Discord allows 10).
pub const MAX_POLL_ANSWERS: usize = 10;
/// Characters of a question kept (Discord allows 300).
pub const MAX_POLL_QUESTION_CHARS: usize = 300;
/// Characters of an answer kept (Discord allows 55).
pub const MAX_POLL_ANSWER_CHARS: usize = 55;

/// The text (and optional emoji) of a question or an answer.
#[derive(Clone, PartialEq, Eq, serde::Serialize, Deserialize, Default)]
pub struct PollMedia {
    #[serde(default, deserialize_with = "lenient_text")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<Emoji>,
}

fn lenient_text<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let text = Option::<String>::deserialize(d)?.unwrap_or_default();
    Ok(text.chars().take(MAX_POLL_QUESTION_CHARS).collect())
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct PollAnswer {
    pub answer_id: u32,
    pub poll_media: PollMedia,
}

/// Votes for one answer, and whether the account is among them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct AnswerCount {
    pub id: u32,
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub me_voted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct PollResults {
    #[serde(default)]
    pub is_finalized: bool,
    #[serde(default, deserialize_with = "lenient_counts")]
    pub answer_counts: Vec<AnswerCount>,
}

fn lenient_counts<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<AnswerCount>, D::Error> {
    let mut counts = Vec::<AnswerCount>::deserialize(d)?;
    counts.truncate(MAX_POLL_ANSWERS);
    Ok(counts)
}

fn lenient_answers<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PollAnswer>, D::Error> {
    // One malformed answer must not discard the message.
    let values = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value::<PollAnswer>(value).ok())
        .map(|mut answer| {
            let cut = answer
                .poll_media
                .text
                .char_indices()
                .nth(MAX_POLL_ANSWER_CHARS);
            if let Some((end, _)) = cut {
                answer.poll_media.text.truncate(end);
            }
            answer
        })
        .take(MAX_POLL_ANSWERS)
        .collect())
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct Poll {
    #[serde(default)]
    pub question: PollMedia,
    #[serde(default, deserialize_with = "lenient_answers")]
    pub answers: Vec<PollAnswer>,
    /// When voting closes (RFC 3339); absent for a poll that never expires.
    #[serde(default)]
    pub expiry: Option<String>,
    #[serde(default)]
    pub allow_multiselect: bool,
    #[serde(default)]
    pub results: Option<PollResults>,
}

// Questions and answers are user text; they never reach a log.
impl fmt::Debug for Poll {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Poll")
            .field("answers", &self.answers.len())
            .field("finalized", &self.finalized())
            .finish_non_exhaustive()
    }
}

impl Poll {
    /// Owned bytes behind this poll, counting the boxed struct itself, at
    /// allocated capacity (the message store accounts by capacity).
    pub fn heap_bytes(&self) -> usize {
        let media = |m: &PollMedia| {
            m.text.capacity()
                + m.emoji
                    .as_ref()
                    .map_or(0, |e| e.name.as_ref().map_or(0, String::capacity))
        };
        size_of::<Self>()
            + media(&self.question)
            + self.answers.capacity() * size_of::<PollAnswer>()
            + self
                .answers
                .iter()
                .map(|a| media(&a.poll_media))
                .sum::<usize>()
            + self.expiry.as_ref().map_or(0, String::capacity)
            + self
                .results
                .as_ref()
                .map_or(0, |r| r.answer_counts.capacity() * size_of::<AnswerCount>())
    }

    /// Shrinks every allocation to its length, so [`heap_bytes`](Self::heap_bytes)
    /// is the real cost once stored.
    pub fn compact(&mut self) {
        fn media(m: &mut PollMedia) {
            m.text.shrink_to_fit();
            if let Some(name) = m.emoji.as_mut().and_then(|e| e.name.as_mut()) {
                name.shrink_to_fit();
            }
        }
        media(&mut self.question);
        self.answers.shrink_to_fit();
        for answer in &mut self.answers {
            media(&mut answer.poll_media);
        }
        if let Some(expiry) = &mut self.expiry {
            expiry.shrink_to_fit();
        }
        if let Some(results) = &mut self.results {
            results.answer_counts.shrink_to_fit();
        }
    }

    /// Whether Discord has counted the poll's final result.
    pub fn finalized(&self) -> bool {
        self.results.as_ref().is_some_and(|r| r.is_finalized)
    }

    /// Whether voting has closed: finalized, or past its expiry at `now_ms`
    /// (Unix milliseconds). An expiry that cannot be read is not "past".
    pub fn ended(&self, now_ms: u64) -> bool {
        self.finalized()
            || self
                .expiry
                .as_deref()
                .and_then(parse_unix_ms)
                .is_some_and(|expiry| expiry <= now_ms)
    }

    /// Votes for an answer and whether the account cast one.
    pub fn count(&self, id: u32) -> (u32, bool) {
        self.results
            .as_ref()
            .and_then(|r| r.answer_counts.iter().find(|c| c.id == id))
            .map_or((0, false), |c| (c.count, c.me_voted))
    }

    /// The answers the account voted for.
    pub fn my_votes(&self) -> Vec<u32> {
        self.answers
            .iter()
            .map(|a| a.answer_id)
            .filter(|&id| self.count(id).1)
            .collect()
    }

    /// Votes over all answers (a multi-select poll counts a voter per answer).
    pub fn total_votes(&self) -> u64 {
        self.results.as_ref().map_or(0, |r| {
            r.answer_counts.iter().map(|c| u64::from(c.count)).sum()
        })
    }

    /// What one vote event does, decided without changing anything: the new
    /// `(count, me)` of the answer, or `None` when it changes nothing. As for
    /// reactions, the account's own vote sets `me` and counts once, so the
    /// Gateway's echo of a vote a REST answer applied first (or the other way
    /// around) is a no-op; anyone else's only moves the count. An answer the
    /// poll does not have, or a removal with nothing to remove, is ignored.
    fn vote_edit(&self, answer: u32, by_me: bool, add: bool) -> Option<(u32, bool)> {
        if !self.answers.iter().any(|a| a.answer_id == answer) {
            return None;
        }
        let (count, me) = self.count(answer);
        let next = if add {
            if by_me && me {
                return None;
            }
            (count.saturating_add(1), me || by_me)
        } else {
            if (by_me && !me) || (!by_me && count == 0) {
                return None;
            }
            (count.saturating_sub(1), me && !by_me)
        };
        (next != (count, me)).then_some(next)
    }

    /// Whether [`apply_vote`](Self::apply_vote) would change the poll.
    pub fn alters_vote(&self, answer: u32, by_me: bool, add: bool) -> bool {
        self.vote_edit(answer, by_me, add).is_some()
    }

    /// Applies one vote event and reports whether the poll changed.
    pub fn apply_vote(&mut self, answer: u32, by_me: bool, add: bool) -> bool {
        let Some((count, me)) = self.vote_edit(answer, by_me, add) else {
            return false;
        };
        let results = self.results.get_or_insert_with(PollResults::default);
        match results.answer_counts.iter_mut().find(|c| c.id == answer) {
            Some(held) => {
                held.count = count;
                held.me_voted = me;
            }
            None => results.answer_counts.push(AnswerCount {
                id: answer,
                count,
                me_voted: me,
            }),
        }
        true
    }

    /// Makes the account's votes exactly `chosen`: the REST answer to
    /// "set my votes". Idempotent, so it composes with the Gateway's echo.
    pub fn set_my_votes(&mut self, chosen: &[u32]) -> bool {
        let ids: Vec<u32> = self.answers.iter().map(|a| a.answer_id).collect();
        let mut changed = false;
        for id in ids {
            let want = chosen.contains(&id);
            if self.count(id).1 != want {
                changed |= self.apply_vote(id, true, want);
            }
        }
        changed
    }

    /// Marks the poll as counted and closed.
    pub fn finalize(&mut self) {
        self.results
            .get_or_insert_with(PollResults::default)
            .is_finalized = true;
    }

    /// Replaces the poll by what Discord reported, except that a finalized
    /// poll is never reopened by an older copy. Reports whether it changed.
    pub fn replace(&mut self, newer: Poll) -> bool {
        if (self.finalized() && !newer.finalized()) || *self == newer {
            return false;
        }
        *self = newer;
        true
    }
}

/// Unix milliseconds of an RFC 3339 timestamp (`2026-10-21T12:00:00.000000+00:00`
/// or `...Z`), or `None` if it is not one.
pub fn parse_unix_ms(raw: &str) -> Option<u64> {
    let b = raw.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let digits = b.get(from..from + len)?;
        digits.iter().all(u8::is_ascii_digit).then(|| {
            digits
                .iter()
                .fold(0i64, |n, d| n * 10 + i64::from(d - b'0'))
        })
    };
    if b.get(4) != Some(&b'-')
        || b.get(7) != Some(&b'-')
        || !matches!(b.get(10), Some(b'T' | b't' | b' '))
    {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut at = 19;
    let mut millis = 0;
    if b.get(at) == Some(&b'.') {
        at += 1;
        let mut scale = 100;
        while let Some(d) = b.get(at).filter(|d| d.is_ascii_digit()) {
            millis += scale * i64::from(d - b'0');
            scale /= 10;
            at += 1;
        }
    }
    let offset_minutes = match b.get(at)? {
        b'Z' | b'z' if at + 1 == b.len() => 0,
        sign @ (b'+' | b'-') if b.get(at + 3) == Some(&b':') && at + 6 == b.len() => {
            let minutes = num(at + 1, 2)? * 60 + num(at + 4, 2)?;
            if *sign == b'+' { minutes } else { -minutes }
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    u64::try_from(seconds * 1_000 + millis).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll(json: serde_json::Value) -> Poll {
        serde_json::from_value(json).unwrap()
    }

    fn lunch() -> Poll {
        poll(serde_json::json!({
            "question": {"text": "Lunch?"},
            "answers": [
                {"answer_id": 1, "poll_media": {"text": "Pizza", "emoji": {"name": "\u{1f355}"}}},
                {"answer_id": 2, "poll_media": {"text": "Soup"}}
            ],
            "expiry": "2026-10-21T12:00:00.000000+00:00",
            "allow_multiselect": false,
            "layout_type": 1,
            "results": {"is_finalized": false, "answer_counts": [{"id": 1, "count": 3, "me_voted": true}]}
        }))
    }

    #[test]
    fn decodes_the_wire_poll_and_hides_text_from_debug() {
        let p = lunch();
        assert_eq!(p.question.text, "Lunch?");
        assert_eq!(p.count(1), (3, true));
        assert_eq!(p.count(2), (0, false));
        assert_eq!(p.my_votes(), vec![1]);
        assert_eq!(p.total_votes(), 3);
        assert!(!format!("{p:?}").contains("Pizza"));
        let again: Poll = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(again, p);
    }

    #[test]
    fn decoding_is_bounded_and_lenient() {
        let answers: Vec<_> = (1..=40)
            .map(|id| serde_json::json!({"answer_id": id, "poll_media": {"text": "x".repeat(500)}}))
            .collect();
        let mut wire = serde_json::json!({"question": {"text": "q"}, "answers": answers});
        wire["answers"]
            .as_array_mut()
            .unwrap()
            .insert(0, serde_json::json!({"nonsense": true}));
        let p = poll(wire);
        assert_eq!(p.answers.len(), MAX_POLL_ANSWERS);
        assert!(p.answers[0].poll_media.text.chars().count() <= MAX_POLL_ANSWER_CHARS);
        assert!(poll(serde_json::json!({})).answers.is_empty());
    }

    #[test]
    fn votes_count_once_and_the_echo_of_the_users_own_vote_changes_nothing() {
        let mut p = lunch();
        // Someone else votes for soup, then removes it again.
        assert!(p.apply_vote(2, false, true));
        assert_eq!(p.count(2), (1, false));
        assert!(p.apply_vote(2, false, false));
        assert_eq!(p.count(2), (0, false));
        assert!(!p.apply_vote(2, false, false), "nothing left to remove");
        // The user's own vote is already held: the Gateway echo is a no-op.
        assert!(!p.apply_vote(1, true, true));
        assert_eq!(p.count(1), (3, true));
        // Removing the user's own vote, then repeating the event.
        assert!(p.apply_vote(1, true, false));
        assert_eq!(p.count(1), (2, false));
        assert!(!p.apply_vote(1, true, false));
        // An unknown answer is ignored.
        assert!(!p.apply_vote(9, false, true));
        // Counts start from nothing when the poll came without results.
        let mut bare = lunch();
        bare.results = None;
        assert!(bare.apply_vote(1, false, true));
        assert_eq!(bare.count(1), (1, false));
    }

    #[test]
    fn setting_the_users_votes_is_idempotent_and_replaces_the_selection() {
        let mut p = lunch();
        assert!(p.set_my_votes(&[2]));
        assert_eq!((p.count(1), p.count(2)), ((2, false), (1, true)));
        assert!(!p.set_my_votes(&[2]), "second application changes nothing");
        assert!(p.set_my_votes(&[]));
        assert_eq!(p.my_votes(), Vec::<u32>::new());
    }

    #[test]
    fn a_finalized_poll_is_never_reopened_by_an_older_copy() {
        let mut p = lunch();
        let mut done = lunch();
        done.results.as_mut().unwrap().is_finalized = true;
        assert!(p.replace(done.clone()));
        assert!(p.finalized() && p.ended(0));
        assert!(!p.replace(lunch()));
        assert!(p.finalized());
        assert!(!p.replace(done), "identical copy changes nothing");
    }

    #[test]
    fn expiry_is_read_with_offsets_and_bad_text_is_not_expired() {
        let p = lunch();
        let expiry = parse_unix_ms("2026-10-21T12:00:00.000000+00:00").unwrap();
        assert_eq!(expiry, 1_792_584_000_000);
        assert!(!p.ended(expiry - 1) && p.ended(expiry));
        assert_eq!(
            parse_unix_ms("2026-10-21T15:00:00.5+03:00"),
            Some(expiry + 500)
        );
        assert_eq!(parse_unix_ms("2026-10-21T12:00:00Z"), Some(expiry));
        for bad in [
            "",
            "tomorrow",
            "2026-13-01T00:00:00Z",
            "2026-10-21T12:00:00",
        ] {
            assert_eq!(parse_unix_ms(bad), None, "{bad}");
        }
        let mut odd = lunch();
        odd.expiry = Some("soon".into());
        assert!(!odd.ended(u64::MAX));
    }
}
