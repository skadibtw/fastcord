//! Cache behavior against sanitized fixtures and generated histories. Pages go
//! through the real REST page decoder; Gateway events are the real typed ones.

use std::ops::RangeInclusive;
use std::sync::LazyLock;

use fastcord_model::{Emoji, MAX_REACTIONS};

use super::*;
use crate::gateway::{
    MessageDelete, MessageDeleteBulk, MessageReaction, MessageReactionRemoveAll,
    MessageReactionRemoveEmoji, decode_fixture,
};
use crate::history::parse_page;

const CHANNEL: Snowflake = Snowflake(500);
const OTHER: Snowflake = Snowflake(501);

const MESSAGE_CREATE: &str = include_str!("../../../../fixtures/model/message_create.json");
const MESSAGE_UPDATE_EDIT: &str =
    include_str!("../../../../fixtures/model/message_update_edit.json");
const MESSAGE_UPDATE_PARTIAL: &str =
    include_str!("../../../../fixtures/model/message_update_partial.json");
const MESSAGE_DELETE_BULK: &str =
    include_str!("../../../../fixtures/gateway/message_delete_bulk.json");
const REACTION_ADD: &str = include_str!("../../../../fixtures/gateway/message_reaction_add.json");
const REACTION_ADD_CUSTOM: &str =
    include_str!("../../../../fixtures/gateway/message_reaction_add_custom.json");
const REACTION_ADD_BURST: &str =
    include_str!("../../../../fixtures/gateway/message_reaction_add_burst.json");
const REACTION_REMOVE: &str =
    include_str!("../../../../fixtures/gateway/message_reaction_remove.json");
const REACTION_REMOVE_ALL: &str =
    include_str!("../../../../fixtures/gateway/message_reaction_remove_all.json");
const REACTION_REMOVE_EMOJI: &str =
    include_str!("../../../../fixtures/gateway/message_reaction_remove_emoji.json");
/// The message every recorded reaction event addresses.
const REACTED: u64 = 1_290_000_000_000_000_010;

/// The account the store under test belongs to, and anyone else.
const ME: Snowflake = Snowflake(2);
const THEM: Snowflake = Snowflake(3);
const UP: &str = "\u{1f44d}";
const DOWN: &str = "\u{1f44e}";

fn id(n: u64) -> Snowflake {
    Snowflake(n)
}

/// The richest message of the sanitized REST fixture (a reply with an
/// attachment), re-addressed per generated message.
static TEMPLATE: LazyLock<Message> = LazyLock::new(|| {
    let page: Vec<Message> = serde_json::from_str(include_str!(
        "../../../../fixtures/rest/channel-messages.json"
    ))
    .unwrap();
    page.into_iter().last().unwrap()
});

fn message_in(channel: Snowflake, n: u64) -> Message {
    let mut message = Message::clone(&TEMPLATE);
    message.id = id(n);
    message.channel_id = channel;
    message.content = format!("generated message {n}");
    message
}

fn message(n: u64) -> Message {
    message_in(CHANNEL, n)
}

/// A page as the server sends it: newest first.
fn page_in(channel: Snowflake, ids: RangeInclusive<u64>) -> Vec<Message> {
    ids.rev().map(|n| message_in(channel, n)).collect()
}

fn page(ids: RangeInclusive<u64>) -> Vec<Message> {
    page_in(CHANNEL, ids)
}

/// Encodes a page and decodes it with the real REST page decoder.
fn through_rest(channel: Snowflake, cursor: HistoryCursor, messages: &[Message]) -> Vec<Message> {
    let body = serde_json::to_vec(messages).unwrap();
    parse_page(&body, channel, cursor).unwrap()
}

fn load_in(
    store: &mut MessageStore,
    channel: Snowflake,
    cursor: HistoryCursor,
    ids: RangeInclusive<u64>,
) -> PageMerge {
    let token = store.begin_page(channel, cursor);
    let messages = through_rest(channel, cursor, &page_in(channel, ids));
    store.merge_page(token, messages)
}

fn load(store: &mut MessageStore, cursor: HistoryCursor, ids: RangeInclusive<u64>) -> PageMerge {
    load_in(store, CHANNEL, cursor, ids)
}

/// The server's answer when there is nothing in that direction: `[]`.
fn load_empty(store: &mut MessageStore, cursor: HistoryCursor) -> PageMerge {
    let token = store.begin_page(CHANNEL, cursor);
    let messages = through_rest(CHANNEL, cursor, &[]);
    store.merge_page(token, messages)
}

fn applied(merge: PageMerge) -> (usize, usize, usize) {
    match merge {
        PageMerge::Applied {
            received,
            held,
            rejected,
        } => (received, held, rejected),
        PageMerge::Stale => panic!("the page was stale"),
    }
}

fn ids_of(store: &MessageStore, channel: Snowflake) -> Vec<u64> {
    store.ids(channel).map(|id| id.0).collect()
}

fn contiguous(ids: &[u64]) -> bool {
    ids.windows(2).all(|pair| pair[1] == pair[0] + 1)
}

/// Every invariant the cache promises, including a from-scratch byte recount.
fn check(store: &MessageStore) {
    assert_eq!(
        store.tracked_bytes(),
        store.recount(),
        "the byte ledger drifted"
    );
    assert!(store.tracked_bytes() <= MESSAGE_BUDGET);
    assert!(store.journal_bytes() <= JOURNAL_BUDGET);
    assert!(store.cache_bytes() <= CACHE_BUDGET);
    assert!(store.messages_held() <= MAX_MESSAGES);
    assert!(store.channels_held() <= MAX_CHANNELS);
    assert!(store.pending_pages() <= MAX_PENDING_PAGES);
    for cache in &store.channels {
        assert!(cache.messages.len() <= MAX_MESSAGES_PER_CHANNEL);
        assert!(
            cache
                .messages
                .iter()
                .zip(cache.messages.iter().skip(1))
                .all(|(earlier, later)| earlier.id < later.id),
            "IDs are ascending and unique"
        );
        assert_eq!(
            cache.slot_bytes,
            cache.messages.iter().map(|slot| slot.bytes).sum::<usize>()
        );
        for slot in &cache.messages {
            assert_eq!(slot.id, slot.message.id);
            assert_eq!(slot.message.channel_id, cache.id);
            assert!(slot.bytes <= MAX_MESSAGE_BYTES);
        }
        match (
            cache.messages.front(),
            cache.messages.back(),
            cache.coverage,
        ) {
            (Some(first), Some(last), Some((lo, hi))) => {
                assert!(
                    lo <= first.id.0 && last.id.0 <= hi,
                    "coverage spans the run"
                );
            }
            (None, None, _) => {}
            _ => panic!("messages without coverage"),
        }
        if cache.coverage.is_none() {
            assert!(
                cache.live && cache.older_exhausted,
                "only a proven-empty channel"
            );
        }
    }
}

/// Loads the newest 500 messages the way a viewport scrolling up does.
fn fill(store: &mut MessageStore, channel: Snowflake, top: u64) {
    load_in(store, channel, HistoryCursor::Latest, top - 49..=top);
    for _ in 0..9 {
        let window = store.window(channel).unwrap();
        let Some(HistoryCursor::Before(edge)) = window.older else {
            panic!("history ended early");
        };
        store.set_focus(channel, window.oldest);
        load_in(
            store,
            channel,
            HistoryCursor::Before(edge),
            edge.0 - 50..=edge.0 - 1,
        );
    }
}

fn create(channel: Snowflake, n: u64) -> Dispatch {
    Dispatch::MessageCreate(Box::new(message_in(channel, n)))
}

fn update(channel: Snowflake, n: u64, fields: &str) -> Dispatch {
    let raw = format!(r#"{{"id":"{n}","channel_id":"{channel}"{fields}}}"#);
    Dispatch::MessageUpdate(Box::new(serde_json::from_str(&raw).unwrap()))
}

fn delete(channel: Snowflake, n: u64) -> Dispatch {
    Dispatch::MessageDelete(MessageDelete {
        id: id(n),
        channel_id: channel,
        guild_id: None,
    })
}

fn bulk(channel: Snowflake, ids: impl IntoIterator<Item = u64>) -> Dispatch {
    Dispatch::MessageDeleteBulk(MessageDeleteBulk {
        ids: ids.into_iter().map(id).collect(),
        channel_id: channel,
        guild_id: None,
    })
}

fn unicode(name: &str) -> Emoji {
    Emoji {
        id: None,
        name: Some(name.to_owned()),
        animated: false,
    }
}

fn custom(emoji: u64, name: &str) -> Emoji {
    Emoji {
        id: Some(id(emoji)),
        name: Some(name.to_owned()),
        animated: true,
    }
}

fn reaction(
    channel: Snowflake,
    n: u64,
    user: Snowflake,
    emoji: &Emoji,
    burst: bool,
) -> Box<MessageReaction> {
    Box::new(MessageReaction {
        channel_id: channel,
        message_id: id(n),
        guild_id: None,
        user_id: user,
        emoji: emoji.clone(),
        burst,
    })
}

fn react(channel: Snowflake, n: u64, user: Snowflake, emoji: &Emoji) -> Dispatch {
    Dispatch::MessageReactionAdd(reaction(channel, n, user, emoji, false))
}

fn burst_react(channel: Snowflake, n: u64, user: Snowflake, emoji: &Emoji) -> Dispatch {
    Dispatch::MessageReactionAdd(reaction(channel, n, user, emoji, true))
}

fn unreact(channel: Snowflake, n: u64, user: Snowflake, emoji: &Emoji) -> Dispatch {
    Dispatch::MessageReactionRemove(reaction(channel, n, user, emoji, false))
}

fn burst_unreact(channel: Snowflake, n: u64, user: Snowflake, emoji: &Emoji) -> Dispatch {
    Dispatch::MessageReactionRemove(reaction(channel, n, user, emoji, true))
}

fn remove_emoji(channel: Snowflake, n: u64, emoji: &Emoji) -> Dispatch {
    Dispatch::MessageReactionRemoveEmoji(Box::new(MessageReactionRemoveEmoji {
        channel_id: channel,
        message_id: id(n),
        guild_id: None,
        emoji: emoji.clone(),
    }))
}

fn remove_all(channel: Snowflake, n: u64) -> Dispatch {
    Dispatch::MessageReactionRemoveAll(MessageReactionRemoveAll {
        channel_id: channel,
        message_id: id(n),
        guild_id: None,
    })
}

/// What an event that changed nothing reports.
fn unchanged(channel: Snowflake) -> StoreChange {
    StoreChange {
        channel: Some(channel),
        changed: false,
        rejected: 0,
    }
}

/// What an event that changed a retained message reports.
fn changed(channel: Snowflake) -> StoreChange {
    StoreChange {
        channel: Some(channel),
        changed: true,
        rejected: 0,
    }
}

/// `(count, me)` of each reaction of a retained message, in order.
fn counts(store: &MessageStore, n: u64) -> Vec<(u32, bool)> {
    store
        .get(CHANNEL, id(n))
        .unwrap()
        .reactions
        .iter()
        .map(|reaction| (reaction.count, reaction.me))
        .collect()
}

/// A deterministic stream of pseudo-random numbers for the flood tests.
fn random(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

/// Who reacts in a flood: the account one time in five, someone else otherwise.
fn reactor(state: &mut u64) -> Snowflake {
    if random(state).is_multiple_of(5) {
        ME
    } else {
        THEM
    }
}

#[derive(Default)]
struct Paging {
    pages: usize,
    received: usize,
    peak_messages: usize,
    peak_bytes: usize,
}

impl Paging {
    fn record(&mut self, store: &MessageStore, merge: PageMerge) {
        let (received, held, rejected) = applied(merge);
        assert_eq!(rejected, 0);
        assert!(held <= MAX_MESSAGES_PER_CHANNEL, "held {held}");
        check(store);
        self.pages += 1;
        self.received += received;
        self.peak_messages = self.peak_messages.max(store.channel_len(CHANNEL));
        self.peak_bytes = self.peak_bytes.max(store.tracked_bytes());
    }
}

#[test]
fn paging_through_10000_messages_holds_at_most_500_and_stays_within_16_mib() {
    const TOTAL: u64 = 10_000;
    let mut store = MessageStore::new();

    // Scroll up from the newest message to the beginning of the channel.
    let mut up = Paging::default();
    let merge = load(&mut store, HistoryCursor::Latest, TOTAL - 49..=TOTAL);
    up.record(&store, merge);
    loop {
        let window = store.window(CHANNEL).unwrap();
        let Some(HistoryCursor::Before(edge)) = window.older else {
            break;
        };
        // The viewport is at the top of what is held when it asks for more.
        store.set_focus(CHANNEL, window.oldest);
        let lowest = edge.0.saturating_sub(50).max(1);
        let merge = load(&mut store, HistoryCursor::Before(edge), lowest..=edge.0 - 1);
        up.record(&store, merge);
    }
    assert_eq!(
        up.received, TOTAL as usize,
        "every message was paged through"
    );
    assert_eq!(
        up.pages, 201,
        "one newest page, 199 full pages, one empty page"
    );
    assert_eq!(up.peak_messages, MAX_MESSAGES_PER_CHANNEL);
    assert!(
        up.peak_bytes < 4 * 1024 * 1024,
        "10,000 retained messages would exceed this; peak was {}",
        up.peak_bytes
    );
    let window = store.window(CHANNEL).unwrap();
    assert_eq!(window.older, None, "the beginning of the channel is held");
    assert_eq!(window.newer, Some(HistoryCursor::After(id(500))));
    assert_eq!(window.oldest, Some(id(1)));
    assert_eq!(
        ids_of(&store, CHANNEL),
        (1..=500).collect::<Vec<u64>>(),
        "the pages nearest the viewport are the ones kept, not the newest 500"
    );

    // Scroll back down to the newest message: evicted pages are fetched again,
    // and now the old end is what goes.
    let mut down = Paging::default();
    loop {
        let window = store.window(CHANNEL).unwrap();
        let Some(HistoryCursor::After(edge)) = window.newer else {
            break;
        };
        store.set_focus(CHANNEL, window.newest);
        let highest = (edge.0 + 50).min(TOTAL);
        let merge = load(&mut store, HistoryCursor::After(edge), edge.0 + 1..=highest);
        down.record(&store, merge);
        assert!(
            contiguous(&ids_of(&store, CHANNEL)),
            "the run never has a gap"
        );
    }
    assert_eq!(down.received, 9_500);
    assert_eq!(down.pages, 191);
    assert!(down.peak_messages <= MAX_MESSAGES_PER_CHANNEL);
    assert_eq!(
        ids_of(&store, CHANNEL),
        (9_501..=TOTAL).collect::<Vec<u64>>()
    );
    let window = store.window(CHANNEL).unwrap();
    assert_eq!(window.newer, None, "the run reaches the live edge again");
    assert_eq!(window.older, Some(HistoryCursor::Before(id(9_501))));

    // An evicted page comes back identical from a refetch.
    assert!(store.get(CHANNEL, id(500)).is_none());
    store.set_focus(CHANNEL, window.oldest);
    let merge = load(&mut store, HistoryCursor::Before(id(9_501)), 9_451..=9_500);
    assert_eq!(applied(merge), (50, 500, 0));
    check(&store);
    assert_eq!(
        store.get(CHANNEL, id(9_500)).unwrap().content,
        "generated message 9500"
    );
    assert!(
        store.get(CHANNEL, id(10_000)).is_none(),
        "the far end was evicted instead"
    );
}

#[test]
fn retention_follows_the_viewport_focus_not_the_newest_messages() {
    let mut store = MessageStore::new();
    fill(&mut store, CHANNEL, 10_000);
    assert_eq!(
        ids_of(&store, CHANNEL),
        (9_501..=10_000).collect::<Vec<u64>>()
    );

    // The viewport sits near the newest messages while an older page arrives:
    // the page is not what is being looked at, so it is the part that goes.
    store.set_focus(CHANNEL, Some(id(9_800)));
    let merge = load(&mut store, HistoryCursor::Before(id(9_501)), 9_451..=9_500);
    assert_eq!(applied(merge), (50, 500, 0));
    assert_eq!(
        ids_of(&store, CHANNEL),
        (9_501..=10_000).collect::<Vec<u64>>()
    );
    let window = store.window(CHANNEL).unwrap();
    assert_eq!(window.older, Some(HistoryCursor::Before(id(9_501))));
    assert_eq!(window.newer, None, "the live edge was not what was shed");
    check(&store);

    // With the viewport at the top the older page stays and the newest go.
    store.set_focus(CHANNEL, Some(id(9_501)));
    let merge = load(&mut store, HistoryCursor::Before(id(9_501)), 9_451..=9_500);
    assert_eq!(applied(merge), (50, 500, 0));
    assert_eq!(
        ids_of(&store, CHANNEL),
        (9_451..=9_950).collect::<Vec<u64>>()
    );
    let window = store.window(CHANNEL).unwrap();
    assert_eq!(window.newer, Some(HistoryCursor::After(id(9_950))));
    assert!(store.get(CHANNEL, id(10_000)).is_none());
    check(&store);

    // A short After page reaches the live edge again.
    store.set_focus(CHANNEL, window.newest);
    let merge = load(&mut store, HistoryCursor::After(id(9_950)), 9_951..=9_960);
    assert_eq!(applied(merge), (10, 500, 0));
    assert_eq!(
        ids_of(&store, CHANNEL),
        (9_461..=9_960).collect::<Vec<u64>>()
    );
    let window = store.window(CHANNEL).unwrap();
    assert_eq!(window.newer, None);
    assert_eq!(window.older, Some(HistoryCursor::Before(id(9_461))));

    // And the live edge accepts Gateway creates again, shedding the far end.
    let change = store.apply_dispatch(&create(CHANNEL, 9_961));
    assert_eq!(
        change,
        StoreChange {
            channel: Some(CHANNEL),
            changed: true,
            rejected: 0
        }
    );
    assert_eq!(
        ids_of(&store, CHANNEL),
        (9_462..=9_961).collect::<Vec<u64>>()
    );
    check(&store);
}

#[test]
fn paging_from_the_live_edge_without_an_anchor_defaults_the_focus_to_that_edge() {
    let mut store = MessageStore::new();
    store.set_focus(CHANNEL, Some(id(1)));
    assert_eq!(store.channels_held(), 0, "focus alone never creates a run");

    load(&mut store, HistoryCursor::Latest, 951..=1_000);
    assert_eq!(
        store.channels[0].focus, None,
        "Latest follows the live edge"
    );
    load(&mut store, HistoryCursor::Before(id(951)), 901..=950);
    assert_eq!(
        store.channels[0].focus,
        Some(id(951)),
        "a viewport that never reported an anchor is at the edge it pages from"
    );

    // An explicit anchor is respected, and Latest follows the live edge again.
    store.set_focus(CHANNEL, Some(id(920)));
    let before: Vec<_> = store.items(CHANNEL).collect();
    let merge = load(&mut store, HistoryCursor::Latest, 951..=1_000);
    assert_eq!(applied(merge), (50, 100, 0));
    assert_eq!(store.channels[0].focus, None);
    assert_eq!(
        store.items(CHANNEL).collect::<Vec<_>>(),
        before,
        "an identical refetch keeps every handle and revision"
    );
    check(&store);
}

#[test]
fn byte_budget_wins_over_the_message_count_for_large_bodies() {
    // Fifty 400 KiB bodies are 20 MiB: more than the cache may hold.
    let mut store = MessageStore::new();
    let mut messages = page(951..=1_000);
    for message in &mut messages {
        message.content = "x".repeat(400 * 1024);
    }
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    let (received, held, rejected) = applied(store.merge_page(token, messages));
    assert_eq!((received, rejected), (50, 0));
    assert!((30..50).contains(&held), "held {held}");
    check(&store);
    assert!(store.tracked_bytes() <= MESSAGE_BUDGET);

    // The viewport follows the live edge, so the newest survive and the
    // request that continues below them is the one the window names.
    let window = store.window(CHANNEL).unwrap();
    let oldest = 1_001 - held as u64;
    assert_eq!(window.newest, Some(id(1_000)));
    assert_eq!(window.oldest, Some(id(oldest)));
    assert_eq!(window.older, Some(HistoryCursor::Before(id(oldest))));
    assert!(contiguous(&ids_of(&store, CHANNEL)));

    // Further large pages keep the total under the budget at every step.
    for round in 0..10u64 {
        let edge = store.window(CHANNEL).unwrap().older.unwrap();
        let HistoryCursor::Before(edge) = edge else {
            unreachable!();
        };
        store.set_focus(CHANNEL, store.window(CHANNEL).unwrap().oldest);
        let top = edge.0 - 1;
        let mut messages = page(top - 49..=top);
        for message in &mut messages {
            message.content = "y".repeat(300 * 1024 + round as usize);
        }
        let token = store.begin_page(CHANNEL, HistoryCursor::Before(edge));
        applied(store.merge_page(token, messages));
        check(&store);
    }
}

#[test]
fn bodies_over_512_kib_are_rejected_from_pages_creates_and_updates_never_truncated() {
    let base = {
        let mut empty = message(1);
        empty.content = String::new();
        message_bytes(&empty)
    };
    let at_cap = MAX_MESSAGE_BYTES - base;
    let mut store = MessageStore::new();

    // A page: a body exactly at the cap is held whole; one byte over is not.
    let mut messages = page(1..=5);
    messages[1].content = "y".repeat(at_cap + 1); // id 4
    messages[2].content = "y".repeat(at_cap); // id 3
    assert_eq!(message_bytes(&messages[2]), MAX_MESSAGE_BYTES);
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    let merge = store.merge_page(token, messages);
    assert_eq!(
        merge,
        PageMerge::Applied {
            received: 5,
            held: 4,
            rejected: 1
        }
    );
    assert_eq!(ids_of(&store, CHANNEL), [1, 2, 3, 5]);
    assert_eq!(store.get(CHANNEL, id(3)).unwrap().content.len(), at_cap);
    check(&store);

    // A Gateway create over the cap is reported and not stored; the next one
    // is appended past the hole.
    let mut huge = message(6);
    huge.content = "y".repeat(at_cap + 1);
    let change = store.apply_dispatch(&Dispatch::MessageCreate(Box::new(huge)));
    assert_eq!(
        change,
        StoreChange {
            channel: Some(CHANNEL),
            changed: false,
            rejected: 1
        }
    );
    store.apply_dispatch(&create(CHANNEL, 7));
    assert_eq!(ids_of(&store, CHANNEL), [1, 2, 3, 5, 7]);

    // An update whose own payload is over the cap removes the stale copy.
    let raw = format!(r#","content":"{}""#, "z".repeat(MAX_MESSAGE_BYTES + 1));
    let change = store.apply_dispatch(&update(CHANNEL, 7, &raw));
    assert_eq!(
        change,
        StoreChange {
            channel: Some(CHANNEL),
            changed: true,
            rejected: 1
        }
    );
    assert!(store.get(CHANNEL, id(7)).is_none());

    // A small partial update that pushes a message at the cap over it does too.
    let mut at_limit = message(8);
    at_limit.content = "y".repeat(at_cap);
    let change = store.apply_dispatch(&Dispatch::MessageCreate(Box::new(at_limit)));
    assert!(change.changed);
    assert_eq!(store.get(CHANNEL, id(8)).unwrap().content.len(), at_cap);
    let grow = format!(
        r#","attachments":[{{"id":"1","filename":"{}","size":1,"url":"https://cdn.example/x"}}]"#,
        "f".repeat(1_024)
    );
    let change = store.apply_dispatch(&update(CHANNEL, 8, &grow));
    assert_eq!(
        change,
        StoreChange {
            channel: Some(CHANNEL),
            changed: true,
            rejected: 1
        }
    );
    assert!(store.get(CHANNEL, id(8)).is_none());
    assert_eq!(ids_of(&store, CHANNEL), [1, 2, 3, 5]);
    check(&store);
}

#[test]
fn partial_updates_preserve_omitted_fields_and_tell_null_from_absent() {
    let created: Message = serde_json::from_str(MESSAGE_CREATE).unwrap();
    let mid = created.id;
    let mut store = MessageStore::new();
    // A channel proven empty accepts live messages.
    let merge = load_empty(&mut store, HistoryCursor::Latest);
    assert_eq!(applied(merge), (0, 0, 0));
    assert_eq!(
        store.window(CHANNEL),
        Some(ChannelWindow {
            len: 0,
            oldest: None,
            newest: None,
            older: None,
            newer: None
        })
    );
    let change = store.apply_dispatch(&Dispatch::MessageCreate(Box::new(created)));
    assert_eq!(
        change,
        StoreChange {
            channel: Some(CHANNEL),
            changed: true,
            rejected: 0
        }
    );
    let held = store.get(CHANNEL, mid).unwrap();
    let revision = store.revision(CHANNEL, mid).unwrap();

    // An embed-only update carries nothing the client models: no change, no
    // new revision, and the same shared handle.
    let partial: MessageUpdate = serde_json::from_str(MESSAGE_UPDATE_PARTIAL).unwrap();
    let change = store.apply_dispatch(&Dispatch::MessageUpdate(Box::new(partial)));
    assert!(!change.changed);
    assert_eq!(store.revision(CHANNEL, mid), Some(revision));
    assert!(Arc::ptr_eq(&held, &store.get(CHANNEL, mid).unwrap()));

    // An edit replaces what it names and keeps everything it omits; the old
    // handle is never mutated.
    let edit: MessageUpdate = serde_json::from_str(MESSAGE_UPDATE_EDIT).unwrap();
    let change = store.apply_dispatch(&Dispatch::MessageUpdate(Box::new(edit.clone())));
    assert!(change.changed);
    let edited = store.get(CHANNEL, mid).unwrap();
    assert_eq!(edited.content, "hello (edited)");
    assert_eq!(
        edited.edited_timestamp.as_deref(),
        Some("2026-10-07T12:05:00.000000+00:00")
    );
    assert_eq!(edited.attachments, held.attachments);
    assert_eq!(edited.reactions, held.reactions);
    assert_eq!(edited.message_reference, held.message_reference);
    assert_eq!(held.content, "hello <:wave:777>");
    let edited_revision = store.revision(CHANNEL, mid).unwrap();
    assert!(edited_revision > revision);
    check(&store);

    // The same edit again changes nothing.
    let change = store.apply_dispatch(&Dispatch::MessageUpdate(Box::new(edit)));
    assert!(!change.changed);
    assert_eq!(store.revision(CHANNEL, mid), Some(edited_revision));
    assert!(Arc::ptr_eq(&edited, &store.get(CHANNEL, mid).unwrap()));

    let apply = |store: &mut MessageStore, fields: &str| {
        store.apply_dispatch(&update(CHANNEL, mid.0, fields))
    };
    let now = |store: &MessageStore| store.get(CHANNEL, mid).unwrap();

    // Omitted keeps; an explicit null clears.
    assert!(apply(&mut store, r#","pinned":true"#).changed);
    assert_eq!(
        now(&store).edited_timestamp.as_deref(),
        Some("2026-10-07T12:05:00.000000+00:00")
    );
    assert!(now(&store).pinned);
    assert!(apply(&mut store, r#","edited_timestamp":null"#).changed);
    assert_eq!(now(&store).edited_timestamp, None);
    assert_eq!(now(&store).content, "hello (edited)");
    assert!(now(&store).pinned);

    // An empty text is a value; an absent one is not.
    assert!(apply(&mut store, r#","content":"""#).changed);
    assert_eq!(now(&store).content, "");
    assert!(apply(&mut store, r#","flags":4"#).changed);
    assert_eq!((now(&store).content.as_str(), now(&store).flags), ("", 4));

    // An empty attachment list clears; an absent one keeps.
    assert_eq!(now(&store).attachments.len(), 1);
    assert!(!apply(&mut store, r#","pinned":true"#).changed);
    assert_eq!(now(&store).attachments.len(), 1);
    assert!(apply(&mut store, r#","attachments":[]"#).changed);
    assert!(now(&store).attachments.is_empty());
    assert_eq!(now(&store).reactions.len(), 1);
    check(&store);

    // An update for a message that is not held changes nothing.
    let change = store.apply_dispatch(&update(CHANNEL, 12, r#","content":"x""#));
    assert!(!change.changed);
    assert_eq!(store.channel_len(CHANNEL), 1);
}

#[test]
fn gateway_creates_updates_deletes_and_bulk_deletes_apply_in_order() {
    let mut store = MessageStore::new();
    load(&mut store, HistoryCursor::Latest, 9..=13);
    assert_eq!(ids_of(&store, CHANNEL), [9, 10, 11, 12, 13]);

    // Creates arrive in order and are appended with increasing revisions.
    for n in [14, 15] {
        let change = store.apply_dispatch(&create(CHANNEL, n));
        assert_eq!(change.channel, Some(CHANNEL));
        assert!(change.changed);
    }
    assert!(store.revision(CHANNEL, id(14)) < store.revision(CHANNEL, id(15)));
    // A duplicate create (replay) changes nothing.
    let revision = store.revision(CHANNEL, id(15));
    assert!(!store.apply_dispatch(&create(CHANNEL, 15)).changed);
    assert_eq!(store.revision(CHANNEL, id(15)), revision);
    // Out of order: 18 is appended, then 17 slots in before it.
    store.apply_dispatch(&create(CHANNEL, 18));
    store.apply_dispatch(&create(CHANNEL, 17));
    assert_eq!(ids_of(&store, CHANNEL), [9, 10, 11, 12, 13, 14, 15, 17, 18]);
    // Older than anything the run vouches for: not held.
    assert!(!store.apply_dispatch(&create(CHANNEL, 3)).changed);
    assert_eq!(store.channel_len(CHANNEL), 9);

    // An update merges into the held message.
    let before = store.revision(CHANNEL, id(12)).unwrap();
    assert!(
        store
            .apply_dispatch(&update(CHANNEL, 12, r#","content":"edited 12""#))
            .changed
    );
    assert_eq!(store.get(CHANNEL, id(12)).unwrap().content, "edited 12");
    assert!(store.revision(CHANNEL, id(12)).unwrap() > before);

    // A delete removes; replaying it is harmless.
    assert!(store.apply_dispatch(&delete(CHANNEL, 14)).changed);
    assert!(!store.apply_dispatch(&delete(CHANNEL, 14)).changed);
    assert_eq!(ids_of(&store, CHANNEL), [9, 10, 11, 12, 13, 15, 17, 18]);

    // The recorded bulk event lists 10, 12, 11, 10: duplicates are harmless.
    let recorded: MessageDeleteBulk = serde_json::from_str(MESSAGE_DELETE_BULK).unwrap();
    assert_eq!(recorded.channel_id, CHANNEL);
    let elsewhere = Dispatch::MessageDeleteBulk(MessageDeleteBulk {
        channel_id: OTHER,
        ids: recorded.ids.clone(),
        guild_id: None,
    });
    assert!(
        !store.apply_dispatch(&elsewhere).changed,
        "another channel is untouched"
    );
    assert_eq!(store.channel_len(CHANNEL), 8);
    let change = store.apply_dispatch(&Dispatch::MessageDeleteBulk(recorded));
    assert_eq!(change.channel, Some(CHANNEL));
    assert!(change.changed);
    assert_eq!(ids_of(&store, CHANNEL), [9, 13, 15, 17, 18]);
    assert!(!store.apply_dispatch(&bulk(CHANNEL, [10, 11, 99])).changed);

    // Events that are not message events are ignored.
    assert_eq!(
        store.apply_dispatch(&Dispatch::Resumed),
        StoreChange::default()
    );
    check(&store);
}

#[test]
fn creates_outside_the_live_edge_are_dropped_unless_a_request_that_predates_them_is_in_flight() {
    let mut store = MessageStore::new();
    // No run and no request: nothing to keep.
    let change = store.apply_dispatch(&create(CHANNEL, 10));
    assert_eq!(
        change,
        StoreChange {
            channel: Some(CHANNEL),
            changed: false,
            rejected: 0
        }
    );
    assert_eq!((store.messages_held(), store.channels_held()), (0, 0));

    // A Latest request is in flight and its snapshot predates message 12.
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    store.apply_dispatch(&create(CHANNEL, 12));
    store.apply_dispatch(&create(CHANNEL, 12));
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(8..=11)),
    );
    assert_eq!(applied(merge), (4, 5, 0));
    assert_eq!(ids_of(&store, CHANNEL), [8, 9, 10, 11, 12]);
    check(&store);

    // A snapshot that already includes it keeps one copy.
    let mut other = MessageStore::new();
    let token = other.begin_page(CHANNEL, HistoryCursor::Latest);
    other.apply_dispatch(&create(CHANNEL, 12));
    let merge = other.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(8..=12)),
    );
    assert_eq!(applied(merge), (5, 5, 0));
    assert_eq!(ids_of(&other, CHANNEL), [8, 9, 10, 11, 12]);

    // A run that no longer reaches the live edge does not take creates either,
    // until an After request that reaches it is in flight.
    let mut scrolled = MessageStore::new();
    fill(&mut scrolled, CHANNEL, 10_000);
    scrolled.set_focus(CHANNEL, Some(id(9_501)));
    load(
        &mut scrolled,
        HistoryCursor::Before(id(9_501)),
        9_451..=9_500,
    );
    assert_eq!(
        ids_of(&scrolled, CHANNEL),
        (9_451..=9_950).collect::<Vec<u64>>()
    );
    assert!(!scrolled.apply_dispatch(&create(CHANNEL, 10_001)).changed);
    assert_eq!(scrolled.channel_len(CHANNEL), 500);

    scrolled.set_focus(CHANNEL, Some(id(9_950)));
    let token = scrolled.begin_page(CHANNEL, HistoryCursor::After(id(9_950)));
    scrolled.apply_dispatch(&create(CHANNEL, 10_001));
    let page_after = through_rest(
        CHANNEL,
        HistoryCursor::After(id(9_950)),
        &page(9_951..=9_960),
    );
    assert_eq!(applied(scrolled.merge_page(token, page_after)).0, 10);
    assert!(scrolled.get(CHANNEL, id(10_001)).is_some());
    let window = scrolled.window(CHANNEL).unwrap();
    assert_eq!((window.newer, window.newest), (None, Some(id(10_001))));
    check(&scrolled);
}

#[test]
fn a_gateway_delete_during_a_page_request_suppresses_the_older_rest_copy() {
    let mut store = MessageStore::new();
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    // Neither is held: the response will still list them.
    assert!(!store.apply_dispatch(&delete(CHANNEL, 7)).changed);
    assert!(!store.apply_dispatch(&bulk(CHANNEL, [3, 4, 4])).changed);
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10)),
    );
    assert_eq!(applied(merge), (10, 7, 0));
    assert_eq!(ids_of(&store, CHANNEL), [1, 2, 5, 6, 8, 9, 10]);
    assert_eq!(
        store.pending_pages(),
        0,
        "the journal is released with the request"
    );
    check(&store);
}

#[test]
fn a_gateway_update_during_a_page_request_beats_the_older_rest_copy() {
    let mut store = MessageStore::new();
    load(&mut store, HistoryCursor::Latest, 1..=10);
    let before: Vec<_> = store.items(CHANNEL).collect();

    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    store.apply_dispatch(&update(CHANNEL, 5, r#","content":"gateway edit""#));
    // The snapshot predates that edit, and holds one the Gateway has not
    // delivered yet.
    let mut snapshot = page(1..=10);
    snapshot
        .iter_mut()
        .find(|message| message.id == id(6))
        .unwrap()
        .content = "edited on the server".to_owned();
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &snapshot),
    );
    assert_eq!(applied(merge), (10, 10, 0));
    assert_eq!(store.get(CHANNEL, id(5)).unwrap().content, "gateway edit");
    assert_eq!(
        store.get(CHANNEL, id(6)).unwrap().content,
        "edited on the server"
    );

    // Only the two changed messages have new revisions.
    let after: Vec<_> = store.items(CHANNEL).collect();
    for ((message, old), (_, new)) in before.iter().zip(&after) {
        assert_eq!(old != new, message.0 == 5 || message.0 == 6, "{message:?}");
    }
    check(&store);
}

#[test]
fn a_page_vouches_for_deletions_but_not_over_messages_created_since_the_request_began() {
    let mut store = MessageStore::new();
    load(&mut store, HistoryCursor::Latest, 1..=10);
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    assert!(store.apply_dispatch(&create(CHANNEL, 11)).changed);
    // Messages 4 and 5 were deleted on the server and the event was missed.
    let snapshot: Vec<Message> = page(1..=10)
        .into_iter()
        .filter(|message| message.id != id(4) && message.id != id(5))
        .collect();
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &snapshot),
    );
    assert_eq!(applied(merge), (8, 9, 0));
    assert_eq!(ids_of(&store, CHANNEL), [1, 2, 3, 6, 7, 8, 9, 10, 11]);
    check(&store);
}

#[test]
fn an_update_to_an_uncached_message_makes_a_page_that_carries_it_stale() {
    let mut store = MessageStore::new();
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    store.apply_dispatch(&update(CHANNEL, 7, r#","content":"edited""#));
    let stale = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10)),
    );
    assert_eq!(stale, PageMerge::Stale);
    assert_eq!((store.channels_held(), store.messages_held()), (0, 0));
    assert_eq!(store.pending_pages(), 0);
    check(&store);

    // Asking again after the event succeeds with current state.
    let merge = load(&mut store, HistoryCursor::Latest, 1..=10);
    assert_eq!(applied(merge), (10, 10, 0));

    // An update the page does not carry, or one that cannot change a message
    // (an embed unfurl), does not taint it.
    let mut fresh = MessageStore::new();
    let token = fresh.begin_page(CHANNEL, HistoryCursor::Latest);
    fresh.apply_dispatch(&update(CHANNEL, 99, r#","content":"elsewhere""#));
    fresh.apply_dispatch(&update(CHANNEL, 7, r#","components":[]"#));
    let merge = fresh.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10)),
    );
    assert_eq!(applied(merge), (10, 10, 0));
}

#[test]
fn journals_are_bounded_and_overflow_makes_the_response_stale() {
    // More deletes than the journal records.
    let mut store = MessageStore::new();
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    for n in 0..=JOURNAL_MAX_IDS as u64 {
        store.apply_dispatch(&delete(CHANNEL, 10_000 + n));
        assert!(store.journal_bytes() <= JOURNAL_BUDGET);
    }
    assert_eq!(
        store.merge_page(
            token,
            through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10))
        ),
        PageMerge::Stale
    );

    // One bulk event far larger than any journal, with duplicates.
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    store.apply_dispatch(&bulk(CHANNEL, (0..2_000).map(|n| 20_000 + n % 700)));
    check(&store);
    assert_eq!(
        store.merge_page(
            token,
            through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10))
        ),
        PageMerge::Stale
    );

    // More creates than the journal holds.
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    for n in 0..=JOURNAL_MAX_CREATES as u64 {
        store.apply_dispatch(&create(CHANNEL, 100 + n));
    }
    check(&store);
    assert_eq!(
        store.merge_page(
            token,
            through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10))
        ),
        PageMerge::Stale
    );

    // Creates larger than the journal's byte budget.
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    for n in 0..3 {
        let mut large = message(200 + n);
        large.content = "x".repeat(400 * 1024);
        store.apply_dispatch(&Dispatch::MessageCreate(Box::new(large)));
        check(&store);
    }
    assert!(store.tracked_bytes() <= MESSAGE_BUDGET);
    assert_eq!(
        store.merge_page(
            token,
            through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10))
        ),
        PageMerge::Stale
    );
    assert_eq!(store.messages_held(), 0, "stale responses change nothing");
}

#[test]
fn a_create_evicted_from_the_live_edge_during_a_latest_request_is_recovered_by_refetching() {
    let mut store = MessageStore::new();
    fill(&mut store, CHANNEL, 10_000);
    store.set_focus(CHANNEL, Some(id(9_501)));
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    // Appended, then shed at once: the viewport is nowhere near it.
    assert!(!store.apply_dispatch(&create(CHANNEL, 10_001)).changed);
    assert_eq!(
        store.window(CHANNEL).unwrap().newer,
        Some(HistoryCursor::After(id(10_000)))
    );
    let page_latest = through_rest(CHANNEL, HistoryCursor::Latest, &page(9_951..=10_000));
    assert_eq!(store.merge_page(token, page_latest), PageMerge::Stale);

    // The window names the request that finds it.
    store.set_focus(CHANNEL, Some(id(10_000)));
    let merge = load(
        &mut store,
        HistoryCursor::After(id(10_000)),
        10_001..=10_001,
    );
    assert_eq!(applied(merge), (1, 500, 0));
    assert!(store.get(CHANNEL, id(10_001)).is_some());
    assert_eq!(store.window(CHANNEL).unwrap().newer, None);
    check(&store);
}

#[test]
fn superseded_cancelled_cleared_and_removed_requests_are_stale() {
    let mut store = MessageStore::new();
    let snapshot = |n: u64| through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=n));

    let first = store.begin_page(CHANNEL, HistoryCursor::Latest);
    let second = store.begin_page(CHANNEL, HistoryCursor::Latest);
    assert_eq!(
        (first.channel(), first.cursor()),
        (CHANNEL, HistoryCursor::Latest)
    );
    assert_ne!(first, second);
    assert_eq!(store.pending_pages(), 1, "one request per channel");
    assert_eq!(store.merge_page(first, snapshot(3)), PageMerge::Stale);
    assert_eq!(applied(store.merge_page(second, snapshot(3))), (3, 3, 0));
    assert_eq!(
        store.merge_page(second, snapshot(3)),
        PageMerge::Stale,
        "a token is spent once"
    );

    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    store.cancel_page(token);
    store.cancel_page(token);
    assert_eq!(store.merge_page(token, snapshot(5)), PageMerge::Stale);
    assert_eq!(store.channel_len(CHANNEL), 3);

    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    store.remove_channel(CHANNEL);
    assert_eq!(store.merge_page(token, snapshot(5)), PageMerge::Stale);
    assert_eq!((store.channels_held(), store.messages_held()), (0, 0));

    let revision = {
        load(&mut store, HistoryCursor::Latest, 1..=3);
        store.revision(CHANNEL, id(3)).unwrap()
    };
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    store.clear();
    assert_eq!(store.merge_page(token, snapshot(5)), PageMerge::Stale);
    assert_eq!(store.tracked_bytes(), MessageStore::new().tracked_bytes());
    load(&mut store, HistoryCursor::Latest, 1..=3);
    assert!(
        store.revision(CHANNEL, id(3)).unwrap() > revision,
        "a revision is never reused"
    );

    // Tracked requests are bounded: the oldest is superseded.
    let tokens: Vec<PageToken> = (0..6)
        .map(|n| store.begin_page(id(600 + n), HistoryCursor::Latest))
        .collect();
    assert_eq!(store.pending_pages(), MAX_PENDING_PAGES);
    assert_eq!(store.merge_page(tokens[0], Vec::new()), PageMerge::Stale);
    assert_eq!(applied(store.merge_page(tokens[5], Vec::new())), (0, 0, 0));
    check(&store);
}

#[test]
fn a_page_that_does_not_touch_the_run_is_stale_and_latest_replaces_a_distant_run() {
    let mut store = MessageStore::new();
    load(&mut store, HistoryCursor::Latest, 951..=1_000);
    let held = ids_of(&store, CHANNEL);

    // Cursors the run no longer reaches (it moved while the request was out).
    let token = store.begin_page(CHANNEL, HistoryCursor::Before(id(500)));
    let far_before = through_rest(CHANNEL, HistoryCursor::Before(id(500)), &page(450..=499));
    assert_eq!(store.merge_page(token, far_before), PageMerge::Stale);
    let token = store.begin_page(CHANNEL, HistoryCursor::After(id(10)));
    let far_after = through_rest(CHANNEL, HistoryCursor::After(id(10)), &page(11..=60));
    assert_eq!(store.merge_page(token, far_after), PageMerge::Stale);
    assert_eq!(ids_of(&store, CHANNEL), held, "nothing changed");
    // Neither can start a run in a channel that has none.
    let token = store.begin_page(OTHER, HistoryCursor::Before(id(500)));
    assert_eq!(store.merge_page(token, Vec::new()), PageMerge::Stale);
    assert_eq!(store.channels_held(), 1);

    // Thousands of messages later, Latest cannot reach the held run: it replaces it.
    let merge = load(&mut store, HistoryCursor::Latest, 5_951..=6_000);
    assert_eq!(applied(merge), (50, 50, 0));
    assert_eq!(
        ids_of(&store, CHANNEL),
        (5_951..=6_000).collect::<Vec<u64>>()
    );
    let window = store.window(CHANNEL).unwrap();
    assert_eq!(window.older, Some(HistoryCursor::Before(id(5_951))));
    assert_eq!(window.newer, None);
    check(&store);
}

#[test]
fn a_jump_replaces_a_distant_run_extends_a_touching_one_and_pages_onward() {
    let mut store = MessageStore::new();
    load(&mut store, HistoryCursor::Latest, 951..=1_000);

    // A jump far back (to message 500, deleted meanwhile): the page around it
    // replaces the live run, vouches for its own span only, and is not live.
    let token = store.begin_page(CHANNEL, HistoryCursor::Around(id(500)));
    let mut around = page(476..=525);
    around.retain(|message| message.id != id(500));
    let around = through_rest(CHANNEL, HistoryCursor::Around(id(500)), &around);
    assert_eq!(applied(store.merge_page(token, around)), (49, 49, 0));
    assert_eq!(ids_of(&store, CHANNEL).first(), Some(&476));
    assert_eq!(ids_of(&store, CHANNEL).last(), Some(&525));
    assert!(store.get(CHANNEL, id(500)).is_none());
    assert!(
        store.covers(CHANNEL, id(500)),
        "the page proves 500 is gone, so it is not asked for again"
    );
    assert!(!store.covers(CHANNEL, id(475)) && !store.covers(CHANNEL, id(526)));
    let window = store.window(CHANNEL).unwrap();
    assert_eq!(window.older, Some(HistoryCursor::Before(id(476))));
    assert_eq!(window.newer, Some(HistoryCursor::After(id(525))));
    check(&store);

    // A jump to a message just past the run extends it instead.
    let merge = load(&mut store, HistoryCursor::Around(id(540)), 515..=564);
    assert_eq!(applied(merge), (50, 88, 0));
    let held = ids_of(&store, CHANNEL);
    assert_eq!((held[0], held[held.len() - 1]), (476, 564));
    assert_eq!(
        store.window(CHANNEL).unwrap().newer,
        Some(HistoryCursor::After(id(564))),
        "a jump never claims the live edge"
    );

    // Paging on from there reaches the live edge as usual.
    let merge = load(&mut store, HistoryCursor::After(id(564)), 565..=590);
    assert_eq!(applied(merge).0, 26);
    assert_eq!(store.window(CHANNEL).unwrap().newer, None);
    check(&store);

    // A jump whose page is empty (nothing there at all) changes nothing.
    let before = ids_of(&store, CHANNEL);
    let merge = load_empty(&mut store, HistoryCursor::Around(id(9_999)));
    assert_eq!(applied(merge), (0, before.len(), 0));
    assert_eq!(ids_of(&store, CHANNEL), before);
    // Nor does one for another channel's messages.
    let token = store.begin_page(CHANNEL, HistoryCursor::Around(id(50)));
    let foreign = page_in(OTHER, 26..=75);
    assert_eq!(
        applied(store.merge_page(token, foreign)),
        (50, before.len(), 50)
    );
    assert_eq!(ids_of(&store, CHANNEL), before);
    check(&store);
}

#[test]
fn a_jump_keeps_its_target_when_the_run_is_trimmed_and_respects_gateway_deletes() {
    let mut store = MessageStore::new();
    fill(&mut store, CHANNEL, 10_000);
    assert_eq!(store.channel_len(CHANNEL), MAX_MESSAGES_PER_CHANNEL);
    // Jumping just below the oldest held message: the run grows past 500,
    // and the end far from the target (the newest messages) is what goes.
    let oldest = ids_of(&store, CHANNEL)[0];
    let target = oldest - 20;
    let token = store.begin_page(CHANNEL, HistoryCursor::Around(id(target)));
    // The target's neighbour is deleted over the Gateway meanwhile.
    store.apply_dispatch(&delete(CHANNEL, target + 1));
    let messages = through_rest(
        CHANNEL,
        HistoryCursor::Around(id(target)),
        &page(target - 25..=target + 24),
    );
    applied(store.merge_page(token, messages));
    let held = ids_of(&store, CHANNEL);
    assert_eq!(held.len(), MAX_MESSAGES_PER_CHANNEL);
    assert_eq!(held[0], target - 25);
    assert!(store.get(CHANNEL, id(target)).is_some());
    assert!(
        store.get(CHANNEL, id(target + 1)).is_none(),
        "a deletion that raced with the jump is not resurrected"
    );
    assert!(store.window(CHANNEL).unwrap().newer.is_some());
    check(&store);
}

#[test]
fn short_pages_prove_the_ends_of_history_and_rejected_records_are_counted() {
    let mut store = MessageStore::new();
    load(&mut store, HistoryCursor::Latest, 10..=12);
    let window = store.window(CHANNEL).unwrap();
    assert_eq!(
        window.older, None,
        "fewer than a page: nothing older exists"
    );
    assert_eq!(window.newer, None);

    // Records of another channel, and records not strictly older than `before`.
    let token = store.begin_page(CHANNEL, HistoryCursor::Before(id(10)));
    let mixed = vec![message(10), message(9), message_in(OTHER, 8)];
    let merge = store.merge_page(token, mixed);
    assert_eq!(applied(merge), (3, 4, 2));
    assert_eq!(ids_of(&store, CHANNEL), [9, 10, 11, 12]);

    // A response with nothing usable in it changes nothing, not even a run.
    let token = store.begin_page(OTHER, HistoryCursor::Latest);
    let merge = store.merge_page(token, vec![message(1)]);
    assert_eq!(applied(merge), (1, 0, 1));
    assert_eq!(store.channels_held(), 1);

    // An empty After page ends at the live edge.
    let mut scrolled = MessageStore::new();
    fill(&mut scrolled, CHANNEL, 10_000);
    scrolled.set_focus(CHANNEL, Some(id(9_501)));
    load(
        &mut scrolled,
        HistoryCursor::Before(id(9_501)),
        9_451..=9_500,
    );
    assert!(scrolled.window(CHANNEL).unwrap().newer.is_some());
    scrolled.set_focus(CHANNEL, Some(id(9_950)));
    load_empty(&mut scrolled, HistoryCursor::After(id(9_950)));
    assert_eq!(scrolled.window(CHANNEL).unwrap().newer, None);
    check(&scrolled);
}

#[test]
fn global_message_budget_sheds_the_least_recently_used_channel_first() {
    let mut store = MessageStore::new();
    let channels: Vec<Snowflake> = (0..5).map(|n| id(1_000 + n)).collect();
    for &channel in &channels[..4] {
        fill(&mut store, channel, 10_000);
    }
    assert_eq!(store.messages_held(), MAX_MESSAGES);
    check(&store);

    // A fifth channel is the most recently used: the oldest-used one gives way.
    let merge = load_in(
        &mut store,
        channels[4],
        HistoryCursor::Latest,
        9_951..=10_000,
    );
    assert_eq!(applied(merge), (50, 50, 0));
    assert_eq!(store.messages_held(), MAX_MESSAGES);
    assert_eq!(store.channel_len(channels[0]), 450);
    for &channel in &channels[1..4] {
        assert_eq!(store.channel_len(channel), 500);
    }
    // The shed messages were the ones farthest from that channel's viewport,
    // which sat at the top.
    assert_eq!(store.window(channels[0]).unwrap().oldest, Some(id(9_501)));
    check(&store);

    // Using a channel again protects it: the next page sheds from whichever
    // channel was used longest ago.
    store.set_focus(channels[0], Some(id(9_501)));
    load_in(
        &mut store,
        channels[4],
        HistoryCursor::Before(id(9_951)),
        9_901..=9_950,
    );
    assert_eq!(store.channel_len(channels[0]), 450);
    assert_eq!(store.channel_len(channels[1]), 450);
    assert_eq!(store.channel_len(channels[2]), 500);
    assert_eq!(store.messages_held(), MAX_MESSAGES);
    check(&store);
}

#[test]
fn channel_runs_are_bounded_and_the_least_recently_used_run_is_dropped() {
    let mut store = MessageStore::new();
    for n in 0..=MAX_CHANNELS as u64 {
        load_in(&mut store, id(2_000 + n), HistoryCursor::Latest, 1..=1);
        check(&store);
    }
    assert_eq!(store.channels_held(), MAX_CHANNELS);
    assert!(
        store.window(id(2_000)).is_none(),
        "the oldest-used run was dropped"
    );
    assert!(store.window(id(2_001)).is_some());
    assert!(store.window(id(2_000 + MAX_CHANNELS as u64)).is_some());
}

#[test]
fn store_and_token_debug_output_never_contains_message_content() {
    let mut store = MessageStore::new();
    load(&mut store, HistoryCursor::Latest, 1..=3);
    let token = store.begin_page(CHANNEL, HistoryCursor::Before(id(1)));
    let shown = format!(
        "{store:?} {token:?} {:?} {:?}",
        store.get(CHANNEL, id(2)),
        store.window(CHANNEL)
    );
    assert!(shown.contains("MessageStore"));
    assert!(!shown.contains("generated message"));
    assert!(!shown.contains("SIGNED") && !shown.contains("cat.png"));
    assert!(!shown.contains("alt_fixture") && !shown.contains("Fixture Author"));
}

#[test]
fn reactions_change_a_held_message_and_bump_its_revision_only_when_something_changed() {
    let (up, down) = (unicode(UP), unicode(DOWN));
    let mut store = MessageStore::new();
    store.set_user(ME);
    load(&mut store, HistoryCursor::Latest, 1..=5);
    let untouched: Vec<_> = store
        .items(CHANNEL)
        .filter(|&(message, _)| message != id(3))
        .collect();
    let before = store.revision(CHANNEL, id(3)).unwrap();

    // Someone else reacts: counted, not the account's own.
    assert_eq!(
        store.apply_dispatch(&react(CHANNEL, 3, THEM, &up)),
        changed(CHANNEL)
    );
    assert_eq!(counts(&store, 3), [(1, false)]);
    let counted = store.revision(CHANNEL, id(3)).unwrap();
    assert!(counted > before);
    assert_eq!(
        store
            .items(CHANNEL)
            .filter(|&(message, _)| message != id(3))
            .collect::<Vec<_>>(),
        untouched,
        "no other message moved"
    );
    check(&store);

    // The account reacts.
    assert_eq!(
        store.apply_dispatch(&react(CHANNEL, 3, ME, &up)),
        changed(CHANNEL)
    );
    assert_eq!(counts(&store, 3), [(2, true)]);
    let own = store.revision(CHANNEL, id(3)).unwrap();
    assert!(own > counted);

    // The Gateway's echo of what a REST answer already applied, or a replay,
    // changes nothing: the same revision and no copy-on-write.
    let held = store.get(CHANNEL, id(3)).unwrap();
    assert_eq!(
        store.apply_dispatch(&react(CHANNEL, 3, ME, &up)),
        unchanged(CHANNEL)
    );
    assert_eq!(store.revision(CHANNEL, id(3)), Some(own));
    assert!(Arc::ptr_eq(&held, &store.get(CHANNEL, id(3)).unwrap()));

    // Copy-on-write: a consumer still holding the handle keeps what it had.
    assert!(
        store
            .apply_dispatch(&react(CHANNEL, 3, THEM, &down))
            .changed
    );
    assert_eq!(held.reactions.len(), 1);
    assert_eq!((held.reactions[0].count, held.reactions[0].me), (2, true));
    assert_eq!(counts(&store, 3), [(2, true), (1, false)]);
    assert!(!Arc::ptr_eq(&held, &store.get(CHANNEL, id(3)).unwrap()));
    check(&store);

    // Taking the account's reaction back, and then the echo of that.
    assert_eq!(
        store.apply_dispatch(&unreact(CHANNEL, 3, ME, &up)),
        changed(CHANNEL)
    );
    assert_eq!(counts(&store, 3), [(1, false), (1, false)]);
    let withdrawn = store.revision(CHANNEL, id(3)).unwrap();
    assert_eq!(
        store.apply_dispatch(&unreact(CHANNEL, 3, ME, &up)),
        unchanged(CHANNEL)
    );
    assert_eq!(store.revision(CHANNEL, id(3)), Some(withdrawn));

    // An entry goes with its last reaction; an emoji nobody used is nothing.
    assert!(
        store
            .apply_dispatch(&unreact(CHANNEL, 3, THEM, &up))
            .changed
    );
    assert_eq!(store.get(CHANNEL, id(3)).unwrap().reactions[0].emoji, down);
    assert_eq!(counts(&store, 3), [(1, false)]);
    let nobody = unicode("nobody");
    for event in [
        unreact(CHANNEL, 3, THEM, &nobody),
        remove_emoji(CHANNEL, 3, &nobody),
        unreact(CHANNEL, 3, THEM, &up),
    ] {
        assert_eq!(
            store.apply_dispatch(&event),
            unchanged(CHANNEL),
            "{event:?}"
        );
    }
    assert_eq!(
        store.apply_dispatch(&remove_emoji(CHANNEL, 3, &down)),
        changed(CHANNEL)
    );
    assert!(store.get(CHANNEL, id(3)).unwrap().reactions.is_empty());
    assert_eq!(
        store.apply_dispatch(&remove_all(CHANNEL, 3)),
        unchanged(CHANNEL),
        "nothing to remove"
    );

    // Remove-all drops every entry and releases the allocation.
    for emoji in [&up, &down, &custom(777, "wave")] {
        assert!(
            store
                .apply_dispatch(&react(CHANNEL, 3, THEM, emoji))
                .changed
        );
    }
    assert_eq!(counts(&store, 3).len(), 3);
    assert_eq!(
        store.apply_dispatch(&remove_all(CHANNEL, 3)),
        changed(CHANNEL)
    );
    assert_eq!(
        store.get(CHANNEL, id(3)).unwrap().reactions.capacity(),
        0,
        "the allocation is released too"
    );
    check(&store);
}

#[test]
fn burst_reactions_count_but_never_mark_the_account() {
    let up = unicode(UP);
    let mut store = MessageStore::new();
    store.set_user(ME);
    load(&mut store, HistoryCursor::Latest, 1..=3);
    assert!(
        store
            .apply_dispatch(&burst_react(CHANNEL, 2, ME, &up))
            .changed
    );
    assert_eq!(counts(&store, 2), [(1, false)]);
    // A burst is not idempotent: it counts every time.
    assert!(
        store
            .apply_dispatch(&burst_react(CHANNEL, 2, ME, &up))
            .changed
    );
    assert_eq!(counts(&store, 2), [(2, false)]);
    assert!(store.apply_dispatch(&react(CHANNEL, 2, ME, &up)).changed);
    assert_eq!(counts(&store, 2), [(3, true)]);
    // Taking a burst back lowers the count and leaves the normal reaction.
    assert!(
        store
            .apply_dispatch(&burst_unreact(CHANNEL, 2, ME, &up))
            .changed
    );
    assert_eq!(counts(&store, 2), [(2, true)]);
    check(&store);
}

#[test]
fn only_the_user_named_with_set_user_reacts_as_the_account() {
    let up = unicode(UP);
    let mut store = MessageStore::new();
    load(&mut store, HistoryCursor::Latest, 1..=3);
    // Nobody is named yet: every reaction is someone else's.
    assert!(store.apply_dispatch(&react(CHANNEL, 2, ME, &up)).changed);
    assert_eq!(counts(&store, 2), [(1, false)]);
    assert!(store.apply_dispatch(&unreact(CHANNEL, 2, ME, &up)).changed);
    assert!(store.get(CHANNEL, id(2)).unwrap().reactions.is_empty());

    // Once named, that user's reaction is the account's own.
    store.set_user(ME);
    assert!(store.apply_dispatch(&react(CHANNEL, 2, THEM, &up)).changed);
    assert!(store.apply_dispatch(&react(CHANNEL, 2, ME, &up)).changed);
    assert_eq!(counts(&store, 2), [(2, true)]);

    // Clearing drops messages, not who is reading them.
    store.clear();
    load(&mut store, HistoryCursor::Latest, 1..=3);
    assert!(store.apply_dispatch(&react(CHANNEL, 3, ME, &up)).changed);
    assert_eq!(counts(&store, 3), [(1, true)]);

    // Another account takes over: only its own reaction is "me" now.
    store.set_user(THEM);
    store.clear();
    load(&mut store, HistoryCursor::Latest, 1..=3);
    assert!(store.apply_dispatch(&react(CHANNEL, 3, ME, &up)).changed);
    assert_eq!(counts(&store, 3), [(1, false)]);
    assert!(store.apply_dispatch(&react(CHANNEL, 3, THEM, &up)).changed);
    assert_eq!(counts(&store, 3), [(2, true)]);
    check(&store);
}

#[test]
fn reactions_for_messages_that_are_not_held_change_nothing_and_keep_no_state() {
    let up = unicode(UP);
    let mut store = MessageStore::new();
    load(&mut store, HistoryCursor::Latest, 1..=5);
    let steady = |store: &MessageStore| {
        (
            store.tracked_bytes(),
            store.messages_held(),
            store.channels_held(),
            store.pending_pages(),
        )
    };
    let before = steady(&store);
    // Messages outside the run, and channels the store never held.
    for (channel, n) in [(CHANNEL, 6), (CHANNEL, 1_000_000), (CHANNEL, 0), (OTHER, 1)] {
        for event in [
            react(channel, n, THEM, &up),
            react(channel, n, ME, &up),
            unreact(channel, n, THEM, &up),
            remove_emoji(channel, n, &up),
            remove_all(channel, n),
        ] {
            assert_eq!(
                store.apply_dispatch(&event),
                unchanged(channel),
                "{event:?}"
            );
        }
    }
    assert_eq!(steady(&store), before);
    assert!(
        store.window(OTHER).is_none(),
        "no run is created for a channel the store never held"
    );
    check(&store);

    // A request in flight for another channel never hears of them.
    let token = store.begin_page(OTHER, HistoryCursor::Latest);
    store.apply_dispatch(&react(CHANNEL, 77, THEM, &up));
    let bytes = store.tracked_bytes();
    store.apply_dispatch(&react(CHANNEL, 78, THEM, &up));
    assert_eq!(store.tracked_bytes(), bytes);
    assert!(store.pending[0].touched.is_empty());
    let merge = store.merge_page(token, through_rest(OTHER, HistoryCursor::Latest, &[]));
    assert_eq!(applied(merge), (0, 0, 0));
}

#[test]
fn a_reaction_for_an_uncached_message_makes_a_page_that_carries_it_stale() {
    let up = unicode(UP);
    for event in [
        react(CHANNEL, 7, THEM, &up),
        unreact(CHANNEL, 7, ME, &up),
        remove_emoji(CHANNEL, 7, &up),
        remove_all(CHANNEL, 7),
    ] {
        let mut store = MessageStore::new();
        let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
        assert_eq!(store.apply_dispatch(&event), unchanged(CHANNEL));
        check(&store);
        assert_eq!((store.channels_held(), store.messages_held()), (0, 0));
        let stale = store.merge_page(
            token,
            through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10)),
        );
        assert_eq!(stale, PageMerge::Stale, "{event:?}");
        assert_eq!((store.channels_held(), store.messages_held()), (0, 0));
        assert_eq!(store.pending_pages(), 0);
        check(&store);
        // Asking again after the event succeeds with current state.
        assert_eq!(
            applied(load(&mut store, HistoryCursor::Latest, 1..=10)),
            (10, 10, 0)
        );

        // A page the event does not concern is not tainted.
        let mut other = MessageStore::new();
        let token = other.begin_page(CHANNEL, HistoryCursor::Latest);
        other.apply_dispatch(&event);
        let merge = other.merge_page(
            token,
            through_rest(CHANNEL, HistoryCursor::Latest, &page(8..=20)),
        );
        assert_eq!(applied(merge), (13, 13, 0), "{event:?}");
        check(&other);
    }
}

#[test]
fn a_gateway_reaction_during_a_page_request_beats_the_older_rest_copy() {
    let up = unicode(UP);
    let mut store = MessageStore::new();
    store.set_user(ME);
    load(&mut store, HistoryCursor::Latest, 1..=10);
    let before: Vec<_> = store.items(CHANNEL).collect();

    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    store.apply_dispatch(&react(CHANNEL, 5, THEM, &up));
    // The snapshot predates that reaction, and carries one the Gateway has not
    // delivered yet.
    let mut snapshot = page(1..=10);
    snapshot
        .iter_mut()
        .find(|message| message.id == id(6))
        .unwrap()
        .reactions = vec![Reaction {
        count: 2,
        me: true,
        emoji: up.clone(),
    }];
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &snapshot),
    );
    assert_eq!(applied(merge), (10, 10, 0));
    assert_eq!(counts(&store, 5), [(1, false)], "the Gateway's state wins");
    assert_eq!(counts(&store, 6), [(2, true)], "the snapshot's otherwise");

    // Only the two changed messages have new revisions.
    let after: Vec<_> = store.items(CHANNEL).collect();
    for ((message, old), (_, new)) in before.iter().zip(&after) {
        assert_eq!(old != new, message.0 == 5 || message.0 == 6, "{message:?}");
    }
    check(&store);
}

#[test]
fn a_reaction_on_a_message_created_during_a_latest_request_lands_on_its_journaled_copy() {
    let (up, wave) = (unicode(UP), custom(777, "wave"));
    let mut store = MessageStore::new();
    store.set_user(ME);
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    assert!(
        !store.apply_dispatch(&create(CHANNEL, 12)).changed,
        "journaled, not held"
    );
    let journaled = store.tracked_bytes();

    for event in [
        react(CHANNEL, 12, THEM, &up),
        react(CHANNEL, 12, ME, &up),
        react(CHANNEL, 12, THEM, &wave),
        react(CHANNEL, 12, THEM, &wave),
    ] {
        // The consumer sees nothing: the copy is not held yet.
        assert_eq!(store.apply_dispatch(&event), unchanged(CHANNEL));
        check(&store);
    }
    assert!(
        store.tracked_bytes() > journaled,
        "the journaled copy grew, and the ledger knows"
    );
    let pending = &store.pending[0];
    assert!(!pending.created_incomplete);
    assert_eq!(pending.created.len(), 1);
    assert_eq!(
        pending.created_bytes,
        message_bytes(&pending.created[0].message)
    );
    assert_eq!(pending.created[0].bytes, pending.created_bytes);
    assert!(
        pending.touched.is_empty(),
        "nothing is left for the response to trip over"
    );

    // The echo of what the copy already shows changes nothing at all.
    let bytes = store.tracked_bytes();
    assert_eq!(
        store.apply_dispatch(&react(CHANNEL, 12, ME, &up)),
        unchanged(CHANNEL)
    );
    assert_eq!(store.tracked_bytes(), bytes);

    // Taking a reaction back and removing an emoji reach the copy as well.
    store.apply_dispatch(&unreact(CHANNEL, 12, ME, &up));
    store.apply_dispatch(&remove_emoji(CHANNEL, 12, &wave));
    check(&store);

    // The response predates the message: the journaled copy, reactions
    // included, is what the run gets.
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(8..=11)),
    );
    assert_eq!(applied(merge), (4, 5, 0));
    assert_eq!(ids_of(&store, CHANNEL), [8, 9, 10, 11, 12]);
    assert_eq!(counts(&store, 12), [(1, false)]);
    assert_eq!(store.pending_pages(), 0);
    check(&store);
}

#[test]
fn a_journaled_copy_that_would_outgrow_the_cap_or_the_journal_is_dropped_and_the_page_goes_stale() {
    let (up, down) = (unicode(UP), unicode(DOWN));
    let base = {
        let mut empty = message(1);
        empty.content = String::new();
        message_bytes(&empty)
    };
    let sized = |n: u64, bytes: usize| {
        let mut built = message(n);
        built.content = "x".repeat(bytes - base);
        assert_eq!(message_bytes(&built), bytes);
        Dispatch::MessageCreate(Box::new(built))
    };

    // A message already at the cap cannot take one more reaction.
    let mut store = MessageStore::new();
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    store.apply_dispatch(&sized(12, MAX_MESSAGE_BYTES));
    assert_eq!(store.pending[0].created.len(), 1);
    check(&store);
    assert_eq!(
        store.apply_dispatch(&react(CHANNEL, 12, THEM, &up)),
        unchanged(CHANNEL)
    );
    let pending = &store.pending[0];
    assert!(pending.created.is_empty() && pending.created_bytes == 0);
    assert!(pending.created_incomplete);
    check(&store);
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(8..=11)),
    );
    assert_eq!(merge, PageMerge::Stale, "the copy was lost, so ask again");
    assert_eq!(store.messages_held(), 0);
    check(&store);

    // The journal's byte budget is exact: a reaction that makes it full to the
    // byte fits, the next one does not.
    let growth = {
        // Cloned, so that capacities equal lengths as they do in the journal.
        let probe = Message::clone(&message(13));
        let mut grown = probe.clone();
        grown.apply_reaction(ReactionChange::Add {
            emoji: &up,
            by_me: false,
            burst: false,
        });
        message_bytes(&grown) - message_bytes(&probe)
    };
    let (first, second) = (480 * 1024, JOURNAL_CREATE_BUDGET - 480 * 1024 - growth);
    let mut store = MessageStore::new();
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    store.apply_dispatch(&sized(12, first));
    store.apply_dispatch(&sized(13, second));
    assert_eq!(store.pending[0].created.len(), 2);
    assert_eq!(store.pending[0].created_bytes, first + second);

    store.apply_dispatch(&react(CHANNEL, 13, THEM, &up));
    let pending = &store.pending[0];
    assert_eq!(pending.created.len(), 2);
    assert!(!pending.created_incomplete);
    assert_eq!(pending.created_bytes, JOURNAL_CREATE_BUDGET);
    check(&store);

    store.apply_dispatch(&react(CHANNEL, 13, THEM, &down));
    let pending = &store.pending[0];
    assert!(pending.created_incomplete);
    assert_eq!(pending.created.len(), 1);
    assert_eq!(pending.created_bytes, first);
    check(&store);
    assert!(store.journal_bytes() <= JOURNAL_BUDGET);
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(8..=11)),
    );
    assert_eq!(merge, PageMerge::Stale);
}

#[test]
fn a_held_message_a_reaction_pushes_over_the_cap_is_removed_like_an_oversized_update() {
    let up = unicode(UP);
    let base = {
        let mut empty = message(1);
        empty.content = String::new();
        message_bytes(&empty)
    };
    let mut store = MessageStore::new();
    load(&mut store, HistoryCursor::Latest, 1..=5);
    let mut at_limit = message(8);
    at_limit.content = "y".repeat(MAX_MESSAGE_BYTES - base);
    assert_eq!(message_bytes(&at_limit), MAX_MESSAGE_BYTES);
    assert!(
        store
            .apply_dispatch(&Dispatch::MessageCreate(Box::new(at_limit)))
            .changed
    );
    assert!(store.get(CHANNEL, id(8)).is_some());

    // A request that began before the reaction must not bring the old copy back.
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    let change = store.apply_dispatch(&react(CHANNEL, 8, THEM, &up));
    assert_eq!(
        change,
        StoreChange {
            channel: Some(CHANNEL),
            changed: true,
            rejected: 1
        }
    );
    assert!(store.get(CHANNEL, id(8)).is_none());
    check(&store);
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10)),
    );
    assert_eq!(applied(merge), (10, 9, 0));
    assert_eq!(ids_of(&store, CHANNEL), [1, 2, 3, 4, 5, 6, 7, 9, 10]);
    check(&store);
}

#[test]
fn more_distinct_uncached_messages_than_the_journal_holds_make_the_page_stale_instead_of_growing_it()
 {
    let up = unicode(UP);
    // Exactly as many as the journal records is still fine for a page that
    // carries none of them...
    let mut store = MessageStore::new();
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    for n in 0..JOURNAL_MAX_IDS as u64 {
        store.apply_dispatch(&react(CHANNEL, 10_000 + n, THEM, &up));
        assert!(store.journal_bytes() <= JOURNAL_BUDGET);
    }
    assert_eq!(store.pending[0].touched.len(), JOURNAL_MAX_IDS);
    // ...and repeating them records nothing more.
    let bytes = store.tracked_bytes();
    for n in 0..JOURNAL_MAX_IDS as u64 {
        store.apply_dispatch(&unreact(CHANNEL, 10_000 + n, ME, &up));
    }
    assert_eq!(store.tracked_bytes(), bytes);
    check(&store);
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10)),
    );
    assert_eq!(applied(merge), (10, 10, 0));

    // One more distinct message and the journal gives up: the response is stale.
    let mut store = MessageStore::new();
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    for n in 0..=JOURNAL_MAX_IDS as u64 {
        store.apply_dispatch(&remove_all(CHANNEL, 10_000 + n));
        assert!(store.journal_bytes() <= JOURNAL_BUDGET);
    }
    assert!(store.pending[0].touched_overflow);
    assert_eq!(store.pending[0].touched.len(), JOURNAL_MAX_IDS);
    check(&store);
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10)),
    );
    assert_eq!(merge, PageMerge::Stale, "even a page that carries none");
    assert_eq!(store.messages_held(), 0);
}

#[test]
fn a_flood_of_reactions_for_uncached_messages_cannot_grow_the_store() {
    let emoji: Vec<Emoji> = (0..50).map(|n| unicode(&format!("emoji-{n}"))).collect();
    let mut store = MessageStore::new();
    store.set_user(ME);
    load(&mut store, HistoryCursor::Latest, 1..=50);
    let steady = (
        store.tracked_bytes(),
        store.messages_held(),
        store.channels_held(),
    );
    let flood = |store: &mut MessageStore| {
        for step in 0..100_000u64 {
            // Ten thousand distinct messages, none of them held.
            let n = 10_000 + step % 10_000;
            let emoji = &emoji[(step % 50) as usize];
            let user = if step % 7 == 0 { ME } else { THEM };
            let event = match step % 4 {
                0 => react(CHANNEL, n, user, emoji),
                1 => unreact(CHANNEL, n, user, emoji),
                2 => remove_emoji(CHANNEL, n, emoji),
                _ => remove_all(CHANNEL, n),
            };
            assert!(!store.apply_dispatch(&event).changed);
        }
    };

    // Nothing is waiting for them: nothing happens at all.
    flood(&mut store);
    assert_eq!(
        (
            store.tracked_bytes(),
            store.messages_held(),
            store.channels_held()
        ),
        steady
    );
    check(&store);

    // A request in flight records at most what its journal holds.
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    flood(&mut store);
    assert!(store.tracked_bytes() <= steady.0 + JOURNAL_BUDGET);
    assert!(store.journal_bytes() <= JOURNAL_BUDGET);
    assert_eq!(store.pending[0].touched.len(), JOURNAL_MAX_IDS);
    assert_eq!((store.messages_held(), store.channels_held()), (50, 1));
    check(&store);
    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10)),
    );
    assert_eq!(merge, PageMerge::Stale);
    assert_eq!((store.messages_held(), store.channels_held()), (50, 1));
    check(&store);
}

#[test]
fn a_flood_of_reactions_against_held_messages_stays_bounded_and_the_ledger_exact() {
    let emoji: Vec<Emoji> = (0..2_000).map(|n| unicode(&format!("emoji-{n}"))).collect();
    let mut store = MessageStore::new();
    store.set_user(ME);
    load(&mut store, HistoryCursor::Latest, 1..=50);
    let mut state = 1;
    for step in 0..100_000u64 {
        let n = 1 + random(&mut state) % 50;
        let emoji = &emoji[(random(&mut state) % 2_000) as usize];
        let user = reactor(&mut state);
        let event = match random(&mut state) % 20 {
            0 => unreact(CHANNEL, n, user, emoji),
            1 => remove_emoji(CHANNEL, n, emoji),
            2 if step % 5_000 == 2 => remove_all(CHANNEL, n),
            _ => react(CHANNEL, n, user, emoji),
        };
        store.apply_dispatch(&event);
        if step % 1_000 == 0 {
            check(&store);
        }
    }
    check(&store);
    let mut full = 0;
    for message in store.messages(CHANNEL) {
        assert!(message.reactions.len() <= MAX_REACTIONS);
        assert!(message_bytes(message) <= MAX_MESSAGE_BYTES);
        full += usize::from(message.reactions.len() == MAX_REACTIONS);
    }
    assert!(full > 0, "the flood did reach the cap");
    assert_eq!(store.messages_held(), 50);
}

#[test]
fn a_flood_of_reactions_against_journaled_copies_stays_bounded_and_the_ledger_exact() {
    let emoji: Vec<Emoji> = (0..200).map(|n| unicode(&format!("emoji-{n}"))).collect();
    let mut store = MessageStore::new();
    store.set_user(ME);
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    for n in 0..JOURNAL_MAX_CREATES as u64 {
        store.apply_dispatch(&create(CHANNEL, 100 + n));
    }
    assert_eq!(store.pending[0].created.len(), JOURNAL_MAX_CREATES);
    let mut state = 7;
    for step in 0..20_000u64 {
        let n = 100 + random(&mut state) % JOURNAL_MAX_CREATES as u64;
        let emoji = &emoji[(random(&mut state) % 200) as usize];
        let user = reactor(&mut state);
        let event = match random(&mut state) % 10 {
            0 => unreact(CHANNEL, n, user, emoji),
            1 => remove_emoji(CHANNEL, n, emoji),
            _ => react(CHANNEL, n, user, emoji),
        };
        assert!(!store.apply_dispatch(&event).changed);
        if step % 500 == 0 {
            check(&store);
            assert!(store.journal_bytes() <= JOURNAL_BUDGET);
        }
    }
    check(&store);
    let pending = &store.pending[0];
    assert!(!pending.created_incomplete);
    assert!(pending.touched.is_empty());

    let merge = store.merge_page(
        token,
        through_rest(CHANNEL, HistoryCursor::Latest, &page(1..=10)),
    );
    assert_eq!(applied(merge), (10, 10 + JOURNAL_MAX_CREATES, 0));
    let mut full = 0;
    for message in store.messages(CHANNEL) {
        assert!(message.reactions.len() <= MAX_REACTIONS);
        full += usize::from(message.reactions.len() == MAX_REACTIONS);
    }
    assert!(full > 0, "the flood did reach the cap");
    check(&store);
}

#[test]
fn the_recorded_reaction_events_apply_through_the_store() {
    let mut store = MessageStore::new();
    store.set_user(ME);
    let token = store.begin_page(CHANNEL, HistoryCursor::Latest);
    store.merge_page(token, vec![message_in(CHANNEL, REACTED)]);
    let mut apply = |name: &str, raw: &str| {
        let change = store.apply_dispatch(&decode_fixture(name, raw));
        assert_eq!(change, changed(CHANNEL), "{name}");
        counts(&store, REACTED)
    };
    // A thumbs-up from another user, then the account's own animated emoji.
    assert_eq!(apply("MESSAGE_REACTION_ADD", REACTION_ADD), [(1, false)]);
    assert_eq!(
        apply("MESSAGE_REACTION_ADD", REACTION_ADD_CUSTOM),
        [(1, false), (1, true)]
    );
    // A burst heart counts but is nobody's own.
    assert_eq!(
        apply("MESSAGE_REACTION_ADD", REACTION_ADD_BURST),
        [(1, false), (1, true), (1, false)]
    );
    // The thumbs-up is taken back; its entry goes with it.
    assert_eq!(
        apply("MESSAGE_REACTION_REMOVE", REACTION_REMOVE),
        [(1, true), (1, false)]
    );
    // The custom emoji is removed for everyone, then everything.
    assert_eq!(
        apply("MESSAGE_REACTION_REMOVE_EMOJI", REACTION_REMOVE_EMOJI),
        [(1, false)]
    );
    assert!(apply("MESSAGE_REACTION_REMOVE_ALL", REACTION_REMOVE_ALL).is_empty());
    check(&store);
}
