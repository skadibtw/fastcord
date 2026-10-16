//! Bounded, single-writer cache of message bodies (SPEC §4.5 message budget,
//! §10 history and virtualization).
//!
//! One [`MessageStore`] belongs to one worker. It retains the actual
//! [`Message`] data the timeline needs behind shared immutable `Arc` handles;
//! the virtual list keeps only IDs, heights, and [`MessageStore::revision`]s,
//! never a second copy of history. Every mutation goes through `&mut self`.
//!
//! # Retention
//! A channel holds at most one *run*: messages that are contiguous in the
//! server's history, ordered by ID, with the ID range the run vouches for
//! (messages deleted or too large to hold are intentional holes, not gaps). A
//! run ends at the live edge only while it is known to include the channel's
//! newest message, and reaches the beginning of history only after a short
//! page proved it. [`ChannelWindow`] exposes exactly the cursors that continue
//! the run in either direction; nothing else about a cold channel is
//! remembered, so there are no cold markers or tombstones.
//!
//! Limits: 500 messages per channel, 2,000 messages and 16 MiB of owned
//! allocations overall (the byte cap wins), 512 KiB per message, 64 channels.
//! Past a per-channel limit the messages **farthest from the viewport focus**
//! go first, from whichever end of the run that is, so paging toward older
//! history keeps the pages being read rather than the newest 500. The focus is
//! the message the viewport is anchored on ([`MessageStore::set_focus`]) or the
//! live edge. Past a global limit the least recently used channel sheds first,
//! by the same rule. Evicted pages are simply fetched again.
//!
//! # Races between REST pages and Gateway events
//! [`MessageStore::begin_page`] opens a bounded journal for its channel. What
//! the Gateway reports while the response is in flight wins over the response:
//! deleted messages are not resurrected, a message the Gateway changed keeps
//! the Gateway's state, and creates the response predates are kept. What the
//! journal cannot record (it is capped in entries and bytes) or cannot apply
//! (an update to a message that was not cached) makes the response
//! [`PageMerge::Stale`] instead of guessing; a stale response changes nothing
//! and the caller asks again.
//!
//! # Reactions
//! Reactions are kept as aggregates (a count and the account's own state per
//! emoji, never the reacting users) and follow the rules of an update. A
//! retained message changes copy-on-write, and only when the event changes what
//! it shows: the Gateway's echo of a reaction a REST answer already applied is
//! a no-op. The journaled copy of a message created while a request is in
//! flight takes the change; for any other message the event is only noted for
//! the request in flight. An event for a message that is not retained
//! allocates nothing and never creates a run.

#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fmt;
use std::mem::size_of;
use std::slice;
use std::sync::Arc;

use fastcord_model::{
    Attachment, Embed, Message, MessageUpdate, Reaction, ReactionChange, Referenced, ReplyPreview,
    Snowflake, User,
};

use crate::gateway::Dispatch;
use crate::history::{HistoryCursor, MESSAGE_PAGE_LIMIT};

/// Message bodies, cached runs, and the pending-page journal together.
pub const MESSAGE_BUDGET: usize = 16 * 1024 * 1024;
/// Retained messages across all channels.
pub const MAX_MESSAGES: usize = 2_000;
/// Retained messages in one channel.
pub const MAX_MESSAGES_PER_CHANNEL: usize = 500;
/// Largest message ([`message_bytes`]) the store will hold. A larger one is
/// rejected and reported, never truncated.
pub const MAX_MESSAGE_BYTES: usize = 512 * 1024;
/// Channels with a retained run.
pub const MAX_CHANNELS: usize = 64;

const MAX_PENDING_PAGES: usize = 4;
const JOURNAL_MAX_IDS: usize = 512;
const JOURNAL_MAX_CREATES: usize = 64;
/// Statically reserved for the journals; the cache gets the rest of the budget,
/// so `cache + journal` can never exceed [`MESSAGE_BUDGET`].
const JOURNAL_BUDGET: usize = 1024 * 1024;
/// Part of the journal budget for ID lists and bookkeeping (about 40 KiB at most).
const JOURNAL_RESERVE: usize = 64 * 1024;
const JOURNAL_CREATE_BUDGET: usize = JOURNAL_BUDGET - JOURNAL_RESERVE;
const CACHE_BUDGET: usize = MESSAGE_BUDGET - JOURNAL_BUDGET;
/// The strong and weak counts in front of the `Arc`'s value.
const ARC_HEADER: usize = 2 * size_of::<usize>();

fn text(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, String::capacity)
}

fn attachment_heap(attachment: &Attachment) -> usize {
    attachment.filename.capacity()
        + attachment.url.capacity()
        + text(&attachment.proxy_url)
        + text(&attachment.content_type)
}

fn user_heap(user: &User) -> usize {
    user.username.capacity() + text(&user.global_name) + text(&user.avatar)
}

fn message_heap(message: &Message) -> usize {
    user_heap(&message.author)
        + message.content.capacity()
        + message.timestamp.capacity()
        + text(&message.edited_timestamp)
        + text(&message.nonce)
        + message.call.as_ref().map_or(0, |call| {
            call.participants.capacity() * size_of::<Snowflake>() + text(&call.ended_timestamp)
        })
        + message.attachments.capacity() * size_of::<Attachment>()
        + message
            .attachments
            .iter()
            .map(attachment_heap)
            .sum::<usize>()
        + message.mentions.capacity() * size_of::<User>()
        + message.mentions.iter().map(user_heap).sum::<usize>()
        + message.mention_roles.capacity() * size_of::<Snowflake>()
        + message.embeds.capacity() * size_of::<Embed>()
        + message.embeds.iter().map(Embed::heap_bytes).sum::<usize>()
        + message.reactions.capacity() * size_of::<Reaction>()
        + message
            .reactions
            .iter()
            .map(|reaction| text(&reaction.emoji.name))
            .sum::<usize>()
        + match &message.referenced_message {
            Referenced::Message(preview) => {
                size_of::<ReplyPreview>() + user_heap(&preview.author) + preview.content.capacity()
            }
            Referenced::Unknown | Referenced::Deleted => 0,
        }
}

fn update_heap(update: &MessageUpdate) -> usize {
    update.content.as_ref().map_or(0, String::capacity)
        + update
            .edited_timestamp
            .as_ref()
            .and_then(Option::as_ref)
            .map_or(0, String::capacity)
        + update.embeds.as_ref().map_or(0, |embeds| {
            embeds.capacity() * size_of::<Embed>()
                + embeds.iter().map(Embed::heap_bytes).sum::<usize>()
        })
        + update.mentions.as_ref().map_or(0, |users| {
            users.capacity() * size_of::<User>() + users.iter().map(user_heap).sum::<usize>()
        })
        + update
            .mention_roles
            .as_ref()
            .map_or(0, |roles| roles.capacity() * size_of::<Snowflake>())
        + update.attachments.as_ref().map_or(0, |attachments| {
            attachments.capacity() * size_of::<Attachment>()
                + attachments.iter().map(attachment_heap).sum::<usize>()
        })
}

/// Estimated owned bytes of one message held through an `Arc`: the allocation
/// with its counts, the inline struct, and every owned string and vector at its
/// allocated capacity (including the nested author). Messages are compacted
/// before they are stored, so capacity equals length for stored bodies.
///
/// This is the unit of every byte limit: the store's 512 KiB per-message cap
/// ([`MAX_MESSAGE_BYTES`]) and its 16 MiB budget, and whatever bounded snapshot
/// a consumer builds from retained handles.
pub fn message_bytes(message: &Message) -> usize {
    ARC_HEADER + size_of::<Message>() + message_heap(message)
}

fn shrink_text(value: &mut Option<String>) {
    if let Some(value) = value {
        value.shrink_to_fit();
    }
}

fn compact_user(user: &mut User) {
    user.username.shrink_to_fit();
    shrink_text(&mut user.global_name);
    shrink_text(&mut user.avatar);
}

fn compact(message: &mut Message) {
    compact_user(&mut message.author);
    message.content.shrink_to_fit();
    message.timestamp.shrink_to_fit();
    shrink_text(&mut message.edited_timestamp);
    shrink_text(&mut message.nonce);
    message.attachments.shrink_to_fit();
    for attachment in &mut message.attachments {
        attachment.filename.shrink_to_fit();
        attachment.url.shrink_to_fit();
        shrink_text(&mut attachment.proxy_url);
        shrink_text(&mut attachment.content_type);
    }
    message.mentions.shrink_to_fit();
    for user in &mut message.mentions {
        compact_user(user);
    }
    message.mention_roles.shrink_to_fit();
    message.embeds.shrink_to_fit();
    for embed in &mut message.embeds {
        embed.compact();
    }
    message.reactions.shrink_to_fit();
    for reaction in &mut message.reactions {
        shrink_text(&mut reaction.emoji.name);
    }
    if let Referenced::Message(preview) = &mut message.referenced_message {
        compact_user(&mut preview.author);
        preview.content.shrink_to_fit();
    }
}

/// Identifies one in-flight page request. Obtained from
/// [`MessageStore::begin_page`], redeemed by [`MessageStore::merge_page`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageToken {
    channel: Snowflake,
    cursor: HistoryCursor,
    serial: u64,
}

impl PageToken {
    pub fn channel(&self) -> Snowflake {
        self.channel
    }

    pub fn cursor(&self) -> HistoryCursor {
        self.cursor
    }
}

/// What [`MessageStore::merge_page`] did with a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageMerge {
    /// The page was reconciled with the cache.
    Applied {
        /// Records in the response, before any were set aside. A response with
        /// fewer than a full page also proves the end of history in its
        /// direction.
        received: usize,
        /// Messages the channel retains after the merge and any eviction.
        held: usize,
        /// Messages that were not stored: a body over [`MAX_MESSAGE_BYTES`], or
        /// a record of another channel or on the wrong side of the cursor.
        /// Never silently truncated; surface it to the user.
        rejected: usize,
    },
    /// The token was cancelled, superseded, or released by a reset, the page no
    /// longer attaches to the run the cache holds, or a Gateway event that raced
    /// with the request makes the response unreliable. Nothing changed; ask
    /// again if the page is still wanted.
    Stale,
}

/// What a Gateway dispatch did to the cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreChange {
    /// The channel the event addressed, for message events.
    pub channel: Option<Snowflake>,
    /// A retained message was added, replaced, changed, or removed.
    pub changed: bool,
    /// Messages not stored (or no longer held) because their body exceeds
    /// [`MAX_MESSAGE_BYTES`].
    pub rejected: usize,
}

/// Where a channel's retained run stands and which pages continue it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelWindow {
    pub len: usize,
    pub oldest: Option<Snowflake>,
    pub newest: Option<Snowflake>,
    /// The request for the next older page, or `None` once the beginning of
    /// the channel is held. Built from the run's coverage, so messages that
    /// could not be stored never make a request repeat.
    pub older: Option<HistoryCursor>,
    /// The request for the next newer page, or `None` while the run reaches
    /// the live edge (new messages arrive over the Gateway).
    pub newer: Option<HistoryCursor>,
}

struct Slot {
    id: Snowflake,
    message: Arc<Message>,
    revision: u64,
    /// `message_bytes` when the message was stored or last changed.
    bytes: usize,
}

struct Journaled {
    message: Arc<Message>,
    bytes: usize,
}

/// One in-flight page request and what the Gateway reported meanwhile.
struct Pending {
    token: PageToken,
    /// Revisions from here on were assigned after the request began.
    begin: u64,
    /// Deleted over the Gateway (cached or not).
    deleted: Vec<Snowflake>,
    /// Changed over the Gateway but not cached, so the change had nowhere to go.
    touched: Vec<Snowflake>,
    /// Created over the Gateway while no run at the live edge could take them.
    created: Vec<Journaled>,
    created_bytes: usize,
    deleted_overflow: bool,
    touched_overflow: bool,
    /// A create was lost (journal full, or evicted from the live edge).
    created_incomplete: bool,
}

impl Pending {
    fn new(token: PageToken, begin: u64) -> Self {
        Self {
            token,
            begin,
            deleted: Vec::new(),
            touched: Vec::new(),
            created: Vec::new(),
            created_bytes: 0,
            deleted_overflow: false,
            touched_overflow: false,
            created_incomplete: false,
        }
    }

    /// Only a response that can make a run live needs the creates it predates.
    fn wants_creates(&self) -> bool {
        matches!(
            self.token.cursor,
            HistoryCursor::Latest | HistoryCursor::After(_)
        )
    }

    fn note_deleted(&mut self, id: Snowflake) {
        if self.deleted_overflow || self.deleted.contains(&id) {
            return;
        }
        if self.deleted.len() >= JOURNAL_MAX_IDS {
            self.deleted_overflow = true;
        } else {
            self.deleted.push(id);
        }
    }

    fn note_touched(&mut self, id: Snowflake) {
        if self.touched_overflow || self.touched.contains(&id) {
            return;
        }
        if self.touched.len() >= JOURNAL_MAX_IDS {
            self.touched_overflow = true;
        } else {
            self.touched.push(id);
        }
    }

    /// Applies a reaction change to the journaled copy `created[at]` of a
    /// message created while this request is in flight, so the response cannot
    /// bring back an older copy without it. `elsewhere` is what the other
    /// requests have journaled. A copy that would outgrow
    /// [`MAX_MESSAGE_BYTES`] or the journal is dropped and the creates marked
    /// incomplete instead, as for a create that does not fit.
    fn change_created(&mut self, at: usize, change: ReactionChange<'_>, elsewhere: usize) {
        let created = &mut self.created[at];
        if !created.message.alters_reactions(change) {
            return;
        }
        let before = created.bytes;
        let message = Arc::make_mut(&mut created.message);
        message.apply_reaction(change);
        let after = message_bytes(message);
        let journaled = elsewhere + self.created_bytes - before + after;
        if after > MAX_MESSAGE_BYTES || journaled > JOURNAL_CREATE_BUDGET {
            self.created_bytes -= before;
            self.created.remove(at);
            self.created_incomplete = true;
        } else {
            created.bytes = after;
            self.created_bytes = self.created_bytes - before + after;
        }
    }

    fn heap_bytes(&self) -> usize {
        (self.deleted.capacity() + self.touched.capacity()) * size_of::<Snowflake>()
            + self.created.capacity() * size_of::<Journaled>()
            + self.created_bytes
    }
}

enum Live {
    Inserted,
    Rejected,
    Ignored,
}

struct ChannelCache {
    id: Snowflake,
    /// Ascending, unique IDs.
    messages: VecDeque<Slot>,
    /// Every message in this inclusive ID range is retained or intentionally
    /// absent (deleted, too large). `None` only for a channel proven empty.
    coverage: Option<(u64, u64)>,
    /// Nothing older than the coverage exists.
    older_exhausted: bool,
    /// The coverage ends at the channel's newest message.
    live: bool,
    /// The message the viewport is anchored on; `None` follows the live edge.
    focus: Option<Snowflake>,
    /// Store tick of the last request, merge, or focus change.
    used: u64,
    slot_bytes: usize,
}

impl ChannelCache {
    fn new(id: Snowflake, used: u64) -> Self {
        Self {
            id,
            messages: VecDeque::new(),
            coverage: None,
            older_exhausted: false,
            live: false,
            focus: None,
            used,
            slot_bytes: 0,
        }
    }

    fn bytes(&self) -> usize {
        self.slot_bytes + self.messages.capacity() * size_of::<Slot>()
    }

    fn position(&self, id: Snowflake) -> Result<usize, usize> {
        self.messages.binary_search_by_key(&id, |slot| slot.id)
    }

    fn focus_position(&self) -> usize {
        let last = self.messages.len().saturating_sub(1);
        match self.focus {
            None => last,
            Some(focus) => self
                .messages
                .partition_point(|slot| slot.id < focus)
                .min(last),
        }
    }

    /// How many messages to drop from the (older, newer) end to shed `excess`,
    /// farthest from the focus first. A tie sheds the newer end, which favors
    /// keeping the older history being paged toward.
    fn plan_trim(&self, excess: usize) -> (usize, usize) {
        let len = self.messages.len();
        let excess = excess.min(len);
        let focus = self.focus_position();
        let (mut drop_old, mut drop_new) = (0, 0);
        while drop_old + drop_new < excess {
            let lo = drop_old;
            let hi = len - 1 - drop_new;
            let at = focus.clamp(lo, hi);
            if at - lo > hi - at {
                drop_old += 1;
            } else {
                drop_new += 1;
            }
        }
        (drop_old, drop_new)
    }

    /// IDs among the `drop_old` oldest and `drop_new` newest messages that were
    /// stored or changed at revision `since` or later.
    fn changed_since(&self, since: u64, drop_old: usize, drop_new: usize) -> Vec<Snowflake> {
        let oldest = self.messages.iter().take(drop_old);
        let newest = self.messages.iter().rev().take(drop_new);
        oldest
            .chain(newest)
            .filter(|slot| slot.revision >= since)
            .map(|slot| slot.id)
            .collect()
    }

    /// Drops from both ends and narrows the coverage to what is still held.
    /// Reports whether the run stopped reaching the live edge.
    fn evict(&mut self, drop_old: usize, drop_new: usize) -> bool {
        let mut freed = 0;
        for _ in 0..drop_new {
            if let Some(slot) = self.messages.pop_back() {
                freed += slot.bytes;
            }
        }
        let drop_old = drop_old.min(self.messages.len());
        freed += self
            .messages
            .drain(..drop_old)
            .map(|slot| slot.bytes)
            .sum::<usize>();
        self.slot_bytes -= freed;
        let live_lost = drop_new > 0 && self.live;
        if drop_new > 0 {
            self.live = false;
        }
        if drop_old > 0 {
            self.older_exhausted = false;
        }
        if let (Some(first), Some(last)) = (self.messages.front(), self.messages.back()) {
            let (lo, hi) = self.coverage.unwrap_or((first.id.0, last.id.0));
            self.coverage = Some((
                if drop_old > 0 { first.id.0 } else { lo },
                if drop_new > 0 { last.id.0 } else { hi },
            ));
        }
        self.shrink();
        live_lost
    }

    fn remove_at(&mut self, at: usize) {
        if let Some(slot) = self.messages.remove(at) {
            self.slot_bytes -= slot.bytes;
        }
        self.shrink();
    }

    fn shrink(&mut self) {
        if self.messages.capacity() > self.messages.len().saturating_mul(2).max(16) {
            self.messages.shrink_to_fit();
        }
    }

    /// A message arriving for a run at the live edge: appended when newer than
    /// the coverage, inserted in order when inside it and not yet held, and
    /// otherwise ignored (a duplicate, or older than anything vouched for).
    /// `make` runs only when the message is actually stored.
    fn live_insert(
        &mut self,
        id: Snowflake,
        oversize: bool,
        make: impl FnOnce() -> (Arc<Message>, usize),
        revision: &mut u64,
    ) -> Live {
        let at = match self.coverage {
            None => {
                self.coverage = Some((id.0, id.0));
                None
            }
            Some((lo, hi)) if id.0 > hi => {
                self.coverage = Some((lo, id.0));
                None
            }
            Some((lo, _)) if id.0 >= lo => match self.position(id) {
                Ok(_) => return Live::Ignored,
                Err(at) => Some(at),
            },
            Some(_) => return Live::Ignored,
        };
        if oversize {
            return Live::Rejected;
        }
        let (message, bytes) = make();
        *revision += 1;
        let slot = Slot {
            id,
            message,
            revision: *revision,
            bytes,
        };
        self.slot_bytes += bytes;
        match at {
            Some(at) => self.messages.insert(at, slot),
            None => self.messages.push_back(slot),
        }
        Live::Inserted
    }
}

#[derive(Clone, Copy)]
enum Target {
    /// The page attaches to the run in `channels[ci]`, whose coverage is `lo..=hi`.
    Merge { ci: usize, lo: u64, hi: u64 },
    /// The page starts a new run, replacing whatever the channel held.
    Fresh,
}

struct MergeRules<'a> {
    /// The inclusive ID range the page vouches for.
    lower: u64,
    upper: u64,
    /// Revisions at or above this were assigned after the request began.
    begin: u64,
    /// Sorted IDs deleted over the Gateway while the request was in flight.
    deleted: &'a [Snowflake],
}

impl MergeRules<'_> {
    fn is_deleted(&self, id: Snowflake) -> bool {
        self.deleted.binary_search(&id).is_ok()
    }

    /// A cached message inside the vouched range that the page does not list,
    /// and that the Gateway has not touched since the request began, was
    /// deleted on the server.
    fn vouched_deleted(&self, slot: &Slot) -> bool {
        (self.lower..=self.upper).contains(&slot.id.0) && slot.revision < self.begin
    }
}

enum Fresh {
    Slot(Slot),
    Rejected,
    Suppressed,
}

struct Merged {
    slots: Vec<Slot>,
    rejected: usize,
}

fn fresh_slot(mut message: Message, rules: &MergeRules<'_>, revision: &mut u64) -> Fresh {
    if rules.is_deleted(message.id) {
        return Fresh::Suppressed;
    }
    if message_bytes(&message) > MAX_MESSAGE_BYTES {
        return Fresh::Rejected;
    }
    compact(&mut message);
    let bytes = message_bytes(&message);
    *revision += 1;
    Fresh::Slot(Slot {
        id: message.id,
        message: Arc::new(message),
        revision: *revision,
        bytes,
    })
}

/// Merges an ID-ascending, deduplicated page into a run, ID by ID:
/// cached-only messages stay unless the page vouches they were deleted; listed
/// messages the cache lacks are added; for both, the Gateway's state wins when
/// it changed the message since the request began, an identical copy keeps its
/// handle and revision, and a differing copy replaces it.
fn merge_slots(
    old: VecDeque<Slot>,
    page: Vec<Message>,
    rules: &MergeRules<'_>,
    revision: &mut u64,
) -> Merged {
    let mut slots = Vec::with_capacity(old.len() + page.len());
    let mut rejected = 0;
    let mut old = old.into_iter().peekable();
    let mut page = page.into_iter().peekable();
    loop {
        let order = match (old.peek(), page.peek()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(cached), Some(listed)) => cached.id.cmp(&listed.id),
        };
        match order {
            Ordering::Less => {
                if let Some(slot) = old.next()
                    && !rules.vouched_deleted(&slot)
                {
                    slots.push(slot);
                }
            }
            Ordering::Greater => {
                if let Some(message) = page.next() {
                    match fresh_slot(message, rules, revision) {
                        Fresh::Slot(slot) => slots.push(slot),
                        Fresh::Rejected => rejected += 1,
                        Fresh::Suppressed => {}
                    }
                }
            }
            Ordering::Equal => {
                let (Some(slot), Some(message)) = (old.next(), page.next()) else {
                    break;
                };
                if rules.is_deleted(slot.id)
                    || slot.revision >= rules.begin
                    || *slot.message == message
                {
                    slots.push(slot);
                } else {
                    match fresh_slot(message, rules, revision) {
                        Fresh::Slot(fresh) => slots.push(fresh),
                        // Now too large to hold: drop the stale cached copy.
                        Fresh::Rejected => rejected += 1,
                        Fresh::Suppressed => {}
                    }
                }
            }
        }
    }
    Merged { slots, rejected }
}

/// The bounded message cache. See the [module documentation](self).
#[derive(Default)]
pub struct MessageStore {
    channels: Vec<ChannelCache>,
    pending: Vec<Pending>,
    /// The last revision handed out; strictly increasing, never reused.
    revision: u64,
    serial: u64,
    tick: u64,
    /// The account whose own reactions set [`Reaction::me`].
    user: Option<Snowflake>,
}

impl MessageStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Names the account whose reactions set [`Reaction::me`]: a reaction event
    /// by this user is the account's own. Until it is called every reaction
    /// counts as someone else's. [`clear`](Self::clear) keeps it, since it drops
    /// messages, not the identity of who is reading them; a different account
    /// calls this again.
    pub fn set_user(&mut self, user: Snowflake) {
        self.user = Some(user);
    }

    // ----- reads -----

    fn cache(&self, channel: Snowflake) -> Option<&ChannelCache> {
        self.channels.iter().find(|cache| cache.id == channel)
    }

    fn index(&self, channel: Snowflake) -> Option<usize> {
        self.channels.iter().position(|cache| cache.id == channel)
    }

    /// Retained message IDs, oldest first. Empty for a channel with no run.
    pub fn channel_ids(&self, channel: Snowflake) -> Vec<Snowflake> {
        self.ids(channel).collect()
    }

    /// [`channel_ids`](Self::channel_ids) without the allocation.
    pub fn ids(&self, channel: Snowflake) -> impl Iterator<Item = Snowflake> + '_ {
        self.cache(channel)
            .into_iter()
            .flat_map(|cache| cache.messages.iter().map(|slot| slot.id))
    }

    /// Retained `(id, revision)` pairs, oldest first: all a layout cache needs.
    pub fn items(&self, channel: Snowflake) -> impl Iterator<Item = (Snowflake, u64)> + '_ {
        self.cache(channel)
            .into_iter()
            .flat_map(|cache| cache.messages.iter().map(|slot| (slot.id, slot.revision)))
    }

    /// Retained messages, oldest first, borrowed rather than shared.
    pub fn messages(&self, channel: Snowflake) -> impl Iterator<Item = &Message> + '_ {
        self.cache(channel)
            .into_iter()
            .flat_map(|cache| cache.messages.iter().map(|slot| &*slot.message))
    }

    /// A shared handle to a retained message. Holding it keeps that body alive
    /// outside the store's accounting, so a consumer must bound what it holds.
    pub fn get(&self, channel: Snowflake, id: Snowflake) -> Option<Arc<Message>> {
        let cache = self.cache(channel)?;
        let at = cache.position(id).ok()?;
        Some(Arc::clone(&cache.messages[at].message))
    }

    /// Changes only when the retained message actually changes (an edit, a
    /// replaced REST copy), never for a repeated or no-op page or event, so a
    /// layout cache keyed by `(id, revision)` stays valid. Never reused.
    pub fn revision(&self, channel: Snowflake, id: Snowflake) -> Option<u64> {
        let cache = self.cache(channel)?;
        let at = cache.position(id).ok()?;
        Some(cache.messages[at].revision)
    }

    pub fn window(&self, channel: Snowflake) -> Option<ChannelWindow> {
        let cache = self.cache(channel)?;
        let older = match cache.coverage {
            Some((lo, _)) if !cache.older_exhausted => Some(HistoryCursor::Before(Snowflake(lo))),
            _ => None,
        };
        let newer = match cache.coverage {
            Some((_, hi)) if !cache.live => Some(HistoryCursor::After(Snowflake(hi))),
            _ => None,
        };
        Some(ChannelWindow {
            len: cache.messages.len(),
            oldest: cache.messages.front().map(|slot| slot.id),
            newest: cache.messages.back().map(|slot| slot.id),
            older,
            newer,
        })
    }

    /// Whether the channel's run vouches for `id`: a message with that ID is
    /// then either held or intentionally absent (deleted, or too large to hold),
    /// so asking Discord for it again cannot find it.
    pub fn covers(&self, channel: Snowflake, id: Snowflake) -> bool {
        self.cache(channel)
            .and_then(|cache| cache.coverage)
            .is_some_and(|(lo, hi)| (lo..=hi).contains(&id.0))
    }

    pub fn channel_len(&self, channel: Snowflake) -> usize {
        self.cache(channel).map_or(0, |cache| cache.messages.len())
    }

    pub fn messages_held(&self) -> usize {
        self.channels.iter().map(|cache| cache.messages.len()).sum()
    }

    pub fn channels_held(&self) -> usize {
        self.channels.len()
    }

    /// Page requests that have begun and neither merged nor been cancelled.
    pub fn pending_pages(&self) -> usize {
        self.pending.len()
    }

    fn cache_bytes(&self) -> usize {
        size_of::<Self>()
            + self.channels.capacity() * size_of::<ChannelCache>()
            + self.channels.iter().map(ChannelCache::bytes).sum::<usize>()
    }

    fn journal_bytes(&self) -> usize {
        self.pending.capacity() * size_of::<Pending>()
            + self.pending.iter().map(Pending::heap_bytes).sum::<usize>()
    }

    /// Estimated owned bytes of everything the store holds: retained messages
    /// (allocations and capacities), run and channel bookkeeping, and the
    /// journals. At most [`MESSAGE_BUDGET`] after every call.
    pub fn tracked_bytes(&self) -> usize {
        self.cache_bytes() + self.journal_bytes()
    }

    /// [`tracked_bytes`](Self::tracked_bytes) recomputed from the messages
    /// themselves instead of the incremental ledger, so tests can prove the
    /// ledger never drifts.
    pub fn recount(&self) -> usize {
        let cache = self
            .channels
            .iter()
            .map(|cache| {
                cache.messages.capacity() * size_of::<Slot>()
                    + cache
                        .messages
                        .iter()
                        .map(|slot| message_bytes(&slot.message))
                        .sum::<usize>()
            })
            .sum::<usize>();
        let journal = self
            .pending
            .iter()
            .map(|pending| {
                (pending.deleted.capacity() + pending.touched.capacity()) * size_of::<Snowflake>()
                    + pending.created.capacity() * size_of::<Journaled>()
                    + pending
                        .created
                        .iter()
                        .map(|journaled| message_bytes(&journaled.message))
                        .sum::<usize>()
            })
            .sum::<usize>();
        size_of::<Self>()
            + self.channels.capacity() * size_of::<ChannelCache>()
            + cache
            + self.pending.capacity() * size_of::<Pending>()
            + journal
    }

    // ----- viewport and paging -----

    /// Reports what the viewport is anchored on: a message ID, or `None` to
    /// follow the live edge. It decides which messages stay when the channel
    /// is over a limit, and marks the channel as in use. Call it with the
    /// actual anchor before asking for another page. A channel with no run is
    /// not created.
    pub fn set_focus(&mut self, channel: Snowflake, focus: Option<Snowflake>) {
        self.tick += 1;
        if let Some(ci) = self.index(channel) {
            let cache = &mut self.channels[ci];
            cache.focus = focus;
            cache.used = self.tick;
        }
    }

    /// Starts a page request for `channel` and returns its token. At most one
    /// request per channel is tracked: a second `begin_page` for the same
    /// channel supersedes the first, whose response will be [`PageMerge::Stale`].
    /// Beyond four tracked requests the oldest is superseded too.
    pub fn begin_page(&mut self, channel: Snowflake, cursor: HistoryCursor) -> PageToken {
        self.pending
            .retain(|pending| pending.token.channel != channel);
        while self.pending.len() >= MAX_PENDING_PAGES {
            self.pending.remove(0);
        }
        self.serial += 1;
        self.tick += 1;
        let token = PageToken {
            channel,
            cursor,
            serial: self.serial,
        };
        self.pending.push(Pending::new(token, self.revision + 1));
        if let Some(ci) = self.index(channel) {
            self.channels[ci].used = self.tick;
        }
        token
    }

    /// Abandons a request (failed, or no longer wanted). Idempotent.
    pub fn cancel_page(&mut self, token: PageToken) {
        self.pending.retain(|pending| pending.token != token);
    }

    /// Reconciles a response with the cache. `messages` is the page exactly as
    /// the server sent it. Pages attach to the channel's run: `Latest` starts
    /// or extends it at the live edge (replacing a run it cannot reach),
    /// `Before` and `After` extend it in their direction, and a page that does
    /// not touch the run is [`PageMerge::Stale`]. `Around` (a jump) extends the
    /// run it touches and otherwise replaces it with a run of just that page,
    /// which vouches only for the IDs between its first and last message (a
    /// short page cannot tell which side ran out); an empty `Around` page
    /// changes nothing.
    pub fn merge_page(&mut self, token: PageToken, messages: Vec<Message>) -> PageMerge {
        let Some(at) = self
            .pending
            .iter()
            .position(|pending| pending.token == token)
        else {
            return PageMerge::Stale;
        };
        let mut pending = self.pending.remove(at);
        let PageToken {
            channel, cursor, ..
        } = token;

        // Only messages of this channel on the requested side of the cursor.
        let received = messages.len();
        let mut rejected = 0;
        let mut page = Vec::with_capacity(received);
        for message in messages {
            let on_side = match cursor {
                HistoryCursor::Latest | HistoryCursor::Around(_) => true,
                HistoryCursor::Before(before) => message.id < before,
                HistoryCursor::After(after) => message.id > after,
            };
            if message.channel_id == channel && on_side {
                page.push(message);
            } else {
                rejected += 1;
            }
        }
        if (received > 0 || matches!(cursor, HistoryCursor::Around(_))) && page.is_empty() {
            return PageMerge::Applied {
                received,
                held: self.channel_len(channel),
                rejected,
            };
        }
        page.sort_unstable_by_key(|message| message.id);
        page.dedup_by_key(|message| message.id);

        // The inclusive ID range the page vouches for. A short page reaches the
        // end of history in its direction.
        let full = received >= MESSAGE_PAGE_LIMIT;
        let first = page.first().map(|message| message.id.0);
        let last = page.last().map(|message| message.id.0);
        let (lower, upper) = match cursor {
            HistoryCursor::Latest => (if full { first.unwrap_or(0) } else { 0 }, u64::MAX),
            HistoryCursor::Before(before) => (
                if full { first.unwrap_or(0) } else { 0 },
                before.0.saturating_sub(1),
            ),
            HistoryCursor::After(after) => (
                after.0.saturating_add(1),
                if full {
                    last.unwrap_or(u64::MAX)
                } else {
                    u64::MAX
                },
            ),
            HistoryCursor::Around(_) => (first.unwrap_or(0), last.unwrap_or(0)),
        };

        // Where it goes: onto the run it touches, or (Latest only) a new run.
        let existing = self.index(channel);
        let run = existing.and_then(|ci| self.channels[ci].coverage.map(|range| (ci, range)));
        let target = match run {
            Some((ci, (lo, hi)))
                if lower <= hi.saturating_add(1) && lo <= upper.saturating_add(1) =>
            {
                Target::Merge { ci, lo, hi }
            }
            _ if matches!(cursor, HistoryCursor::Latest | HistoryCursor::Around(_)) => {
                Target::Fresh
            }
            _ => return PageMerge::Stale,
        };
        let live_before = match target {
            Target::Merge { ci, .. } => self.channels[ci].live,
            Target::Fresh => false,
        };
        let live_after = live_before
            || cursor == HistoryCursor::Latest
            || (matches!(cursor, HistoryCursor::After(_)) && !full);

        // A Gateway event that raced with the request may make it unreliable.
        pending.deleted.sort_unstable();
        pending.touched.sort_unstable();
        if pending.deleted_overflow
            || pending.touched_overflow
            || (live_after && pending.created_incomplete)
            || page
                .iter()
                .any(|message| pending.touched.binary_search(&message.id).is_ok())
        {
            return PageMerge::Stale;
        }

        self.tick += 1;
        let ci = match target {
            Target::Merge { ci, .. } => ci,
            Target::Fresh => self.fresh_channel(channel, existing),
        };
        let old = std::mem::take(&mut self.channels[ci].messages);
        let merged = merge_slots(
            old,
            page,
            &MergeRules {
                lower,
                upper,
                begin: pending.begin,
                deleted: &pending.deleted,
            },
            &mut self.revision,
        );
        rejected += merged.rejected;
        let cache = &mut self.channels[ci];
        cache.slot_bytes = merged.slots.iter().map(|slot| slot.bytes).sum();
        let mut slots = merged.slots;
        slots.shrink_to_fit();
        cache.messages = VecDeque::from(slots);

        match target {
            Target::Fresh => {
                cache.coverage = first.zip(last);
                let around = matches!(cursor, HistoryCursor::Around(_));
                cache.older_exhausted = !full && !around;
                cache.live = !around;
            }
            Target::Merge { lo, hi, .. } => {
                let page_lo = match cursor {
                    HistoryCursor::After(after) => Some(after.0.saturating_add(1)),
                    HistoryCursor::Latest | HistoryCursor::Before(_) | HistoryCursor::Around(_) => {
                        first
                    }
                };
                let page_hi = match cursor {
                    HistoryCursor::Before(before) => Some(before.0.saturating_sub(1)),
                    HistoryCursor::Latest | HistoryCursor::After(_) | HistoryCursor::Around(_) => {
                        last
                    }
                };
                cache.coverage =
                    Some((lo.min(page_lo.unwrap_or(lo)), hi.max(page_hi.unwrap_or(hi))));
                match cursor {
                    HistoryCursor::Latest => {
                        cache.live = true;
                        cache.older_exhausted |= !full;
                    }
                    HistoryCursor::Before(_) => cache.older_exhausted |= !full,
                    HistoryCursor::After(_) => cache.live |= !full,
                    HistoryCursor::Around(_) => {}
                }
            }
        }
        match cursor {
            HistoryCursor::Latest => cache.focus = None,
            // A viewport that never reported an anchor is at the edge it is
            // paging from.
            HistoryCursor::Before(edge) | HistoryCursor::After(edge) => {
                if cache.focus.is_none() {
                    cache.focus = Some(edge);
                }
            }
            // The reader is being taken to the jump's target.
            HistoryCursor::Around(target) => cache.focus = Some(target),
        }
        cache.used = self.tick;

        // Messages created over the Gateway while the response was in flight
        // and that it predates, now that the run reaches the live edge.
        if self.channels[ci].live {
            for journaled in std::mem::take(&mut pending.created) {
                let id = journaled.message.id;
                if pending.deleted.binary_search(&id).is_ok() {
                    continue;
                }
                let Journaled { message, bytes } = journaled;
                self.channels[ci].live_insert(
                    id,
                    false,
                    move || (message, bytes),
                    &mut self.revision,
                );
            }
        }

        self.trim_channel(ci);
        self.enforce_budget();
        PageMerge::Applied {
            received,
            held: self.channel_len(channel),
            rejected,
        }
    }

    fn fresh_channel(&mut self, channel: Snowflake, existing: Option<usize>) -> usize {
        if let Some(ci) = existing {
            self.channels[ci] = ChannelCache::new(channel, self.tick);
            return ci;
        }
        if self.channels.len() >= MAX_CHANNELS {
            let victim = self
                .channels
                .iter()
                .enumerate()
                .min_by_key(|(_, cache)| cache.used)
                .map(|(index, _)| index);
            if let Some(victim) = victim {
                self.channels.remove(victim);
            }
        }
        self.channels.push(ChannelCache::new(channel, self.tick));
        self.channels.len() - 1
    }

    // ----- Gateway events -----

    /// Applies a message event in order. Other events are ignored: channel and
    /// guild lifecycle belongs to the caller ([`remove_channel`](Self::remove_channel)),
    /// and `READY` to [`clear`](Self::clear). The event is only borrowed; a
    /// message is copied once, and only when it is actually kept.
    ///
    /// * create: held only by a run at the live edge; otherwise dropped unless
    ///   a `Latest`/`After` request is in flight that may predate it;
    /// * update: merged into a held message (fields the event omits are kept);
    ///   a message that is not held cannot be updated, and a request in flight
    ///   learns of it;
    /// * delete and bulk delete: remove held messages, and suppress them in any
    ///   response still in flight. No tombstone outlives that request;
    /// * reaction add, remove, remove emoji, and remove all: change the
    ///   aggregate of a held message, but only when that alters it (a repeated
    ///   event, or the echo of the account's own reaction, is no change and
    ///   keeps the revision and the shared handle). A message that is not held
    ///   gets nothing: not a run, not any state. A request in flight is
    ///   updated instead: the journaled copy of a message created meanwhile
    ///   takes the change, any other message is noted like an update to one
    ///   that is not cached. Reactions by the user named with
    ///   [`set_user`](Self::set_user) are the account's own.
    pub fn apply_dispatch(&mut self, event: &Dispatch) -> StoreChange {
        match event {
            Dispatch::MessageCreate(message) => self.gateway_create(message),
            Dispatch::MessageUpdate(update) => self.gateway_update(update),
            Dispatch::MessageDelete(deleted) => {
                self.gateway_delete(deleted.channel_id, slice::from_ref(&deleted.id))
            }
            Dispatch::MessageDeleteBulk(deleted) => {
                self.gateway_delete(deleted.channel_id, &deleted.ids)
            }
            Dispatch::MessageReactionAdd(reaction) => self.gateway_reaction(
                reaction.channel_id,
                reaction.message_id,
                ReactionChange::Add {
                    emoji: &reaction.emoji,
                    by_me: self.user == Some(reaction.user_id),
                    burst: reaction.burst,
                },
            ),
            Dispatch::MessageReactionRemove(reaction) => self.gateway_reaction(
                reaction.channel_id,
                reaction.message_id,
                ReactionChange::Remove {
                    emoji: &reaction.emoji,
                    by_me: self.user == Some(reaction.user_id),
                    burst: reaction.burst,
                },
            ),
            Dispatch::MessageReactionRemoveEmoji(removed) => self.gateway_reaction(
                removed.channel_id,
                removed.message_id,
                ReactionChange::RemoveEmoji(&removed.emoji),
            ),
            Dispatch::MessageReactionRemoveAll(removed) => self.gateway_reaction(
                removed.channel_id,
                removed.message_id,
                ReactionChange::RemoveAll,
            ),
            _ => StoreChange::default(),
        }
    }

    fn gateway_create(&mut self, message: &Message) -> StoreChange {
        let channel = message.channel_id;
        let mut change = StoreChange {
            channel: Some(channel),
            ..StoreChange::default()
        };
        let estimate = message_bytes(message);
        let oversize = estimate > MAX_MESSAGE_BYTES;
        if let Some(ci) = self.index(channel)
            && self.channels[ci].live
        {
            let make = || {
                let owned = message.clone();
                let bytes = message_bytes(&owned);
                (Arc::new(owned), bytes)
            };
            match self.channels[ci].live_insert(message.id, oversize, make, &mut self.revision) {
                Live::Inserted => {
                    self.trim_channel(ci);
                    self.enforce_budget();
                    change.changed = self.get(channel, message.id).is_some();
                }
                Live::Rejected => change.rejected = 1,
                Live::Ignored => {}
            }
            return change;
        }
        // Not at the live edge: only a request that may predate the message
        // can still need it.
        let journaled: usize = self
            .pending
            .iter()
            .map(|pending| pending.created_bytes)
            .sum();
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.token.channel == channel && pending.wants_creates())
        {
            if oversize {
                change.rejected = 1;
            } else if pending.created.len() >= JOURNAL_MAX_CREATES
                || journaled + estimate > JOURNAL_CREATE_BUDGET
            {
                pending.created_incomplete = true;
            } else {
                let owned = message.clone();
                let bytes = message_bytes(&owned);
                pending.created_bytes += bytes;
                pending.created.push(Journaled {
                    message: Arc::new(owned),
                    bytes,
                });
            }
        }
        change
    }

    /// The cache index and position of a retained message.
    fn held(&self, channel: Snowflake, id: Snowflake) -> Option<(usize, usize)> {
        let ci = self.index(channel)?;
        let at = self.channels[ci].position(id).ok()?;
        Some((ci, at))
    }

    /// Drops a retained message that no longer fits [`MAX_MESSAGE_BYTES`]
    /// (never truncated), and has any request in flight suppress its older copy.
    fn reject_held(&mut self, ci: usize, at: usize) -> StoreChange {
        let channel = self.channels[ci].id;
        let id = self.channels[ci].messages[at].id;
        self.channels[ci].remove_at(at);
        self.note_deleted(channel, id);
        StoreChange {
            channel: Some(channel),
            changed: true,
            rejected: 1,
        }
    }

    /// Changes the retained message at `channels[ci].messages[at]` in place
    /// through `edit` and settles its ledgers: a new revision and byte count,
    /// or removal when it no longer fits [`MAX_MESSAGE_BYTES`].
    fn edit_held(&mut self, ci: usize, at: usize, edit: impl FnOnce(&mut Message)) -> StoreChange {
        let cache = &mut self.channels[ci];
        let channel = cache.id;
        let slot = &mut cache.messages[at];
        let before = slot.bytes;
        // Copy-on-write: a consumer still holding the previous handle keeps it.
        let message = Arc::make_mut(&mut slot.message);
        edit(message);
        let after = message_bytes(message);
        if after > MAX_MESSAGE_BYTES {
            return self.reject_held(ci, at);
        }
        self.revision += 1;
        let slot = &mut cache.messages[at];
        slot.bytes = after;
        slot.revision = self.revision;
        cache.slot_bytes = cache.slot_bytes - before + after;
        self.enforce_budget();
        StoreChange {
            channel: Some(channel),
            changed: true,
            rejected: 0,
        }
    }

    fn gateway_update(&mut self, update: &MessageUpdate) -> StoreChange {
        let channel = update.channel_id;
        let unchanged = StoreChange {
            channel: Some(channel),
            ..StoreChange::default()
        };
        let Some((ci, at)) = self.held(channel, update.id) else {
            // Nothing is held to merge into; a response in flight may carry the
            // copy from before this update.
            if update.has_fields() {
                self.note_touched(channel, update.id);
            }
            return unchanged;
        };
        if !update.alters(&self.channels[ci].messages[at].message) {
            return unchanged;
        }
        if update_heap(update) > MAX_MESSAGE_BYTES {
            return self.reject_held(ci, at);
        }
        self.edit_held(ci, at, |message| message.apply(update.clone()))
    }

    fn gateway_reaction(
        &mut self,
        channel: Snowflake,
        id: Snowflake,
        change: ReactionChange<'_>,
    ) -> StoreChange {
        let unchanged = StoreChange {
            channel: Some(channel),
            ..StoreChange::default()
        };
        let Some((ci, at)) = self.held(channel, id) else {
            self.reaction_unheld(channel, id, change);
            return unchanged;
        };
        if !self.channels[ci].messages[at]
            .message
            .alters_reactions(change)
        {
            return unchanged;
        }
        self.edit_held(ci, at, |message| {
            message.apply_reaction(change);
        })
    }

    /// A reaction event for a message that is not retained. Nothing is
    /// allocated or kept for the message, but a request in flight for the
    /// channel may carry an older copy of it: the copy created while the
    /// request is in flight takes the change, and any other message is noted
    /// like an update to an uncached one. Without such a request nothing at
    /// all happens.
    fn reaction_unheld(&mut self, channel: Snowflake, id: Snowflake, change: ReactionChange<'_>) {
        let Some(index) = self
            .pending
            .iter()
            .position(|pending| pending.token.channel == channel)
        else {
            return;
        };
        let Some(at) = self.pending[index]
            .created
            .iter()
            .position(|created| created.message.id == id)
        else {
            self.pending[index].note_touched(id);
            return;
        };
        let elsewhere = self
            .pending
            .iter()
            .enumerate()
            .filter(|&(other, _)| other != index)
            .map(|(_, pending)| pending.created_bytes)
            .sum();
        self.pending[index].change_created(at, change, elsewhere);
    }

    fn gateway_delete(&mut self, channel: Snowflake, ids: &[Snowflake]) -> StoreChange {
        let mut change = StoreChange {
            channel: Some(channel),
            ..StoreChange::default()
        };
        let held = self.index(channel);
        for &id in ids {
            if let Some(ci) = held
                && let Ok(at) = self.channels[ci].position(id)
            {
                self.channels[ci].remove_at(at);
                change.changed = true;
            }
            self.note_deleted(channel, id);
        }
        change
    }

    fn note_deleted(&mut self, channel: Snowflake, id: Snowflake) {
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.token.channel == channel)
        {
            pending.note_deleted(id);
        }
    }

    fn note_touched(&mut self, channel: Snowflake, id: Snowflake) {
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.token.channel == channel)
        {
            pending.note_touched(id);
        }
    }

    // ----- eviction -----

    fn trim_channel(&mut self, ci: usize) {
        let len = self.channels[ci].messages.len();
        if len > MAX_MESSAGES_PER_CHANNEL {
            self.evict(ci, len - MAX_MESSAGES_PER_CHANNEL);
        }
    }

    /// Sheds `count` messages from channel `ci`, farthest from its focus first.
    /// A channel left with nothing is dropped: its coverage means nothing
    /// without messages. `ci` is invalid afterwards.
    fn evict(&mut self, ci: usize, count: usize) {
        let (drop_old, drop_new) = self.channels[ci].plan_trim(count);
        let channel = self.channels[ci].id;
        // A message the Gateway changed while a page request is in flight must
        // not come back from that request's older copy once it is evicted.
        if let Some(pending) = self
            .pending
            .iter_mut()
            .find(|pending| pending.token.channel == channel)
        {
            for id in self.channels[ci].changed_since(pending.begin, drop_old, drop_new) {
                pending.note_touched(id);
            }
        }
        let live_lost = self.channels[ci].evict(drop_old, drop_new);
        if live_lost {
            // Messages created while a request is in flight may just have been
            // evicted from the live edge; the response cannot restore them.
            for pending in &mut self.pending {
                if pending.token.channel == channel && pending.wants_creates() {
                    pending.created_incomplete = true;
                }
            }
        }
        if self.channels[ci].messages.is_empty() {
            self.channels.remove(ci);
        }
    }

    fn enforce_budget(&mut self) {
        while self.messages_held() > MAX_MESSAGES || self.cache_bytes() > CACHE_BUDGET {
            let victim = self
                .channels
                .iter()
                .enumerate()
                .filter(|(_, cache)| !cache.messages.is_empty())
                .min_by_key(|(_, cache)| cache.used)
                .map(|(index, _)| index);
            let Some(victim) = victim else {
                break;
            };
            self.evict(victim, 1);
        }
    }

    // ----- lifecycle -----

    /// Drops a channel's run and any request in flight for it (channel deleted,
    /// access lost, or no longer wanted).
    pub fn remove_channel(&mut self, channel: Snowflake) {
        self.channels.retain(|cache| cache.id != channel);
        self.pending
            .retain(|pending| pending.token.channel != channel);
    }

    /// Drops everything and releases the allocations: logout, a new session's
    /// `READY`. Tokens issued before are stale. Revisions are never reused. The
    /// account named by [`set_user`](Self::set_user) stays.
    pub fn clear(&mut self) {
        self.channels = Vec::new();
        self.pending = Vec::new();
    }
}

// Message bodies never reach logs: the store names only counts.
impl fmt::Debug for MessageStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageStore")
            .field("channels", &self.channels.len())
            .field("messages", &self.messages_held())
            .field("pending_pages", &self.pending.len())
            .field("bytes", &self.tracked_bytes())
            .finish()
    }
}
