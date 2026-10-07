//! One guild member list, kept only for the rows the server maintains for us.
//!
//! A member list is a flat sequence of rows (section headers and members) that
//! can be very long. The server only describes the ranges we subscribed to, so
//! the list is held sparsely as sorted, non-overlapping segments of known
//! rows. Operations address positions in the whole flattened list: an INSERT
//! or DELETE shifts every later row, known or not, which is why later segments
//! move with it. This is the model the official client applies the same
//! operations to (docs/PROTOCOL.md).

use std::mem::size_of;

use fastcord_model::Snowflake;

use crate::gateway::{ListGroup, ListRow, MemberListId, MemberListOp};

/// Known rows one list may hold. A list normally holds at most
/// `MAX_RANGES * RANGE_BLOCK` (300) rows; anything beyond this limit is
/// dropped from the far end rather than grown.
pub const MAX_ROWS: usize = 1_000;

/// Past this index nothing is addressable; keeps positions in `u32`.
const END_OF_ROWS: u64 = u32::MAX as u64 + 1;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Segment {
    start: u32,
    rows: Vec<ListRow>,
}

impl Segment {
    /// One past the last row, as a `u64` so a segment at the very end of the
    /// index space has no overflow.
    fn end(&self) -> u64 {
        u64::from(self.start) + self.rows.len() as u64
    }
}

/// The rows we know of one member list, plus its headline numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberList {
    id: MemberListId,
    member_count: u32,
    online_count: u32,
    groups: Vec<ListGroup>,
    segments: Vec<Segment>,
    known: usize,
    /// Cached row-vector allocations; ordinary member writes need not scan
    /// the sparse list merely to update the store's ledger.
    rows_bytes: usize,
    /// Store tick of the last update; the newest list of a guild is the one
    /// on screen.
    pub(crate) updated: u64,
}

impl MemberList {
    pub(crate) fn new(id: MemberListId) -> Self {
        Self {
            id,
            member_count: 0,
            online_count: 0,
            groups: Vec::new(),
            segments: Vec::new(),
            known: 0,
            rows_bytes: 0,
            updated: 0,
        }
    }

    pub fn id(&self) -> &MemberListId {
        &self.id
    }

    /// Members in the whole list, as last reported.
    pub fn member_count(&self) -> u32 {
        self.member_count
    }

    pub fn online_count(&self) -> u32 {
        self.online_count
    }

    /// The sections of the list with their sizes, as last reported.
    pub fn groups(&self) -> &[ListGroup] {
        &self.groups
    }

    /// How many rows are currently known.
    pub fn known_rows(&self) -> usize {
        self.known
    }

    /// The row at `index` of the whole list, if it is known.
    pub fn row(&self, index: u32) -> Option<&ListRow> {
        let at = self
            .segments
            .partition_point(|segment| segment.end() <= u64::from(index));
        let segment = self.segments.get(at)?;
        segment.rows.get(index.checked_sub(segment.start)? as usize)
    }

    /// The known rows of `start..=end`, in order, with their indices.
    pub fn rows(&self, start: u32, end: u32) -> impl Iterator<Item = (u32, &ListRow)> {
        self.segments
            .iter()
            .filter(move |segment| segment.start <= end && segment.end() > u64::from(start))
            .flat_map(move |segment| {
                segment
                    .rows
                    .iter()
                    .enumerate()
                    .map(move |(offset, row)| (segment.start + offset as u32, row))
            })
            .filter(move |(index, _)| (start..=end).contains(index))
    }

    /// Users that the known rows show.
    pub fn member_ids(&self) -> impl Iterator<Item = Snowflake> + '_ {
        self.segments
            .iter()
            .flat_map(|segment| &segment.rows)
            .filter_map(|row| match row {
                ListRow::Member(user) => Some(*user),
                _ => None,
            })
    }

    /// Estimated heap and inline size.
    pub(crate) fn bytes(&self) -> usize {
        self.base_bytes() + self.rows_bytes
    }

    fn base_bytes(&self) -> usize {
        size_of::<Self>()
            + self.id.0.capacity()
            + self.groups.capacity() * size_of::<ListGroup>()
            + self.segments.capacity() * size_of::<Segment>()
    }

    pub(crate) fn recount(&self) -> usize {
        self.base_bytes()
            + self
                .segments
                .iter()
                .map(|segment| segment.rows.capacity() * size_of::<ListRow>())
                .sum::<usize>()
    }

    pub(crate) fn set_header(
        &mut self,
        member_count: Option<u32>,
        online_count: Option<u32>,
        groups: Option<Vec<ListGroup>>,
    ) {
        if let Some(member_count) = member_count {
            self.member_count = member_count;
        }
        if let Some(online_count) = online_count {
            self.online_count = online_count;
        }
        if let Some(groups) = groups {
            self.groups = groups;
        }
    }

    /// Forgets every row (counts are kept).
    pub(crate) fn clear_rows(&mut self) {
        self.segments = Vec::new();
        self.known = 0;
        self.rows_bytes = 0;
    }

    /// Releases rows outside the consumer's current viewport. Overlapping or
    /// unordered ranges are accepted without copying either the rows or the
    /// range list.
    pub(crate) fn retain_ranges(&mut self, ranges: &[(u32, u32)]) {
        if ranges.is_empty() {
            self.clear_rows();
            return;
        }
        if self.segments.iter().all(|segment| {
            ranges
                .iter()
                .any(|&(start, end)| start <= segment.start && u64::from(end) + 1 >= segment.end())
        }) {
            return;
        }
        let mut cursor = 0u64;
        while let Some(&(start, end)) = ranges
            .iter()
            .filter(|&&(start, end)| start <= end && u64::from(end) >= cursor)
            .min_by_key(|&&(start, _)| start)
        {
            if u64::from(start) > cursor {
                self.remove_range(cursor as u32, start - 1);
            }
            cursor = u64::from(end) + 1;
        }
        if cursor < END_OF_ROWS {
            self.remove_range(cursor as u32, u32::MAX);
        }
        self.normalize();
    }

    pub(crate) fn apply(&mut self, op: MemberListOp) {
        match op {
            MemberListOp::Sync { start, end, rows } => self.sync(start, end, rows),
            MemberListOp::Insert { index, row } => self.insert(index, row),
            MemberListOp::Update { index, row } => self.update(index, row),
            MemberListOp::Delete { index } => self.delete(index),
            MemberListOp::Invalidate { start, end } => self.remove_range(start, end),
            MemberListOp::Unknown => self.clear_rows(),
        }
        self.normalize();
    }

    /// Index of the first segment that ends after `index`.
    fn position(&self, index: u32) -> usize {
        self.segments
            .partition_point(|segment| segment.end() <= u64::from(index))
    }

    fn sync(&mut self, start: u32, end: u32, mut rows: Vec<ListRow>) {
        if end < start {
            return;
        }
        let span = u64::from(end) - u64::from(start) + 1;
        rows.truncate(span.min(MAX_ROWS as u64) as usize);
        self.remove_range(start, end);
        if rows.is_empty() {
            return;
        }
        let at = self
            .segments
            .partition_point(|segment| segment.start < start);
        self.segments.insert(at, Segment { start, rows });
    }

    fn insert(&mut self, index: u32, row: ListRow) {
        let at = self.position(index);
        match self.segments.get_mut(at) {
            Some(segment) if segment.start <= index => {
                segment.rows.insert((index - segment.start) as usize, row);
                self.shift(at + 1, true);
            }
            _ => {
                if at > 0 && self.segments[at - 1].end() == u64::from(index) {
                    self.segments[at - 1].rows.push(row);
                    self.shift(at, true);
                } else {
                    // Inside a range we maintain, but nothing is known there.
                    self.segments.insert(
                        at,
                        Segment {
                            start: index,
                            rows: vec![row],
                        },
                    );
                    self.shift(at + 1, true);
                }
            }
        }
    }

    fn update(&mut self, index: u32, row: ListRow) {
        let at = self.position(index);
        if let Some(segment) = self.segments.get_mut(at)
            && segment.start <= index
        {
            segment.rows[(index - segment.start) as usize] = row;
        }
    }

    fn delete(&mut self, index: u32) {
        let at = self.position(index);
        let mut first_later = at;
        if let Some(segment) = self.segments.get_mut(at)
            && segment.start <= index
        {
            segment.rows.remove((index - segment.start) as usize);
            first_later = at + 1;
        }
        // Every later row moves up, whether or not we know it.
        self.shift(first_later, false);
    }

    /// Moves every segment from `from` on by one row.
    fn shift(&mut self, from: usize, up: bool) {
        for segment in self.segments.iter_mut().skip(from) {
            if up {
                match segment.start.checked_add(1) {
                    Some(start) => segment.start = start,
                    // Pushed past the last addressable row.
                    None => segment.rows.clear(),
                }
            } else {
                segment.start = segment.start.saturating_sub(1);
            }
        }
    }

    /// Forgets the known rows of `start..=end`; neighbours stay where they are.
    fn remove_range(&mut self, start: u32, end: u32) {
        if end < start {
            return;
        }
        let first = self.position(start);
        let after = self
            .segments
            .partition_point(|segment| segment.start <= end);
        if first >= after {
            return;
        }
        let to = u64::from(end) + 1;
        let keep_left = self.segments[first].start < start;
        let keep_right = self.segments[after - 1].end() > to;
        if first + 1 == after && keep_left && keep_right {
            let segment = &mut self.segments[first];
            let right = segment
                .rows
                .split_off((to - u64::from(segment.start)) as usize);
            segment.rows.truncate((start - segment.start) as usize);
            self.segments.insert(
                first + 1,
                Segment {
                    start: end + 1,
                    rows: right,
                },
            );
            return;
        }
        let mut from = first;
        let mut until = after;
        if keep_left {
            let segment = &mut self.segments[first];
            segment.rows.truncate((start - segment.start) as usize);
            from += 1;
        }
        if keep_right {
            let segment = &mut self.segments[after - 1];
            drop(
                segment
                    .rows
                    .drain(..(to - u64::from(segment.start)) as usize),
            );
            segment.start = end + 1;
            until -= 1;
        }
        drop(self.segments.drain(from..until));
    }

    /// Restores the invariants after any operation: no empty segment, nothing
    /// past the last addressable row, touching segments joined, and a bounded
    /// number of rows.
    fn normalize(&mut self) {
        let mut previous: Option<usize> = None;
        for at in 0..self.segments.len() {
            let segment = &mut self.segments[at];
            let room = END_OF_ROWS.saturating_sub(u64::from(segment.start));
            if segment.rows.len() as u64 > room {
                segment.rows.truncate(room as usize);
            }
            if segment.rows.is_empty() {
                continue;
            }
            if let Some(before) = previous
                && self.segments[before].end() >= u64::from(self.segments[at].start)
            {
                // Touching or overlapping: the later segment's rows win.
                let (earlier, later) = self.segments.split_at_mut(at);
                let previous = &mut earlier[before];
                let segment = &mut later[0];
                previous
                    .rows
                    .truncate((u64::from(segment.start) - u64::from(previous.start)) as usize);
                previous.rows.append(&mut segment.rows);
            } else {
                previous = Some(at);
            }
        }
        self.segments.retain(|segment| !segment.rows.is_empty());
        let mut known: usize = self.segments.iter().map(|segment| segment.rows.len()).sum();
        while known > MAX_ROWS {
            let Some(last) = self.segments.last_mut() else {
                break;
            };
            let excess = (known - MAX_ROWS).min(last.rows.len());
            last.rows.truncate(last.rows.len() - excess);
            known -= excess;
            if last.rows.is_empty() {
                self.segments.pop();
            }
        }
        for segment in &mut self.segments {
            if segment.rows.capacity() > MAX_ROWS
                || segment.rows.capacity() > segment.rows.len().saturating_mul(2)
            {
                segment.rows.shrink_to_fit();
            }
        }
        if self.segments.capacity() > self.segments.len().saturating_mul(2) {
            self.segments.shrink_to_fit();
        }
        self.known = known;
        self.rows_bytes = self
            .segments
            .iter()
            .map(|segment| segment.rows.capacity() * size_of::<ListRow>())
            .sum();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{GroupId, ListGroup};

    fn member(id: u64) -> ListRow {
        ListRow::Member(Snowflake(id))
    }

    fn group(id: GroupId, count: u32) -> ListRow {
        ListRow::Group(ListGroup { id, count })
    }

    fn list() -> MemberList {
        MemberList::new(MemberListId("everyone".to_owned()))
    }

    fn sync(list: &mut MemberList, start: u32, end: u32, rows: Vec<ListRow>) {
        list.apply(MemberListOp::Sync { start, end, rows });
    }

    /// The known rows as `(index, user)`, groups shown as `0`.
    fn dump(list: &MemberList) -> Vec<(u32, u64)> {
        list.rows(0, u32::MAX)
            .map(|(index, row)| match row {
                ListRow::Member(user) => (index, user.0),
                ListRow::Group(_) => (index, 0),
                ListRow::Unreadable => (index, u64::MAX),
            })
            .collect()
    }

    fn sample() -> MemberList {
        let mut list = list();
        sync(
            &mut list,
            0,
            99,
            vec![
                group(GroupId::Online, 3),
                member(1),
                member(2),
                member(3),
                group(GroupId::Offline, 1),
                member(4),
            ],
        );
        list
    }

    #[test]
    fn sync_installs_rows_and_a_short_sync_ends_the_list() {
        let list = sample();
        assert_eq!(
            dump(&list),
            [(0, 0), (1, 1), (2, 2), (3, 3), (4, 0), (5, 4)]
        );
        assert_eq!(list.known_rows(), 6);
        assert_eq!(list.row(2), Some(&member(2)));
        assert_eq!(list.row(6), None);
        assert_eq!(
            list.member_ids().map(|id| id.0).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
    }

    #[test]
    fn insert_shifts_every_later_row_including_other_segments() {
        let mut list = sample();
        sync(
            &mut list,
            200,
            202,
            vec![member(20), member(21), member(22)],
        );
        list.apply(MemberListOp::Insert {
            index: 2,
            row: member(9),
        });
        assert_eq!(
            dump(&list),
            [
                (0, 0),
                (1, 1),
                (2, 9),
                (3, 2),
                (4, 3),
                (5, 0),
                (6, 4),
                (201, 20),
                (202, 21),
                (203, 22)
            ]
        );
    }

    #[test]
    fn insert_at_the_end_of_a_range_and_into_an_empty_list_works() {
        let mut list = sample();
        list.apply(MemberListOp::Insert {
            index: 6,
            row: member(5),
        });
        assert_eq!(list.row(6), Some(&member(5)));
        assert_eq!(list.known_rows(), 7);

        let mut empty = self::list();
        sync(&mut empty, 0, 99, Vec::new());
        assert_eq!(empty.known_rows(), 0);
        empty.apply(MemberListOp::Insert {
            index: 0,
            row: member(7),
        });
        assert_eq!(dump(&empty), [(0, 7)]);
    }

    #[test]
    fn delete_removes_and_closes_the_gap_even_across_segments() {
        let mut list = sample();
        sync(&mut list, 100, 101, vec![member(10), member(11)]);
        list.apply(MemberListOp::Delete { index: 1 });
        assert_eq!(
            dump(&list),
            [(0, 0), (1, 2), (2, 3), (3, 0), (4, 4), (99, 10), (100, 11)]
        );
        // Deleting in a hole (nothing known there) still moves later rows up.
        list.apply(MemberListOp::Delete { index: 50 });
        assert_eq!(list.row(98), Some(&member(10)));
        assert_eq!(list.row(99), Some(&member(11)));
        // A shifted segment that now touches the previous one joins it.
        for _ in 0..93 {
            list.apply(MemberListOp::Delete { index: 5 });
        }
        assert_eq!(list.row(4), Some(&member(4)));
        assert_eq!(list.row(5), Some(&member(10)));
        assert_eq!(list.row(6), Some(&member(11)));
        assert_eq!(list.known_rows(), 7);
    }

    #[test]
    fn update_replaces_a_known_row_and_ignores_unknown_ones() {
        let mut list = sample();
        list.apply(MemberListOp::Update {
            index: 3,
            row: member(33),
        });
        assert_eq!(list.row(3), Some(&member(33)));
        list.apply(MemberListOp::Update {
            index: 500,
            row: member(34),
        });
        assert_eq!(list.known_rows(), 6);
        assert_eq!(list.row(500), None);
    }

    #[test]
    fn invalidate_forgets_a_range_and_splits_segments_without_shifting() {
        let mut list = sample();
        list.apply(MemberListOp::Invalidate { start: 2, end: 3 });
        assert_eq!(dump(&list), [(0, 0), (1, 1), (4, 0), (5, 4)]);
        assert_eq!(list.known_rows(), 4);
        list.apply(MemberListOp::Invalidate { start: 0, end: 99 });
        assert_eq!(list.known_rows(), 0);
        // Reversed or empty ranges are ignored.
        let mut list = sample();
        list.apply(MemberListOp::Invalidate { start: 5, end: 1 });
        assert_eq!(list.known_rows(), 6);
    }

    #[test]
    fn sync_replaces_only_its_range_and_truncates_to_its_span() {
        let mut list = sample();
        sync(
            &mut list,
            2,
            3,
            vec![member(7), member(8), member(9), member(10)],
        );
        assert_eq!(
            dump(&list),
            [(0, 0), (1, 1), (2, 7), (3, 8), (4, 0), (5, 4)]
        );
        // A SYNC with fewer rows than its span says the rest is gone.
        sync(&mut list, 3, 99, vec![member(1)]);
        assert_eq!(dump(&list), [(0, 0), (1, 1), (2, 7), (3, 1)]);
    }

    #[test]
    fn unknown_operations_distrust_the_whole_list() {
        let mut list = sample();
        list.set_header(
            Some(40),
            Some(12),
            Some(vec![ListGroup {
                id: GroupId::Online,
                count: 12,
            }]),
        );
        list.apply(MemberListOp::Unknown);
        assert_eq!(list.known_rows(), 0);
        assert_eq!((list.member_count(), list.online_count()), (40, 12));
        assert_eq!(list.groups().len(), 1);
    }

    #[test]
    fn omitted_headers_preserve_values_but_explicit_zero_and_empty_clear_them() {
        let mut list = list();
        list.set_header(
            Some(42),
            Some(10),
            Some(vec![ListGroup {
                id: GroupId::Offline,
                count: 32,
            }]),
        );
        list.set_header(None, None, None);
        assert_eq!((list.member_count(), list.online_count()), (42, 10));
        assert_eq!(list.groups()[0].count, 32);
        list.set_header(Some(0), Some(0), Some(Vec::new()));
        assert_eq!((list.member_count(), list.online_count()), (0, 0));
        assert!(list.groups().is_empty());
    }

    #[test]
    fn hostile_indices_and_sizes_stay_bounded_and_in_range() {
        let mut list = list();
        sync(
            &mut list,
            u32::MAX - 1,
            u32::MAX,
            vec![member(1), member(2)],
        );
        list.apply(MemberListOp::Insert {
            index: u32::MAX - 1,
            row: member(3),
        });
        // The row pushed past the last address is dropped, not wrapped.
        assert_eq!(dump(&list), [(u32::MAX - 1, 3), (u32::MAX, 1)]);
        list.apply(MemberListOp::Delete { index: 0 });
        assert_eq!(list.row(u32::MAX - 2), Some(&member(3)));

        let mut big = self::list();
        sync(
            &mut big,
            0,
            u32::MAX,
            (0..MAX_ROWS as u64 * 3).map(member).collect(),
        );
        assert_eq!(big.known_rows(), MAX_ROWS);
        assert_eq!(big.row(0), Some(&member(0)));
        assert_eq!(big.row(MAX_ROWS as u32), None, "the far end is dropped");
        assert!(big.bytes() < 100 * 1024);
    }

    #[test]
    fn rows_are_returned_in_order_within_the_requested_window() {
        let mut list = sample();
        sync(&mut list, 100, 101, vec![member(10), member(11)]);
        let window: Vec<u32> = list.rows(4, 100).map(|(index, _)| index).collect();
        assert_eq!(window, [4, 5, 100]);
        assert_eq!(list.rows(7, 99).count(), 0);
    }

    #[test]
    fn truncation_and_invalidation_release_excess_row_capacity() {
        let mut list = list();
        let mut rows = Vec::with_capacity(MAX_ROWS * 128);
        rows.extend((0..MAX_ROWS as u64 * 3).map(member));
        sync(&mut list, 0, u32::MAX, rows);
        assert_eq!(list.known_rows(), MAX_ROWS);
        assert!(list.segments[0].rows.capacity() <= MAX_ROWS);
        assert_eq!(list.bytes(), list.recount());

        list.apply(MemberListOp::Invalidate {
            start: 1,
            end: u32::MAX,
        });
        assert_eq!(list.known_rows(), 1);
        assert_eq!(list.segments[0].rows.capacity(), 1);
        assert_eq!(list.bytes(), list.recount());

        list.clear_rows();
        assert_eq!(list.segments.capacity(), 0);
        assert_eq!(list.rows_bytes, 0);
        assert_eq!(list.bytes(), list.recount());
    }

    #[test]
    fn viewport_clipping_preserves_indices_and_compacts_sparse_allocations() {
        let mut list = list();
        sync(&mut list, 0, 599, (0..600).map(member).collect());
        sync(
            &mut list,
            1_000,
            1_009,
            (1_000..1_010).map(member).collect(),
        );
        list.retain_ranges(&[(270, 310), (50, 79), (200, 289)]);
        assert_eq!(list.known_rows(), 141);
        assert_eq!(list.row(50), Some(&member(50)));
        assert_eq!(list.row(79), Some(&member(79)));
        assert_eq!(list.row(80), None);
        assert_eq!(list.row(199), None);
        assert_eq!(list.row(200), Some(&member(200)));
        assert_eq!(list.row(310), Some(&member(310)));
        assert_eq!(list.row(311), None);
        assert_eq!(list.row(1_000), None);
        let capacity: usize = list
            .segments
            .iter()
            .map(|segment| segment.rows.capacity())
            .sum();
        assert!(capacity <= list.known_rows() * 2);
        assert_eq!(list.bytes(), list.recount());

        list.retain_ranges(&[]);
        assert_eq!(list.known_rows(), 0);
        assert_eq!(list.segments.capacity(), 0);
        assert_eq!(list.bytes(), list.recount());
    }
}
