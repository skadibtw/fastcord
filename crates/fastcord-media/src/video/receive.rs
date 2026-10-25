//! Receiving video: ordering, loss repair, frame completion, and the
//! statistics receiver reports carry (SPEC §8.3).
//!
//! One [`ReceiveBuffer`] follows one sender's SSRC in the extended sequence
//! space. It hands out frames strictly in order and only when every packet of
//! the frame has arrived: a frame is the contiguous run ending at a packet with
//! the marker bit (or just before a timestamp change). Anything missing is
//! asked for with NACKs. A frame that cannot be completed within
//! [`FRAME_DEADLINE`] is given up on together with everything up to its end, and
//! the buffer reports itself [`broken`](ReceiveBuffer::take_broken): the caller
//! must wait for a keyframe instead of feeding a decoder damaged frames.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::time::Instant;

use super::{MAX_PACKETS, VideoError};
use crate::rtcp::ReportBlock;
use crate::rtp::{RtpHeader, SequenceTracker, VIDEO_CLOCK_HZ};

/// Payload bytes buffered while frames wait to complete: two of the largest
/// frames.
pub(crate) const RECEIVE_BYTES: usize = 4 << 20;
/// How long a frame may stay incomplete before it is abandoned. Two NACK
/// rounds fit at the round trips of Discord's voice servers; a longer wait
/// would only add latency to every loss.
pub(crate) const FRAME_DEADLINE: Duration = Duration::from_millis(200);
/// A gap this young is reordering, not loss: the first NACK waits for it.
pub(crate) const REORDER_WAIT: Duration = Duration::from_millis(10);
/// Times one lost packet is requested before the deadline decides.
const MAX_NACK_REQUESTS: u8 = 6;
/// Sequence numbers named by one NACK round.
pub(crate) const MAX_NACK_BATCH: usize = 64;
/// A jump this far from the highest sequence (or this far back) is a sender
/// restart or an outage, not loss to repair.
const MAX_GAP: u64 = 512;
/// The largest RTP payload accepted: an Ethernet MTU.
const MAX_PAYLOAD: usize = 1500;

struct Stored {
    timestamp: u32,
    marker: bool,
    payload: Vec<u8>,
}

struct Gap {
    detected: Instant,
    requested: Option<Instant>,
    requests: u8,
}

/// The payloads of one complete frame, in order.
pub(crate) struct Assembled {
    pub(crate) timestamp: u32,
    pub(crate) payloads: Vec<Vec<u8>>,
}

pub(crate) struct ReceiveBuffer {
    packets: BTreeMap<u64, Stored>,
    bytes: usize,
    sequences: SequenceTracker,
    /// The first sequence not yet handed out.
    next: Option<u64>,
    highest: u64,
    gaps: BTreeMap<u64, Gap>,
    /// When the buffer began waiting without delivering a frame.
    waiting_since: Option<Instant>,
    broken: bool,
}

impl ReceiveBuffer {
    pub(crate) fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            bytes: 0,
            sequences: SequenceTracker::default(),
            next: None,
            highest: 0,
            gaps: BTreeMap::new(),
            waiting_since: None,
            broken: false,
        }
    }

    /// Whether frames were abandoned since the last call. Cleared by the call.
    pub(crate) fn take_broken(&mut self) -> bool {
        std::mem::take(&mut self.broken)
    }

    #[cfg(test)]
    pub(crate) fn gap_count(&self) -> usize {
        self.gaps.len()
    }

    #[cfg(test)]
    pub(crate) fn buffered_bytes(&self) -> usize {
        self.bytes
    }

    /// Everything buffered is dropped and the stream restarts at `header`.
    fn restart(&mut self, now: Instant, header: &RtpHeader, payload: Vec<u8>) {
        self.packets.clear();
        self.bytes = 0;
        self.gaps.clear();
        self.sequences = SequenceTracker::default();
        let sequence = self.sequences.extend(header.sequence);
        self.next = Some(sequence);
        self.highest = sequence;
        self.waiting_since = None;
        self.broken = true;
        self.store(now, sequence, header, payload);
    }

    fn store(&mut self, now: Instant, sequence: u64, header: &RtpHeader, payload: Vec<u8>) {
        self.bytes += payload.len();
        self.packets.insert(
            sequence,
            Stored {
                timestamp: header.timestamp,
                marker: header.marker,
                payload,
            },
        );
        self.waiting_since.get_or_insert(now);
    }

    /// Adds one packet of the stream. Duplicates and packets from before the
    /// frontier are dropped silently. An owned payload is stored without a
    /// copy (the receive path hands over the buffer it decrypted into).
    pub(crate) fn push(
        &mut self,
        now: Instant,
        header: &RtpHeader,
        payload: impl Into<Vec<u8>>,
    ) -> Result<(), VideoError> {
        let payload = payload.into();
        if payload.is_empty() || payload.len() > MAX_PAYLOAD {
            return Err(VideoError::Malformed);
        }
        let sequence = self.sequences.extend(header.sequence);
        let Some(next) = self.next else {
            self.next = Some(sequence);
            self.highest = sequence;
            self.store(now, sequence, header, payload);
            return Ok(());
        };
        if sequence < next {
            if next - sequence > MAX_GAP {
                self.restart(now, header, payload);
            }
            return Ok(());
        }
        if sequence > self.highest + MAX_GAP {
            self.restart(now, header, payload);
            return Ok(());
        }
        if self.packets.contains_key(&sequence) {
            return Ok(());
        }
        // The window from the frontier is bounded as well as what it stores:
        // jumps just under MAX_GAP were each accepted and each added a gap per
        // missing number (about a million for 2048 stored packets), which
        // `next_wake` walked on every datagram. No frame spans more than
        // MAX_PACKETS, so neither may the window.
        if self.bytes + payload.len() > RECEIVE_BYTES
            || self.packets.len() >= MAX_PACKETS
            || sequence - next >= MAX_PACKETS as u64
        {
            self.restart(now, header, payload);
            return Err(VideoError::TooLarge);
        }
        for missing in self.highest + 1..sequence {
            self.gaps.entry(missing).or_insert(Gap {
                detected: now,
                requested: None,
                requests: 0,
            });
        }
        self.gaps.remove(&sequence);
        self.highest = self.highest.max(sequence);
        self.store(now, sequence, header, payload);
        Ok(())
    }

    /// The next frame, if every one of its packets is here. Frames come out in
    /// order, one per call.
    pub(crate) fn pop_frame(&mut self, now: Instant) -> Option<Assembled> {
        let next = self.next?;
        let mut timestamp = None;
        let mut end = None;
        for (expected, (&sequence, packet)) in (next..).zip(self.packets.range(next..)) {
            if sequence != expected {
                return None;
            }
            match timestamp {
                None => timestamp = Some(packet.timestamp),
                Some(first) if first != packet.timestamp => {
                    // The next frame began without a marker closing this one.
                    end = Some(sequence - 1);
                    break;
                }
                Some(_) => {}
            }
            if packet.marker {
                end = Some(sequence);
                break;
            }
        }
        let end = end?;
        let mut frame = self.packets.split_off(&(end + 1));
        std::mem::swap(&mut self.packets, &mut frame);
        self.gaps = self.gaps.split_off(&(end + 1));
        self.next = Some(end + 1);
        self.waiting_since = (!self.packets.is_empty()).then_some(now);
        let payloads: Vec<Vec<u8>> = frame.into_values().map(|stored| stored.payload).collect();
        self.bytes -= payloads.iter().map(Vec::len).sum::<usize>();
        Some(Assembled {
            timestamp: timestamp.unwrap_or_default(),
            payloads,
        })
    }

    /// Abandons the frame at the frontier if it has waited past
    /// [`FRAME_DEADLINE`]: everything up to the first marker is dropped, and
    /// the next frame starts after it. Returns whether anything was given up.
    pub(crate) fn expire(&mut self, now: Instant) -> bool {
        let Some(since) = self.waiting_since else {
            return false;
        };
        if now.saturating_duration_since(since) < FRAME_DEADLINE {
            return false;
        }
        let Some(next) = self.next else {
            return false;
        };
        let target = self
            .packets
            .range(next..)
            .find(|(_, packet)| packet.marker)
            .map_or(self.highest, |(&sequence, _)| sequence);
        let rest = self.packets.split_off(&(target + 1));
        self.packets = rest;
        self.bytes = self
            .packets
            .values()
            .map(|stored| stored.payload.len())
            .sum();
        self.gaps = self.gaps.split_off(&(target + 1));
        self.next = Some(target + 1);
        self.waiting_since = (!self.packets.is_empty()).then_some(now);
        self.broken = true;
        true
    }

    /// Sequence numbers to request now. Each is asked for once its gap is
    /// older than [`REORDER_WAIT`] and again every `retry`, at most
    /// [`MAX_NACK_REQUESTS`] times; the frame deadline ends the rest.
    pub(crate) fn nacks(&mut self, now: Instant, retry: Duration) -> Vec<u16> {
        let mut due = Vec::new();
        for (&sequence, gap) in &mut self.gaps {
            if gap.requests >= MAX_NACK_REQUESTS || gap_due(gap, retry) > now {
                continue;
            }
            gap.requested = Some(now);
            gap.requests += 1;
            due.push(sequence as u16);
            if due.len() == MAX_NACK_BATCH {
                break;
            }
        }
        due
    }

    /// The next time [`nacks`](Self::nacks) or [`expire`](Self::expire) has work.
    pub(crate) fn next_wake(&self, retry: Duration) -> Option<Instant> {
        let nack = self
            .gaps
            .values()
            .filter(|gap| gap.requests < MAX_NACK_REQUESTS)
            .map(|gap| gap_due(gap, retry))
            .min();
        let deadline = self.waiting_since.map(|since| since + FRAME_DEADLINE);
        [nack, deadline].into_iter().flatten().min()
    }
}

fn gap_due(gap: &Gap, retry: Duration) -> Instant {
    gap.requested
        .map_or(gap.detected + REORDER_WAIT, |last| last + retry)
}

/// RFC 3550 reception statistics of one SSRC: what a receiver report says
/// about it.
pub(crate) struct ReceptionStats {
    epoch: Instant,
    sequences: SequenceTracker,
    base: Option<u64>,
    highest: u64,
    received: u64,
    expected_prior: u64,
    received_prior: u64,
    /// Interarrival jitter, scaled by 16 (RFC 3550 A.8).
    jitter: u64,
    transit: Option<u32>,
    /// The middle 32 NTP bits of the sender's latest report and when it came.
    last_sender_report: Option<(u32, Instant)>,
}

impl ReceptionStats {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            epoch: now,
            sequences: SequenceTracker::default(),
            base: None,
            highest: 0,
            received: 0,
            expected_prior: 0,
            received_prior: 0,
            jitter: 0,
            transit: None,
            last_sender_report: None,
        }
    }

    /// Counts one received packet of the stream.
    pub(crate) fn note_packet(&mut self, now: Instant, sequence: u16, timestamp: u32) {
        let extended = self.sequences.extend(sequence);
        self.base.get_or_insert(extended);
        self.highest = self.highest.max(extended);
        self.received += 1;
        let arrival = (now.saturating_duration_since(self.epoch).as_nanos()
            * u128::from(VIDEO_CLOCK_HZ)
            / 1_000_000_000) as u32;
        let transit = arrival.wrapping_sub(timestamp);
        if let Some(previous) = self.transit {
            let difference = transit.wrapping_sub(previous) as i32;
            let magnitude = u64::from(difference.unsigned_abs());
            self.jitter = (self.jitter + magnitude).saturating_sub((self.jitter + 8) >> 4);
        }
        self.transit = Some(transit);
    }

    /// The sender's report: its NTP time (middle 32 bits) is echoed back so
    /// the sender can measure the round trip.
    pub(crate) fn note_sender_report(&mut self, now: Instant, ntp_middle: u32) {
        self.last_sender_report = Some((ntp_middle, now));
    }

    /// A packet recovered through retransmission counts as received (the loss
    /// is repaired), but its late arrival says nothing about jitter.
    pub(crate) fn note_recovered(&mut self) {
        self.received += 1;
    }

    /// The report block for `ssrc` since the previous one.
    pub(crate) fn block(&mut self, now: Instant, ssrc: u32) -> Option<ReportBlock> {
        let base = self.base?;
        let expected = self.highest - base + 1;
        let expected_interval = expected - self.expected_prior;
        let received_interval = self.received - self.received_prior;
        self.expected_prior = expected;
        self.received_prior = self.received;
        let lost_interval = expected_interval as i64 - received_interval as i64;
        let fraction_lost = if expected_interval == 0 || lost_interval <= 0 {
            0
        } else {
            ((lost_interval << 8) / expected_interval as i64).min(255) as u8
        };
        let (last_sender_report, delay) = self.last_sender_report.map_or((0, 0), |(ntp, at)| {
            let elapsed = now.saturating_duration_since(at).as_nanos();
            (ntp, (elapsed * 65_536 / 1_000_000_000) as u32)
        });
        Some(ReportBlock {
            ssrc,
            fraction_lost,
            cumulative_lost: (expected as i64 - self.received as i64).clamp(-0x80_0000, 0x7F_FFFF)
                as i32,
            // The tracker starts one cycle in; the report counts from zero.
            highest_sequence: (self.highest - (1 << 16)) as u32,
            jitter: (self.jitter >> 4) as u32,
            last_sender_report,
            delay_since_last_sender_report: delay,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(sequence: u16, timestamp: u32, marker: bool) -> RtpHeader {
        RtpHeader {
            marker,
            ..RtpHeader::new(103, sequence, timestamp, 7)
        }
    }

    fn push(buffer: &mut ReceiveBuffer, now: Instant, sequence: u16, timestamp: u32, marker: bool) {
        buffer
            .push(
                now,
                &header(sequence, timestamp, marker),
                [sequence as u8, 1, 2],
            )
            .unwrap();
    }

    fn payload_sequences(frame: &Assembled) -> Vec<u8> {
        frame.payloads.iter().map(|payload| payload[0]).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn an_in_order_frame_is_delivered_when_its_marker_arrives() {
        let now = Instant::now();
        let mut buffer = ReceiveBuffer::new();
        push(&mut buffer, now, 10, 3000, false);
        push(&mut buffer, now, 11, 3000, false);
        assert!(buffer.pop_frame(now).is_none());
        push(&mut buffer, now, 12, 3000, true);
        let frame = buffer.pop_frame(now).unwrap();
        assert_eq!(frame.timestamp, 3000);
        assert_eq!(payload_sequences(&frame), [10, 11, 12]);
        assert!(buffer.pop_frame(now).is_none());
        assert_eq!(buffer.buffered_bytes(), 0);
        assert!(!buffer.take_broken());
        assert_eq!(buffer.next_wake(Duration::from_millis(50)), None);
    }

    #[tokio::test(start_paused = true)]
    async fn reordered_packets_assemble_in_sequence_order_across_the_wrap() {
        let now = Instant::now();
        let mut buffer = ReceiveBuffer::new();
        // The frame spans 65534, 65535, 0, 1; 65535 arrives last.
        for (sequence, marker) in [(65_534, false), (0, false), (1, true)] {
            push(&mut buffer, now, sequence, 3000, marker);
        }
        assert!(buffer.pop_frame(now).is_none());
        // The hole at 65535 is a gap that will be asked for.
        assert_eq!(
            buffer.nacks(now + REORDER_WAIT, Duration::from_millis(50)),
            [65_535]
        );
        push(&mut buffer, now, 65_535, 3000, false);
        let frame = buffer.pop_frame(now).unwrap();
        assert_eq!(payload_sequences(&frame), [254, 255, 0, 1]);
    }

    #[tokio::test(start_paused = true)]
    async fn nacks_wait_for_reordering_then_retry_at_the_interval_and_stop() {
        let now = Instant::now();
        let retry = Duration::from_millis(40);
        let mut buffer = ReceiveBuffer::new();
        push(&mut buffer, now, 1, 100, false);
        push(&mut buffer, now, 4, 100, false);
        assert_eq!(buffer.next_wake(retry), Some(now + REORDER_WAIT));
        assert!(
            buffer
                .nacks(now + Duration::from_millis(9), retry)
                .is_empty()
        );
        assert_eq!(buffer.nacks(now + REORDER_WAIT, retry), [2, 3]);
        // Asked again only after the retry interval.
        assert!(
            buffer
                .nacks(now + Duration::from_millis(49), retry)
                .is_empty()
        );
        assert_eq!(buffer.nacks(now + Duration::from_millis(50), retry), [2, 3]);
        // One of them arrives: only the other is still requested.
        push(&mut buffer, now, 2, 100, false);
        assert_eq!(buffer.nacks(now + Duration::from_millis(90), retry), [3]);
        // At most six requests per packet.
        let mut at = now + Duration::from_millis(90);
        // Packet 3 was requested at 10, 50, and 90 ms; the rest of its six.
        let mut requests = 3;
        for _ in 0..20 {
            at += retry;
            requests += buffer.nacks(at, retry).len();
        }
        assert_eq!(requests, usize::from(MAX_NACK_REQUESTS));
        // Once the requests are used up only the deadline keeps the buffer's wake.
        assert_eq!(
            buffer.next_wake(retry),
            Some(buffer.waiting_since.unwrap() + FRAME_DEADLINE)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_repaired_loss_reconstructs_the_frame_exactly() {
        let now = Instant::now();
        let mut buffer = ReceiveBuffer::new();
        push(&mut buffer, now, 20, 6000, false);
        push(&mut buffer, now, 22, 6000, false);
        push(&mut buffer, now, 23, 6000, true);
        assert!(buffer.pop_frame(now).is_none());
        // The retransmission arrives (as the original sequence, after RTX
        // unwrapping) well before the deadline.
        push(
            &mut buffer,
            now + Duration::from_millis(80),
            21,
            6000,
            false,
        );
        let frame = buffer.pop_frame(now + Duration::from_millis(80)).unwrap();
        assert_eq!(payload_sequences(&frame), [20, 21, 22, 23]);
        assert!(!buffer.take_broken());
        assert!(!buffer.expire(now + FRAME_DEADLINE * 2));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unrepaired_frame_is_abandoned_and_the_buffer_reports_it_broken() {
        let now = Instant::now();
        let mut buffer = ReceiveBuffer::new();
        // Frame A (30..=32) lost its middle packet; frame B (33, 34) is whole.
        push(&mut buffer, now, 30, 1000, false);
        push(&mut buffer, now, 32, 1000, true);
        push(&mut buffer, now, 33, 2000, false);
        push(&mut buffer, now, 34, 2000, true);
        assert!(buffer.pop_frame(now).is_none());
        assert!(!buffer.expire(now + FRAME_DEADLINE - Duration::from_millis(1)));
        assert!(!buffer.take_broken());
        assert!(buffer.expire(now + FRAME_DEADLINE));
        assert!(buffer.take_broken());
        assert!(!buffer.take_broken());
        // Frame B is complete and follows frame A; it is still delivered, and
        // it was broken by the loss before it.
        let frame = buffer.pop_frame(now + FRAME_DEADLINE).unwrap();
        assert_eq!(
            (frame.timestamp, payload_sequences(&frame)),
            (2000, vec![33, 34])
        );
        assert_eq!(buffer.buffered_bytes(), 0);
        // The abandoned packet arriving now is late and ignored.
        push(&mut buffer, now + FRAME_DEADLINE, 31, 1000, false);
        assert!(buffer.pop_frame(now + FRAME_DEADLINE).is_none());
        assert!(
            buffer
                .nacks(now + FRAME_DEADLINE * 2, Duration::from_millis(30))
                .is_empty()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_without_its_marker_ends_before_the_next_timestamp() {
        let now = Instant::now();
        let mut buffer = ReceiveBuffer::new();
        push(&mut buffer, now, 5, 1000, false);
        push(&mut buffer, now, 6, 1000, false);
        assert!(buffer.pop_frame(now).is_none());
        push(&mut buffer, now, 7, 4000, false);
        let frame = buffer.pop_frame(now).unwrap();
        assert_eq!(payload_sequences(&frame), [5, 6]);
        push(&mut buffer, now, 8, 4000, true);
        assert_eq!(payload_sequences(&buffer.pop_frame(now).unwrap()), [7, 8]);
    }

    #[tokio::test(start_paused = true)]
    async fn duplicates_and_late_packets_change_nothing() {
        let now = Instant::now();
        let mut buffer = ReceiveBuffer::new();
        push(&mut buffer, now, 40, 100, true);
        push(&mut buffer, now, 40, 100, true);
        assert_eq!(buffer.buffered_bytes(), 3);
        assert!(buffer.pop_frame(now).is_some());
        push(&mut buffer, now, 40, 100, true);
        push(&mut buffer, now, 39, 90, true);
        assert!(buffer.pop_frame(now).is_none());
        assert_eq!(buffer.buffered_bytes(), 0);
        assert!(buffer.next_wake(Duration::from_millis(30)).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_sender_restart_or_a_huge_gap_restarts_without_unbounded_state() {
        let now = Instant::now();
        let mut buffer = ReceiveBuffer::new();
        push(&mut buffer, now, 100, 100, false);
        // Beyond the gap bound: everything buffered is dropped, no NACK storm.
        push(&mut buffer, now, 100 + 600, 200, false);
        assert!(buffer.take_broken());
        assert_eq!(buffer.buffered_bytes(), 3);
        assert!(
            buffer
                .nacks(now + FRAME_DEADLINE, Duration::from_millis(30))
                .is_empty()
        );
        // A packet far behind the frontier is a restarted sender, too.
        push(&mut buffer, now, 100, 300, true);
        assert!(buffer.take_broken());
        assert_eq!(payload_sequences(&buffer.pop_frame(now).unwrap()), [100]);
    }

    /// Jumps just under the per-jump bound cannot pile up gaps: the window
    /// from the frontier is bounded, so the gap table is too.
    #[tokio::test(start_paused = true)]
    async fn repeated_large_jumps_keep_the_gap_table_bounded() {
        let now = Instant::now();
        let mut buffer = ReceiveBuffer::new();
        let mut sequence = 0u32;
        for _ in 0..200 {
            let _ = buffer.push(now, &header(sequence as u16, 100, false), [1u8]);
            sequence += 511;
            assert!(buffer.gap_count() < MAX_PACKETS, "{}", buffer.gap_count());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_buffer_is_bounded_in_packets_and_bytes() {
        let now = Instant::now();
        let mut buffer = ReceiveBuffer::new();
        // Packets of one endless frame, never closed by a marker.
        let mut refused = 0;
        for sequence in 0..(MAX_PACKETS as u32 + 10) {
            let result = buffer.push(now, &header(sequence as u16, 100, false), [0x41; 1500]);
            if result.is_err() {
                refused += 1;
            }
            assert!(buffer.buffered_bytes() <= RECEIVE_BYTES);
        }
        assert!(refused >= 1);
        assert!(buffer.take_broken());
        // Invalid payloads are refused before any state changes.
        let mut fresh = ReceiveBuffer::new();
        assert_eq!(
            fresh.push(now, &header(1, 1, true), []),
            Err(VideoError::Malformed)
        );
        assert_eq!(
            fresh.push(now, &header(1, 1, true), vec![0; MAX_PAYLOAD + 1]),
            Err(VideoError::Malformed)
        );
        assert_eq!(fresh.buffered_bytes(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn nack_batches_are_capped() {
        let now = Instant::now();
        let mut buffer = ReceiveBuffer::new();
        push(&mut buffer, now, 0, 100, false);
        push(&mut buffer, now, 200, 100, false);
        let first = buffer.nacks(now + REORDER_WAIT, Duration::from_millis(30));
        assert_eq!(first.len(), MAX_NACK_BATCH);
        assert_eq!(first[0], 1);
        // The next round continues with the packets not yet requested.
        let second = buffer.nacks(now + REORDER_WAIT, Duration::from_millis(30));
        assert_eq!(second[0], 65);
    }

    #[tokio::test(start_paused = true)]
    async fn reception_statistics_report_loss_jitter_and_the_sender_clock() {
        let now = Instant::now();
        let mut stats = ReceptionStats::new(now);
        assert!(stats.block(now, 7).is_none());
        // 10 packets expected, 8 received (21 and 24 never arrive), 33 ms apart
        // exactly at the 3,000-tick RTP spacing: no jitter.
        for index in 0..10u16 {
            if index == 1 || index == 4 {
                continue;
            }
            let at = now + Duration::from_nanos(u64::from(index) * 100_000_000 / 3);
            stats.note_packet(at, 20 + index, u32::from(index) * 3000);
        }
        let later = now + Duration::from_secs(1);
        stats.note_sender_report(later, 0x1234_5678);
        let block = stats.block(later + Duration::from_millis(500), 7).unwrap();
        assert_eq!(block.ssrc, 7);
        assert_eq!(block.cumulative_lost, 2);
        // 2 of 10 lost: 51/256.
        assert_eq!(block.fraction_lost, 51);
        assert_eq!(block.highest_sequence, 29);
        assert_eq!(block.last_sender_report, 0x1234_5678);
        // Half a second in 1/65536 s units.
        assert_eq!(block.delay_since_last_sender_report, 32_768);
        assert!(block.jitter <= 1, "{}", block.jitter);
        // The next report covers only what arrived since: nothing lost.
        stats.note_packet(later, 30, 90_000);
        let next = stats.block(later, 7).unwrap();
        assert_eq!(next.fraction_lost, 0);
        assert_eq!(next.cumulative_lost, 2);
        assert_eq!(next.highest_sequence, 30);
    }

    #[tokio::test(start_paused = true)]
    async fn jitter_grows_with_irregular_arrival_and_the_highest_sequence_counts_cycles() {
        let now = Instant::now();
        let mut stats = ReceptionStats::new(now);
        for index in 0..40u16 {
            // Alternately 20 ms early and late against a steady timestamp.
            let skew = if index % 2 == 0 { 0 } else { 20 };
            let at = now + Duration::from_millis(u64::from(index) * 33 + skew);
            stats.note_packet(at, 65_520u16.wrapping_add(index), u32::from(index) * 2970);
        }
        let block = stats.block(now + Duration::from_secs(2), 7).unwrap();
        assert!(block.jitter > 100, "{}", block.jitter);
        // 65_520 + 39 wrapped once: cycle 1, sequence 23.
        assert_eq!(block.highest_sequence, (1 << 16) | 23);
        assert_eq!(block.cumulative_lost, 0);
    }
}
