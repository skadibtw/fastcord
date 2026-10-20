//! A bounded Ogg Opus demuxer for local attachment playback (RFC 7845).
//!
//! Only the first logical stream is read: mono or stereo (channel mapping family 0) Opus whose
//! packets are exactly 20 ms, the frame size the voice engine plays. Anything else is
//! [`OggError::Unsupported`] and stays Download/Open-only in the UI. Every page is CRC-checked,
//! a packet is limited to [`MAX_PACKET_BYTES`] and the reader holds at most one page (64 KiB)
//! plus one partial packet, whatever the file size. Seeking bisects on page granule positions,
//! so it costs a few dozen small reads instead of an index.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};

use crate::codec::{EncodedPacket, FRAME_SAMPLES, MAX_PACKET_BYTES};

/// Samples (48 kHz) per millisecond.
const PER_MS: u64 = 48;
/// Largest Ogg page: 27-byte header, 255 lacing values, 255 segments of 255 bytes.
const MAX_BODY: usize = 255 * 255;
/// Bytes searched forward for the next page marker before giving up.
const SCAN_LIMIT: u64 = 256 * 1024;
/// Bytes at the end of the file searched for the last page (its granule is the duration).
const TAIL: u64 = 128 * 1024;
/// OpusTags may carry cover art; it is skipped, but not without limit.
const HEADER_PACKET_LIMIT: usize = 4 * 1024 * 1024;
const FLAG_CONTINUED: u8 = 1;
const FLAG_BEGIN: u8 = 2;
const FLAG_END: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OggError {
    /// The file could not be read.
    Io,
    /// Not valid Ogg Opus: bad capture pattern, checksum, structure, or truncated headers.
    Malformed,
    /// Valid, but not Ogg Opus this player plays: other codec, surround, or non-20 ms frames.
    Unsupported,
}

impl std::fmt::Display for OggError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Io => "the audio file could not be read",
            Self::Malformed => "the audio file is damaged",
            Self::Unsupported => "this Ogg audio format is not supported",
        })
    }
}

impl std::error::Error for OggError {}

impl From<std::io::Error> for OggError {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut value = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 0x8000_0000 != 0 {
                (value << 1) ^ 0x04c1_1db7
            } else {
                value << 1
            };
            bit += 1;
        }
        table[i] = value;
        i += 1;
    }
    table
}

static CRC: [u32; 256] = crc_table();

fn crc(parts: &[&[u8]]) -> u32 {
    let mut value = 0u32;
    for part in parts {
        for &byte in *part {
            value = (value << 8) ^ CRC[usize::from((value >> 24) as u8 ^ byte)];
        }
    }
    value
}

struct Page {
    offset: u64,
    end: u64,
    flags: u8,
    /// `None` when no packet finishes on the page.
    granule: Option<u64>,
    serial: u32,
    lacing: Vec<u8>,
    body: Vec<u8>,
}

/// The page starting exactly at `offset`; `Ok(None)` at the end of the file.
fn read_page<R: Read + Seek>(r: &mut R, offset: u64, len: u64) -> Result<Option<Page>, OggError> {
    if offset >= len {
        return Ok(None);
    }
    r.seek(SeekFrom::Start(offset))?;
    let mut header = [0u8; 27];
    r.read_exact(&mut header).map_err(|_| OggError::Malformed)?;
    if &header[..4] != b"OggS" || header[4] != 0 {
        return Err(OggError::Malformed);
    }
    let mut lacing = vec![0u8; usize::from(header[26])];
    r.read_exact(&mut lacing).map_err(|_| OggError::Malformed)?;
    let body_len: usize = lacing.iter().map(|&v| usize::from(v)).sum();
    debug_assert!(body_len <= MAX_BODY);
    let mut body = vec![0u8; body_len];
    r.read_exact(&mut body).map_err(|_| OggError::Malformed)?;
    let stored = u32::from_le_bytes([header[22], header[23], header[24], header[25]]);
    header[22..26].fill(0);
    if crc(&[&header, &lacing, &body]) != stored {
        return Err(OggError::Malformed);
    }
    let granule = u64::from_le_bytes(header[6..14].try_into().unwrap_or([0; 8]));
    Ok(Some(Page {
        offset,
        end: offset + 27 + lacing.len() as u64 + body_len as u64,
        flags: header[5],
        granule: (granule != u64::MAX).then_some(granule),
        serial: u32::from_le_bytes(header[14..18].try_into().unwrap_or([0; 4])),
        lacing,
        body,
    }))
}

/// The first valid page that starts in `[from, min(to, from + SCAN_LIMIT))`.
fn scan_page<R: Read + Seek>(
    r: &mut R,
    from: u64,
    to: u64,
    len: u64,
) -> Result<Option<Page>, OggError> {
    let stop = to.min(len).min(from.saturating_add(SCAN_LIMIT));
    let mut at = from;
    let mut chunk = [0u8; 4096];
    while at < stop {
        r.seek(SeekFrom::Start(at))?;
        let want = chunk.len().min((len - at) as usize);
        let got = r.read(&mut chunk[..want])?;
        if got == 0 {
            return Ok(None);
        }
        for (index, window) in chunk[..got].windows(4).enumerate() {
            let candidate = at + index as u64;
            if candidate >= stop {
                return Ok(None);
            }
            if window == b"OggS" {
                match read_page(r, candidate, len) {
                    Ok(Some(page)) => return Ok(Some(page)),
                    Ok(None) | Err(OggError::Malformed) => {}
                    Err(error) => return Err(error),
                }
                // read_page moved the cursor; the chunk is still intact in memory.
            }
        }
        if got < 4 {
            return Ok(None);
        }
        at += (got - 3) as u64;
    }
    Ok(None)
}

/// Samples in the 20 ms check: the duration of one Opus packet from its table-of-contents
/// byte (RFC 6716 §3.1), or `None` for a malformed packet.
fn packet_samples(packet: &[u8]) -> Option<usize> {
    let toc = *packet.first()?;
    let config = usize::from(toc >> 3);
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][config % 4],
        12 | 14 => 480,
        13 | 15 => 960,
        _ => [120, 240, 480, 960][config % 4],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => usize::from(*packet.get(1)? & 0x3f),
    };
    Some(frame * frames)
}

/// An Ogg Opus stream positioned at an audio packet.
pub struct OggOpus<R> {
    r: R,
    len: u64,
    serial: u32,
    pre_skip: u64,
    data_start: u64,
    duration_ms: Option<u64>,
    cursor: u64,
    partial: Vec<u8>,
    discarding: bool,
    queue: VecDeque<Vec<u8>>,
    packet_limit: usize,
    finished: bool,
    /// Granule position (48 kHz samples, pre-skip included) of the next packet.
    granule: u64,
}

impl<R: Read + Seek> OggOpus<R> {
    /// Parses the headers and finds the duration. Rejects what the player cannot play.
    pub fn open(mut r: R) -> Result<Self, OggError> {
        let len = r.seek(SeekFrom::End(0))?;
        let first = read_page(&mut r, 0, len)?.ok_or(OggError::Malformed)?;
        if first.flags & FLAG_BEGIN == 0 {
            return Err(OggError::Malformed);
        }
        let mut this = Self {
            r,
            len,
            serial: first.serial,
            pre_skip: 0,
            data_start: 0,
            duration_ms: None,
            cursor: 0,
            partial: Vec::new(),
            discarding: false,
            queue: VecDeque::new(),
            packet_limit: HEADER_PACKET_LIMIT,
            finished: false,
            granule: 0,
        };
        let head = this.next_raw()?.ok_or(OggError::Malformed)?;
        if !head.starts_with(b"OpusHead") {
            return Err(OggError::Unsupported);
        }
        if head.len() < 19 || head[8] >> 4 != 0 || !(1..=2).contains(&head[9]) || head[18] != 0 {
            return Err(OggError::Unsupported);
        }
        this.pre_skip = u64::from(u16::from_le_bytes([head[10], head[11]]));
        let tags = this.next_raw()?.ok_or(OggError::Malformed)?;
        if !tags.starts_with(b"OpusTags") || !this.queue.is_empty() {
            return Err(OggError::Malformed);
        }
        this.data_start = this.cursor;
        this.packet_limit = MAX_PACKET_BYTES;
        this.duration_ms = this
            .last_granule()?
            .map(|g| g.saturating_sub(this.pre_skip) / PER_MS);
        this.reposition(this.data_start, 0);
        Ok(this)
    }

    /// Length of the stream, when the last page says.
    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    /// Start of the next packet, in milliseconds.
    pub fn position_ms(&self) -> u64 {
        self.granule.saturating_sub(self.pre_skip) / PER_MS
    }

    fn last_granule(&mut self) -> Result<Option<u64>, OggError> {
        let mut at = self.len.saturating_sub(TAIL).max(self.data_start);
        let mut last = None;
        while let Some(page) = scan_page(&mut self.r, at, self.len, self.len)? {
            if page.serial == self.serial && page.granule.is_some() {
                last = page.granule;
            }
            at = page.end;
        }
        Ok(last)
    }

    fn reposition(&mut self, offset: u64, granule: u64) {
        self.cursor = offset;
        self.granule = granule;
        self.partial.clear();
        self.queue.clear();
        self.discarding = false;
        self.finished = false;
    }

    /// The next complete packet of the stream, whatever its size.
    fn next_raw(&mut self) -> Result<Option<Vec<u8>>, OggError> {
        loop {
            if let Some(packet) = self.queue.pop_front() {
                return Ok(Some(packet));
            }
            if self.finished {
                return Ok(None);
            }
            let Some(page) = read_page(&mut self.r, self.cursor, self.len)? else {
                self.finished = true;
                return Ok(None);
            };
            self.cursor = page.end;
            if page.serial != self.serial {
                // A chained stream or a stray page: only the first stream plays.
                self.finished = page.flags & FLAG_BEGIN != 0;
                continue;
            }
            if page.flags & FLAG_END != 0 {
                self.finished = true;
            }
            self.ingest(&page)?;
        }
    }

    fn ingest(&mut self, page: &Page) -> Result<(), OggError> {
        if page.flags & FLAG_CONTINUED == 0 {
            self.partial.clear();
            self.discarding = false;
        } else if self.partial.is_empty() {
            // The start of a packet we never saw (after a seek): drop its tail.
            self.discarding = true;
        }
        let mut at = 0;
        for &segment in &page.lacing {
            let segment = usize::from(segment);
            if !self.discarding {
                self.partial.extend_from_slice(&page.body[at..at + segment]);
                if self.partial.len() > self.packet_limit {
                    return Err(OggError::Unsupported);
                }
            }
            at += segment;
            if segment < 255 {
                let packet = std::mem::take(&mut self.partial);
                if self.discarding {
                    // Skipped, but it still took its 20 ms of the timeline.
                    self.discarding = false;
                    self.granule += FRAME_SAMPLES as u64;
                } else {
                    self.queue.push_back(packet);
                }
            }
        }
        Ok(())
    }

    /// The next audio packet, or `None` at the end of the stream.
    pub fn next_packet(&mut self) -> Result<Option<EncodedPacket>, OggError> {
        let Some(raw) = self.next_raw()? else {
            return Ok(None);
        };
        if packet_samples(&raw) != Some(FRAME_SAMPLES) {
            return Err(OggError::Unsupported);
        }
        self.granule += FRAME_SAMPLES as u64;
        EncodedPacket::from_slice(&raw)
            .map(Some)
            .map_err(|_| OggError::Unsupported)
    }

    /// Repositions so that the next packet starts at `ms` (to the packet).
    pub fn seek_ms(&mut self, ms: u64) -> Result<(), OggError> {
        let target = ms.saturating_mul(PER_MS).saturating_add(self.pre_skip);
        // Bisect on page ends; reading restarts from the whole page that ended at `low`, so a
        // packet that straddles the boundary is not lost.
        let (mut low, mut start, mut high) = (self.data_start, (self.data_start, 0u64), self.len);
        while high > low {
            let mid = low + (high - low) / 2;
            match scan_page(&mut self.r, mid, high, self.len)? {
                Some(page) if page.serial == self.serial => match page.granule {
                    Some(granule) if granule < target => {
                        let done = page.lacing.iter().filter(|&&v| v < 255).count() as u64;
                        low = page.end;
                        start = (
                            page.offset,
                            granule.saturating_sub(done * FRAME_SAMPLES as u64),
                        );
                    }
                    _ => high = mid,
                },
                _ => high = mid,
            }
        }
        self.reposition(start.0, start.1);
        while self.granule + FRAME_SAMPLES as u64 <= target {
            if self.next_packet()?.is_none() {
                break;
            }
        }
        Ok(())
    }
}

/// Builds Ogg Opus bytes for tests here and in the player.
#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    /// A 20 ms CELT packet carrying `id` after its table-of-contents byte.
    pub fn packet(id: u16, extra: usize) -> Vec<u8> {
        let mut packet = vec![0xF8, id as u8, (id >> 8) as u8];
        packet.resize(3 + extra, 7);
        packet
    }

    pub fn packet_id(packet: &[u8]) -> u16 {
        u16::from(packet[1]) | u16::from(packet[2]) << 8
    }

    fn page(
        serial: u32,
        flags: u8,
        granule: u64,
        sequence: u32,
        segments: &[u8],
        body: &[u8],
    ) -> Vec<u8> {
        let mut out = b"OggS".to_vec();
        out.extend_from_slice(&[0, flags]);
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&serial.to_le_bytes());
        out.extend_from_slice(&sequence.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.push(segments.len() as u8);
        out.extend_from_slice(segments);
        out.extend_from_slice(body);
        let checksum = crc(&[&out]);
        out[22..26].copy_from_slice(&checksum.to_le_bytes());
        out
    }

    pub fn head(channels: u8, family: u8) -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, channels]);
        head.extend_from_slice(&312u16.to_le_bytes());
        head.extend_from_slice(&48_000u32.to_le_bytes());
        head.extend_from_slice(&[0, 0, family]);
        head
    }

    pub fn lacing(len: usize) -> Vec<u8> {
        let mut values = vec![255u8; len / 255];
        values.push((len % 255) as u8);
        values
    }

    /// A stream whose pages hold at most `max_segments` lacing values, so packets of a few
    /// hundred bytes cross page boundaries.
    pub fn stream(head: &[u8], packets: &[Vec<u8>], max_segments: usize) -> Vec<u8> {
        let serial = 0x1234;
        let mut out = page(serial, FLAG_BEGIN, 0, 0, &lacing(head.len()), head);
        let tags = b"OpusTagsXXXX".to_vec();
        out.extend(page(serial, 0, 0, 1, &lacing(tags.len()), &tags));
        // (lacing value, bytes, packet index that ends with this segment)
        let mut segments: Vec<(u8, Vec<u8>, Option<usize>)> = Vec::new();
        for (index, packet) in packets.iter().enumerate() {
            let values = lacing(packet.len());
            let mut at = 0;
            for (n, &value) in values.iter().enumerate() {
                let bytes = packet[at..at + usize::from(value)].to_vec();
                at += usize::from(value);
                segments.push((value, bytes, (n + 1 == values.len()).then_some(index)));
            }
        }
        let mut continued = false;
        let chunks: Vec<_> = segments.chunks(max_segments).collect();
        for (sequence, (n, chunk)) in (2..).zip(chunks.iter().enumerate()) {
            let last_end = chunk.iter().rev().find_map(|segment| segment.2);
            let granule =
                last_end.map_or(u64::MAX, |index| (index as u64 + 1) * FRAME_SAMPLES as u64);
            let mut flags = if continued { FLAG_CONTINUED } else { 0 };
            if n + 1 == chunks.len() {
                flags |= FLAG_END;
            }
            let values: Vec<u8> = chunk.iter().map(|segment| segment.0).collect();
            let body: Vec<u8> = chunk.iter().flat_map(|segment| segment.1.clone()).collect();
            out.extend(page(serial, flags, granule, sequence, &values, &body));
            continued = chunk.last().is_some_and(|segment| segment.2.is_none());
        }
        out
    }

    pub fn sample(count: u16, max_segments: usize) -> Vec<u8> {
        let packets: Vec<_> = (0..count).map(|id| packet(id, 700)).collect();
        stream(&head(2, 0), &packets, max_segments)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use std::io::Cursor;

    #[test]
    fn packets_cross_pages_and_come_out_in_order_with_the_duration() {
        // 700-byte packets are 3 segments; 5 per page splits every other packet.
        let mut stream = OggOpus::open(Cursor::new(sample(200, 5))).unwrap();
        assert_eq!(stream.duration_ms(), Some((200 * 960 - 312) / 48));
        for id in 0..200u16 {
            let packet = stream.next_packet().unwrap().unwrap();
            assert_eq!(packet_id(packet.as_bytes()), id);
            assert_eq!(packet.as_bytes().len(), 703);
        }
        assert_eq!(stream.next_packet().unwrap(), None);
        assert_eq!(stream.next_packet().unwrap(), None);
    }

    #[test]
    fn seek_lands_on_the_requested_packet_from_any_page_alignment() {
        let mut stream = OggOpus::open(Cursor::new(sample(500, 7))).unwrap();
        for (ms, id) in [(0, 0u16), (20, 1), (1980, 99), (2000, 100), (9980, 499)] {
            stream.seek_ms(ms).unwrap();
            let packet = stream.next_packet().unwrap().unwrap();
            assert_eq!(packet_id(packet.as_bytes()), id, "{ms} ms");
        }
        // Past the end: nothing left to play.
        stream.seek_ms(60_000).unwrap();
        assert_eq!(stream.next_packet().unwrap(), None);
        // Backwards after the end works again.
        stream.seek_ms(40).unwrap();
        assert_eq!(
            packet_id(stream.next_packet().unwrap().unwrap().as_bytes()),
            2
        );
        assert_eq!(stream.position_ms(), 53);
    }

    #[test]
    fn a_damaged_page_is_malformed_not_a_panic() {
        let mut bytes = sample(100, 5);
        let at = bytes.len() / 2;
        bytes[at] ^= 0x55;
        let mut stream = OggOpus::open(Cursor::new(bytes)).unwrap();
        let mut result = Ok(());
        for _ in 0..100 {
            match stream.next_packet() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
        }
        assert_eq!(result, Err(OggError::Malformed));
    }

    #[test]
    fn unsupported_and_garbage_inputs_are_rejected_cleanly() {
        let packets: Vec<_> = (0..4).map(|id| packet(id, 10)).collect();
        // Surround mapping, three channels, and a Vorbis-style first packet.
        for head in [
            head(2, 1),
            head(3, 0),
            b"\x01vorbis....................".to_vec(),
        ] {
            let bytes = stream(&head, &packets, 255);
            assert!(matches!(
                OggOpus::open(Cursor::new(bytes)),
                Err(OggError::Unsupported)
            ));
        }
        // A 10 ms CELT packet is valid Opus but not what the engine plays.
        let mut odd = packets.clone();
        odd[2][0] = 0xF0;
        let mut stream = OggOpus::open(Cursor::new(stream(&head(1, 0), &odd, 255))).unwrap();
        assert!(stream.next_packet().unwrap().is_some());
        assert!(stream.next_packet().unwrap().is_some());
        assert!(matches!(stream.next_packet(), Err(OggError::Unsupported)));
        for garbage in [
            vec![],
            vec![0u8; 100],
            b"OggS\0\x02".to_vec(),
            vec![b'O'; 70_000],
        ] {
            assert!(OggOpus::open(Cursor::new(garbage)).is_err());
        }
        // Truncated anywhere: errors or ends, never panics or loops.
        let whole = sample(40, 5);
        for cut in (0..whole.len()).step_by(37) {
            if let Ok(mut stream) = OggOpus::open(Cursor::new(whole[..cut].to_vec())) {
                for _ in 0..60 {
                    if !matches!(stream.next_packet(), Ok(Some(_))) {
                        break;
                    }
                }
                let _ = stream.seek_ms(300);
            }
        }
    }

    #[test]
    fn an_oversized_packet_is_unsupported() {
        let packets = vec![packet(0, 10), packet(1, MAX_PACKET_BYTES + 10)];
        let mut stream = OggOpus::open(Cursor::new(stream(&head(2, 0), &packets, 255))).unwrap();
        // The page holding the oversized packet is refused as a whole.
        assert!(matches!(stream.next_packet(), Err(OggError::Unsupported)));
    }

    #[test]
    fn real_libopus_packets_survive_the_container_and_decode() {
        use crate::codec::{CHANNELS, MAX_FRAME_SAMPLES, VoiceDecoder, VoiceEncoder};
        let mut encoder = VoiceEncoder::new(32_000).unwrap();
        let mut packets = Vec::new();
        for n in 0..50 {
            let mut mono = [0f32; FRAME_SAMPLES];
            for (i, sample) in mono.iter_mut().enumerate() {
                let t = (n * FRAME_SAMPLES + i) as f32 / 48_000.0;
                *sample = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3;
            }
            let mut out = EncodedPacket::from_slice(&[]).unwrap();
            encoder.encode_mono(&mono, &mut out).unwrap();
            packets.push(out.as_bytes().to_vec());
        }
        let mut stream = OggOpus::open(Cursor::new(stream(&head(1, 0), &packets, 255))).unwrap();
        let mut decoder = VoiceDecoder::new().unwrap();
        let mut pcm = vec![0f32; MAX_FRAME_SAMPLES * CHANNELS];
        let mut energy = 0.0f32;
        for expected in &packets {
            let packet = stream.next_packet().unwrap().unwrap();
            assert_eq!(packet.as_bytes(), expected.as_slice());
            let frames = decoder.decode(packet.as_bytes(), &mut pcm).unwrap();
            assert_eq!(frames, FRAME_SAMPLES);
            energy += pcm[..frames * CHANNELS].iter().map(|v| v * v).sum::<f32>();
        }
        assert!(energy > 10.0, "{energy}");
        assert_eq!(stream.next_packet().unwrap(), None);
    }

    #[test]
    fn frame_durations_follow_the_toc() {
        assert_eq!(packet_samples(&[0xF8]), Some(960));
        assert_eq!(packet_samples(&[0xF0]), Some(480));
        assert_eq!(packet_samples(&[0x78]), Some(960));
        assert_eq!(packet_samples(&[0x08]), Some(960));
        assert_eq!(packet_samples(&[0xF9]), Some(1920));
        assert_eq!(packet_samples(&[0xFB, 2]), Some(1920));
        assert_eq!(packet_samples(&[0xFB]), None);
        assert_eq!(packet_samples(&[]), None);
    }
}
