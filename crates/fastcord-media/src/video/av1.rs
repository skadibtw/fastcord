//! AV1 RTP reception (AOM RTP Payload Format for AV1 v1.0.0, §§4–5).
//!
//! The Z/Y flags join OBU fragments, W describes aggregation lengths, and N
//! starts a coded video sequence. RTP element lengths are not OBU sizes. The
//! output normally restores minimal low-overhead OBU sizes; at the DAVE
//! boundary the final OBU instead has no size, exactly as libdave's AV1
//! transform writes it before appending its authenticated trailer.

use super::{MAX_FRAME_BYTES, MAX_PACKETS, VideoError};

const HAS_EXTENSION: u8 = 4;
const HAS_SIZE: u8 = 2;
/// Parser metadata is bounded as well as frame bytes (at most 256 KiB).
const MAX_ELEMENTS: usize = 4096;

/// OBU framing required at the receiving stage.
#[derive(Clone, Copy)]
pub enum Framing {
    /// Low-overhead OBU stream for a decoder, every OBU explicitly sized.
    LowOverhead,
    /// Non-final OBUs sized; the final OBU includes the DAVE trailer.
    Dave,
}

#[derive(Clone, Copy)]
struct Element<'a> {
    bytes: &'a [u8],
    start: bool,
    end: bool,
    packet: usize,
    new_sequence: bool,
}

/// Traverses payload elements without allocating or copying fragments.
struct Elements<'a> {
    payloads: &'a [&'a [u8]],
    packet: usize,
    rest: &'a [u8],
    header: u8,
    index: usize,
    expected: usize,
    continuing: bool,
    count: usize,
    bytes: usize,
}

impl<'a> Elements<'a> {
    fn new(payloads: &'a [&'a [u8]]) -> Result<Self, VideoError> {
        if payloads.is_empty() || payloads.len() > MAX_PACKETS {
            return Err(VideoError::TooLarge);
        }
        Ok(Self {
            payloads,
            packet: 0,
            rest: &[],
            header: 0,
            index: 0,
            expected: 0,
            continuing: false,
            count: 0,
            bytes: 0,
        })
    }

    fn next(&mut self) -> Result<Option<Element<'a>>, VideoError> {
        if self.rest.is_empty() {
            if self.index != 0 {
                if self.expected != 0 && self.index != self.expected {
                    return Err(VideoError::Malformed);
                }
                self.continuing = self.header & 0x40 != 0;
                self.packet += 1;
            }
            let Some(payload) = self.payloads.get(self.packet) else {
                return if self.continuing {
                    Err(VideoError::Incomplete)
                } else {
                    Ok(None)
                };
            };
            let (&header, rest) = payload.split_first().ok_or(VideoError::Malformed)?;
            if header & 7 != 0 || (header & 0x88 == 0x88) || (header & 8 != 0 && self.packet != 0) {
                return Err(VideoError::Malformed);
            }
            if (header & 0x80 != 0) != self.continuing {
                return Err(VideoError::Incomplete);
            }
            if rest.is_empty() {
                return Err(VideoError::Malformed);
            }
            self.header = header;
            self.expected = usize::from((header >> 4) & 3);
            self.index = 0;
            self.rest = rest;
        }
        self.index += 1;
        self.count += 1;
        if self.count > MAX_ELEMENTS {
            return Err(VideoError::TooLarge);
        }
        let len = if self.expected != 0 && self.index == self.expected {
            self.rest.len()
        } else {
            let (len, size) = leb128(self.rest)?;
            self.rest = &self.rest[size..];
            len
        };
        if len == 0 {
            return Err(VideoError::Malformed);
        }
        let (bytes, rest) = self
            .rest
            .split_at_checked(len)
            .ok_or(VideoError::Malformed)?;
        self.rest = rest;
        self.bytes = self.bytes.checked_add(len).ok_or(VideoError::TooLarge)?;
        if self.bytes > MAX_FRAME_BYTES {
            return Err(VideoError::TooLarge);
        }
        if self.expected != 0 && self.rest.is_empty() && self.index != self.expected {
            return Err(VideoError::Malformed);
        }
        Ok(Some(Element {
            bytes,
            start: self.index != 1 || self.header & 0x80 == 0,
            end: !self.rest.is_empty() || self.header & 0x40 == 0,
            packet: self.packet,
            new_sequence: self.header & 8 != 0,
        }))
    }
}

/// AV1 leb128 is at most eight bytes and represents a 32-bit OBU size.
fn leb128(bytes: &[u8]) -> Result<(usize, usize), VideoError> {
    let mut value = 0_u64;
    for index in 0..8 {
        let byte = *bytes.get(index).ok_or(VideoError::Malformed)?;
        value |= u64::from(byte & 0x7F) << (index * 7);
        if byte & 0x80 == 0 {
            return if value <= u64::from(u32::MAX) {
                Ok((value as usize, index + 1))
            } else {
                Err(VideoError::Malformed)
            };
        }
    }
    Err(VideoError::Malformed)
}

fn write_size(mut size: usize, out: &mut Vec<u8>) {
    while size >= 128 {
        out.push((size as u8 & 0x7F) | 0x80);
        size >>= 7;
    }
    out.push(size as u8);
}

fn size_len(size: usize) -> usize {
    if size < 128 {
        1
    } else if size < 16_384 {
        2
    } else if size < 2_097_152 {
        3
    } else {
        4
    }
}

struct Obu {
    prefix: [u8; 10],
    prefix_len: usize,
    total: usize,
    header_len: usize,
    skip: usize,
    payload_len: usize,
    ignored: bool,
}

impl Obu {
    fn new() -> Self {
        Self {
            prefix: [0; 10],
            prefix_len: 0,
            total: 0,
            header_len: 0,
            skip: 0,
            payload_len: 0,
            ignored: false,
        }
    }

    fn extend(&mut self, bytes: &[u8]) {
        let take = bytes.len().min(self.prefix.len() - self.prefix_len);
        self.prefix[self.prefix_len..self.prefix_len + take].copy_from_slice(&bytes[..take]);
        self.prefix_len += take;
        self.total += bytes.len();
    }

    fn finish(&mut self) -> Result<(), VideoError> {
        let prefix = &self.prefix[..self.prefix_len];
        let header = *prefix.first().ok_or(VideoError::Malformed)?;
        let kind = (header >> 3) & 15;
        if header & 0x81 != 0 || !matches!(kind, 1..=8 | 15) {
            return Err(VideoError::Malformed);
        }
        self.header_len = 1 + usize::from(header & HAS_EXTENSION != 0);
        if self.header_len == 2 && prefix.get(1).is_none_or(|byte| byte & 7 != 0) {
            return Err(VideoError::Malformed);
        }
        self.skip = self.header_len;
        if header & HAS_SIZE != 0 {
            let (len, size) = leb128(&prefix[self.header_len..])?;
            self.skip += size;
            if self.total - self.skip != len {
                return Err(VideoError::Malformed);
            }
        }
        self.payload_len = self.total - self.skip;
        // §5: these OBUs MUST be ignored on receipt.
        self.ignored = matches!(kind, 2 | 8);
        Ok(())
    }
}

/// Reassembles one complete temporal unit in RTP sequence order. On error no
/// partial output survives. Returns the first packet's coded-sequence flag;
/// AV1 payload bits are encrypted by DAVE, so they cannot identify keyframes.
pub fn depacketize(
    payloads: &[&[u8]],
    framing: Framing,
    out: &mut Vec<u8>,
) -> Result<bool, VideoError> {
    out.clear();
    let result = assemble(payloads, framing, out);
    if result.is_err() {
        out.clear();
    }
    result
}

fn assemble(payloads: &[&[u8]], framing: Framing, out: &mut Vec<u8>) -> Result<bool, VideoError> {
    let mut elements = Elements::new(payloads)?;
    let mut obus = Vec::<Obu>::new();
    let mut new_sequence = false;
    // Validate and size first, so each payload byte is copied only once, even
    // when a length/extension header is split over several packets.
    while let Some(element) = elements.next()? {
        if element.start {
            obus.push(Obu::new());
        }
        let obu = obus.last_mut().ok_or(VideoError::Incomplete)?;
        obu.extend(element.bytes);
        if element.packet == 0 {
            new_sequence = element.new_sequence;
        }
        if element.end {
            obu.finish()?;
        }
    }
    let last = obus
        .iter()
        .rposition(|obu| !obu.ignored)
        .ok_or(VideoError::Incomplete)?;
    let sized = |index| matches!(framing, Framing::LowOverhead) || index != last;
    let mut len = 0;
    for (index, obu) in obus.iter().enumerate().filter(|(_, obu)| !obu.ignored) {
        len += obu.header_len
            + obu.payload_len
            + if sized(index) {
                size_len(obu.payload_len)
            } else {
                0
            };
        if len > MAX_FRAME_BYTES {
            return Err(VideoError::TooLarge);
        }
    }
    out.reserve(len);
    let mut elements = Elements::new(payloads)?;
    let mut index = 0;
    let mut skip = 0;
    let mut previous_packet = None;
    let mut packet_layer = None;
    while let Some(element) = elements.next()? {
        let obu = &obus[index];
        if previous_packet != Some(element.packet) {
            previous_packet = Some(element.packet);
            packet_layer = None;
        }
        if obu.header_len == 2 {
            let layer = obu.prefix[1] & 0xF8;
            if packet_layer.is_some_and(|other| other != layer) {
                return Err(VideoError::Malformed);
            }
            packet_layer = Some(layer);
        }
        if element.start {
            skip = obu.skip;
            if !obu.ignored {
                out.push((obu.prefix[0] & !HAS_SIZE) | if sized(index) { HAS_SIZE } else { 0 });
                if obu.header_len == 2 {
                    out.push(obu.prefix[1]);
                }
                if sized(index) {
                    write_size(obu.payload_len, out);
                }
            }
        }
        let omitted = skip.min(element.bytes.len());
        skip -= omitted;
        if !obu.ignored {
            out.extend_from_slice(&element.bytes[omitted..]);
        }
        if element.end {
            index += 1;
        }
    }
    Ok(new_sequence)
}

/// Restores sizes after authentication. DAVE removes the final OBU size to
/// make room for its trailer; native low-overhead decoders require it back.
pub fn normalize(frame: &[u8], out: &mut Vec<u8>) -> Result<(), VideoError> {
    out.clear();
    let result = normalize_inner(frame, out);
    if result.is_err() {
        out.clear();
    }
    result
}

fn normalize_inner(mut frame: &[u8], out: &mut Vec<u8>) -> Result<(), VideoError> {
    if frame.is_empty() || frame.len() > MAX_FRAME_BYTES {
        return Err(VideoError::TooLarge);
    }
    while !frame.is_empty() {
        let header_len = 1 + usize::from(frame[0] & HAS_EXTENSION != 0);
        let (len, size) = if frame[0] & HAS_SIZE != 0 {
            leb128(frame.get(header_len..).ok_or(VideoError::Malformed)?)?
        } else {
            (
                frame
                    .len()
                    .checked_sub(header_len)
                    .ok_or(VideoError::Malformed)?,
                0,
            )
        };
        let end = header_len
            .checked_add(size)
            .and_then(|start| start.checked_add(len))
            .ok_or(VideoError::TooLarge)?;
        let bytes = frame.get(..end).ok_or(VideoError::Malformed)?;
        let mut obu = Obu::new();
        obu.extend(bytes);
        obu.finish()?;
        if !obu.ignored {
            if out.len() + header_len + size_len(len) + len > MAX_FRAME_BYTES {
                return Err(VideoError::TooLarge);
            }
            out.push(frame[0] | HAS_SIZE);
            if header_len == 2 {
                out.push(frame[1]);
            }
            write_size(len, out);
            out.extend_from_slice(&bytes[header_len + size..]);
        }
        frame = &frame[end..];
    }
    if out.is_empty() {
        return Err(VideoError::Incomplete);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obu_headers_and_lengths_may_be_fragmented_at_every_byte() {
        // Metadata OBU, extension header and a two-byte size.
        let mut obu = vec![0x2E, 0x28, 0x82, 0x01];
        obu.extend(0..130_u8);
        for cut in 1..obu.len() {
            let mut first = vec![0x50];
            first.extend_from_slice(&obu[..cut]);
            let mut last = vec![0x90];
            last.extend_from_slice(&obu[cut..]);
            let mut output = Vec::new();
            assert!(!depacketize(&[&first, &last], Framing::LowOverhead, &mut output).unwrap());
            assert_eq!(output, obu, "cut {cut}");
        }
    }

    #[test]
    fn dave_framing_keeps_non_final_sizes_and_omits_the_final_one() {
        let packets: &[&[u8]] = &[&[0x28, 2, 0x08, 1, 0x30, 2, 3]];
        let mut output = Vec::new();
        assert!(depacketize(packets, Framing::Dave, &mut output).unwrap());
        assert_eq!(output, [0x0A, 1, 1, 0x30, 2, 3]);
        let mut canonical = Vec::new();
        normalize(&output, &mut canonical).unwrap();
        assert_eq!(canonical, [0x0A, 1, 1, 0x32, 2, 2, 3]);
    }

    #[test]
    fn frame_and_parser_metadata_are_byte_bounded_and_errors_clear_output() {
        let oversized = vec![0x30; MAX_FRAME_BYTES + 1];
        let mut packet = vec![0x10];
        packet.extend_from_slice(&oversized);
        let mut output = vec![9];
        assert_eq!(
            depacketize(&[&packet], Framing::Dave, &mut output),
            Err(VideoError::TooLarge)
        );
        assert!(output.is_empty());
        let many = vec![&[0x10, 0x30][..]; MAX_PACKETS + 1];
        assert_eq!(
            depacketize(&many, Framing::Dave, &mut output),
            Err(VideoError::TooLarge)
        );
        let mut packet = vec![0];
        for _ in 0..=MAX_ELEMENTS {
            packet.extend_from_slice(&[1, 0x30]);
        }
        assert_eq!(
            depacketize(&[&packet], Framing::Dave, &mut output),
            Err(VideoError::TooLarge)
        );
        assert!(output.is_empty());
    }

    #[test]
    fn malformed_aggregation_and_fragment_edges_never_return_partial_obus() {
        let cases: &[&[&[u8]]] = &[
            &[&[]],
            &[&[0x11, 0x30, 1]],
            &[&[0x98, 0x30, 1]],
            &[&[0x90, 1]],
            &[&[0x50, 0x30, 1]],
            &[&[0x50, 0x30, 1], &[0x10, 2]],
            &[&[0x20, 2, 0x30, 1]],
            &[&[0, 0]],
            &[&[0, 0x80]],
            &[&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0x10]],
            &[&[0x10, 0xB0, 1]],
            &[&[0x10, 0x31, 1]],
            &[&[0x10, 0x34]],
            &[&[0x10, 0x34, 1, 2]],
            &[&[0x10, 0x32, 2, 1]],
            &[&[0x20, 3, 0x2C, 0, 1, 0x2C, 8, 2]],
        ];
        for packets in cases {
            let mut output = vec![9];
            assert!(
                depacketize(packets, Framing::LowOverhead, &mut output).is_err(),
                "{packets:02x?}"
            );
            assert!(output.is_empty());
        }
    }

    #[test]
    fn every_truncation_and_bit_flip_is_handled_without_panicking() {
        let packet = [0x28, 3, 0x0C, 0, 1, 0x30, 2, 3];
        let mut output = Vec::new();
        for end in 0..=packet.len() {
            let _ = depacketize(&[&packet[..end]], Framing::LowOverhead, &mut output);
        }
        for index in 0..packet.len() {
            for bit in 0..8 {
                let mut changed = packet;
                changed[index] ^= 1 << bit;
                let _ = depacketize(&[&changed], Framing::LowOverhead, &mut output);
            }
        }
    }
}
