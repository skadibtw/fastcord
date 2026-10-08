//! Variable-height virtual list for message history (SPEC §10, "Virtualization").
//!
//! Rows have unknown, changing heights (wrapped text, attachments, later
//! images), so the list is built from three cooperating parts that never need a
//! character-count guess:
//!
//! * [`VariableList`] is purely numeric. It owns the ordered message IDs
//!   (at most [`MAX_ITEMS`]), the heights measured for each ID keyed by width
//!   bucket (at most two buckets per ID), a Fenwick prefix-sum index over the
//!   heights of the current bucket, and the logical scroll position: an anchor
//!   (message ID plus the pixel offset inside that row). It never sees message
//!   bodies. It lives next to the message store in the account worker, which
//!   tells it the IDs, viewport reports and measurements, and asks it for a
//!   [`Window`]: the rows to build (the visible ones plus one viewport of
//!   overscan on either side) and the exact spacers standing in for the rest.
//!   Heights are evicted together with their IDs.
//! * [`view`] builds only those rows between a top and a bottom spacer inside a
//!   stock `scrollable`, wrapped by a widget that sees the real layout.
//!   Heights are measured from that layout, never estimated from text.
//! * [`Tracker`] is that widget's decision logic, kept free of iced types so it
//!   is testable. It keeps the anchor row at the same screen position when
//!   anything above it changes height (history prepended, a row deleted, text
//!   rewrapped by a resize, an attachment or image growing), applies the
//!   worker's scroll requests exactly once, keeps the newest row in view only
//!   while the viewport was already at the bottom, and reports the viewport and
//!   the heights that actually changed. It corrects the offset before the
//!   scrollable handles an event, so a change and its correction land in the
//!   same frame instead of one frame apart.
//!
//! Nothing here runs on a timer: reports are produced only while handling
//! events iced already delivers, and an idle window produces none.
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};

use fastcord_model::Snowflake;
use iced::widget::{Column, scrollable, space};
use iced::{Element, Length, Renderer, Theme};
use iced_renderer::core::widget::operation::scrollable::{AbsoluteOffset, scroll_to};
use iced_renderer::core::{self, layout, mouse, overlay, renderer, widget};

/// Most rows ever indexed: the per-channel message cache bound (SPEC §12).
pub const MAX_ITEMS: usize = 500;
/// Normal ceiling on rows built at once. It only trims overscan; every visible
/// row is always built.
pub const MAX_WINDOW_ROWS: usize = 128;

/// Fixed-point resolution of cached heights (1/64 px). Exact in `f32`, so
/// prefix sums never drift however often a height is corrected.
const UNITS: f32 = 64.0;
/// Stand-in for a row that was never laid out at any width.
const DEFAULT_ROW_HEIGHT: f32 = 56.0;
const MIN_ROW_HEIGHT: f32 = 1.0;
/// Cached heights saturate here; the layout itself is never clipped.
const MAX_ROW_HEIGHT: f32 = 32_768.0;
const MIN_VIEWPORT: f32 = 1.0;
const MAX_VIEWPORT: f32 = 16_384.0;
const DEFAULT_HEIGHT: f32 = 600.0;
const DEFAULT_WIDTH: f32 = 640.0;
/// Widths inside the same 32 px bucket share a cached height.
const BUCKET_WIDTH: f32 = 32.0;
/// Bound for any coordinate taken from outside, so fixed-point sums cannot overflow.
const MAX_COORD: f32 = 1.0e8;
/// Sub-pixel layout rounding is neither corrected nor reported.
const EPSILON: f32 = 0.75;
/// Scrolling by less than this inside one row is not worth reporting.
const REPORT_STEP: f32 = 4.0;
/// The viewport counts as being at the bottom within this distance.
const BOTTOM_SLACK: f32 = 2.0;

/// A message in display order (oldest first). `revision` changes whenever the
/// message body does, which is when its cached height stops being trustworthy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: Snowflake,
    pub revision: u64,
}

/// A scroll position that survives layout changes: the first row touching the
/// top of the viewport and how many pixels of it are scrolled past. Negative
/// after the row at the top of the viewport was deleted: the position then
/// sits above the row that took its place.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub id: Snowflake,
    pub in_row: f32,
}

/// What the widget observed after laying the built rows out. Absolute
/// `offset` is in the widget's own content coordinates; the model never
/// trusts it when `anchor` identifies a row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub offset: f32,
    pub height: f32,
    pub width: f32,
    /// The row at the top of the viewport, if that point lies inside a built row
    /// (it lies inside a spacer after a scrollbar drag far outside the window).
    pub anchor: Option<Anchor>,
    /// The viewport shows the bottom of the retained rows.
    pub at_bottom: bool,
    /// Serial of the newest [`ScrollRequest`] the widget has applied.
    pub applied: u64,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            offset: 0.0,
            height: DEFAULT_HEIGHT,
            width: DEFAULT_WIDTH,
            anchor: None,
            at_bottom: true,
            applied: 0,
        }
    }
}

/// The height one row actually laid out to. Keyed by `(id, width_bucket)` in the
/// model; ignored when `revision` is not the row's current one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measurement {
    pub id: Snowflake,
    pub revision: u64,
    pub width_bucket: u32,
    pub height: f32,
}

/// Where the worker wants the viewport put.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollTarget {
    /// This row at this in-row offset.
    Anchor(Anchor),
    /// The newest row, kept in view while the viewport stays at the bottom.
    Bottom,
}

/// A scroll the widget must perform once. It stays in every snapshot until a
/// [`Viewport`] with `applied >= serial` acknowledges it, so a snapshot dropped
/// by a latest-value channel cannot lose it. Serials are unique across lists.
///
/// The worker's idea of the reader's position is always a little behind the
/// widget's, so a request that only repairs it must not drag a reader who has
/// moved on: `only_from` names the row the worker believed the reader was on, and
/// the widget skips (but acknowledges) the request when the reader is elsewhere.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollRequest {
    pub serial: u64,
    pub target: ScrollTarget,
    pub only_from: Option<Snowflake>,
}

/// What the widget reports back to the worker.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Report {
    Viewport(Viewport),
    Measured(Measurement),
}

/// The rows to build for the current position, with the spacers around them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Window {
    /// Indices (into the retained IDs) of the rows to build.
    pub range: Range<usize>,
    /// Height of the spacer standing in for the rows before `range`.
    pub top: f32,
    /// Height of the spacer standing in for the rows after `range`.
    pub bottom: f32,
    /// Where the model puts the top of the viewport, in content coordinates.
    /// Use [`VariableList::index_at`] to find the first visible row.
    pub offset: f32,
    /// Viewport height the window was computed for.
    pub height: f32,
    /// Width bucket the cached heights were taken from.
    pub width_bucket: u32,
    /// The viewport shows the bottom of the retained rows.
    pub at_bottom: bool,
    /// New rows keep the viewport at the bottom: `at_bottom` and the newest
    /// retained row is the channel's newest.
    pub pinned: bool,
}

/// Quantizes a measured height. Only finite values are accepted by callers.
fn row_units(px: f32) -> u32 {
    (px.clamp(MIN_ROW_HEIGHT, MAX_ROW_HEIGHT) * UNITS).round() as u32
}

fn to_units(px: f32) -> u64 {
    (px.clamp(0.0, MAX_COORD) * UNITS).round() as u64
}

fn signed_units(px: f32) -> i64 {
    (px.clamp(-MAX_COORD, MAX_COORD) * UNITS).round() as i64
}

fn to_px(units: u64) -> f32 {
    units as f32 / UNITS
}

/// Cached heights are shared by widths inside one 32 px bucket.
pub fn width_bucket(width: f32) -> u32 {
    (width.clamp(0.0, MAX_COORD) / BUCKET_WIDTH) as u32
}

fn finite(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}

static SERIALS: AtomicU64 = AtomicU64::new(0);

/// Requests are ordered across lists: a widget that applied serial 7 for a
/// replaced list must not ignore the replacement's first request.
fn next_serial() -> u64 {
    SERIALS.fetch_add(1, Ordering::Relaxed) + 1
}

/// Binary indexed tree over fixed-point row heights: point update, prefix sum,
/// and "which row contains this offset" in O(log n).
#[derive(Debug, Default)]
struct Fenwick {
    /// `tree[0]` is unused; `tree[i]` covers `(i - lowbit(i), i]`.
    tree: Vec<u64>,
}

impl Fenwick {
    fn rebuild(&mut self, values: impl ExactSizeIterator<Item = u32>) {
        let n = values.len();
        self.tree.clear();
        self.tree.reserve(n + 1);
        self.tree.push(0);
        self.tree.extend(values.map(u64::from));
        for i in 1..=n {
            let parent = i + i.isolate_lowest_one();
            if parent <= n {
                let value = self.tree[i];
                self.tree[parent] += value;
            }
        }
    }

    fn len(&self) -> usize {
        self.tree.len().saturating_sub(1)
    }

    fn add(&mut self, index: usize, delta: i64) {
        let n = self.len();
        let mut i = index + 1;
        while i <= n {
            self.tree[i] = self.tree[i].wrapping_add_signed(delta);
            i += i.isolate_lowest_one();
        }
    }

    /// Sum of the first `count` heights.
    fn prefix(&self, count: usize) -> u64 {
        let mut i = count.min(self.len());
        let mut sum = 0;
        while i > 0 {
            sum += self.tree[i];
            i -= i.isolate_lowest_one();
        }
        sum
    }

    fn total(&self) -> u64 {
        self.prefix(self.len())
    }

    /// Index of the row containing `target`: the number of leading rows whose
    /// heights sum to at most `target`. `len()` when `target` is past the end.
    fn find(&self, target: u64) -> usize {
        let n = self.len();
        if n == 0 {
            return 0;
        }
        let mut position = 0;
        let mut remaining = target;
        let mut step = 1usize << (usize::BITS - 1 - n.leading_zeros());
        while step > 0 {
            let next = position + step;
            if next <= n && self.tree[next] <= remaining {
                position = next;
                remaining -= self.tree[next];
            }
            step >>= 1;
        }
        position
    }
}

#[derive(Clone, Copy, Debug)]
struct Cached {
    bucket: u32,
    units: u32,
}

#[derive(Clone, Debug)]
struct Slot {
    item: Item,
    /// Height used for the current bucket: measured, borrowed from the other
    /// bucket, or the default.
    height: u32,
    /// Most recent measurement, then the one before it from another bucket.
    recent: Option<Cached>,
    older: Option<Cached>,
}

impl Slot {
    fn new(item: Item) -> Self {
        Self {
            item,
            height: row_units(DEFAULT_ROW_HEIGHT),
            recent: None,
            older: None,
        }
    }

    /// Exact bucket first; otherwise the last known height at any width is the
    /// best estimate until the row is laid out again.
    fn height_for(&self, bucket: u32) -> u32 {
        [self.recent, self.older]
            .into_iter()
            .flatten()
            .find(|cached| cached.bucket == bucket)
            .or(self.recent)
            .map_or_else(|| row_units(DEFAULT_ROW_HEIGHT), |cached| cached.units)
    }

    fn record(&mut self, bucket: u32, units: u32) {
        let measured = Some(Cached { bucket, units });
        if self.recent.is_some_and(|cached| cached.bucket == bucket) {
            self.recent = measured;
        } else {
            self.older = self.recent;
            self.recent = measured;
        }
    }
}

/// Copyable summary of what a viewport report may change.
#[derive(Clone, Copy, PartialEq)]
struct Inputs {
    height: f32,
    width: f32,
    bucket: u32,
    anchor: Option<Anchor>,
    follow: bool,
    request: Option<ScrollRequest>,
}

/// Numeric model of the retained rows. See the module documentation.
#[derive(Debug)]
pub struct VariableList {
    slots: Vec<Slot>,
    index: HashMap<Snowflake, usize>,
    sums: Fenwick,
    bucket: u32,
    height: f32,
    width: f32,
    anchor: Option<Anchor>,
    /// The viewport last reported itself at the bottom of the retained rows.
    follow: bool,
    /// The newest retained row is the channel's newest row.
    live: bool,
    request: Option<ScrollRequest>,
}

impl Default for VariableList {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            index: HashMap::new(),
            sums: Fenwick::default(),
            bucket: width_bucket(DEFAULT_WIDTH),
            height: DEFAULT_HEIGHT,
            width: DEFAULT_WIDTH,
            anchor: None,
            follow: true,
            live: true,
            request: None,
        }
    }
}

impl VariableList {
    /// Replaces the retained rows, oldest first. At most [`MAX_ITEMS`] are
    /// kept (the newest, if more are given) and repeated IDs are ignored.
    /// Heights are kept for IDs that stay and dropped with the ones that go;
    /// a row whose revision changed keeps its old height as an estimate until
    /// it is measured again. The anchor is an ID, so rows added or removed above
    /// it do not move it; if its own row is gone the nearest surviving row
    /// takes over without moving what is on screen. Returns whether anything
    /// changed.
    pub fn set_items(&mut self, items: &[Item]) -> bool {
        let items = &items[items.len().saturating_sub(MAX_ITEMS)..];
        if self.slots.len() == items.len()
            && self
                .slots
                .iter()
                .zip(items)
                .all(|(slot, item)| slot.item == *item)
        {
            return false;
        }
        let mut index = HashMap::with_capacity(items.len());
        let mut kept = Vec::with_capacity(items.len());
        for item in items {
            if let Entry::Vacant(vacant) = index.entry(item.id) {
                vacant.insert(kept.len());
                kept.push(*item);
            }
        }
        // Rows appended below a reader who is not following the live edge (paging
        // forward through history) are not "following": the viewport stays where it is.
        let tail_moved =
            self.slots.last().map(|slot| slot.item.id) != kept.last().map(|item| item.id);
        if tail_moved && !self.pinned() {
            self.follow = false;
        }
        let replacement = self
            .anchor
            .filter(|anchor| !index.contains_key(&anchor.id))
            .map(|anchor| (anchor.id, self.nearest_survivor(anchor, &index)));
        let mut previous: HashMap<Snowflake, Slot> = std::mem::take(&mut self.slots)
            .into_iter()
            .map(|slot| (slot.item.id, slot))
            .collect();
        self.slots = kept
            .into_iter()
            .map(|item| match previous.remove(&item.id) {
                Some(mut slot) => {
                    slot.item = item;
                    slot
                }
                None => Slot::new(item),
            })
            .collect();
        self.index = index;
        self.rebuild();
        if let Some((gone, replacement)) = replacement {
            self.anchor = replacement;
            if let Some(anchor) = replacement
                && !self.pinned()
            {
                self.request = Some(ScrollRequest {
                    serial: next_serial(),
                    target: ScrollTarget::Anchor(anchor),
                    only_from: Some(gone),
                });
            }
        }
        true
    }

    /// Whether the newest retained row is the channel's newest message. While it
    /// is not, reaching the bottom of the retained rows does not pin the
    /// viewport there: rows appended by paging must not drag the reader along.
    /// Call it after [`VariableList::set_items`] with the new rows.
    pub fn set_live(&mut self, live: bool) {
        self.live = live;
    }

    /// Applies what the widget observed. Returns whether the window inputs
    /// (viewport size, position, follow state, pending request) changed.
    ///
    /// A report produced before the widget applied the pending request still
    /// describes the old position, so it updates sizes but not the position.
    pub fn viewport(&mut self, report: Viewport) -> bool {
        let before = self.inputs();
        if self
            .request
            .is_some_and(|request| report.applied >= request.serial)
        {
            self.request = None;
        }
        self.height = finite(report.height, MIN_VIEWPORT, MAX_VIEWPORT, self.height);
        self.width = finite(report.width, 0.0, MAX_COORD, self.width);
        let bucket = width_bucket(self.width);
        if bucket != self.bucket {
            self.set_bucket(bucket);
        }
        if self.request.is_none() {
            self.follow = report.at_bottom;
            self.anchor = report
                .anchor
                .filter(|anchor| self.index.contains_key(&anchor.id))
                .map(|anchor| Anchor {
                    id: anchor.id,
                    in_row: finite(anchor.in_row, -MAX_COORD, MAX_COORD, 0.0),
                })
                .or_else(|| self.anchor_at(finite(report.offset, 0.0, MAX_COORD, 0.0)));
        }
        self.inputs() != before
    }

    /// Records the height a row laid out to. Returns whether the heights of the
    /// current width bucket changed (so the window may have). Measurements for
    /// unknown IDs, stale revisions, or non-finite heights are ignored.
    pub fn measure(&mut self, measurement: Measurement) -> bool {
        let Some(&at) = self.index.get(&measurement.id) else {
            return false;
        };
        if !measurement.height.is_finite() {
            return false;
        }
        let slot = &mut self.slots[at];
        if slot.item.revision != measurement.revision {
            return false;
        }
        let units = row_units(measurement.height);
        slot.record(measurement.width_bucket, units);
        if measurement.width_bucket != self.bucket || slot.height == units {
            return false;
        }
        let delta = i64::from(units) - i64::from(slot.height);
        slot.height = units;
        self.sums.add(at, delta);
        true
    }

    /// Scrolls to the newest row and keeps it in view. Call it once the list
    /// holds the channel's newest rows; it marks the list live.
    pub fn jump_latest(&mut self) {
        self.live = true;
        self.follow = true;
        self.request = Some(ScrollRequest {
            serial: next_serial(),
            target: ScrollTarget::Bottom,
            only_from: None,
        });
    }

    /// The scroll the widget has not acknowledged yet. Put it in every snapshot.
    pub fn scroll_request(&self) -> Option<ScrollRequest> {
        self.request
    }

    /// The row at the top of the viewport and the offset inside it.
    pub fn anchor(&self) -> Option<Anchor> {
        self.anchor_at(to_px(self.visible().0))
    }

    /// Index of the row containing `offset` (content coordinates); the number
    /// of rows when `offset` is past the end. Use it with [`Window::offset`].
    pub fn index_at(&self, offset: f32) -> usize {
        self.sums.find(to_units(offset))
    }

    /// The rows to build for the current position and the spacers around them.
    pub fn window(&self) -> Window {
        self.window_for_range(self.overscan())
    }

    /// Spacers for a subset or superset of the rows, for example after the
    /// worker trims the rows it can afford to keep decoded. The range is
    /// clamped to the retained rows; the spacers always add up to the whole list.
    pub fn window_for_range(&self, range: Range<usize>) -> Window {
        let n = self.slots.len();
        let start = range.start.min(n);
        let end = range.end.clamp(start, n);
        Window {
            range: start..end,
            top: to_px(self.sums.prefix(start)),
            bottom: to_px(self.sums.total() - self.sums.prefix(end)),
            offset: to_px(self.visible().0),
            height: self.height,
            width_bucket: self.bucket,
            at_bottom: self.follow,
            pinned: self.pinned(),
        }
    }

    fn pinned(&self) -> bool {
        self.follow && self.live
    }

    fn inputs(&self) -> Inputs {
        Inputs {
            height: self.height,
            width: self.width,
            bucket: self.bucket,
            anchor: self.anchor,
            follow: self.follow,
            request: self.request,
        }
    }

    fn rebuild(&mut self) {
        self.sums.rebuild(self.slots.iter().map(|slot| slot.height));
    }

    fn set_bucket(&mut self, bucket: u32) {
        self.bucket = bucket;
        for slot in &mut self.slots {
            slot.height = slot.height_for(bucket);
        }
        self.rebuild();
    }

    /// The model's scroll offset in fixed point.
    fn offset(&self) -> u64 {
        let max = self.sums.total().saturating_sub(to_units(self.height));
        if self.pinned() {
            return max;
        }
        let Some(anchor) = self.anchor else {
            return max;
        };
        let Some(&at) = self.index.get(&anchor.id) else {
            return max;
        };
        let top = self.sums.prefix(at) as i64;
        (top + signed_units(anchor.in_row)).clamp(0, max as i64) as u64
    }

    /// Model offset and the rows intersecting the viewport there.
    fn visible(&self) -> (u64, Range<usize>) {
        let n = self.slots.len();
        if n == 0 {
            return (0, 0..0);
        }
        let viewport = to_units(self.height).max(1);
        let offset = self.offset();
        let first = self.sums.find(offset).min(n - 1);
        let end = (self.sums.find(offset + viewport - 1) + 1).min(n);
        (offset, first..end.max(first + 1))
    }

    /// Visible rows plus one viewport of overscan on each side, trimmed from
    /// whichever side has more overscan when it exceeds [`MAX_WINDOW_ROWS`].
    fn overscan(&self) -> Range<usize> {
        let n = self.slots.len();
        let (offset, visible) = self.visible();
        if n == 0 {
            return 0..0;
        }
        let viewport = to_units(self.height).max(1);
        let mut start = self
            .sums
            .find(offset.saturating_sub(viewport))
            .min(visible.start);
        let mut end = (self.sums.find(offset + 2 * viewport - 1) + 1)
            .min(n)
            .max(visible.end);
        let cap = MAX_WINDOW_ROWS.max(visible.len());
        while end - start > cap {
            if visible.start - start >= end - visible.end {
                start += 1;
            } else {
                end -= 1;
            }
        }
        start..end
    }

    fn anchor_at(&self, offset: f32) -> Option<Anchor> {
        let last = self.slots.len().checked_sub(1)?;
        let at = self.index_at(offset).min(last);
        let inside = to_units(offset).saturating_sub(self.sums.prefix(at));
        Some(Anchor {
            id: self.slots[at].item.id,
            in_row: to_px(inside.min(u64::from(self.slots[at].height))),
        })
    }

    /// The anchor that keeps what is on screen where it is after the anchor's
    /// row disappeared: the next surviving row (else the previous one), at the
    /// offset that leaves the viewport exactly where it was.
    fn nearest_survivor(
        &self,
        anchor: Anchor,
        survivors: &HashMap<Snowflake, usize>,
    ) -> Option<Anchor> {
        let at = *self.index.get(&anchor.id)?;
        let offset = self.sums.prefix(at) as i64 + signed_units(anchor.in_row);
        let alive = |i: &usize| survivors.contains_key(&self.slots[*i].item.id);
        let pick = (at + 1..self.slots.len())
            .find(alive)
            .or_else(|| (0..at).rev().find(alive))?;
        Some(Anchor {
            id: self.slots[pick].item.id,
            in_row: (offset - self.sums.prefix(pick) as i64) as f32 / UNITS,
        })
    }

    /// Number of cached heights: bounded by two per retained row.
    #[cfg(test)]
    pub fn cache_len(&self) -> usize {
        self.slots
            .iter()
            .map(|slot| usize::from(slot.recent.is_some()) + usize::from(slot.older.is_some()))
            .sum()
    }

    /// Heap held by the index, in bytes (capacities, not lengths).
    #[cfg(test)]
    pub fn retained_bytes(&self) -> usize {
        self.slots.capacity() * std::mem::size_of::<Slot>()
            + self.sums.tree.capacity() * std::mem::size_of::<u64>()
            + self.index.capacity() * (std::mem::size_of::<(Snowflake, usize)>() + 1)
    }
}

/// One built row as laid out. `top` is relative to the content origin, so it
/// includes the top spacer.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RowBox {
    id: Snowflake,
    revision: u64,
    top: f32,
    height: f32,
}

/// The real layout of the built rows inside the scrollable.
struct Geometry<'a> {
    rows: &'a [RowBox],
    /// Height of the whole content: spacers plus rows.
    content: f32,
    viewport: f32,
    /// Width available to the rows.
    width: f32,
}

impl Geometry<'_> {
    fn max_offset(&self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }

    fn at_bottom(&self, offset: f32) -> bool {
        offset >= self.max_offset() - BOTTOM_SLACK
    }

    /// The built row containing `offset`, if the point is inside one.
    fn anchor_at(&self, offset: f32) -> Option<Anchor> {
        let at = self
            .rows
            .partition_point(|row| row.top + row.height <= offset);
        let row = self.rows.get(at)?;
        (row.top <= offset).then_some(Anchor {
            id: row.id,
            in_row: offset - row.top,
        })
    }
}

/// Where the reader is, as far as the layout is concerned.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Position {
    Bottom,
    Row(Anchor),
}

impl Position {
    /// The offset that puts this position where it was, in the current layout.
    fn offset(self, geometry: &Geometry<'_>) -> Option<f32> {
        let max = geometry.max_offset();
        match self {
            Position::Bottom => Some(max),
            Position::Row(anchor) => geometry
                .rows
                .iter()
                .find(|row| row.id == anchor.id)
                .map(|row| (row.top + anchor.in_row).clamp(0.0, max)),
        }
    }

    fn at(geometry: &Geometry<'_>, offset: f32, pinned: bool) -> Option<Position> {
        if pinned && geometry.at_bottom(offset) {
            Some(Position::Bottom)
        } else {
            geometry.anchor_at(offset).map(Position::Row)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Sent {
    id: Snowflake,
    revision: u64,
    bucket: u32,
    units: u32,
}

/// What the worker currently believes about the viewport, from its window.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ModelView {
    height: f32,
    bucket: u32,
    at_bottom: bool,
}

/// Keeps the reader's position through layout changes and reports upward.
/// See the module documentation; the widget calls [`Tracker::correct`] before
/// the scrollable handles an event and [`Tracker::settle`] after it.
#[derive(Debug)]
struct Tracker {
    position: Option<Position>,
    /// The offset the tracker last placed or observed.
    last: f32,
    /// Highest request serial applied (never reset: serials are global).
    applied: u64,
    pinned: bool,
    reported: Option<Viewport>,
    /// The worker belief a report already corrected, so a worker that has not
    /// caught up yet is not told the same thing on every redraw.
    corrected: Option<ModelView>,
    sent: Vec<Sent>,
    scratch: Vec<Sent>,
}

impl Default for Tracker {
    fn default() -> Self {
        Self {
            position: Some(Position::Bottom),
            last: 0.0,
            applied: 0,
            pinned: true,
            reported: None,
            corrected: None,
            sent: Vec::new(),
            scratch: Vec::new(),
        }
    }
}

impl Tracker {
    /// Starts over for a different list (another channel): at the bottom, with
    /// nothing reported yet.
    fn reset(&mut self) {
        *self = Self {
            applied: self.applied,
            ..Self::default()
        };
    }

    /// Runs before the scrollable handles an event. Returns the offset the
    /// scrollable must be set to first, if any: the worker's pending request,
    /// or whatever restores the reader's position after the layout changed
    /// while the offset did not.
    fn correct(
        &mut self,
        geometry: &Geometry<'_>,
        offset: f32,
        request: Option<ScrollRequest>,
        pinned: bool,
    ) -> Option<f32> {
        self.pinned = pinned;
        if let Some(request) = request.filter(|request| request.serial > self.applied) {
            self.applied = request.serial;
            // A request means the worker's state moved on (a new list, a repaired
            // anchor): tell it everything again.
            self.reported = None;
            self.corrected = None;
            self.sent.clear();
            // A repair only applies while the reader is still on the row it is about.
            let relevant = request.only_from.is_none_or(
                |gone| matches!(self.position, Some(Position::Row(anchor)) if anchor.id == gone),
            );
            let position = match request.target {
                ScrollTarget::Bottom => Position::Bottom,
                ScrollTarget::Anchor(anchor) => Position::Row(anchor),
            };
            if relevant && let Some(target) = position.offset(geometry) {
                self.position = Some(position);
                return self.place(target, offset);
            }
            // Not relevant any more, or the row is not among the built ones:
            // acknowledged and dropped, never retried against a later layout.
        }
        if !pinned && self.position == Some(Position::Bottom) {
            self.position = Position::at(geometry, offset, false);
        }
        if (offset - self.last).abs() <= EPSILON {
            let target = self.position?.offset(geometry)?;
            return self.place(target, offset);
        }
        None
    }

    fn place(&mut self, target: f32, offset: f32) -> Option<f32> {
        self.last = target;
        ((target - offset).abs() > EPSILON).then_some(target)
    }

    /// Runs after the scrollable handled the event, with the offset it ended at.
    fn settle(&mut self, geometry: &Geometry<'_>, offset: f32) {
        if (offset - self.last).abs() > EPSILON {
            // The scrollable moved by itself: the reader scrolled, or the content
            // shrank under the offset.
            self.position = Position::at(geometry, offset, self.pinned);
            self.last = offset;
        } else if self.pinned && geometry.at_bottom(offset) {
            self.position = Some(Position::Bottom);
        } else if self.position.is_none() {
            // The viewport was over a spacer (a scrollbar drag far outside the
            // built rows); now that rows are built there, hold on to one.
            self.position = Position::at(geometry, offset, self.pinned);
        }
    }

    /// The viewport to report, when it differs from the last one reported or
    /// from what the worker believes (`model`): a replaced list starts out wrong
    /// about the height, the width bucket, and whether the reader is at the bottom.
    fn report(
        &mut self,
        geometry: &Geometry<'_>,
        offset: f32,
        model: ModelView,
    ) -> Option<Viewport> {
        let height = geometry.viewport.clamp(MIN_VIEWPORT, MAX_VIEWPORT);
        let report = Viewport {
            offset,
            height,
            width: geometry.width,
            anchor: geometry.anchor_at(offset),
            at_bottom: geometry.at_bottom(offset),
            applied: self.applied,
        };
        let disagrees = (model.height - height).abs() > EPSILON
            || model.bucket != width_bucket(report.width)
            || model.at_bottom != report.at_bottom;
        if !disagrees {
            self.corrected = None;
        }
        let changed = (disagrees && self.corrected != Some(model))
            || self.reported.is_none_or(|old| {
                old.height != report.height
                    || old.width != report.width
                    || old.at_bottom != report.at_bottom
                    || old.applied != report.applied
                    || match (old.anchor, report.anchor) {
                        (Some(a), Some(b)) => {
                            a.id != b.id || (a.in_row - b.in_row).abs() >= REPORT_STEP
                        }
                        (None, None) => (old.offset - report.offset).abs() >= REPORT_STEP,
                        _ => true,
                    }
            });
        if changed {
            self.reported = Some(report);
            self.corrected = disagrees.then_some(model);
            Some(report)
        } else {
            None
        }
    }

    /// Publishes the heights that are new since the last call: rows never
    /// reported, rows whose revision changed, rows laid out at a new width
    /// bucket, and rows whose height changed (an image arrived, text rewrapped).
    fn measurements(&mut self, geometry: &Geometry<'_>, mut publish: impl FnMut(Measurement)) {
        let bucket = width_bucket(geometry.width);
        self.scratch.clear();
        for row in geometry.rows {
            let sent = Sent {
                id: row.id,
                revision: row.revision,
                bucket,
                units: row_units(row.height),
            };
            if !self.sent.contains(&sent) {
                publish(Measurement {
                    id: row.id,
                    revision: row.revision,
                    width_bucket: bucket,
                    height: to_px(u64::from(sent.units)),
                });
            }
            self.scratch.push(sent);
        }
        std::mem::swap(&mut self.sent, &mut self.scratch);
    }
}

/// Layout facts read from the scrollable for one event.
#[derive(Clone, Copy)]
struct Frame {
    content: f32,
    viewport: f32,
    width: f32,
}

/// Widget state kept in the iced tree: it survives view rebuilds.
#[derive(Default)]
struct State {
    key: u64,
    tracker: Tracker,
    rows: Vec<RowBox>,
}

impl State {
    /// Reads the built rows' real positions: the scrollable's content is
    /// `[top spacer, rows..., bottom spacer]`. `None` when it is not (nothing is
    /// then corrected or reported until it is).
    fn read(&mut self, items: &[Item], scroll: core::Layout<'_>) -> Option<Frame> {
        let content = scroll.children().next()?;
        let mut children = content.children();
        if children.len() != items.len() + 2 {
            return None;
        }
        let origin = content.bounds();
        children.next();
        self.rows.clear();
        self.rows
            .extend(items.iter().zip(children).map(|(item, row)| {
                let bounds = row.bounds();
                RowBox {
                    id: item.id,
                    revision: item.revision,
                    top: bounds.y - origin.y,
                    height: bounds.height,
                }
            }));
        Some(Frame {
            content: origin.height,
            viewport: scroll.bounds().height,
            width: origin.width,
        })
    }
}

fn scroll_id() -> widget::Id {
    widget::Id::new("fastcord-variable-list")
}

/// Reads the scrollable's current vertical offset (its rounded translation).
struct Probe {
    target: widget::Id,
    offset: Option<f32>,
}

impl widget::Operation for Probe {
    // Never descend into the rows: only the scrollable itself is asked.
    fn traverse(&mut self, _operate: &mut dyn FnMut(&mut dyn widget::Operation)) {}

    fn scrollable(
        &mut self,
        id: Option<&widget::Id>,
        _bounds: core::Rectangle,
        _content_bounds: core::Rectangle,
        translation: core::Vector,
        _state: &mut dyn widget::operation::Scrollable,
    ) {
        if id == Some(&self.target) {
            self.offset = Some(translation.y);
        }
    }
}

fn probe<Message>(
    scroll: &mut Element<'_, Message>,
    tree: &mut widget::Tree,
    layout: core::Layout<'_>,
    renderer: &Renderer,
) -> Option<f32> {
    let mut probe = Probe {
        target: scroll_id(),
        offset: None,
    };
    scroll
        .as_widget_mut()
        .operate(tree, layout, renderer, &mut probe);
    probe.offset
}

fn scroll_content<Message>(
    scroll: &mut Element<'_, Message>,
    tree: &mut widget::Tree,
    layout: core::Layout<'_>,
    renderer: &Renderer,
    offset: f32,
) {
    let mut operation = scroll_to::<()>(
        scroll_id(),
        AbsoluteOffset {
            x: None,
            y: Some(offset),
        },
    );
    scroll
        .as_widget_mut()
        .operate(tree, layout, renderer, &mut operation);
}

/// Wraps the scrollable of built rows: forwards everything to it, and around
/// each event corrects the offset and reports what the layout looks like.
struct MeasuredScroll<'a, Message> {
    scroll: Element<'a, Message>,
    items: Vec<Item>,
    key: u64,
    pinned: bool,
    request: Option<ScrollRequest>,
    model: ModelView,
    on_report: Box<dyn Fn(Report) -> Message + 'a>,
}

impl<'a, Message: 'a> core::Widget<Message, Theme, Renderer> for MeasuredScroll<'a, Message> {
    fn size(&self) -> core::Size<core::Length> {
        self.scroll.as_widget().size()
    }

    fn size_hint(&self) -> core::Size<core::Length> {
        self.scroll.as_widget().size_hint()
    }

    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<State>()
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(State::default())
    }

    fn children(&self) -> Vec<widget::Tree> {
        vec![widget::Tree::new(&self.scroll)]
    }

    fn diff(&self, tree: &mut widget::Tree) {
        tree.diff_children(std::slice::from_ref(&self.scroll));
    }

    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.scroll
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: core::Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.scroll
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &core::Event,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn core::Clipboard,
        shell: &mut core::Shell<'_, Message>,
        viewport: &core::Rectangle,
    ) {
        let redraw = matches!(
            event,
            core::Event::Window(core::window::Event::RedrawRequested(_))
        );
        let state = tree.state.downcast_mut::<State>();
        let scroll_tree = &mut tree.children[0];
        if state.key != self.key {
            state.key = self.key;
            state.tracker.reset();
        }
        let Some(frame) = state.read(&self.items, layout) else {
            self.scroll.as_widget_mut().update(
                scroll_tree,
                event,
                layout,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
            return;
        };
        let geometry = Geometry {
            rows: &state.rows,
            content: frame.content,
            viewport: frame.viewport,
            width: frame.width,
        };
        if let Some(offset) = probe(&mut self.scroll, scroll_tree, layout, renderer)
            && let Some(target) =
                state
                    .tracker
                    .correct(&geometry, offset, self.request, self.pinned)
        {
            scroll_content(&mut self.scroll, scroll_tree, layout, renderer, target);
            if !redraw {
                shell.request_redraw();
            }
        }
        self.scroll.as_widget_mut().update(
            scroll_tree,
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if let Some(offset) = probe(&mut self.scroll, scroll_tree, layout, renderer) {
            state.tracker.settle(&geometry, offset);
            if redraw {
                if let Some(report) = state.tracker.report(&geometry, offset, self.model) {
                    shell.publish((self.on_report)(Report::Viewport(report)));
                }
                state.tracker.measurements(&geometry, |measurement| {
                    shell.publish((self.on_report)(Report::Measured(measurement)));
                });
            }
        }
    }

    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &core::Rectangle,
    ) {
        self.scroll.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &core::Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.scroll.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut widget::Tree,
        layout: core::Layout<'b>,
        renderer: &Renderer,
        viewport: &core::Rectangle,
        translation: core::Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.scroll.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

/// Builds the list: `rows` (already limited to `window.range`, in order) between
/// the window's spacers, inside a vertically scrolling area that fills its
/// parent. `key` identifies the list's content (the channel): when it changes
/// the widget forgets the reader's position and starts at the bottom. `scroll`
/// is the model's unacknowledged request; `on_report` turns what the widget
/// observes into the caller's message.
pub fn view<'a, Message: 'a>(
    key: u64,
    window: &Window,
    scroll: Option<ScrollRequest>,
    rows: impl IntoIterator<Item = (Item, Element<'a, Message>)>,
    on_report: impl Fn(Report) -> Message + 'a,
) -> Element<'a, Message> {
    let rows = rows.into_iter();
    let capacity = rows.size_hint().0;
    let mut items = Vec::with_capacity(capacity);
    let mut children: Vec<Element<'a, Message>> = Vec::with_capacity(capacity + 2);
    children.push(space().height(window.top).into());
    for (item, element) in rows {
        items.push(item);
        children.push(element);
    }
    children.push(space().height(window.bottom).into());
    let content = Column::from_vec(children).width(Length::Fill);
    let scrolling: Element<'a, Message> = scrollable(content)
        .id(scroll_id())
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
    Element::new(MeasuredScroll {
        scroll: scrolling,
        items,
        key,
        pinned: window.pinned,
        request: scroll,
        model: ModelView {
            height: window.height,
            bucket: window.width_bucket,
            at_bottom: window.at_bottom,
        },
        on_report: Box::new(on_report),
    })
}

/// Test harness: the real widget tree, laid out and fed events by the same calls
/// iced makes, on the CPU renderer iced falls back to (no adapter or window
/// needed), so geometry and text layout are the real thing.
#[cfg(test)]
pub(crate) mod harness {
    use super::*;

    /// Polls a future that needs no executor.
    fn block_on<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("future needs an executor"),
        }
    }

    fn software_renderer() -> Renderer {
        use renderer::Headless;
        block_on(<Renderer as Headless>::new(
            core::Font::DEFAULT,
            core::Pixels(16.0),
            Some("tiny-skia"),
        ))
        .expect("the software renderer needs no adapter")
    }

    pub struct Ui<'a, Message> {
        renderer: Renderer,
        element: Element<'a, Message>,
        tree: widget::Tree,
        node: layout::Node,
        size: core::Size,
    }

    impl<'a, Message: 'a> Ui<'a, Message> {
        pub fn new(width: f32, height: f32, element: Element<'a, Message>) -> Self {
            let tree = widget::Tree::new(&element);
            let mut ui = Self {
                renderer: software_renderer(),
                element,
                tree,
                node: layout::Node::new(core::Size::ZERO),
                size: core::Size::new(width, height),
            };
            ui.relayout();
            ui
        }

        /// Rebuilds the view like iced does after an update: the widget tree
        /// (and so the tracker) survives, the layout is recomputed.
        pub fn show(&mut self, element: Element<'a, Message>) {
            self.element = element;
            self.tree.diff(&self.element);
            self.relayout();
        }

        fn relayout(&mut self) {
            let limits = layout::Limits::new(core::Size::ZERO, self.size);
            self.node =
                self.element
                    .as_widget_mut()
                    .layout(&mut self.tree, &self.renderer, &limits);
        }

        /// One `RedrawRequested`, returning what the widget published.
        pub fn redraw(&mut self) -> Vec<Message> {
            let mut messages = Vec::new();
            {
                let mut shell = core::Shell::new(&mut messages);
                self.element.as_widget_mut().update(
                    &mut self.tree,
                    &core::Event::Window(core::window::Event::RedrawRequested(
                        core::time::Instant::now(),
                    )),
                    core::Layout::new(&self.node),
                    mouse::Cursor::Unavailable,
                    &self.renderer,
                    &mut core::clipboard::Null,
                    &mut shell,
                    &core::Rectangle::with_size(self.size),
                );
            }
            messages
        }

        /// Draws the current layout, as iced does after `redraw`.
        pub fn draw(&mut self) {
            self.element.as_widget().draw(
                &self.tree,
                &mut self.renderer,
                &Theme::Dark,
                &renderer::Style::default(),
                core::Layout::new(&self.node),
                mouse::Cursor::Unavailable,
                &core::Rectangle::with_size(self.size),
            );
        }

        /// The scrollable's real offset.
        pub fn offset(&mut self) -> f32 {
            probe(
                &mut self.element,
                &mut self.tree,
                core::Layout::new(&self.node),
                &self.renderer,
            )
            .expect("the scrollable answers")
        }

        /// The reader scrolls (the scrollable moves without the widget's help).
        pub fn scroll_to(&mut self, offset: f32) {
            scroll_content(
                &mut self.element,
                &mut self.tree,
                core::Layout::new(&self.node),
                &self.renderer,
                offset,
            );
        }

        /// The pointer moves to `(x, y)` and clicks there, as a user does;
        /// returns whatever the widgets published on the way.
        pub fn click(&mut self, x: f32, y: f32) -> Vec<Message> {
            let position = core::Point::new(x, y);
            let cursor = mouse::Cursor::Available(position);
            let mut messages = Vec::new();
            for event in [
                mouse::Event::CursorMoved { position },
                mouse::Event::ButtonPressed(mouse::Button::Left),
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ] {
                let mut shell = core::Shell::new(&mut messages);
                self.element.as_widget_mut().update(
                    &mut self.tree,
                    &core::Event::Mouse(event),
                    core::Layout::new(&self.node),
                    cursor,
                    &self.renderer,
                    &mut core::clipboard::Null,
                    &mut shell,
                    &core::Rectangle::with_size(self.size),
                );
            }
            messages
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> Snowflake {
        Snowflake(n)
    }

    fn items(ids: std::ops::RangeInclusive<u64>) -> Vec<Item> {
        ids.map(|n| Item {
            id: id(n),
            revision: 1,
        })
        .collect()
    }

    /// Deterministic base height of a message: 28..=177 px.
    fn base(n: u64) -> f32 {
        28.0 + ((n.wrapping_mul(2_654_435_761) >> 7) % 150) as f32
    }

    /// Stands in for iced: lays out the rows the model asks for with their
    /// real heights (a function of the message and the width, like wrapped
    /// text), holds the scrollable's offset, and runs the real [`Tracker`].
    struct Sim {
        list: VariableList,
        tracker: Tracker,
        heights: HashMap<u64, f32>,
        offset: f32,
        viewport: f32,
        width: f32,
        /// The row under the top edge of the viewport after the last event, in
        /// the layout that was on screen when the event was handled.
        top: Option<Anchor>,
    }

    impl Sim {
        fn new(viewport: f32, width: f32) -> Self {
            Self {
                list: VariableList::default(),
                tracker: Tracker::default(),
                heights: HashMap::new(),
                offset: 0.0,
                viewport,
                width,
                top: None,
            }
        }

        fn real(&self, id: Snowflake) -> f32 {
            self.heights
                .get(&id.0)
                .copied()
                .unwrap_or_else(|| (base(id.0) * 640.0 / self.width * 2.0).round() / 2.0)
        }

        /// The built rows' real boxes and the content height.
        fn layout(&self) -> (Vec<RowBox>, f32) {
            let window = self.list.window();
            let mut top = window.top;
            let mut rows = Vec::new();
            for slot in &self.list.slots[window.range.clone()] {
                let height = self.real(slot.item.id);
                rows.push(RowBox {
                    id: slot.item.id,
                    revision: slot.item.revision,
                    top,
                    height,
                });
                top += height;
            }
            (rows, top + window.bottom)
        }

        /// What the scrollable does with an absolute target: clamp and round.
        fn scrolled(&self, target: f32, content: f32) -> f32 {
            target
                .clamp(0.0, (content - self.viewport).max(0.0))
                .round()
        }

        /// One widget `update`: correct, the reader acts (`wheel`), settle,
        /// report. Returns whether anything moved or was reported.
        fn update(&mut self, wheel: f32) -> bool {
            let window = self.list.window();
            let (rows, content) = self.layout();
            let geometry = Geometry {
                rows: &rows,
                content,
                viewport: self.viewport,
                width: self.width,
            };
            let mut busy = false;
            if let Some(target) = self.tracker.correct(
                &geometry,
                self.offset,
                self.list.scroll_request(),
                window.pinned,
            ) {
                self.offset = self.scrolled(target, content);
                busy = true;
            }
            if wheel != 0.0 {
                self.offset = self.scrolled(self.offset + wheel, content);
                busy = true;
            }
            self.tracker.settle(&geometry, self.offset);
            self.top = geometry.anchor_at(self.offset);
            let model = ModelView {
                height: window.height,
                bucket: window.width_bucket,
                at_bottom: window.at_bottom,
            };
            if let Some(report) = self.tracker.report(&geometry, self.offset, model) {
                self.list.viewport(report);
                busy = true;
            }
            let list = &mut self.list;
            self.tracker.measurements(&geometry, |measurement| {
                list.measure(measurement);
                busy = true;
            });
            busy
        }

        /// Runs updates until the widget and the model agree.
        fn settle(&mut self) {
            for _ in 0..32 {
                if !self.update(0.0) {
                    return;
                }
            }
            panic!("widget and model did not converge");
        }

        fn wheel(&mut self, delta: f32) {
            self.update(delta);
            self.settle();
        }

        fn screen_y(&self, id: Snowflake) -> f32 {
            let (rows, _) = self.layout();
            rows.iter()
                .find(|row| row.id == id)
                .unwrap_or_else(|| panic!("row {id} is not built"))
                .top
                - self.offset
        }

        fn at_bottom(&self) -> bool {
            let (_, content) = self.layout();
            self.offset >= (content - self.viewport).max(0.0) - BOTTOM_SLACK
        }

        /// A conversation of `ids` open at the newest message.
        fn opened(ids: std::ops::RangeInclusive<u64>) -> Self {
            let mut sim = Self::new(600.0, 640.0);
            sim.list.set_items(&items(ids));
            sim.list.jump_latest();
            sim.settle();
            sim
        }
    }

    fn assert_near(left: f32, right: f32) {
        assert!((left - right).abs() <= 1.0, "{left} is not near {right}");
    }

    #[test]
    fn fenwick_agrees_with_a_linear_scan() {
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        for len in [0usize, 1, 2, 3, 7, 64, 500] {
            let mut heights: Vec<u32> = (0..len).map(|_| 64 + next(20_000) as u32).collect();
            let mut tree = Fenwick::default();
            tree.rebuild(heights.iter().copied());
            for _ in 0..200 {
                if len > 0 && next(2) == 0 {
                    let at = next(len as u64) as usize;
                    let new = 64 + next(20_000) as u32;
                    tree.add(at, i64::from(new) - i64::from(heights[at]));
                    heights[at] = new;
                }
                let total: u64 = heights.iter().map(|&h| u64::from(h)).sum();
                assert_eq!(tree.total(), total);
                let count = next(len as u64 + 1) as usize;
                assert_eq!(
                    tree.prefix(count),
                    heights[..count].iter().map(|&h| u64::from(h)).sum::<u64>()
                );
                let target = next(total + 100);
                let mut expected = 0;
                let mut sum = 0;
                for &h in &heights {
                    if sum + u64::from(h) <= target {
                        sum += u64::from(h);
                        expected += 1;
                    } else {
                        break;
                    }
                }
                assert_eq!(tree.find(target), expected, "len {len} target {target}");
            }
        }
    }

    #[test]
    fn window_builds_visible_rows_plus_one_viewport_of_overscan() {
        let mut list = VariableList::default();
        list.set_items(&items(1..=200));
        list.jump_latest();
        // Default heights are 56 px and the default viewport is 600 px.
        list.viewport(Viewport {
            offset: 56.0 * 100.0,
            at_bottom: false,
            anchor: Some(Anchor {
                id: id(101),
                in_row: 0.0,
            }),
            applied: u64::MAX,
            ..Viewport::default()
        });
        let window = list.window();
        assert!(!window.at_bottom && !window.pinned);
        assert_eq!(window.offset, 5600.0);
        let first_visible = list.index_at(window.offset);
        assert_eq!(first_visible, 100);
        // 600 px = 10.7 rows: rows 100..=110 are visible; one viewport above and
        // below adds rows 89..=99 and 111..=121.
        assert_eq!(window.range, 89..122);
        assert_eq!(window.top, 89.0 * 56.0);
        assert_eq!(window.bottom, (200.0 - 122.0) * 56.0);
        // The spacers always add up to the whole list.
        let built = (window.range.end - window.range.start) as f32 * 56.0;
        assert_eq!(window.top + built + window.bottom, 200.0 * 56.0);
    }

    #[test]
    fn widget_count_follows_the_viewport_not_the_history() {
        let built = |rows: u64, viewport: f32| {
            let mut list = VariableList::default();
            list.set_items(&items(1..=rows));
            list.jump_latest();
            list.viewport(Viewport {
                height: viewport,
                applied: u64::MAX,
                ..Viewport::default()
            });
            list.window().range.len()
        };
        // The same viewport builds the same rows whether 200 or 500 are retained.
        assert_eq!(built(200, 600.0), built(500, 600.0));
        // A taller viewport builds proportionally more, and always fewer than
        // the retained rows.
        let small = built(500, 600.0);
        let large = built(500, 1200.0);
        assert!(small < 40, "{small}");
        assert!(
            large >= small * 3 / 2 && large <= MAX_WINDOW_ROWS,
            "{large}"
        );
        // Tiny rows never exceed the cap beyond the visible rows themselves.
        let mut list = VariableList::default();
        list.set_items(&items(1..=500));
        for n in 1..=500 {
            list.measure(Measurement {
                id: id(n),
                revision: 1,
                width_bucket: list.bucket,
                height: 10.0,
            });
        }
        list.viewport(Viewport {
            height: 3000.0,
            applied: u64::MAX,
            ..Viewport::default()
        });
        let window = list.window();
        assert_eq!(window.range.len(), 300, "all 300 visible rows are built");
    }

    #[test]
    fn pinned_window_ends_at_the_newest_row() {
        let mut list = VariableList::default();
        list.set_items(&items(1..=300));
        let window = list.window();
        assert!(window.at_bottom && window.pinned);
        assert_eq!(window.range.end, 300);
        assert_eq!(window.bottom, 0.0);
        list.set_live(false);
        let window = list.window();
        assert!(window.at_bottom && !window.pinned);
    }

    #[test]
    fn prepending_history_keeps_the_anchor_on_screen() {
        let mut sim = Sim::opened(101..=160);
        sim.wheel(-1500.0);
        let anchor = sim.list.anchor.expect("reader has an anchor");
        let before = sim.screen_y(anchor.id);
        // Fifty older messages arrive, none of them measured yet.
        sim.list.set_items(&items(51..=160));
        sim.settle();
        assert_eq!(sim.list.anchor.expect("anchor kept").id, anchor.id);
        assert_near(sim.screen_y(anchor.id), before);
        // Keep prepending while the reader keeps scrolling up: no jump at any step.
        for start in (1..=50).rev().step_by(10) {
            sim.wheel(-700.0);
            let anchor = sim.list.anchor.expect("anchor");
            let before = sim.screen_y(anchor.id);
            sim.list.set_items(&items(start..=160));
            sim.settle();
            assert_near(sim.screen_y(anchor.id), before);
        }
    }

    #[test]
    fn rows_measured_above_the_viewport_do_not_move_it() {
        // Scrolling up through never-measured rows: each row that enters the
        // overscan replaces an estimate with its real height. After every wheel
        // step the widget has already reported; the worker has not yet rebuilt the
        // window. The row under the top edge must not move while it does.
        let mut sim = Sim::opened(1..=300);
        for _ in 0..40 {
            sim.update(-450.0);
            let top = sim.top.expect("a built row is under the top edge");
            sim.settle();
            assert_near(sim.screen_y(top.id), -top.in_row);
        }
        assert!(sim.list.cache_len() > 0);
        assert!(
            sim.offset > 0.0,
            "the reader never reached the first message"
        );
    }

    #[test]
    fn a_row_above_the_anchor_growing_keeps_the_anchor_on_screen() {
        let mut sim = Sim::opened(1..=120);
        sim.wheel(-1200.0);
        let anchor = sim.list.anchor.expect("anchor");
        let before = sim.screen_y(anchor.id);
        let above = id(anchor.id.0 - 1);
        // An attachment image finishes loading: the row is now 520 px tall.
        sim.heights.insert(above.0, 520.0);
        let mut changed = items(1..=120);
        changed[(above.0 - 1) as usize].revision = 2;
        sim.list.set_items(&changed);
        sim.settle();
        assert_near(sim.screen_y(anchor.id), before);
        // The model learned the measured height by a height measurement event.
        let at = sim.list.index[&above];
        assert_eq!(sim.list.slots[at].height, row_units(520.0));
    }

    #[test]
    fn the_anchor_row_growing_keeps_the_in_row_offset() {
        let mut sim = Sim::opened(1..=120);
        sim.wheel(-1000.0);
        let anchor = sim.list.anchor.expect("anchor");
        let before = sim.screen_y(anchor.id);
        sim.heights.insert(anchor.id.0, 640.0);
        let mut changed = items(1..=120);
        changed[(anchor.id.0 - 1) as usize].revision = 2;
        sim.list.set_items(&changed);
        sim.settle();
        assert_near(sim.screen_y(anchor.id), before);
        assert_near(sim.list.anchor.expect("anchor").in_row, anchor.in_row);
    }

    #[test]
    fn deleting_rows_above_the_anchor_does_not_move_the_viewport() {
        let mut sim = Sim::opened(1..=160);
        sim.wheel(-1800.0);
        let anchor = sim.list.anchor.expect("anchor");
        let before = sim.screen_y(anchor.id);
        let remaining: Vec<Item> = items(1..=160)
            .into_iter()
            .filter(|item| !(anchor.id.0 - 6..anchor.id.0).contains(&item.id.0))
            .collect();
        sim.list.set_items(&remaining);
        sim.settle();
        assert_eq!(sim.list.anchor.expect("anchor").id, anchor.id);
        assert_near(sim.screen_y(anchor.id), before);
    }

    #[test]
    fn deleting_the_anchor_row_keeps_the_next_row_where_it_was() {
        let mut sim = Sim::opened(1..=160);
        sim.wheel(-1800.0);
        let anchor = sim.list.anchor.expect("anchor");
        let next = id(anchor.id.0 + 1);
        let before = sim.screen_y(next);
        let remaining: Vec<Item> = items(1..=160)
            .into_iter()
            .filter(|item| item.id != anchor.id)
            .collect();
        sim.list.set_items(&remaining);
        assert_eq!(
            sim.list.scroll_request().map(|request| request.only_from),
            Some(Some(anchor.id)),
            "a repair for the deleted row is requested"
        );
        sim.settle();
        assert_near(sim.screen_y(next), before);
        assert!(
            sim.list.scroll_request().is_none(),
            "the widget acknowledged it"
        );
    }

    #[test]
    fn resizing_the_width_keeps_the_anchor_on_screen() {
        let mut sim = Sim::opened(1..=160);
        sim.wheel(-1800.0);
        let anchor = sim.list.anchor.expect("anchor");
        let before = sim.screen_y(anchor.id);
        let wide = sim.list.bucket;
        sim.width = 480.0;
        sim.settle();
        assert_ne!(sim.list.bucket, wide);
        assert_near(sim.screen_y(anchor.id), before);
        // Narrower rows are taller; both widths stay cached for the built rows.
        let at = sim.list.index[&anchor.id];
        let slot = &sim.list.slots[at];
        assert_eq!(slot.height, row_units(sim.real(anchor.id)));
        assert!(slot.older.is_some_and(|cached| cached.bucket == wide));
        // Back to the first width: the old heights are found again, still no jump.
        sim.width = 640.0;
        sim.settle();
        assert_near(sim.screen_y(anchor.id), before);
    }

    #[test]
    fn resizing_the_height_does_not_move_the_top_of_the_viewport() {
        let mut sim = Sim::opened(1..=160);
        sim.wheel(-1800.0);
        let anchor = sim.list.anchor.expect("anchor");
        let before = sim.screen_y(anchor.id);
        sim.viewport = 900.0;
        sim.settle();
        assert_near(sim.screen_y(anchor.id), before);
        assert_eq!(sim.list.window().height, 900.0);
    }

    #[test]
    fn new_messages_are_followed_only_from_the_bottom() {
        let mut sim = Sim::opened(1..=60);
        assert!(sim.at_bottom() && sim.list.window().pinned);
        // At the bottom a new message stays in view, however tall it is.
        sim.heights.insert(61, 340.0);
        sim.list.set_items(&items(1..=61));
        sim.settle();
        assert!(sim.at_bottom());
        sim.list.set_items(&items(1..=62));
        sim.settle();
        assert!(sim.at_bottom());
        // A late height change of the newest row (an image) keeps it in view too.
        sim.heights.insert(62, 700.0);
        let mut grown = items(1..=62);
        grown[61].revision = 2;
        sim.list.set_items(&grown);
        sim.settle();
        assert!(sim.at_bottom());
        // Scrolled up, new messages do not move the viewport and the jump control shows.
        sim.wheel(-600.0);
        assert!(!sim.at_bottom() && !sim.list.window().at_bottom);
        let anchor = sim.list.anchor.expect("anchor");
        let before = sim.screen_y(anchor.id);
        sim.list.set_items(&items(1..=64));
        sim.settle();
        assert_near(sim.screen_y(anchor.id), before);
        assert!(!sim.list.window().at_bottom);
        // Jumping to the latest message returns to the bottom and acknowledges.
        sim.list.jump_latest();
        sim.settle();
        assert!(sim.at_bottom() && sim.list.window().pinned);
        assert!(sim.list.scroll_request().is_none());
    }

    #[test]
    fn rows_appended_by_paging_forward_do_not_drag_the_reader() {
        let mut sim = Sim::opened(1..=100);
        sim.list.set_live(false);
        sim.wheel(-900.0);
        sim.wheel(900.0 + 100_000.0);
        assert!(sim.at_bottom());
        let newest = sim.list.anchor().expect("anchor").id;
        let before = sim.screen_y(newest);
        sim.list.set_items(&items(1..=150));
        sim.settle();
        assert_near(sim.screen_y(newest), before);
        assert!(!sim.list.window().pinned);
    }

    #[test]
    fn a_scroll_request_is_applied_once_and_kept_until_acknowledged() {
        let mut list = VariableList::default();
        list.set_items(&items(1..=50));
        list.jump_latest();
        let request = list.scroll_request().expect("pending");
        assert_eq!(request.target, ScrollTarget::Bottom);
        // Reports that predate the request leave it pending and do not move the position.
        let stale = Viewport {
            at_bottom: false,
            anchor: Some(Anchor {
                id: id(3),
                in_row: 5.0,
            }),
            applied: request.serial - 1,
            ..Viewport::default()
        };
        list.viewport(stale);
        assert_eq!(list.scroll_request(), Some(request));
        assert!(
            list.window().at_bottom,
            "stale report must not unpin the list"
        );
        // Taking a snapshot does not consume it.
        let _ = list.window();
        assert_eq!(list.scroll_request(), Some(request));
        list.viewport(Viewport {
            applied: request.serial,
            ..Viewport::default()
        });
        assert_eq!(list.scroll_request(), None);
        // The tracker applies each serial once and then follows the position.
        let rows = [RowBox {
            id: id(1),
            revision: 1,
            top: 0.0,
            height: 2000.0,
        }];
        let geometry = Geometry {
            rows: &rows,
            content: 2000.0,
            viewport: 500.0,
            width: 640.0,
        };
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.correct(&geometry, 0.0, Some(request), true),
            Some(1500.0)
        );
        assert_eq!(
            tracker.correct(&geometry, 1500.0, Some(request), true),
            None
        );
        assert_eq!(tracker.applied, request.serial);
    }

    #[test]
    fn unresolvable_requests_are_acknowledged_and_dropped() {
        let rows = [RowBox {
            id: id(1),
            revision: 1,
            top: 0.0,
            height: 100.0,
        }];
        let geometry = Geometry {
            rows: &rows,
            content: 100.0,
            viewport: 50.0,
            width: 640.0,
        };
        let request = ScrollRequest {
            serial: next_serial(),
            target: ScrollTarget::Anchor(Anchor {
                id: id(99),
                in_row: 0.0,
            }),
            only_from: None,
        };
        let mut tracker = Tracker {
            position: Some(Position::Row(Anchor {
                id: id(1),
                in_row: 10.0,
            })),
            last: 10.0,
            ..Tracker::default()
        };
        assert_eq!(tracker.correct(&geometry, 10.0, Some(request), false), None);
        assert_eq!(tracker.applied, request.serial);
        // Not retried later when the row appears.
        let rows = [
            RowBox {
                id: id(99),
                revision: 1,
                top: 0.0,
                height: 100.0,
            },
            RowBox {
                id: id(1),
                revision: 1,
                top: 100.0,
                height: 100.0,
            },
        ];
        let geometry = Geometry {
            rows: &rows,
            content: 200.0,
            viewport: 50.0,
            width: 640.0,
        };
        assert_eq!(
            tracker.correct(&geometry, 10.0, Some(request), false),
            Some(110.0)
        );
    }

    #[test]
    fn repairs_apply_only_while_the_reader_is_still_on_the_deleted_row() {
        // Row 2 was deleted; what was 170 px down (70 px into row 2) is now covered
        // by row 3, whose top used to be 30 px below the viewport's top edge.
        let rows =
            [(1, 0.0), (3, 100.0), (4, 200.0), (5, 300.0), (6, 400.0)].map(|(n, top)| RowBox {
                id: id(n),
                revision: 1,
                top,
                height: 100.0,
            });
        let geometry = Geometry {
            rows: &rows,
            content: 500.0,
            viewport: 200.0,
            width: 640.0,
        };
        let repair = ScrollRequest {
            serial: next_serial(),
            target: ScrollTarget::Anchor(Anchor {
                id: id(3),
                in_row: -30.0,
            }),
            only_from: Some(id(2)),
        };
        // A reader on `row`, `in_row` px into it, with the scrollable at `offset`.
        let on = |row, in_row, offset| Tracker {
            position: Some(Position::Row(Anchor {
                id: id(row),
                in_row,
            })),
            last: offset,
            pinned: false,
            ..Tracker::default()
        };
        // Still on the deleted row: row 3 stays 30 px below the top edge.
        let mut tracker = on(2, 70.0, 170.0);
        assert_eq!(
            tracker.correct(&geometry, 170.0, Some(repair), false),
            Some(70.0)
        );
        assert_eq!(
            tracker.position,
            Some(Position::Row(Anchor {
                id: id(3),
                in_row: -30.0
            }))
        );
        // Already on row 4 when the repair arrives: not dragged back, still acknowledged.
        let late = ScrollRequest {
            serial: next_serial(),
            ..repair
        };
        let mut tracker = on(4, 10.0, 210.0);
        assert_eq!(tracker.correct(&geometry, 210.0, Some(late), false), None);
        assert_eq!(tracker.applied, late.serial);
        assert_eq!(
            tracker.position,
            Some(Position::Row(Anchor {
                id: id(4),
                in_row: 10.0
            }))
        );
    }

    #[test]
    fn tracker_restores_the_position_after_a_layout_shift() {
        let build = |shift: f32| -> Vec<RowBox> {
            (0..6)
                .map(|i| RowBox {
                    id: id(i + 1),
                    revision: 1,
                    top: shift + i as f32 * 100.0,
                    height: 100.0,
                })
                .collect()
        };
        let rows = build(0.0);
        let geometry = Geometry {
            rows: &rows,
            content: 600.0,
            viewport: 200.0,
            width: 640.0,
        };
        let mut tracker = Tracker {
            pinned: false,
            position: None,
            ..Tracker::default()
        };
        // The reader scrolls to 230: row 3 at 30 px.
        tracker.settle(&geometry, 230.0);
        assert_eq!(
            tracker.position,
            Some(Position::Row(Anchor {
                id: id(3),
                in_row: 30.0
            }))
        );
        // 80 px appear above (a prepended row or a taller row): the offset did not move.
        let rows = build(80.0);
        let geometry = Geometry {
            rows: &rows,
            content: 680.0,
            viewport: 200.0,
            width: 640.0,
        };
        assert_eq!(tracker.correct(&geometry, 230.0, None, false), Some(310.0));
        // Once the scrollable is there nothing more is needed.
        assert_eq!(tracker.correct(&geometry, 310.0, None, false), None);
        // A scroll by the reader is never undone.
        tracker.settle(&geometry, 420.0);
        assert_eq!(tracker.correct(&geometry, 420.0, None, false), None);
        assert_eq!(
            tracker.position,
            Some(Position::Row(Anchor {
                id: id(4),
                in_row: 40.0
            }))
        );
    }

    #[test]
    fn tracker_keeps_the_bottom_pinned_as_content_grows() {
        let rows = [RowBox {
            id: id(1),
            revision: 1,
            top: 0.0,
            height: 1000.0,
        }];
        let geometry = Geometry {
            rows: &rows,
            content: 1000.0,
            viewport: 400.0,
            width: 640.0,
        };
        let mut tracker = Tracker::default();
        assert_eq!(tracker.correct(&geometry, 0.0, None, true), Some(600.0));
        tracker.settle(&geometry, 600.0);
        let rows = [RowBox {
            height: 1300.0,
            ..rows[0]
        }];
        let grown = Geometry {
            rows: &rows,
            content: 1300.0,
            ..geometry
        };
        assert_eq!(tracker.correct(&grown, 600.0, None, true), Some(900.0));
        // Reading upward unpins; growth no longer moves the viewport.
        tracker.settle(&grown, 500.0);
        let more = [RowBox {
            height: 1500.0,
            ..rows[0]
        }];
        let grown = Geometry {
            rows: &more,
            content: 1500.0,
            ..grown
        };
        assert_eq!(tracker.correct(&grown, 500.0, None, true), None);
        // If the worker stops pinning (older pages above), a bottom position becomes a row position.
        let mut tracker = Tracker::default();
        assert_eq!(tracker.correct(&grown, 0.0, None, true), Some(1100.0));
        assert_eq!(tracker.correct(&grown, 1100.0, None, false), None);
        assert!(matches!(tracker.position, Some(Position::Row(_))));
    }

    #[test]
    fn tracker_publishes_measurements_only_for_heights_that_changed() {
        let row = |n: u64, revision: u64, top: f32, height: f32| RowBox {
            id: id(n),
            revision,
            top,
            height,
        };
        let publish = |tracker: &mut Tracker, rows: &[RowBox], width: f32| {
            let geometry = Geometry {
                rows,
                content: 1000.0,
                viewport: 400.0,
                width,
            };
            let mut out = Vec::new();
            tracker.measurements(&geometry, |m| out.push(m));
            out
        };
        let mut tracker = Tracker::default();
        let first = publish(
            &mut tracker,
            &[row(1, 1, 0.0, 100.0), row(2, 1, 100.0, 40.5)],
            640.0,
        );
        assert_eq!(first.len(), 2);
        assert_eq!(first[1].height, 40.5);
        assert_eq!(first[0].width_bucket, width_bucket(640.0));
        // Nothing changed: nothing is published, however often layout runs.
        let rows = [row(1, 1, 0.0, 100.0), row(2, 1, 100.0, 40.5)];
        assert!(publish(&mut tracker, &rows, 640.0).is_empty());
        assert!(publish(&mut tracker, &rows, 640.0).is_empty());
        // Row 1 grows because an image arrived: exactly one event, for row 1.
        let rows = [row(1, 1, 0.0, 420.0), row(2, 1, 420.0, 40.5)];
        let grown = publish(&mut tracker, &rows, 640.0);
        assert_eq!(grown.len(), 1);
        assert_eq!((grown[0].id, grown[0].height), (id(1), 420.0));
        // The same height under a new revision is still a new measurement.
        let rows = [row(1, 2, 0.0, 420.0), row(2, 1, 420.0, 40.5)];
        let revised = publish(&mut tracker, &rows, 640.0);
        assert_eq!(revised.len(), 1);
        assert_eq!(revised[0].revision, 2);
        // A different width bucket measures everything again.
        let narrow = publish(&mut tracker, &rows, 300.0);
        assert_eq!(narrow.len(), 2);
        assert!(narrow.iter().all(|m| m.width_bucket == width_bucket(300.0)));
        // Sub-quantum noise is not a change.
        let rows = [row(1, 2, 0.0, 420.004), row(2, 1, 420.0, 40.5)];
        assert!(publish(&mut tracker, &rows, 300.0).is_empty());
    }

    #[test]
    fn tracker_reports_the_viewport_only_when_it_changes() {
        let rows: Vec<RowBox> = (0..5)
            .map(|i| RowBox {
                id: id(i + 1),
                revision: 1,
                top: 50.0 + i as f32 * 200.0,
                height: 200.0,
            })
            .collect();
        let geometry = Geometry {
            rows: &rows,
            content: 1100.0,
            viewport: 400.0,
            width: 640.0,
        };
        let bucket = width_bucket(640.0);
        // The worker's belief: viewport 400 px, this bucket, and whether the
        // reader is at the bottom.
        let believes = |at_bottom: bool| ModelView {
            height: 400.0,
            bucket,
            at_bottom,
        };
        let report = |tracker: &mut Tracker, offset: f32, at_bottom: bool| {
            tracker.report(&geometry, offset, believes(at_bottom))
        };
        let mut tracker = Tracker::default();
        let first = report(&mut tracker, 0.0, false).expect("first report");
        assert_eq!((first.height, first.width), (400.0, 640.0));
        assert_eq!(first.anchor, None, "offset 0 is inside the top spacer");
        assert!(!first.at_bottom);
        assert_eq!(report(&mut tracker, 0.0, false), None);
        assert_eq!(report(&mut tracker, 2.0, false), None);
        let moved = report(&mut tracker, 260.0, false).expect("moved");
        assert_eq!(
            moved.anchor,
            Some(Anchor {
                id: id(2),
                in_row: 10.0
            })
        );
        assert_eq!(report(&mut tracker, 262.0, false), None);
        let bottom = report(&mut tracker, 700.0, true).expect("bottom");
        assert!(bottom.at_bottom);
        assert_eq!(report(&mut tracker, 700.0, true), None);
        // A replaced list starts out wrong about the height, the bucket, or whether
        // the reader is at the bottom: each is corrected without the layout
        // changing, and a worker that has not caught up yet is told only once.
        let wrong_height = ModelView {
            height: 600.0,
            ..believes(true)
        };
        assert!(tracker.report(&geometry, 700.0, wrong_height).is_some());
        assert_eq!(tracker.report(&geometry, 700.0, wrong_height), None);
        let wrong_bucket = ModelView {
            bucket: bucket + 1,
            ..believes(true)
        };
        assert!(tracker.report(&geometry, 700.0, wrong_bucket).is_some());
        assert!(tracker.report(&geometry, 700.0, believes(false)).is_some());
        // Once the worker agrees, being reset to the same wrong belief is corrected again.
        assert_eq!(report(&mut tracker, 700.0, true), None);
        assert!(tracker.report(&geometry, 700.0, wrong_height).is_some());
    }

    #[test]
    fn height_and_position_inputs_are_sanitized() {
        let mut list = VariableList::default();
        list.set_items(&items(1..=10));
        assert!(!list.measure(Measurement {
            id: id(1),
            revision: 1,
            width_bucket: list.bucket,
            height: f32::NAN,
        }));
        assert!(!list.measure(Measurement {
            id: id(1),
            revision: 1,
            width_bucket: list.bucket,
            height: f32::INFINITY,
        }));
        list.viewport(Viewport {
            offset: f32::NAN,
            height: f32::NAN,
            width: f32::INFINITY,
            anchor: Some(Anchor {
                id: id(3),
                in_row: f32::NAN,
            }),
            ..Viewport::default()
        });
        let window = list.window();
        assert!(window.height.is_finite() && window.offset.is_finite());
        assert!(window.top.is_finite() && window.bottom.is_finite());
        // Extreme but finite heights saturate instead of overflowing the index.
        assert!(list.measure(Measurement {
            id: id(2),
            revision: 1,
            width_bucket: list.bucket,
            height: f32::MAX,
        }));
        assert_eq!(list.slots[1].height, row_units(MAX_ROW_HEIGHT));
        assert!(list.window().bottom >= 0.0);
    }

    #[test]
    fn stale_or_unknown_measurements_are_ignored() {
        let mut list = VariableList::default();
        list.set_items(&items(1..=5));
        let current = list.bucket;
        let measure = |id, revision, height| Measurement {
            id,
            revision,
            width_bucket: current,
            height,
        };
        assert!(list.measure(measure(id(2), 1, 90.0)));
        assert!(!list.measure(measure(id(2), 1, 90.0)), "unchanged");
        assert!(!list.measure(measure(id(2), 7, 300.0)), "wrong revision");
        assert!(!list.measure(measure(id(99), 1, 300.0)), "unknown id");
        assert_eq!(list.slots[1].height, row_units(90.0));
        // A new revision makes the old height an estimate until it is measured again.
        let mut revised = items(1..=5);
        revised[1].revision = 2;
        assert!(list.set_items(&revised));
        assert_eq!(list.slots[1].height, row_units(90.0));
        assert!(!list.measure(measure(id(2), 1, 30.0)), "stale revision");
        assert!(list.measure(measure(id(2), 2, 30.0)));
    }

    #[test]
    fn cache_holds_two_widths_per_row_and_follows_the_ids() {
        let mut list = VariableList::default();
        list.set_items(&items(1..=MAX_ITEMS as u64));
        let current = list.bucket;
        for bucket in [current, current + 1, current + 2] {
            for n in 1..=MAX_ITEMS as u64 {
                list.measure(Measurement {
                    id: id(n),
                    revision: 1,
                    width_bucket: bucket,
                    height: 40.0 + bucket as f32,
                });
            }
        }
        assert_eq!(
            list.cache_len(),
            2 * MAX_ITEMS,
            "two buckets per row at most"
        );
        assert!(
            list.retained_bytes() < 128 * 1024,
            "{}",
            list.retained_bytes()
        );
        // Rows that leave take their heights with them; rows that stay keep theirs.
        list.set_items(&items(401..=MAX_ITEMS as u64));
        assert_eq!(list.cache_len(), 2 * 100);
        assert!(!list.measure(Measurement {
            id: id(5),
            revision: 1,
            width_bucket: current,
            height: 99.0,
        }));
        assert_eq!(list.index.len(), 100);
        list.set_items(&[]);
        assert_eq!(list.cache_len(), 0);
        assert_eq!(list.window(), VariableList::default().window());
    }

    #[test]
    fn items_are_capped_and_deduplicated() {
        let mut list = VariableList::default();
        list.set_items(&items(1..=700));
        assert_eq!(list.slots.len(), MAX_ITEMS);
        assert_eq!(list.slots[0].item.id, id(201));
        let mut repeated = items(1..=3);
        repeated.extend(items(2..=4));
        assert!(list.set_items(&repeated));
        let ids: Vec<u64> = list.slots.iter().map(|slot| slot.item.id.0).collect();
        assert_eq!(ids, [1, 2, 3, 4]);
        assert!(
            !list.set_items(&items(1..=4)),
            "unchanged input reports no change"
        );
    }

    #[test]
    fn window_for_range_keeps_the_spacers_exact() {
        let mut list = VariableList::default();
        list.set_items(&items(1..=100));
        for n in 1..=100 {
            list.measure(Measurement {
                id: id(n),
                revision: 1,
                width_bucket: list.bucket,
                height: 30.0 + n as f32,
            });
        }
        let total: f32 = (1..=100).map(|n| 30.0 + n as f32).sum();
        for range in [
            0..0,
            0..100,
            10..20,
            99..100,
            40..400,
            Range { start: 70, end: 30 },
        ] {
            let window = list.window_for_range(range);
            let built: f32 = window.range.clone().map(|i| 30.0 + (i + 1) as f32).sum();
            assert_eq!(
                window.top + built + window.bottom,
                total,
                "{:?}",
                window.range
            );
            assert!(window.range.end <= 100 && window.range.start <= window.range.end);
        }
        assert_eq!(list.index_at(0.0), 0);
        assert_eq!(list.index_at(31.0), 1, "first row is 31 px tall");
        assert_eq!(list.index_at(total), 100);
        assert_eq!(list.index_at(total + 1.0e6), 100);
    }

    #[test]
    fn ten_thousand_messages_page_through_a_bounded_list() {
        const NEWEST: u64 = 10_000;
        const PAGE: u64 = 50;
        let mut sim = Sim::new(700.0, 640.0);
        // `lo..=hi` are the retained messages: the newest page first.
        let mut lo = NEWEST - PAGE + 1;
        let mut hi = NEWEST;
        sim.list.set_items(&items(lo..=hi));
        sim.list.jump_latest();
        sim.settle();
        let mut seen = PAGE;
        let mut widest_window = 0;
        let check = |sim: &Sim, lo: u64, hi: u64| {
            assert!(sim.list.slots.len() <= MAX_ITEMS);
            assert_eq!(sim.list.slots.len() as u64, hi - lo + 1);
            assert!(sim.list.cache_len() <= 2 * MAX_ITEMS);
            assert!(sim.list.retained_bytes() < 128 * 1024);
            let window = sim.list.window();
            let built = window.range.len();
            assert!(built <= MAX_WINDOW_ROWS, "{built} rows built");
            built
        };
        // Read upward all the way to the first message, paging older when near the top.
        let mut older_pages = 0;
        loop {
            sim.wheel(-650.0);
            widest_window = widest_window.max(check(&sim, lo, hi));
            let window = sim.list.window();
            if window.offset <= 80.0 && lo > 1 {
                let new_lo = lo.saturating_sub(PAGE).max(1);
                seen += lo - new_lo;
                lo = new_lo;
                // Older rows are added at the top; the newest ones fall out of the cache.
                if hi - lo + 1 > MAX_ITEMS as u64 {
                    hi = lo + MAX_ITEMS as u64 - 1;
                }
                let anchor = sim.list.anchor.expect("anchor");
                let before = sim.screen_y(anchor.id);
                sim.list.set_items(&items(lo..=hi));
                sim.list.set_live(hi == NEWEST);
                sim.settle();
                assert_near(sim.screen_y(anchor.id), before);
                older_pages += 1;
            }
            if lo == 1 && sim.list.window().offset <= 1.0 {
                break;
            }
            assert!(older_pages < 400, "never reached the first message");
        }
        assert_eq!(seen, NEWEST, "every message passed through the list");
        assert!(hi < NEWEST, "the newest messages were evicted on the way");
        assert!(widest_window <= MAX_WINDOW_ROWS);
        // And back down to the newest message, paging newer when at the bottom.
        let mut newer_pages = 0;
        while hi < NEWEST {
            sim.wheel(900.0);
            check(&sim, lo, hi);
            if sim.at_bottom() {
                let new_hi = (hi + PAGE).min(NEWEST);
                hi = new_hi;
                if hi - lo + 1 > MAX_ITEMS as u64 {
                    lo = hi - MAX_ITEMS as u64 + 1;
                }
                let anchor = sim.list.anchor.expect("anchor");
                let before = sim.screen_y(anchor.id);
                sim.list.set_items(&items(lo..=hi));
                sim.list.set_live(hi == NEWEST);
                sim.settle();
                assert_near(sim.screen_y(anchor.id), before);
                newer_pages += 1;
            }
            assert!(newer_pages < 400, "never reached the newest message");
        }
        sim.list.jump_latest();
        sim.settle();
        assert!(sim.at_bottom());
        assert_eq!(
            sim.list.slots.last().map(|slot| slot.item.id),
            Some(id(NEWEST))
        );
    }

    use harness::Ui;

    fn fixed(height: f32) -> Element<'static, Report> {
        space().width(Length::Fill).height(height).into()
    }

    /// What the worker believes while the widget is in the state a test sets up.
    fn model(pinned: bool) -> Window {
        Window {
            height: 400.0,
            width_bucket: width_bucket(640.0),
            at_bottom: pinned,
            pinned,
            ..Window::default()
        }
    }

    /// The list over rows of fixed heights, `(id, revision, height)`, so the
    /// expected geometry is exact on every platform.
    fn list(
        window: &Window,
        request: Option<ScrollRequest>,
        rows: &[(u64, u64, f32)],
    ) -> Element<'static, Report> {
        let rows = rows.iter().map(|&(n, revision, height)| {
            (
                Item {
                    id: id(n),
                    revision,
                },
                fixed(height),
            )
        });
        view(1, window, request, rows, |report| report)
    }

    fn ten_rows() -> Vec<(u64, u64, f32)> {
        (1..=10).map(|n| (n, 1, 100.0 + n as f32)).collect()
    }

    fn measured(reports: &[Report]) -> Vec<(Snowflake, u64, f32)> {
        reports
            .iter()
            .filter_map(|report| match report {
                Report::Measured(m) => Some((m.id, m.revision, m.height)),
                Report::Viewport(_) => None,
            })
            .collect()
    }

    #[test]
    fn widget_applies_the_bottom_request_and_reports_what_it_laid_out() {
        let mut ui = Ui::new(640.0, 400.0, list(&Window::default(), None, &[]));
        let request = ScrollRequest {
            serial: next_serial(),
            target: ScrollTarget::Bottom,
            only_from: None,
        };
        // Rows are 101..=110 px tall: 1055 px of content in a 400 px viewport.
        ui.show(list(&model(true), Some(request), &ten_rows()));
        let reports = ui.redraw();
        assert_eq!(ui.offset(), 655.0, "scrolled to the bottom before drawing");
        let Some(Report::Viewport(viewport)) = reports.first().copied() else {
            panic!("the viewport comes first: {reports:?}");
        };
        assert_eq!(
            (viewport.offset, viewport.height, viewport.width),
            (655.0, 400.0, 640.0)
        );
        // Row 7 spans 621..728, so 34 px of it are above the viewport.
        assert_eq!(
            viewport.anchor,
            Some(Anchor {
                id: id(7),
                in_row: 34.0
            })
        );
        assert!(viewport.at_bottom);
        assert_eq!(viewport.applied, request.serial);
        // Every built row reports the height it really laid out to, in the width bucket
        // it was laid out at.
        assert_eq!(
            measured(&reports),
            (1..=10)
                .map(|n| (id(n), 1, 100.0 + n as f32))
                .collect::<Vec<_>>()
        );
        assert!(reports[1..].iter().all(
            |report| matches!(report, Report::Measured(m) if m.width_bucket == width_bucket(640.0))
        ));
        // An idle window publishes nothing, however often iced redraws it.
        for _ in 0..5 {
            assert!(ui.redraw().is_empty());
        }
    }

    #[test]
    fn widget_keeps_the_reader_in_place_when_a_row_above_grows() {
        let mut ui = Ui::new(640.0, 400.0, list(&Window::default(), None, &[]));
        let request = ScrollRequest {
            serial: next_serial(),
            target: ScrollTarget::Bottom,
            only_from: None,
        };
        let rows = ten_rows();
        ui.show(list(&model(true), Some(request), &rows));
        ui.redraw();
        // The reader scrolls up: row 3 (203..306) is at the top edge, 97 px in.
        ui.scroll_to(300.0);
        let reports = ui.redraw();
        assert_eq!(reports.len(), 1, "{reports:?}");
        let Some(Report::Viewport(viewport)) = reports.first().copied() else {
            panic!("{reports:?}");
        };
        assert_eq!(
            viewport.anchor,
            Some(Anchor {
                id: id(3),
                in_row: 97.0
            })
        );
        assert!(!viewport.at_bottom);
        // An image arrives in row 2: it is now 352 px tall, 250 px more.
        let mut grown = rows.clone();
        grown[1] = (2, 2, 352.0);
        ui.show(list(&model(false), None, &grown));
        let reports = ui.redraw();
        assert_eq!(
            ui.offset(),
            550.0,
            "the offset moved with the content, in the same frame"
        );
        // The reader is where they were, so only the new height is news.
        assert_eq!(measured(&reports), [(id(2), 2, 352.0)]);
        assert_eq!(reports.len(), 1, "{reports:?}");
        // Rows deleted above work the same way.
        grown.remove(0);
        ui.show(list(&model(false), None, &grown));
        ui.redraw();
        assert_eq!(ui.offset(), 449.0, "101 px fewer above the reader");
    }

    #[test]
    fn widget_follows_new_rows_only_while_pinned_and_drops_requests_for_unbuilt_rows() {
        let mut ui = Ui::new(640.0, 400.0, list(&Window::default(), None, &[]));
        let request = ScrollRequest {
            serial: next_serial(),
            target: ScrollTarget::Bottom,
            only_from: None,
        };
        let mut rows = ten_rows();
        ui.show(list(&model(true), Some(request), &rows));
        ui.redraw();
        // A message arrives while the reader is at the bottom: it stays in view.
        rows.push((11, 1, 200.0));
        ui.show(list(&model(true), Some(request), &rows));
        let reports = ui.redraw();
        assert_eq!(ui.offset(), 855.0);
        assert_eq!(measured(&reports), [(id(11), 1, 200.0)]);
        // Not pinned (the reader scrolled away), the same arrival moves nothing.
        ui.scroll_to(100.0);
        ui.redraw();
        rows.push((12, 1, 90.0));
        ui.show(list(&model(false), None, &rows));
        ui.redraw();
        assert_eq!(ui.offset(), 100.0);
        // A request for a row that is not built is acknowledged, never applied.
        let missing = ScrollRequest {
            serial: next_serial(),
            target: ScrollTarget::Anchor(Anchor {
                id: id(99),
                in_row: 0.0,
            }),
            only_from: None,
        };
        ui.show(list(&model(false), Some(missing), &rows));
        let reports = ui.redraw();
        let viewport = reports
            .iter()
            .find_map(|report| match report {
                Report::Viewport(viewport) => Some(*viewport),
                Report::Measured(_) => None,
            })
            .expect("the acknowledgement is reported");
        assert_eq!(viewport.applied, missing.serial);
        assert_eq!(ui.offset(), 100.0);
        // A request for a built row puts that row at the top edge.
        let to_row = ScrollRequest {
            serial: next_serial(),
            target: ScrollTarget::Anchor(Anchor {
                id: id(5),
                in_row: 4.0,
            }),
            only_from: None,
        };
        ui.show(list(&model(false), Some(to_row), &rows));
        ui.redraw();
        assert_eq!(ui.offset(), 414.0, "row 5 starts at 410");
    }
}
