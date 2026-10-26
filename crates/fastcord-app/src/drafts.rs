//! Per-conversation drafts, kept in memory for the signed-in session only
//! (milestone 50). A draft is everything the composer holds for one
//! conversation: the text, the reply target with its notification choice, the
//! chosen files (paths and metadata, never contents) and the `@everyone`
//! choice. Nothing here is written to disk; the store is dropped with the
//! account screen on logout.
//!
//! The store is bounded in count and in text bytes. The least recently left
//! draft is the first to go, and every eviction is counted so the composer can
//! say so: a draft is never lost silently.

use fastcord_discord::UploadFile;
use fastcord_model::Snowflake;

use crate::byte_lru::ByteLru;
use crate::outbox::Reply;

/// Conversations whose unsent draft is remembered while the user is elsewhere.
pub const MAX_DRAFTS: usize = 64;
/// UTF-8 bytes of draft text kept across all conversations.
pub const MAX_DRAFT_TEXT_BYTES: usize = 256 * 1024;

// The editor caps text at `MAX_EDITOR_CHARS` (UTF-8 at most 4 bytes each), so one
// draft always fits the budget and `put` never refuses a draft for its size.
const _: () = assert!(crate::composer::MAX_EDITOR_CHARS * 4 <= MAX_DRAFT_TEXT_BYTES);

/// What the composer held for one conversation.
#[derive(Default)]
pub struct Draft {
    pub text: String,
    pub files: Vec<UploadFile>,
    pub replying: Option<Reply>,
    pub everyone: bool,
}

impl Draft {
    /// Nothing worth keeping: blank text, no files, no reply. (The
    /// `@everyone` choice alone is not a draft.)
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.files.is_empty() && self.replying.is_none()
    }
}

pub struct DraftStore(ByteLru<Snowflake, Draft>);

impl Default for DraftStore {
    fn default() -> Self {
        Self(ByteLru::new(MAX_DRAFT_TEXT_BYTES, MAX_DRAFTS))
    }
}

impl DraftStore {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn contains(&self, channel: Snowflake) -> bool {
        self.0.contains_key(&channel)
    }

    #[cfg(test)]
    pub fn text_bytes(&self) -> usize {
        self.0.bytes()
    }

    /// Removes and returns the draft of `channel`.
    pub fn take(&mut self, channel: Snowflake) -> Option<Draft> {
        self.0.remove(&channel)
    }

    /// Keeps the draft of `channel` (an empty one removes it). Returns how
    /// many other drafts were discarded to stay within the budget. A draft
    /// whose text alone exceeds the budget is not kept and counts as one.
    pub fn put(&mut self, channel: Snowflake, draft: Draft) -> usize {
        self.take(channel);
        if draft.is_empty() {
            return 0;
        }
        let size = draft.text.len();
        self.0.insert(channel, draft, size).unwrap_or(1)
    }

    /// Forgets every draft (logout).
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Draft {
        Draft {
            text: value.to_owned(),
            ..Draft::default()
        }
    }

    #[test]
    fn put_and_take_round_trip_and_blank_drafts_are_not_kept() {
        let mut store = DraftStore::default();
        assert_eq!(store.put(Snowflake(1), text("hello")), 0);
        assert_eq!(store.put(Snowflake(2), text("  \n")), 0);
        assert_eq!(store.len(), 1);
        assert_eq!(store.take(Snowflake(1)).unwrap().text, "hello");
        assert!(store.take(Snowflake(1)).is_none());
        assert_eq!(store.text_bytes(), 0);
        // A reply with no text is still a draft.
        let reply = Reply {
            message: Snowflake(9),
            author: "a".into(),
            content: "b".into(),
            mention_author: true,
        };
        store.put(
            Snowflake(3),
            Draft {
                replying: Some(reply.clone()),
                ..Draft::default()
            },
        );
        assert_eq!(store.take(Snowflake(3)).unwrap().replying, Some(reply));
    }

    #[test]
    fn the_count_budget_evicts_the_least_recently_left_and_says_so() {
        let mut store = DraftStore::default();
        for id in 0..MAX_DRAFTS as u64 {
            assert_eq!(store.put(Snowflake(id), text("x")), 0);
        }
        assert_eq!(store.put(Snowflake(1_000), text("newest")), 1);
        assert_eq!(store.len(), MAX_DRAFTS);
        assert!(store.take(Snowflake(0)).is_none(), "the oldest went");
        assert!(store.take(Snowflake(1)).is_some());
        assert!(store.take(Snowflake(1_000)).is_some());
    }

    #[test]
    fn the_text_budget_evicts_old_drafts_but_never_the_one_just_left() {
        let mut store = DraftStore::default();
        let big = "y".repeat(MAX_DRAFT_TEXT_BYTES / 2 + 1);
        assert_eq!(store.put(Snowflake(1), text(&big)), 0);
        assert_eq!(store.put(Snowflake(2), text(&big)), 1);
        assert!(store.text_bytes() <= MAX_DRAFT_TEXT_BYTES);
        assert!(store.take(Snowflake(1)).is_none());
        assert!(store.take(Snowflake(2)).is_some());
        // Replacing a conversation's draft does not double count it.
        store.put(Snowflake(5), text("a"));
        store.put(Snowflake(5), text("bb"));
        assert_eq!(store.text_bytes(), 2);
    }

    #[test]
    fn clear_forgets_everything() {
        let mut store = DraftStore::default();
        store.put(Snowflake(1), text("secret"));
        store.clear();
        assert_eq!(store.len(), 0);
        assert_eq!(store.text_bytes(), 0);
    }
}
