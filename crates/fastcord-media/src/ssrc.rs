//! Who sends which SSRC (SPEC §6.2).
//!
//! RTP packets name only an SSRC; the user behind it comes from voice Gateway
//! Speaking (opcode 5) and Video (opcode 12) events and is forgotten on Client
//! Disconnect (opcode 13). Media can race ahead of those events, so packets
//! from an unknown SSRC wait briefly in a [`PendingSsrc`] buffer and are never
//! attributed to an arbitrary user.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use fastcord_model::Snowflake;
use tokio::time::Instant;

/// More distinct SSRCs than any voice channel produces (99 users with audio,
/// simulcast video, and RTX). A server sending more is not followed further.
pub const MAX_MAPPINGS: usize = 1024;
/// How long media from an unannounced SSRC is held for its Speaking/Video
/// event.
pub const PENDING_WINDOW: Duration = Duration::from_millis(100);
/// Byte cap for held media: 100 ms of a busy channel's audio.
pub const PENDING_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StreamKind {
    Audio,
    Video,
    /// RTX repair stream of a video stream.
    Retransmission,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SsrcOwner {
    pub user_id: Snowflake,
    pub kind: StreamKind,
}

/// A video stream announced in opcode 12.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoStream {
    pub ssrc: u32,
    pub rtx_ssrc: Option<u32>,
}

/// SSRC ownership for one voice connection. SSRC 0 means "none" on the wire
/// and is never mapped.
#[derive(Debug, Default)]
pub struct SsrcMap {
    owners: HashMap<u32, SsrcOwner>,
}

impl SsrcMap {
    pub fn owner(&self, ssrc: u32) -> Option<SsrcOwner> {
        self.owners.get(&ssrc).copied()
    }

    pub fn len(&self) -> usize {
        self.owners.len()
    }

    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    /// The user's audio SSRC from a Speaking event. Returns whether the SSRC
    /// is now mapped to this user.
    pub fn speaking(&mut self, user_id: Snowflake, ssrc: u32) -> bool {
        self.replace(user_id, &[StreamKind::Audio], [(ssrc, StreamKind::Audio)])
    }

    /// A Video event: the audio SSRC it names plus every primary and RTX
    /// stream, replacing the user's previous video streams. An empty stream
    /// list clears the user's video.
    pub fn video(&mut self, user_id: Snowflake, audio_ssrc: u32, streams: &[VideoStream]) {
        let mut kinds: &[StreamKind] = &[StreamKind::Video, StreamKind::Retransmission];
        let audio = [(audio_ssrc, StreamKind::Audio)];
        if audio_ssrc != 0 {
            kinds = &[
                StreamKind::Audio,
                StreamKind::Video,
                StreamKind::Retransmission,
            ];
        }
        let video = streams.iter().flat_map(|stream| {
            std::iter::once((stream.ssrc, StreamKind::Video))
                .chain(stream.rtx_ssrc.map(|rtx| (rtx, StreamKind::Retransmission)))
        });
        self.replace(user_id, kinds, audio.into_iter().chain(video));
    }

    /// Client Disconnect: every SSRC of the user is discarded.
    pub fn remove_user(&mut self, user_id: Snowflake) {
        self.owners.retain(|_, owner| owner.user_id != user_id);
    }

    pub fn clear(&mut self) {
        self.owners.clear();
    }

    /// Drops the user's mappings of `kinds`, then maps `entries` to the user.
    /// An SSRC another user owned moves to this user. Returns whether every
    /// nonzero entry was mapped.
    fn replace(
        &mut self,
        user_id: Snowflake,
        kinds: &[StreamKind],
        entries: impl IntoIterator<Item = (u32, StreamKind)>,
    ) -> bool {
        self.owners
            .retain(|_, owner| owner.user_id != user_id || !kinds.contains(&owner.kind));
        let mut all = true;
        for (ssrc, kind) in entries {
            if ssrc == 0 {
                continue;
            }
            if self.owners.len() >= MAX_MAPPINGS && !self.owners.contains_key(&ssrc) {
                all = false;
                continue;
            }
            self.owners.insert(ssrc, SsrcOwner { user_id, kind });
        }
        all
    }
}

/// Media from SSRCs nobody has announced yet, held for [`PENDING_WINDOW`] and
/// at most [`PENDING_BYTES`]. The oldest entries go first when either bound
/// is reached.
#[derive(Debug)]
pub struct PendingSsrc<T> {
    entries: VecDeque<Pending<T>>,
    bytes: usize,
    window: Duration,
    capacity: usize,
}

#[derive(Debug)]
struct Pending<T> {
    arrived: Instant,
    ssrc: u32,
    bytes: usize,
    item: T,
}

impl<T> Default for PendingSsrc<T> {
    fn default() -> Self {
        Self::new(PENDING_WINDOW, PENDING_BYTES)
    }
}

impl<T> PendingSsrc<T> {
    pub fn new(window: Duration, capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            window,
            capacity,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Holds `item` (accounted as `bytes`). Returns how many held items were
    /// dropped to make room, including `item` itself if it can never fit.
    pub fn push(&mut self, now: Instant, ssrc: u32, bytes: usize, item: T) -> usize {
        let mut dropped = self.expire(now);
        if bytes > self.capacity {
            return dropped + 1;
        }
        while self.bytes + bytes > self.capacity {
            self.pop_front();
            dropped += 1;
        }
        self.bytes += bytes;
        self.entries.push_back(Pending {
            arrived: now,
            ssrc,
            bytes,
            item,
        });
        dropped
    }

    /// Removes and returns the held items of `ssrc` in arrival order.
    pub fn take(&mut self, ssrc: u32) -> Vec<T> {
        let mut taken = Vec::new();
        let mut kept = VecDeque::with_capacity(self.entries.len());
        for entry in self.entries.drain(..) {
            if entry.ssrc == ssrc {
                self.bytes -= entry.bytes;
                taken.push(entry.item);
            } else {
                kept.push_back(entry);
            }
        }
        self.entries = kept;
        taken
    }

    /// Drops items older than the window; returns how many.
    pub fn expire(&mut self, now: Instant) -> usize {
        let mut dropped = 0;
        while self
            .entries
            .front()
            .is_some_and(|entry| now.saturating_duration_since(entry.arrived) >= self.window)
        {
            self.pop_front();
            dropped += 1;
        }
        dropped
    }

    /// When the oldest held item expires.
    pub fn next_expiry(&self) -> Option<Instant> {
        self.entries
            .front()
            .map(|entry| entry.arrived + self.window)
    }

    pub fn clear(&mut self) {
        self.entries = VecDeque::new();
        self.bytes = 0;
    }

    fn pop_front(&mut self) {
        if let Some(entry) = self.entries.pop_front() {
            self.bytes -= entry.bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: Snowflake = Snowflake(1);
    const BOB: Snowflake = Snowflake(2);

    fn owner(user_id: Snowflake, kind: StreamKind) -> Option<SsrcOwner> {
        Some(SsrcOwner { user_id, kind })
    }

    #[test]
    fn speaking_maps_audio_and_a_new_ssrc_replaces_the_old_one() {
        let mut map = SsrcMap::default();
        assert!(map.speaking(ALICE, 100));
        assert_eq!(map.owner(100), owner(ALICE, StreamKind::Audio));
        // SSRC change (e.g. reconnect): old SSRC no longer routes to Alice.
        map.speaking(ALICE, 101);
        assert_eq!(map.owner(100), None);
        assert_eq!(map.owner(101), owner(ALICE, StreamKind::Audio));
        // A reassigned SSRC moves to its new owner.
        map.speaking(BOB, 101);
        assert_eq!(map.owner(101), owner(BOB, StreamKind::Audio));
        assert_eq!(map.len(), 1);
        // SSRC 0 is "none".
        map.speaking(ALICE, 0);
        assert_eq!(map.owner(0), None);
    }

    #[test]
    fn video_events_replace_streams_and_disconnect_forgets_everything() {
        let mut map = SsrcMap::default();
        map.speaking(ALICE, 100);
        map.video(
            ALICE,
            100,
            &[
                VideoStream {
                    ssrc: 200,
                    rtx_ssrc: Some(201),
                },
                VideoStream {
                    ssrc: 202,
                    rtx_ssrc: None,
                },
            ],
        );
        assert_eq!(map.owner(100), owner(ALICE, StreamKind::Audio));
        assert_eq!(map.owner(200), owner(ALICE, StreamKind::Video));
        assert_eq!(map.owner(201), owner(ALICE, StreamKind::Retransmission));
        assert_eq!(map.owner(202), owner(ALICE, StreamKind::Video));
        // Clearing video (audio_ssrc 0, no streams) keeps audio.
        map.video(ALICE, 0, &[]);
        assert_eq!(map.owner(100), owner(ALICE, StreamKind::Audio));
        assert_eq!(map.owner(200), None);
        assert_eq!(map.owner(201), None);
        map.speaking(BOB, 300);
        map.remove_user(ALICE);
        assert_eq!(map.owner(100), None);
        assert_eq!(map.owner(300), owner(BOB, StreamKind::Audio));
    }

    #[test]
    fn mappings_are_capped() {
        let mut map = SsrcMap::default();
        for ssrc in 1..=MAX_MAPPINGS as u32 {
            assert!(map.speaking(Snowflake(u64::from(ssrc)), ssrc));
        }
        assert!(!map.speaking(Snowflake(99_999), 99_999));
        assert_eq!(map.owner(99_999), None);
        // An existing user may still change its own SSRC.
        assert!(map.speaking(Snowflake(1), 50_000));
        assert_eq!(map.len(), MAX_MAPPINGS);
    }

    #[tokio::test(start_paused = true)]
    async fn pending_media_is_bounded_in_time_and_bytes() {
        let mut pending = PendingSsrc::new(Duration::from_millis(100), 100);
        let start = Instant::now();
        assert_eq!(pending.push(start, 7, 40, "a"), 0);
        assert_eq!(
            pending.push(start + Duration::from_millis(50), 8, 40, "b"),
            0
        );
        assert_eq!(
            pending.next_expiry(),
            Some(start + Duration::from_millis(100))
        );
        // Over the byte cap: the oldest goes.
        assert_eq!(
            pending.push(start + Duration::from_millis(60), 7, 40, "c"),
            1
        );
        assert_eq!(pending.bytes(), 80);
        // Larger than the whole cap: never held.
        assert_eq!(
            pending.push(start + Duration::from_millis(60), 9, 101, "x"),
            1
        );
        assert_eq!(pending.len(), 2);
        assert_eq!(pending.take(7), ["c"]);
        assert_eq!(pending.bytes(), 40);
        assert_eq!(pending.take(7), Vec::<&str>::new());
        // "b" arrived at 50 ms and expires at 150 ms.
        assert_eq!(pending.expire(start + Duration::from_millis(149)), 0);
        assert_eq!(pending.expire(start + Duration::from_millis(150)), 1);
        assert!(pending.is_empty());
        assert_eq!(pending.bytes(), 0);
        assert_eq!(pending.next_expiry(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn take_keeps_arrival_order_and_other_ssrcs() {
        let mut pending = PendingSsrc::default();
        let now = Instant::now();
        for (ssrc, item) in [(1, 10), (2, 20), (1, 11), (3, 30), (1, 12)] {
            pending.push(now, ssrc, 100, item);
        }
        assert_eq!(pending.take(1), [10, 11, 12]);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending.bytes(), 200);
        assert_eq!(pending.take(3), [30]);
        assert_eq!(pending.take(2), [20]);
    }
}
