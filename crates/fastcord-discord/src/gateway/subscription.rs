//! Lazy guild subscriptions over opcode 37 (SPEC §4.4).
//!
//! The consumer states what it wants as a declarative [`SubscriptionTarget`]
//! (selected channel and member-list viewport, the guild of the active voice
//! channel, members it needs). The connection task compares that with what it
//! last transmitted in this session and sends only the difference, coalescing
//! rapid changes into one frame. Resume replays active state and releases guilds
//! no longer wanted without forgetting what the previous connection sent.
//!
//! Evidence and open points (docs/PROTOCOL.md, risk U2): the field set and its
//! semantics come from community documentation, not from a live capture. Every
//! field is therefore sent explicitly for each guild that changes, so the
//! result is the same whether or not the server treats omitted fields as
//! "unchanged". A guild is released by sending it with `typing: false` and
//! everything else empty, never by omission.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use fastcord_model::Snowflake;
use tokio::sync::watch;
use tokio::time::Instant;

use super::wire;

/// Member-list ranges are requested in blocks of this many rows.
pub const RANGE_BLOCK: u32 = 100;
/// At most this many live blocks of the current visible member-list viewport.
pub const MAX_RANGES: usize = 3;
/// Individually subscribed members per guild. Keeps one guild's subscription
/// far below the 15 KiB outbound ceiling.
pub const MAX_MEMBERS_PER_GUILD: usize = 200;
/// Rows beyond this are not addressable; keeps range arithmetic in `u32`.
const MAX_ROW: u32 = 10_000_000;

/// Changes made within this window of the first one are sent as one frame.
pub(crate) const COALESCE: Duration = Duration::from_millis(250);
/// A deferred send (the send ceiling was nearly reached) is retried after this.
const RETRY: Duration = Duration::from_secs(5);
/// Frames of each 60 s window kept for control traffic besides timed heartbeats.
pub(crate) const CONTROL_RESERVE: usize = 20;

/// An inclusive row range of a member list.
pub type MemberRange = (u32, u32);

/// What is subscribed for one guild.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuildSubscription {
    /// The guild itself is subscribed.
    pub subscribed: bool,
    /// Member-list ranges by channel.
    pub channels: BTreeMap<Snowflake, Vec<MemberRange>>,
    /// Individually subscribed members.
    pub members: BTreeSet<Snowflake>,
}

impl GuildSubscription {
    /// Everything off: the explicit way to leave a guild.
    pub fn released() -> Self {
        Self {
            subscribed: false,
            channels: BTreeMap::new(),
            members: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    guild: Snowflake,
    channel: Snowflake,
    /// Rows in view, inclusive.
    viewport: (u32, u32),
}

/// The subscriptions the consumer wants right now. Equal targets produce no
/// traffic.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscriptionTarget {
    selected: Option<Selection>,
    voice_guild: Option<Snowflake>,
    members: BTreeMap<Snowflake, BTreeSet<Snowflake>>,
}

impl SubscriptionTarget {
    /// Shows `channel` of `guild`. A different channel starts at the top of its
    /// member list; reselecting the current one keeps the viewport.
    pub fn select_channel(&mut self, guild: Snowflake, channel: Snowflake) {
        let same = self
            .selected
            .is_some_and(|sel| sel.guild == guild && sel.channel == channel);
        if !same {
            self.selected = Some(Selection {
                guild,
                channel,
                viewport: (0, 0),
            });
        }
        self.prune();
    }

    /// Nothing guild-related is on screen (a direct message, or no selection).
    pub fn clear_selection(&mut self) {
        self.selected = None;
        self.prune();
    }

    /// The member-list rows in view. Ignored without a selection.
    pub fn set_viewport(&mut self, first_row: u32, last_row: u32) {
        if let Some(selected) = &mut self.selected {
            let first = first_row.min(MAX_ROW);
            selected.viewport = (first, last_row.clamp(first, MAX_ROW));
        }
    }

    /// The guild of the active voice channel, kept subscribed while another
    /// guild is browsed.
    pub fn set_voice_guild(&mut self, guild: Option<Snowflake>) {
        self.voice_guild = guild;
        self.prune();
    }

    /// Members of `guild` needed individually (visible authors, reply
    /// targets, voice participants, permission computation). Replaces the
    /// previous set; only the selected and voice guilds keep one, and at most
    /// [`MAX_MEMBERS_PER_GUILD`] of the lowest IDs are kept.
    pub fn set_member_interest(
        &mut self,
        guild: Snowflake,
        users: impl IntoIterator<Item = Snowflake>,
    ) {
        if !self.is_subscribed_guild(guild) {
            self.members.remove(&guild);
            return;
        }
        let mut kept = BTreeSet::new();
        for user in users {
            if kept.len() < MAX_MEMBERS_PER_GUILD {
                kept.insert(user);
            } else if kept.last().is_some_and(|largest| user < *largest) && !kept.contains(&user) {
                kept.pop_last();
                kept.insert(user);
            }
        }
        self.members.insert(guild, kept);
    }

    pub fn selected_guild(&self) -> Option<Snowflake> {
        self.selected.map(|sel| sel.guild)
    }

    pub fn selected_channel(&self) -> Option<Snowflake> {
        self.selected.map(|sel| sel.channel)
    }

    pub fn voice_guild(&self) -> Option<Snowflake> {
        self.voice_guild
    }

    /// The members individually requested for `guild`.
    pub fn member_interest(&self, guild: Snowflake) -> impl Iterator<Item = Snowflake> + '_ {
        self.members.get(&guild).into_iter().flatten().copied()
    }

    /// The live member-list ranges of the selected channel.
    pub fn ranges(&self) -> Vec<MemberRange> {
        self.selected
            .map_or_else(Vec::new, |sel| live_ranges(sel.viewport))
    }

    fn is_subscribed_guild(&self, guild: Snowflake) -> bool {
        self.selected_guild() == Some(guild) || self.voice_guild == Some(guild)
    }

    /// Drops what no longer belongs to a subscribed guild, so the target (and
    /// with it the frames) stays bounded however often the consumer navigates.
    fn prune(&mut self) {
        let selected = self.selected_guild();
        let voice = self.voice_guild;
        self.members
            .retain(|guild, _| Some(*guild) == selected || Some(*guild) == voice);
    }

    /// The per-guild state this target asks for.
    pub fn guilds(&self) -> BTreeMap<Snowflake, GuildSubscription> {
        let mut guilds = BTreeMap::new();
        let members = |guild: Snowflake| self.member_interest(guild).collect::<BTreeSet<_>>();
        if let Some(selected) = self.selected {
            guilds.insert(
                selected.guild,
                GuildSubscription {
                    subscribed: true,
                    channels: BTreeMap::from([(selected.channel, live_ranges(selected.viewport))]),
                    members: members(selected.guild),
                },
            );
        }
        if let Some(guild) = self.voice_guild {
            guilds.entry(guild).or_insert_with(|| GuildSubscription {
                subscribed: true,
                channels: BTreeMap::new(),
                members: members(guild),
            });
        }
        guilds
    }
}

/// Blocks touched by the viewport, at most [`MAX_RANGES`] in total. The initial
/// selection has viewport `(0, 0)`, so it starts with one 100-entry block.
fn live_ranges(viewport: (u32, u32)) -> Vec<MemberRange> {
    let top = viewport.0.min(MAX_ROW);
    let first = top / RANGE_BLOCK;
    let last = viewport.1.clamp(top, MAX_ROW) / RANGE_BLOCK;
    (first..=last)
        .take(MAX_RANGES)
        .map(|block| (block * RANGE_BLOCK, block * RANGE_BLOCK + RANGE_BLOCK - 1))
        .collect()
}

/// A consumer's handle to the subscriptions of one [`Gateway`](super::Gateway).
/// Clones share one target.
#[derive(Clone)]
pub struct Subscriptions {
    target: Arc<watch::Sender<SubscriptionTarget>>,
}

impl fmt::Debug for Subscriptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Subscriptions")
    }
}

impl Subscriptions {
    pub(crate) fn new() -> (Self, watch::Receiver<SubscriptionTarget>) {
        let (sender, receiver) = watch::channel(SubscriptionTarget::default());
        (
            Self {
                target: Arc::new(sender),
            },
            receiver,
        )
    }

    /// Changes the target. Nothing is sent when the result equals the old
    /// target; otherwise the connection sends the difference shortly after,
    /// together with any further changes made meanwhile.
    pub fn update(&self, change: impl FnOnce(&mut SubscriptionTarget)) {
        self.target.send_if_modified(|target| {
            let before = target.clone();
            change(target);
            *target != before
        });
    }

    /// The current target.
    pub fn snapshot(&self) -> SubscriptionTarget {
        self.target.borrow().clone()
    }
}

/// One opcode 37 frame and the guild states it carries.
#[derive(Debug)]
pub(crate) struct Batch {
    pub(crate) frame: String,
    guilds: Vec<(Snowflake, GuildSubscription)>,
}

fn frame_of(guilds: &[(Snowflake, GuildSubscription)]) -> Result<String, wire::OutboundError> {
    let refs: Vec<(Snowflake, &GuildSubscription)> =
        guilds.iter().map(|(guild, sub)| (*guild, sub)).collect();
    wire::guild_subscriptions_bulk(&refs)
}

/// Session-owned record of what the server holds, and when to send next.
/// A reconnect pauses sends without discarding the ledger. READY starts a new
/// ledger; RESUMED replays active guilds and explicitly releases obsolete ones.
pub(crate) struct Subscriber {
    sent: BTreeMap<Snowflake, GuildSubscription>,
    replay: BTreeSet<Snowflake>,
    deadline: Option<Instant>,
    live: bool,
}

impl Subscriber {
    pub(crate) fn new() -> Self {
        Self {
            sent: BTreeMap::new(),
            replay: BTreeSet::new(),
            deadline: None,
            live: false,
        }
    }

    /// No subscriptions may be sent until this connection receives READY or
    /// RESUMED. What the previous connection sent still belongs to the session.
    pub(crate) fn pause(&mut self) {
        self.live = false;
        self.deadline = None;
    }

    /// READY arrived: the server created a new session with no subscriptions.
    pub(crate) fn start(&mut self, now: Instant) {
        self.sent.clear();
        self.replay.clear();
        self.live = true;
        self.deadline = Some(now);
    }

    /// RESUMED arrived: repeat active state, but retain the ledger so obsolete
    /// guilds are explicitly released, even after changes while disconnected.
    pub(crate) fn resume(&mut self, now: Instant) {
        self.replay = self.sent.keys().copied().collect();
        self.live = true;
        self.deadline = Some(now);
    }

    /// The target changed. The first change opens a coalescing window; changes
    /// inside it are included in the same send.
    pub(crate) fn changed(&mut self, now: Instant) {
        self.deadline.get_or_insert(now + COALESCE);
    }

    /// When to send next, once the session is ready.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.live.then_some(self.deadline).flatten()
    }

    /// Postpones a send that did not fit the send ceiling.
    pub(crate) fn defer(&mut self, now: Instant) {
        self.deadline = Some(now + RETRY);
    }

    pub(crate) fn settle(&mut self) {
        self.deadline = None;
    }

    /// The frames that bring the server from what it holds to `target`.
    pub(crate) fn plan(
        &self,
        target: &SubscriptionTarget,
    ) -> Result<Vec<Batch>, wire::OutboundError> {
        let wanted = target.guilds();
        // Release obsolete guilds before adding new ones, including when the
        // send budget allows only part of the plan on this turn.
        let mut changes: Vec<(Snowflake, GuildSubscription)> = self
            .sent
            .keys()
            .filter(|guild| !wanted.contains_key(guild))
            .map(|guild| (*guild, GuildSubscription::released()))
            .collect();
        changes.extend(wanted.into_iter().filter(|(guild, state)| {
            self.replay.contains(guild) || self.sent.get(guild) != Some(state)
        }));

        batch(changes)
    }

    /// Records a frame the socket accepted.
    pub(crate) fn commit(&mut self, batch: Batch) {
        for (guild, state) in batch.guilds {
            self.replay.remove(&guild);
            if state.subscribed {
                self.sent.insert(guild, state);
            } else {
                self.sent.remove(&guild);
            }
        }
    }
}

/// Packs guild states into as few frames as the outbound ceiling allows.
/// A guild that cannot fit by itself fails the whole plan, never disappearing
/// silently or producing a substitute empty frame.
fn batch(changes: Vec<(Snowflake, GuildSubscription)>) -> Result<Vec<Batch>, wire::OutboundError> {
    let mut batches = Vec::new();
    let mut current: Vec<(Snowflake, GuildSubscription)> = Vec::new();
    let mut current_frame = None;
    for change in changes {
        current.push(change);
        match frame_of(&current) {
            Ok(frame) => current_frame = Some(frame),
            Err(error) => {
                let overflow = current.pop().ok_or(error)?;
                let frame = current_frame.take().ok_or(error)?;
                batches.push(Batch {
                    frame,
                    guilds: std::mem::take(&mut current),
                });
                current.push(overflow);
                current_frame = Some(frame_of(&current)?);
            }
        }
    }
    if let Some(frame) = current_frame {
        batches.push(Batch {
            frame,
            guilds: current,
        });
    }
    Ok(batches)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn id(value: u64) -> Snowflake {
        Snowflake(value)
    }

    #[test]
    fn viewport_rounds_only_visible_blocks_and_caps_the_range_count() {
        assert_eq!(live_ranges((0, 0)), [(0, 99)]);
        assert_eq!(live_ranges((0, 99)), [(0, 99)]);
        assert_eq!(live_ranges((40, 140)), [(0, 99), (100, 199)]);
        assert_eq!(live_ranges((99, 100)), [(0, 99), (100, 199)]);
        assert_eq!(live_ranges((250, 260)), [(200, 299)]);
        assert_eq!(
            live_ranges((250, 420)),
            [(200, 299), (300, 399), (400, 499)]
        );
        assert_eq!(live_ranges((250, 0)), [(200, 299)]);
        // Never more than three live ranges, however tall the viewport.
        assert_eq!(live_ranges((0, 5_000)).len(), MAX_RANGES);
        assert_eq!(
            live_ranges((900, 9_999)),
            [(900, 999), (1_000, 1_099), (1_100, 1_199)]
        );
        // Hostile or absurd rows stay inside `u32` and the addressable rows.
        let huge = live_ranges((u32::MAX, u32::MAX));
        assert_eq!(huge, [(MAX_ROW, MAX_ROW + RANGE_BLOCK - 1)]);
        assert!(huge.iter().all(|(start, end)| start < end));
    }

    #[test]
    fn the_target_subscribes_only_the_selected_and_voice_guilds() {
        let mut target = SubscriptionTarget::default();
        assert!(target.guilds().is_empty());

        target.select_channel(id(1), id(11));
        target.set_voice_guild(Some(id(2)));
        let guilds = target.guilds();
        assert_eq!(guilds.keys().copied().collect::<Vec<_>>(), [id(1), id(2)]);
        assert_eq!(guilds[&id(1)].channels[&id(11)], [(0, 99)]);
        assert!(guilds[&id(2)].channels.is_empty());
        assert!(guilds.values().all(|g| g.subscribed));

        // The voice guild being browsed is one subscription, with ranges.
        target.select_channel(id(2), id(22));
        let guilds = target.guilds();
        assert_eq!(guilds.len(), 1);
        assert_eq!(guilds[&id(2)].channels[&id(22)], [(0, 99)]);

        target.clear_selection();
        target.set_voice_guild(None);
        assert!(target.guilds().is_empty());
    }

    #[test]
    fn member_interest_is_kept_only_for_subscribed_guilds_and_is_capped() {
        let mut target = SubscriptionTarget::default();
        target.select_channel(id(1), id(11));
        target.set_member_interest(id(1), [id(5), id(6)]);
        target.set_member_interest(id(9), [id(5)]);
        assert_eq!(
            target.member_interest(id(1)).collect::<Vec<_>>(),
            [id(5), id(6)]
        );
        assert_eq!(target.member_interest(id(9)).count(), 0);

        // Navigating away drops the interest with the subscription.
        target.select_channel(id(3), id(33));
        assert_eq!(target.member_interest(id(1)).count(), 0);

        target.set_member_interest(id(3), (1..=1_000).map(id));
        assert_eq!(target.member_interest(id(3)).count(), MAX_MEMBERS_PER_GUILD);
    }

    #[test]
    fn member_interest_keeps_the_lowest_unique_ids_from_large_unsorted_input() {
        let mut target = SubscriptionTarget::default();
        target.select_channel(id(1), id(11));
        target.set_member_interest(
            id(1),
            (2..=10_000)
                .rev()
                .chain(std::iter::repeat_n(2, 1_000))
                .chain([1])
                .map(id),
        );
        assert_eq!(
            target.member_interest(id(1)).collect::<Vec<_>>(),
            (1..=MAX_MEMBERS_PER_GUILD as u64)
                .map(id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn reselecting_keeps_the_viewport_and_a_new_channel_resets_it() {
        let mut target = SubscriptionTarget::default();
        target.select_channel(id(1), id(11));
        target.set_viewport(150, 170);
        assert_eq!(target.ranges(), [(100, 199)]);
        target.select_channel(id(1), id(11));
        assert_eq!(target.ranges(), [(100, 199)]);
        target.select_channel(id(1), id(12));
        assert_eq!(target.ranges(), [(0, 99)]);
        // A viewport without a selection is ignored.
        target.clear_selection();
        target.set_viewport(500, 600);
        assert!(target.ranges().is_empty());
    }

    #[test]
    fn plan_sends_only_the_difference_and_releases_explicitly() {
        let mut subscriber = Subscriber::new();
        let mut target = SubscriptionTarget::default();
        target.select_channel(id(1), id(11));

        let batches = subscriber.plan(&target).unwrap();
        assert_eq!(batches.len(), 1);
        let sent: Value = serde_json::from_str(&batches[0].frame).unwrap();
        assert_eq!(sent["op"], 37);
        assert_eq!(
            sent["d"]["subscriptions"],
            json!({"1": {
                "typing": true, "threads": false, "activities": false,
                "member_updates": false, "members": [],
                "channels": {"11": [[0, 99]]}, "thread_member_lists": []
            }})
        );
        for batch in batches {
            subscriber.commit(batch);
        }
        assert!(
            subscriber.plan(&target).unwrap().is_empty(),
            "nothing changed"
        );

        // Navigating: the old guild is released with every field explicit.
        target.select_channel(id(2), id(22));
        let batches = subscriber.plan(&target).unwrap();
        assert_eq!(batches.len(), 1);
        let sent: Value = serde_json::from_str(&batches[0].frame).unwrap();
        assert_eq!(
            sent["d"]["subscriptions"]["1"],
            json!({
                "typing": false, "threads": false, "activities": false,
                "member_updates": false, "members": [],
                "channels": {}, "thread_member_lists": []
            })
        );
        assert_eq!(sent["d"]["subscriptions"]["2"]["typing"], true);
        assert_eq!(sent["d"]["subscriptions"].as_object().unwrap().len(), 2);
        for batch in batches {
            subscriber.commit(batch);
        }
        assert!(subscriber.plan(&target).unwrap().is_empty());
        assert_eq!(subscriber.sent.keys().copied().collect::<Vec<_>>(), [id(2)]);
    }

    #[test]
    fn a_new_session_resends_the_complete_wanted_state() {
        let mut subscriber = Subscriber::new();
        let mut target = SubscriptionTarget::default();
        target.select_channel(id(1), id(11));
        target.set_voice_guild(Some(id(2)));
        let batches = subscriber.plan(&target).unwrap();
        for batch in batches {
            subscriber.commit(batch);
        }
        assert!(subscriber.plan(&target).unwrap().is_empty());

        let now = Instant::now();
        subscriber.start(now);
        assert_eq!(subscriber.deadline(), Some(now));
        let again = subscriber.plan(&target).unwrap();
        assert_eq!(again.len(), 1);
        let sent: Value = serde_json::from_str(&again[0].frame).unwrap();
        assert_eq!(sent["d"]["subscriptions"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn resume_replays_active_guilds_and_releases_disconnected_changes() {
        let now = Instant::now();
        let mut subscriber = Subscriber::new();
        subscriber.start(now);
        let mut target = SubscriptionTarget::default();
        target.select_channel(id(1), id(11));
        target.set_voice_guild(Some(id(2)));
        target.set_member_interest(id(2), [id(7)]);
        for batch in subscriber.plan(&target).unwrap() {
            subscriber.commit(batch);
        }
        subscriber.settle();

        subscriber.pause();
        target.select_channel(id(3), id(33));
        subscriber.changed(now);
        assert_eq!(subscriber.deadline(), None, "wait for RESUMED");
        assert_eq!(subscriber.sent.len(), 2, "the old session ledger survives");
        subscriber.resume(now);
        assert_eq!(subscriber.deadline(), Some(now));
        let batches = subscriber.plan(&target).unwrap();
        assert_eq!(batches.len(), 1);
        let frame: Value = serde_json::from_str(&batches[0].frame).unwrap();
        assert_eq!(
            frame["d"]["subscriptions"]["1"],
            json!({
                "typing": false, "threads": false, "activities": false,
                "member_updates": false, "members": [],
                "channels": {}, "thread_member_lists": []
            })
        );
        assert_eq!(frame["d"]["subscriptions"]["2"]["members"], json!(["7"]));
        assert_eq!(frame["d"]["subscriptions"]["2"]["typing"], true);
        assert_eq!(
            frame["d"]["subscriptions"]["3"]["channels"],
            json!({"33": [[0, 99]]})
        );
        for batch in batches {
            subscriber.commit(batch);
        }
        assert!(subscriber.plan(&target).unwrap().is_empty());
        assert!(subscriber.replay.is_empty());
        assert_eq!(
            subscriber.sent.keys().copied().collect::<Vec<_>>(),
            [id(2), id(3)]
        );

        subscriber.pause();
        target.clear_selection();
        target.set_voice_guild(None);
        subscriber.resume(now);
        let batches = subscriber.plan(&target).unwrap();
        assert_eq!(batches.len(), 1);
        let frame: Value = serde_json::from_str(&batches[0].frame).unwrap();
        let released = frame["d"]["subscriptions"].as_object().unwrap();
        assert_eq!(released.len(), 2);
        assert!(released.values().all(|state| state
            == &json!({
                "typing": false, "threads": false, "activities": false,
                "member_updates": false, "members": [],
                "channels": {}, "thread_member_lists": []
            })));
        for batch in batches {
            subscriber.commit(batch);
        }
        assert!(subscriber.sent.is_empty());
        assert!(subscriber.plan(&target).unwrap().is_empty());
    }

    #[test]
    fn new_ready_discards_obsolete_subscriptions_without_releasing_the_old_session() {
        let mut subscriber = Subscriber::new();
        let mut target = SubscriptionTarget::default();
        target.select_channel(id(1), id(11));
        target.set_voice_guild(Some(id(2)));
        for batch in subscriber.plan(&target).unwrap() {
            subscriber.commit(batch);
        }
        subscriber.pause();
        target.select_channel(id(3), id(33));
        target.set_voice_guild(None);
        subscriber.start(Instant::now());
        assert!(subscriber.sent.is_empty());
        let batches = subscriber.plan(&target).unwrap();
        assert_eq!(batches.len(), 1);
        let frame: Value = serde_json::from_str(&batches[0].frame).unwrap();
        assert_eq!(
            frame["d"]["subscriptions"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["3"]
        );
    }

    #[test]
    fn the_coalescing_window_opens_once_and_waits_for_the_session() {
        let mut subscriber = Subscriber::new();
        let t0 = Instant::now();
        subscriber.changed(t0);
        assert_eq!(subscriber.deadline(), None, "not ready yet");
        subscriber.start(t0);
        subscriber.settle();
        assert_eq!(subscriber.deadline(), None);
        subscriber.changed(t0);
        subscriber.changed(t0 + Duration::from_millis(200));
        assert_eq!(subscriber.deadline(), Some(t0 + COALESCE));
    }

    #[test]
    fn frames_split_before_the_outbound_ceiling_and_never_fall_back() {
        // Many guilds at once, each with the maximum member interest, cannot
        // fit one frame; every part must fit and nothing may be lost.
        let changes: Vec<_> = (1..=6u64)
            .map(|guild| {
                (
                    id(guild),
                    GuildSubscription {
                        subscribed: true,
                        channels: BTreeMap::from([(id(900 + guild), live_ranges((0, 500)))]),
                        members: (0..MAX_MEMBERS_PER_GUILD as u64)
                            .map(|n| id(175_928_847_299_117_063 + n))
                            .collect(),
                    },
                )
            })
            .collect();
        let batches = batch(changes).unwrap();
        assert!(batches.len() >= 2);
        let guilds: usize = batches.iter().map(|batch| batch.guilds.len()).sum();
        assert_eq!(guilds, 6);
        let mut subscriber = Subscriber::new();
        for batch in batches {
            assert!(batch.frame.len() <= wire::MAX_OUTBOUND_BYTES);
            let sent: Value = serde_json::from_str(&batch.frame).unwrap();
            assert_eq!(sent["op"], 37, "only opcode 37 is ever produced");
            subscriber.commit(batch);
        }
        assert_eq!(subscriber.sent.len(), 6);
    }

    #[test]
    fn an_oversized_guild_fails_the_whole_plan_instead_of_disappearing() {
        let oversized = GuildSubscription {
            subscribed: true,
            channels: BTreeMap::new(),
            members: (0..wire::MAX_OUTBOUND_BYTES as u64)
                .map(|n| id(u64::MAX - n))
                .collect(),
        };
        assert_eq!(
            batch(vec![(id(1), oversized.clone())]).unwrap_err(),
            wire::OutboundError
        );
        assert_eq!(
            batch(vec![
                (id(2), GuildSubscription::released()),
                (id(1), oversized)
            ])
            .unwrap_err(),
            wire::OutboundError
        );
    }

    #[test]
    fn one_guild_at_the_caps_always_fits_a_frame() {
        let mut target = SubscriptionTarget::default();
        target.select_channel(id(u64::MAX), id(u64::MAX - 1));
        target.set_viewport(0, u32::MAX);
        target.set_voice_guild(Some(id(u64::MAX - 2)));
        target.set_member_interest(id(u64::MAX), (0..1_000).map(|n| id(u64::MAX - n)));
        target.set_member_interest(id(u64::MAX - 2), (0..1_000).map(|n| id(u64::MAX - n)));
        let batches = Subscriber::new().plan(&target).unwrap();
        assert_eq!(batches.len(), 1);
        assert!(batches[0].frame.len() < wire::MAX_OUTBOUND_BYTES);
    }
}
