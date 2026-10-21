//! Sending video: RTP packets, bounded pacing, retransmission, and the
//! congestion defaults of SPEC §8.3.
//!
//! A frame arrives already sealed by DAVE. It is packetized once; the payload
//! bytes stay in one shared allocation that the pacer queue and the repair
//! ring both reference, so a retransmission copies nothing until it is written
//! into a datagram. Sequence numbers are assigned only after the pacer accepts
//! a whole frame, so a refused frame never leaves a gap in the RTP stream.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::h264::{self, Part};
use super::{CodecOffer, MAX_PACKETS, MAX_PAYLOAD_BYTES, SendOptions, VideoError};
use crate::rtp::VIDEO_CLOCK_HZ;

/// Normal-frame queue budget. An empty queue may admit a keyframe using
/// KEYFRAME_WINDOW so a normal desktop IDR cannot permanently block recovery.
pub(crate) const PACER_WINDOW: Duration = Duration::from_millis(100);
/// One recovery/keyframe may use extra room, still below the receiver's
/// 200 ms frame deadline. Rate and burst credit do not increase.
const KEYFRAME_WINDOW: Duration = Duration::from_millis(180);
/// The pacer drains at 2.5 times the target bitrate (the pacing multiplier
/// libwebrtc defaults to), so a keyframe leaves in tens of milliseconds and
/// ordinary frames leave in tens of milliseconds. Keyframes get separate,
/// bounded headroom without changing this rate.
const PACING_PERCENT: u64 = 250;
/// Credit an idle or late pacer may spend at once. A coarse OS timer can wake
/// the task this late without lowering the average rate.
const PACER_BURST: Duration = Duration::from_millis(20);
/// How long a sent packet stays available for retransmission.
pub(crate) const REPAIR_WINDOW: Duration = Duration::from_millis(500);
/// The retransmission ring's byte bound.
pub(crate) const REPAIR_BYTES: usize = 2 << 20;
pub(crate) const MIN_BITRATE: u32 = 64_000;
/// How long loss must stay low before the bitrate rises again.
const STABLE_INTERVAL: Duration = Duration::from_secs(5);
/// Keyframe requests closer together than this are one request.
const KEYFRAME_COALESCE: Duration = Duration::from_millis(300);
/// RTP header (12), extension preamble (4), AEAD tag (16), nonce suffix (4).
const PACKET_OVERHEAD: usize = 36;
/// Extension elements of a primary packet: send time, playout delay, content
/// type, and stream ID, each padded to a word.
pub(crate) const PRIMARY_EXTENSION_BYTES: usize = 16;
/// Extension elements of an RTX packet: send time and repaired stream ID.
pub(crate) const RTX_EXTENSION_BYTES: usize = 8;
/// Fraction lost above which the sender reduces its rate: 13/256, over 5%.
const LOSS_REDUCE: u8 = 12;
/// Greatest fraction lost (in 1/256) under which the bitrate may rise: 5/256 is
/// 1.95%, so "below 2%".
const LOSS_RAISE: u8 = 5;
/// The least time between two rate cuts.
const REDUCE_COOLDOWN: Duration = Duration::from_secs(1);
/// Round-trip growth over the running minimum that counts as congestion.
const RTT_GROWTH: Duration = Duration::from_millis(100);

/// One RTP packet of an outgoing frame, or a retransmission of one.
#[derive(Clone, Debug)]
pub(crate) struct Packet {
    frame: Arc<[u8]>,
    part: Part,
    pub(crate) sequence: u16,
    pub(crate) timestamp: u32,
    /// For an RTX packet, the sequence number it repeats; `sequence` is then
    /// the RTX stream's own.
    pub(crate) original: Option<u16>,
}

impl Packet {
    pub(crate) fn marker(&self) -> bool {
        self.part.marker
    }

    /// RTP payload bytes: the codec payload plus, for RTX, the two-byte
    /// original sequence number (RFC 4588 §4).
    pub(crate) fn payload_len(&self) -> usize {
        self.part.len() + 2 * usize::from(self.original.is_some())
    }

    /// Bytes this packet occupies on the wire.
    fn wire_len(&self) -> usize {
        let extension = if self.original.is_some() {
            RTX_EXTENSION_BYTES
        } else {
            PRIMARY_EXTENSION_BYTES
        };
        PACKET_OVERHEAD + extension + self.payload_len()
    }

    /// Appends the RTP payload to `out`.
    pub(crate) fn write_payload(&self, out: &mut Vec<u8>) {
        if let Some(original) = self.original {
            out.extend_from_slice(&original.to_be_bytes());
        }
        self.part.write(&self.frame, out);
    }
}

/// Spreads packets over time at 2.5 times the target bitrate and refuses what
/// cannot leave within [`PACER_WINDOW`] at that rate.
pub(crate) struct Pacer {
    queue: VecDeque<(Instant, Packet)>,
    bytes: usize,
    /// Pacing rate in bits per second.
    rate: u64,
    /// The virtual clock the next packet may leave at.
    ready_at: Option<Instant>,
    /// Retransmissions at the front of the queue, in the order requested.
    repairs: usize,
    window: Duration,
}

impl Pacer {
    pub(crate) fn new(bitrate: u32) -> Self {
        Self {
            queue: VecDeque::new(),
            bytes: 0,
            rate: pacing_rate(bitrate),
            ready_at: None,
            repairs: 0,
            window: PACER_WINDOW,
        }
    }

    pub(crate) fn set_bitrate(&mut self, bitrate: u32) {
        self.rate = pacing_rate(bitrate);
    }

    /// What the window holds at the current rate.
    fn capacity(&self) -> usize {
        (self.rate / 8 * self.window.as_millis() as u64 / 1000) as usize
    }

    fn fits(&self, now: Instant, bytes: usize, packets: usize) -> bool {
        // A queue whose oldest packet is already past the window is behind
        // schedule: new work only makes the lateness worse.
        let stale = self
            .queue
            .front()
            .is_some_and(|(queued, _)| now.saturating_duration_since(*queued) > self.window);
        !stale && self.bytes + bytes <= self.capacity() && self.queue.len() + packets <= MAX_PACKETS
    }

    fn fits_keyframe(&self, bytes: usize, packets: usize) -> bool {
        self.queue.is_empty()
            && packets <= MAX_PACKETS
            && transmit_time(bytes, self.rate) <= KEYFRAME_WINDOW
    }

    /// Queues a whole frame, or none of it. A large keyframe may extend an
    /// empty queue's time budget; it never accelerates the network pacer.
    fn push_frame(
        &mut self,
        now: Instant,
        packets: Vec<Packet>,
        keyframe: bool,
    ) -> Result<(), VideoError> {
        let bytes = packets.iter().map(Packet::wire_len).sum();
        if !self.fits(now, bytes, packets.len()) {
            if !keyframe || !self.fits_keyframe(bytes, packets.len()) {
                return Err(VideoError::QueueFull);
            }
            self.window = KEYFRAME_WINDOW;
        }
        self.bytes += bytes;
        self.queue
            .extend(packets.into_iter().map(|packet| (now, packet)));
        Ok(())
    }

    /// Queues a retransmission ahead of new media.
    fn push_repair(&mut self, now: Instant, packet: Packet) -> Result<(), VideoError> {
        let bytes = packet.wire_len();
        if !self.fits(now, bytes, 1) {
            return Err(VideoError::QueueFull);
        }
        self.bytes += bytes;
        self.queue.insert(self.repairs, (now, packet));
        self.repairs += 1;
        Ok(())
    }

    /// The next packet if the pacing clock allows it at `now`.
    pub(crate) fn pop(&mut self, now: Instant) -> Option<Packet> {
        if self.ready_at.is_some_and(|ready| ready > now) {
            return None;
        }
        let (_, packet) = self.queue.pop_front()?;
        self.repairs = self.repairs.saturating_sub(1);
        let bytes = packet.wire_len();
        self.bytes -= bytes;
        if self.queue.is_empty() {
            self.window = PACER_WINDOW;
        }
        let earliest = now.checked_sub(PACER_BURST).unwrap_or(now);
        let base = self.ready_at.map_or(now, |ready| ready.max(earliest));
        self.ready_at = Some(base + transmit_time(bytes, self.rate));
        Some(packet)
    }

    /// When the pacer next has a packet it may send, if it has any.
    pub(crate) fn next_wake(&self) -> Option<Instant> {
        let (queued, _) = self.queue.front()?;
        Some(self.ready_at.map_or(*queued, |ready| ready.max(*queued)))
    }

    #[cfg(test)]
    pub(crate) fn queued_bytes(&self) -> usize {
        self.bytes
    }
}

fn pacing_rate(bitrate: u32) -> u64 {
    u64::from(bitrate.max(MIN_BITRATE)) * PACING_PERCENT / 100
}

fn transmit_time(bytes: usize, rate: u64) -> Duration {
    Duration::from_nanos((bytes as u64 * 8_000_000_000).div_ceil(rate.max(1)))
}

/// Recently sent primary packets, for answering NACKs, bounded by age and
/// bytes. Eviction takes a whole frame at a time, so the memory the ring
/// holds is exactly what it accounts for.
pub(crate) struct RepairRing {
    packets: VecDeque<(Instant, Packet)>,
    bytes: usize,
}

impl RepairRing {
    pub(crate) fn new() -> Self {
        Self {
            packets: VecDeque::new(),
            bytes: 0,
        }
    }

    fn evict_frame(&mut self) {
        let Some((_, first)) = self.packets.pop_front() else {
            return;
        };
        self.bytes -= first.part.len();
        while let Some((_, next)) = self.packets.front() {
            if !Arc::ptr_eq(&first.frame, &next.frame) {
                break;
            }
            self.bytes -= next.part.len();
            self.packets.pop_front();
        }
    }

    fn expire(&mut self, now: Instant) {
        while self
            .packets
            .front()
            .is_some_and(|(sent, _)| now.saturating_duration_since(*sent) > REPAIR_WINDOW)
        {
            self.evict_frame();
        }
    }

    pub(crate) fn remember(&mut self, now: Instant, packet: Packet) {
        self.expire(now);
        let bytes = packet.part.len();
        while self.bytes + bytes > REPAIR_BYTES && !self.packets.is_empty() {
            self.evict_frame();
        }
        self.bytes += bytes;
        self.packets.push_back((now, packet));
    }

    /// The packet sent as `sequence`, if it is still within the window.
    pub(crate) fn get(&mut self, now: Instant, sequence: u16) -> Option<Packet> {
        self.expire(now);
        self.packets
            .iter()
            .rev()
            .find(|(_, packet)| packet.sequence == sequence)
            .map(|(_, packet)| packet.clone())
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
}

/// The conservative defaults of SPEC §8.3, not Discord's proprietary
/// controller: cut 20% on over 5% loss, a congested pacer, or a round trip
/// 100 ms over the running minimum; honor a lower server bitrate at once;
/// raise at most 5% per stable interval after several seconds under 2% loss.
/// Cuts are at least [`REDUCE_COOLDOWN`] apart, so a pacer that stays full for
/// several frames costs one cut, not a collapse to the floor.
pub(crate) struct Congestion {
    /// The initial and highest bitrate.
    ceiling: u32,
    bitrate: u32,
    /// The lowest bitrate the server or receiver asked for.
    limit: u32,
    min_rtt: Option<Duration>,
    stable_since: Instant,
    last_reduction: Option<Instant>,
}

impl Congestion {
    pub(crate) fn new(bitrate: u32, now: Instant) -> Self {
        Self {
            ceiling: bitrate,
            bitrate,
            limit: bitrate,
            min_rtt: None,
            stable_since: now,
            last_reduction: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn bitrate(&self) -> u32 {
        self.bitrate
    }

    fn change(&mut self, bitrate: u32) -> Option<u32> {
        let bitrate = bitrate.clamp(MIN_BITRATE, self.limit.min(self.ceiling).max(MIN_BITRATE));
        (bitrate != self.bitrate).then(|| {
            self.bitrate = bitrate;
            bitrate
        })
    }

    /// Cuts the rate by 20%, as for loss or a pacer over its bound.
    pub(crate) fn reduce(&mut self, now: Instant) -> Option<u32> {
        self.stable_since = now;
        if self
            .last_reduction
            .is_some_and(|at| now.saturating_duration_since(at) < REDUCE_COOLDOWN)
        {
            return None;
        }
        self.last_reduction = Some(now);
        self.change(self.bitrate / 5 * 4)
    }

    /// A server or receiver bitrate request: applied at once when it is lower
    /// than the current rate, and remembered as the highest rate to return to.
    pub(crate) fn limit(&mut self, bits_per_second: u64, now: Instant) -> Option<u32> {
        self.limit = u32::try_from(bits_per_second)
            .unwrap_or(u32::MAX)
            .clamp(MIN_BITRATE, self.ceiling);
        self.stable_since = now;
        self.change(self.bitrate.min(self.limit))
    }

    /// A reception report about our stream: its fraction lost in 1/256 and,
    /// when measurable, the round trip.
    pub(crate) fn report(
        &mut self,
        fraction_lost: u8,
        rtt: Option<Duration>,
        now: Instant,
    ) -> Option<u32> {
        let excessive_rtt = rtt.is_some_and(|rtt| {
            let minimum = self.min_rtt.map_or(rtt, |minimum| minimum.min(rtt));
            self.min_rtt = Some(minimum);
            rtt > minimum + RTT_GROWTH
        });
        if fraction_lost > LOSS_REDUCE || excessive_rtt {
            self.reduce(now)
        } else if fraction_lost <= LOSS_RAISE {
            if now.saturating_duration_since(self.stable_since) >= STABLE_INTERVAL {
                self.stable_since = now;
                self.change(self.bitrate + self.bitrate / 20)
            } else {
                None
            }
        } else {
            self.stable_since = now;
            None
        }
    }
}

/// Everything one connection needs to send a screen layer.
pub(crate) struct VideoSender {
    pub(crate) offer: CodecOffer,
    pub(crate) ssrc: u32,
    pub(crate) rtx_ssrc: u32,
    pub(crate) options: SendOptions,
    sequence: u16,
    rtx_sequence: u16,
    rtp_base: u32,
    origin: Option<Duration>,
    last_timestamp: Option<u32>,
    pacer: Pacer,
    ring: RepairRing,
    pub(crate) congestion: Congestion,
    /// Sender report counters: primary and RTX packets and payload octets.
    pub(crate) packets: u32,
    pub(crate) octets: u32,
    pub(crate) rtx_packets: u32,
    pub(crate) rtx_octets: u32,
    /// The packet counts at the last report, to skip idle streams.
    pub(crate) reported: (u32, u32),
    /// When the latest frame was queued and the RTP timestamp it carries.
    clock: Option<(Instant, u32)>,
    keyframe_requested: Option<Instant>,
}

impl VideoSender {
    pub(crate) fn new(
        offer: CodecOffer,
        ssrc: u32,
        rtx_ssrc: u32,
        options: SendOptions,
        start: (u16, u32, u16),
        now: Instant,
    ) -> Self {
        let (sequence, rtp_base, rtx_sequence) = start;
        Self {
            offer,
            ssrc,
            rtx_ssrc,
            options,
            sequence,
            rtx_sequence,
            rtp_base,
            origin: None,
            last_timestamp: None,
            pacer: Pacer::new(options.bitrate),
            ring: RepairRing::new(),
            congestion: Congestion::new(options.bitrate, now),
            packets: 0,
            octets: 0,
            rtx_packets: 0,
            rtx_octets: 0,
            reported: (0, 0),
            clock: None,
            keyframe_requested: None,
        }
    }

    /// A cheap early test, before DAVE spends time sealing a frame the pacer
    /// would refuse. The final test is [`enqueue`](Self::enqueue)'s.
    pub(crate) fn can_accept(&self, now: Instant, frame_bytes: usize) -> bool {
        let packets = frame_bytes / MAX_PAYLOAD_BYTES + 1;
        self.pacer.fits(
            now,
            frame_bytes + packets * (PACKET_OVERHEAD + PRIMARY_EXTENSION_BYTES),
            packets,
        )
    }

    pub(crate) fn can_accept_frame(
        &self,
        now: Instant,
        frame_bytes: usize,
        keyframe: bool,
    ) -> bool {
        let packets = frame_bytes / MAX_PAYLOAD_BYTES + 1;
        self.can_accept(now, frame_bytes)
            || (keyframe
                && self.pacer.fits_keyframe(
                    frame_bytes + packets * (PACKET_OVERHEAD + PRIMARY_EXTENSION_BYTES),
                    packets,
                ))
    }

    pub(crate) fn has_queued_packets(&self) -> bool {
        !self.pacer.queue.is_empty()
    }

    /// Packetizes a sealed frame and queues it for pacing. On any error the
    /// RTP sequence and timestamp state is untouched.
    #[cfg(test)]
    pub(crate) fn enqueue(
        &mut self,
        now: Instant,
        timestamp: Duration,
        sealed: Vec<u8>,
    ) -> Result<(), VideoError> {
        self.enqueue_frame(now, timestamp, sealed, false)
    }

    pub(crate) fn enqueue_frame(
        &mut self,
        now: Instant,
        timestamp: Duration,
        sealed: Vec<u8>,
        keyframe: bool,
    ) -> Result<(), VideoError> {
        let parts = h264::packetize(&sealed, MAX_PAYLOAD_BYTES)?;
        let frame: Arc<[u8]> = sealed.into();
        let rtp = self.rtp_timestamp(timestamp);
        let mut sequence = self.sequence;
        let packets: Vec<Packet> = parts
            .into_iter()
            .map(|part| {
                let packet = Packet {
                    frame: Arc::clone(&frame),
                    part,
                    sequence,
                    timestamp: rtp,
                    original: None,
                };
                sequence = sequence.wrapping_add(1);
                packet
            })
            .collect();
        self.pacer.push_frame(now, packets, keyframe)?;
        self.commit_timestamp(timestamp, rtp);
        self.sequence = sequence;
        self.clock = Some((now, rtp));
        Ok(())
    }

    /// The RTP timestamp a frame captured at `timestamp` would carry: the
    /// 90 kHz clock from the first frame, strictly increasing so two frames
    /// never share a timestamp. Nothing is recorded until
    /// [`commit_timestamp`](Self::commit_timestamp), so a refused frame leaves
    /// no trace.
    fn rtp_timestamp(&self, timestamp: Duration) -> u32 {
        let origin = self.origin.unwrap_or(timestamp);
        let elapsed = timestamp.saturating_sub(origin);
        let ticks = elapsed.as_nanos() * u128::from(VIDEO_CLOCK_HZ) / 1_000_000_000;
        let rtp = self.rtp_base.wrapping_add(ticks as u32);
        match self.last_timestamp {
            Some(last) if (rtp.wrapping_sub(last) as i32) <= 0 => last.wrapping_add(1),
            _ => rtp,
        }
    }

    fn commit_timestamp(&mut self, timestamp: Duration, rtp: u32) {
        self.origin.get_or_insert(timestamp);
        self.last_timestamp = Some(rtp);
    }

    /// Queues retransmissions for NACKed `sequences` that are still in the
    /// ring. Returns how many were queued; the rest are gone or do not fit.
    pub(crate) fn repair(&mut self, now: Instant, sequences: impl Iterator<Item = u16>) -> usize {
        let mut queued = 0;
        for sequence in sequences {
            let Some(original) = self.ring.get(now, sequence) else {
                continue;
            };
            let packet = Packet {
                original: Some(sequence),
                sequence: self.rtx_sequence,
                ..original
            };
            if self.pacer.push_repair(now, packet).is_err() {
                break;
            }
            self.rtx_sequence = self.rtx_sequence.wrapping_add(1);
            queued += 1;
        }
        queued
    }

    /// The next packet due at `now`. Primary packets are remembered for
    /// retransmission as they leave.
    pub(crate) fn next_packet(&mut self, now: Instant) -> Option<Packet> {
        let packet = self.pacer.pop(now)?;
        let octets = packet.payload_len() as u32;
        if packet.original.is_some() {
            self.rtx_packets = self.rtx_packets.wrapping_add(1);
            self.rtx_octets = self.rtx_octets.wrapping_add(octets);
        } else {
            self.packets = self.packets.wrapping_add(1);
            self.octets = self.octets.wrapping_add(octets);
            self.ring.remember(now, packet.clone());
        }
        Some(packet)
    }

    pub(crate) fn next_wake(&self) -> Option<Instant> {
        self.pacer.next_wake()
    }

    /// The RTP timestamp the stream has reached at `now`, for a sender
    /// report's wall-clock mapping.
    pub(crate) fn rtp_timestamp_at(&self, now: Instant) -> Option<u32> {
        let (queued, rtp) = self.clock?;
        let ticks = now.saturating_duration_since(queued).as_nanos() * u128::from(VIDEO_CLOCK_HZ)
            / 1_000_000_000;
        Some(rtp.wrapping_add(ticks as u32))
    }

    /// Applies a new target bitrate to pacing.
    pub(crate) fn apply_bitrate(&mut self, bitrate: u32) {
        self.pacer.set_bitrate(bitrate);
    }

    /// Whether a keyframe request should reach the encoder: requests within
    /// 300 ms of the previous one are one request.
    pub(crate) fn keyframe_wanted(&mut self, now: Instant) -> bool {
        if self
            .keyframe_requested
            .is_some_and(|at| now.saturating_duration_since(at) < KEYFRAME_COALESCE)
        {
            return false;
        }
        self.keyframe_requested = Some(now);
        true
    }

    #[cfg(test)]
    pub(crate) fn pacer_bytes(&self) -> usize {
        self.pacer.queued_bytes()
    }

    #[cfg(test)]
    pub(crate) fn ring_bytes(&self) -> usize {
        self.ring.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bytes: usize) -> Vec<u8> {
        let mut frame = vec![0, 0, 0, 1, 0x65];
        frame.resize(bytes, 0x57);
        frame
    }

    fn sender(now: Instant) -> VideoSender {
        VideoSender::new(
            CodecOffer::h264(true, true),
            7,
            8,
            SendOptions::SCREEN_720P30,
            (65_534, 90_000, 500),
            now,
        )
    }

    fn drain(sender: &mut VideoSender, now: Instant) -> Vec<Packet> {
        let mut out = Vec::new();
        while let Some(packet) = sender.next_packet(now) {
            out.push(packet);
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn sequence_numbers_wrap_and_the_marker_ends_the_frame() {
        let now = Instant::now();
        let mut sender = sender(now);
        sender
            .enqueue(now, Duration::from_millis(5), frame(4000))
            .unwrap();
        let mut sent = Vec::new();
        let mut at = now;
        while sent.len() < 4 {
            sent.extend(drain(&mut sender, at));
            at += Duration::from_millis(5);
        }
        assert_eq!(
            sent.iter().map(|p| p.sequence).collect::<Vec<_>>(),
            [65_534, 65_535, 0, 1]
        );
        assert!(sent.iter().all(|p| p.timestamp == 90_000));
        assert_eq!(
            sent.iter().map(Packet::marker).collect::<Vec<_>>(),
            [false, false, false, true]
        );
        // Single NAL unit, three FU-A: every datagram stays within bounds.
        assert!(
            sent.iter()
                .all(|p| p.wire_len() <= super::super::MAX_DATAGRAM_BYTES)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timestamps_follow_the_90_khz_clock_and_never_repeat() {
        let now = Instant::now();
        let mut sender = sender(now);
        sender
            .enqueue(now, Duration::from_secs(10), frame(100))
            .unwrap();
        sender
            .enqueue(now, Duration::from_millis(10_033), frame(100))
            .unwrap();
        // The same source time again, and one earlier: strictly increasing.
        sender
            .enqueue(now, Duration::from_millis(10_033), frame(100))
            .unwrap();
        sender
            .enqueue(now, Duration::from_secs(9), frame(100))
            .unwrap();
        let sent = drain_all(&mut sender, now).await;
        assert_eq!(
            sent.iter().map(|p| p.timestamp).collect::<Vec<_>>(),
            [90_000, 90_000 + 2_970, 90_000 + 2_971, 90_000 + 2_972]
        );
    }

    async fn drain_all(sender: &mut VideoSender, mut at: Instant) -> Vec<Packet> {
        let mut out = Vec::new();
        for _ in 0..10_000 {
            out.extend(drain(sender, at));
            if sender.next_wake().is_none() {
                break;
            }
            at += Duration::from_millis(1);
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn a_desktop_keyframe_can_use_bounded_headroom_without_bursting_or_growing_the_next_queue()
     {
        let now = Instant::now();
        let mut sender = sender(now);
        assert!(!sender.can_accept(now, 100_000));
        assert!(sender.can_accept_frame(now, 100_000, true));
        sender
            .enqueue_frame(now, Duration::ZERO, frame(100_000), true)
            .unwrap();
        assert_eq!(sender.pacer.window, KEYFRAME_WINDOW);
        assert!(sender.pacer_bytes() <= sender.pacer.capacity());
        assert_eq!(drain(&mut sender, now).len(), 1);
        let mut at = now;
        let mut sent = 1;
        while sender.next_wake().is_some() {
            at += Duration::from_millis(1);
            sent += drain(&mut sender, at).len();
            assert!(at - now <= KEYFRAME_WINDOW);
        }
        assert!(sent > 80);
        assert!(at - now > PACER_WINDOW);
        assert_eq!(sender.pacer.capacity(), 78_125);
        assert!(!sender.can_accept(now, 100_000));
    }

    #[tokio::test(start_paused = true)]
    async fn keyframe_headroom_never_extends_an_occupied_queue_or_admits_an_unbounded_frame() {
        let now = Instant::now();
        let mut sender = sender(now);
        sender.enqueue(now, Duration::ZERO, frame(40_000)).unwrap();
        let sequence = sender.sequence;
        assert!(!sender.can_accept_frame(now, 100_000, true));
        assert_eq!(
            sender.enqueue_frame(now, Duration::from_millis(33), frame(100_000), true),
            Err(VideoError::QueueFull)
        );
        assert_eq!(sender.sequence, sequence);
        assert_eq!(sender.pacer.window, PACER_WINDOW);
        drain_all(&mut sender, now).await;
        assert!(!sender.can_accept_frame(Instant::now(), 200_000, true));
    }

    #[tokio::test(start_paused = true)]
    async fn the_pacer_is_byte_and_time_bounded_and_a_refusal_burns_no_sequence() {
        let now = Instant::now();
        let mut sender = sender(now);
        // The window holds 2.5 Mbit/s * 2.5 * 100 ms = 78,125 bytes.
        assert_eq!(sender.pacer.capacity(), 78_125);
        assert_eq!(
            sender.enqueue(now, Duration::ZERO, frame(100_000)),
            Err(VideoError::QueueFull)
        );
        assert!(!sender.can_accept(now, 100_000));
        assert!(sender.can_accept(now, 40_000));
        assert_eq!(sender.sequence, 65_534);
        assert!(sender.last_timestamp.is_none());
        sender.enqueue(now, Duration::ZERO, frame(40_000)).unwrap();
        assert!(sender.pacer_bytes() >= 40_000);
        // A second frame that no longer fits is refused whole.
        assert_eq!(
            sender.enqueue(now, Duration::from_millis(33), frame(40_000)),
            Err(VideoError::QueueFull)
        );
        let after_first = sender.sequence;
        assert_eq!(after_first, 65_534u16.wrapping_add(36));
        // Paced out over time: at 6.25 Mbit/s 40 KB takes about 51 ms, not at once.
        let first_burst = drain(&mut sender, now);
        assert!(first_burst.len() < 36);
        let mut at = now;
        let mut total = first_burst.len();
        for _ in 0..200 {
            at += Duration::from_millis(1);
            total += drain(&mut sender, at).len();
            if total == 36 {
                break;
            }
        }
        assert_eq!(total, 36);
        assert!(at - now <= Duration::from_millis(60), "{:?}", at - now);
        assert_eq!(sender.pacer_bytes(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_wake_catches_up_within_the_burst_credit() {
        let now = Instant::now();
        let mut sender = sender(now);
        sender.enqueue(now, Duration::ZERO, frame(30_000)).unwrap();
        // One packet leaves at once; then the OS timer wakes 16 ms late.
        assert_eq!(drain(&mut sender, now).len(), 1);
        let late = now + Duration::from_millis(16);
        let burst = drain(&mut sender, late).len();
        // 16 ms at 6.25 Mbit/s is about 12.5 KB: ten 1.2 KB packets, not one.
        assert!((8..=13).contains(&burst), "{burst}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_queue_refuses_new_frames() {
        let now = Instant::now();
        let mut sender = sender(now);
        sender.enqueue(now, Duration::ZERO, frame(2000)).unwrap();
        let later = now + PACER_WINDOW + Duration::from_millis(1);
        assert!(!sender.can_accept(later, 100));
        assert_eq!(
            sender.enqueue(later, Duration::from_millis(33), frame(100)),
            Err(VideoError::QueueFull)
        );
        // Once drained the pacer accepts again.
        drain_all(&mut sender, later).await;
        sender
            .enqueue(
                later + Duration::from_secs(1),
                Duration::from_millis(33),
                frame(100),
            )
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn retransmissions_repeat_the_payload_on_the_rtx_stream_ahead_of_new_media() {
        let now = Instant::now();
        let mut sender = sender(now);
        sender.enqueue(now, Duration::ZERO, frame(3000)).unwrap();
        let sent = drain_all(&mut sender, now).await;
        assert_eq!(sent.len(), 3);
        // A NACK for the middle packet, one unknown sequence, and a duplicate.
        sender
            .enqueue(now, Duration::from_millis(33), frame(100))
            .unwrap();
        let later = now + Duration::from_millis(50);
        assert_eq!(sender.repair(later, [65_535, 3_000, 65_535].into_iter()), 2);
        let next = sender.next_packet(later).unwrap();
        assert_eq!(next.original, Some(65_535));
        assert_eq!(next.sequence, 500);
        assert_eq!(next.timestamp, 90_000);
        let mut payload = Vec::new();
        next.write_payload(&mut payload);
        let mut original = Vec::new();
        sent[1].write_payload(&mut original);
        assert_eq!(&payload[..2], 65_535u16.to_be_bytes());
        assert_eq!(&payload[2..], original);
        assert_eq!(next.marker(), sent[1].marker());
        assert_eq!(next.payload_len(), original.len() + 2);
        // The repair left before the queued new frame, and its sequence
        // numbers belong to the RTX stream.
        let again = sender
            .next_packet(later + Duration::from_millis(10))
            .unwrap();
        assert_eq!(again.original, Some(65_535));
        assert_eq!(again.sequence, 501);
        let fresh = sender
            .next_packet(later + Duration::from_millis(20))
            .unwrap();
        assert_eq!(fresh.original, None);
        assert_eq!(sender.rtx_packets, 2);
        assert_eq!(
            (sender.packets, sender.rtx_octets),
            (4, 2 * (original.len() as u32 + 2))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_repair_ring_is_bounded_by_age_and_by_bytes() {
        let now = Instant::now();
        let mut sender = sender(now);
        sender.enqueue(now, Duration::ZERO, frame(500)).unwrap();
        drain(&mut sender, now);
        assert!(sender.ring.get(now + REPAIR_WINDOW, 65_534).is_some());
        let expired = now + REPAIR_WINDOW + Duration::from_nanos(1);
        assert!(sender.ring.get(expired, 65_534).is_none());
        assert_eq!(sender.ring_bytes(), 0);

        // Bytes: frames of 60 KB pushed through the ring until it is full.
        let mut ring = RepairRing::new();
        let mut at = now;
        let mut sequence = 0u16;
        let mut sealed_frames = Vec::new();
        for _ in 0..100 {
            let data: Arc<[u8]> = frame(60_000).into();
            for part in h264::packetize(&data, MAX_PAYLOAD_BYTES).unwrap() {
                ring.remember(
                    at,
                    Packet {
                        frame: Arc::clone(&data),
                        part,
                        sequence,
                        timestamp: 0,
                        original: None,
                    },
                );
                sequence = sequence.wrapping_add(1);
            }
            sealed_frames.push(Arc::downgrade(&data));
            at += Duration::from_millis(1);
            assert!(ring.bytes() <= REPAIR_BYTES);
        }
        // The oldest frames are gone entirely: no allocation outlives its
        // accounting.
        assert!(sealed_frames[0].upgrade().is_none());
        assert!(sealed_frames[99].upgrade().is_some());
        assert!(ring.bytes() > REPAIR_BYTES - 60_000 - 100 * 60);
    }

    #[tokio::test(start_paused = true)]
    async fn congestion_follows_the_spec_defaults() {
        let now = Instant::now();
        let mut c = Congestion::new(2_500_000, now);
        // Over 5% loss: cut by 20%.
        assert_eq!(c.report(13, None, now), Some(2_000_000));
        // Exactly 5% (12/256) is tolerated; so is 3%, but neither raises.
        assert_eq!(c.report(12, None, now), None);
        assert_eq!(c.report(8, None, now), None);
        // A server limit applies at once.
        assert_eq!(c.limit(900_000, now), Some(900_000));
        // A higher request lifts the cap without raising the rate by itself;
        // the rate returns slowly, 5% per stable interval.
        assert_eq!(c.limit(1_200_000, now), None);
        assert_eq!(c.report(0, None, now + Duration::from_secs(4)), None);
        assert_eq!(
            c.report(0, None, now + Duration::from_secs(5)),
            Some(945_000)
        );
        // Never above the limit, however long it stays clean.
        let mut at = now + Duration::from_secs(5);
        for _ in 0..100 {
            at += Duration::from_secs(5);
            c.report(0, None, at);
        }
        assert_eq!(c.bitrate(), 1_200_000);
        // A higher limit lifts the cap but never beyond the initial rate.
        c.limit(9_000_000, at);
        for _ in 0..200 {
            at += Duration::from_secs(5);
            c.report(0, None, at);
        }
        assert_eq!(c.bitrate(), 2_500_000);
        // Round trip growth over the running minimum counts as congestion.
        let at = at + Duration::from_secs(1);
        assert_eq!(c.report(0, Some(Duration::from_millis(40)), at), None);
        assert_eq!(c.report(0, Some(Duration::from_millis(139)), at), None);
        assert_eq!(
            c.report(0, Some(Duration::from_millis(141)), at),
            Some(2_000_000)
        );
        // Cuts are a second apart at least: a burst of congestion signals is
        // one cut. The floor holds.
        assert_eq!(c.reduce(at + Duration::from_millis(500)), None);
        assert_eq!(c.bitrate(), 2_000_000);
        let mut later = at;
        for _ in 0..40 {
            later += Duration::from_secs(2);
            c.reduce(later);
        }
        assert_eq!(c.bitrate(), MIN_BITRATE);
        assert_eq!(c.limit(1, later), None);
    }

    #[tokio::test(start_paused = true)]
    async fn keyframe_requests_are_coalesced() {
        let now = Instant::now();
        let mut sender = sender(now);
        assert!(sender.keyframe_wanted(now));
        assert!(!sender.keyframe_wanted(now + Duration::from_millis(299)));
        assert!(sender.keyframe_wanted(now + Duration::from_millis(300)));
    }

    #[tokio::test(start_paused = true)]
    async fn the_sender_report_clock_extrapolates_from_the_last_frame() {
        let now = Instant::now();
        let mut sender = sender(now);
        assert_eq!(sender.rtp_timestamp_at(now), None);
        sender.enqueue(now, Duration::ZERO, frame(100)).unwrap();
        assert_eq!(sender.rtp_timestamp_at(now), Some(90_000));
        assert_eq!(
            sender.rtp_timestamp_at(now + Duration::from_millis(100)),
            Some(99_000)
        );
    }
}
