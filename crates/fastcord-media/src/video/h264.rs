//! H.264 over RTP (RFC 6184, packetization mode 1).
//!
//! Frames are Annex B access units. On the send side they have already been
//! through DAVE, whose H.264 transform rewrites every start code to the long
//! form `00 00 00 01`, leaves each NAL header (and a slice's first bytes)
//! readable, and guarantees the ciphertext holds no start-code sequence, so
//! start codes alone delimit NAL units, encrypted or not. The packetizer
//! therefore never looks inside a NAL, and the depacketizer reproduces the
//! same long start codes, so DAVE sees on receipt exactly the bytes it sealed.
//!
//! Sending uses single NAL unit packets and FU-A fragmentation. Receiving also
//! accepts STAP-A aggregation; interleaved formats (STAP-B, MTAP, FU-B) are
//! refused, as are decoding-order numbers.

use std::ops::Range;

use super::VideoError;

/// RFC 6184 §5.2 NAL unit types used on the wire.
const TYPE_STAP_A: u8 = 24;
const TYPE_FU_A: u8 = 28;
const TYPE_IDR: u8 = 5;
const LONG_START_CODE: [u8; 4] = [0, 0, 0, 1];
/// An FU-A payload starts with the FU indicator and the FU header.
const FU_PREFIX: usize = 2;
/// The fewest bytes a payload may hold for FU-A to make progress.
pub(crate) const MIN_PAYLOAD_BYTES: usize = FU_PREFIX + 1;

/// One RTP payload of an outgoing frame: `prefix` (an FU-A indicator and
/// header, or nothing for a single NAL unit packet) followed by `range` of
/// the frame. The payload is borrowed from the frame; nothing is copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Part {
    pub(crate) range: Range<usize>,
    pub(crate) prefix: [u8; FU_PREFIX],
    pub(crate) prefix_len: u8,
    /// The last payload of the frame: its RTP packet carries the marker bit.
    pub(crate) marker: bool,
}

impl Part {
    pub(crate) fn len(&self) -> usize {
        self.range.len() + usize::from(self.prefix_len)
    }

    /// Appends the RTP payload to `out`.
    pub(crate) fn write(&self, frame: &[u8], out: &mut Vec<u8>) {
        out.extend_from_slice(&self.prefix[..usize::from(self.prefix_len)]);
        out.extend_from_slice(&frame[self.range.clone()]);
    }
}

fn valid_nal_header(header: u8) -> bool {
    // forbidden_zero_bit clear, and a type that is a NAL unit rather than an
    // RTP aggregation or fragmentation marker.
    header & 0x80 == 0 && (1..TYPE_STAP_A).contains(&(header & 0x1F))
}

/// Where a start code begins and where its NAL unit begins, searching from
/// `from`. `00 00 00 01` is preferred over `00 00 01` at each position, so
/// the leading zero of a long start code is never mistaken for NAL content,
/// while zeros ending a NAL (possible in ciphertext) are kept.
fn next_start_code(frame: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= frame.len() {
        if frame[i] == 0 && frame[i + 1] == 0 {
            if frame[i + 2] == 1 {
                return Some((i, i + 3));
            }
            if frame[i + 2] == 0 && frame.get(i + 3) == Some(&1) {
                return Some((i, i + 4));
            }
        }
        i += 1;
    }
    None
}

/// The NAL unit byte ranges of an Annex B frame that begins with a start
/// code. Empty NAL units and NAL headers that cannot travel in an RTP single
/// NAL unit packet are errors.
pub(crate) fn nal_units(frame: &[u8]) -> Result<Vec<Range<usize>>, VideoError> {
    if frame.is_empty() || frame.len() > super::MAX_FRAME_BYTES {
        return Err(VideoError::TooLarge);
    }
    let mut ranges = Vec::new();
    let Some((first, mut begin)) = next_start_code(frame, 0) else {
        return Err(VideoError::Malformed);
    };
    if first != 0 {
        return Err(VideoError::Malformed);
    }
    loop {
        let next = next_start_code(frame, begin);
        let end = next.map_or(frame.len(), |(code, _)| code);
        let nal = frame.get(begin..end).ok_or(VideoError::Malformed)?;
        let &header = nal.first().ok_or(VideoError::Malformed)?;
        if !valid_nal_header(header) {
            return Err(VideoError::Malformed);
        }
        ranges.push(begin..end);
        if ranges.len() > super::MAX_PACKETS {
            return Err(VideoError::TooLarge);
        }
        match next {
            Some((_, nal_begin)) => begin = nal_begin,
            None => return Ok(ranges),
        }
    }
}

/// Splits `frame` into RTP payloads of at most `max_payload` bytes: a NAL
/// unit that fits is one packet, a larger one is fragmented into FU-A units.
/// The last payload carries the marker.
pub(crate) fn packetize(frame: &[u8], max_payload: usize) -> Result<Vec<Part>, VideoError> {
    debug_assert!(max_payload >= MIN_PAYLOAD_BYTES);
    let mut parts = Vec::new();
    for range in nal_units(frame)? {
        if range.len() <= max_payload {
            parts.push(Part {
                range,
                prefix: [0; FU_PREFIX],
                prefix_len: 0,
                marker: false,
            });
        } else {
            let header = frame[range.start];
            let indicator = (header & 0xE0) | TYPE_FU_A;
            let mut offset = range.start + 1;
            while offset < range.end {
                let end = (offset + max_payload - FU_PREFIX).min(range.end);
                let start = offset == range.start + 1;
                let last = end == range.end;
                let fu_header = (header & 0x1F) | u8::from(start) << 7 | u8::from(last) << 6;
                parts.push(Part {
                    range: offset..end,
                    prefix: [indicator, fu_header],
                    prefix_len: FU_PREFIX as u8,
                    marker: false,
                });
                offset = end;
            }
        }
        if parts.len() > super::MAX_PACKETS {
            return Err(VideoError::TooLarge);
        }
    }
    if let Some(last) = parts.last_mut() {
        last.marker = true;
    }
    Ok(parts)
}

fn push_nal_start(out: &mut Vec<u8>, header: u8) -> Result<(), VideoError> {
    if out.len() + LONG_START_CODE.len() + 1 > super::MAX_FRAME_BYTES {
        return Err(VideoError::TooLarge);
    }
    out.extend_from_slice(&LONG_START_CODE);
    out.push(header);
    Ok(())
}

fn push_nal(out: &mut Vec<u8>, nal: &[u8]) -> Result<(), VideoError> {
    if out.len() + LONG_START_CODE.len() + nal.len() > super::MAX_FRAME_BYTES {
        return Err(VideoError::TooLarge);
    }
    out.extend_from_slice(&LONG_START_CODE);
    out.extend_from_slice(nal);
    Ok(())
}

/// Reassembles the payloads of one frame, in order, into Annex B with long
/// start codes. Anything short of a complete, well-formed frame is an error
/// and leaves `out` empty: a decoder never sees partial output.
pub(crate) fn depacketize(payloads: &[&[u8]], out: &mut Vec<u8>) -> Result<(), VideoError> {
    out.clear();
    let result = assemble(payloads, out);
    if result.is_err() {
        out.clear();
    }
    result
}

fn assemble(payloads: &[&[u8]], out: &mut Vec<u8>) -> Result<(), VideoError> {
    // The NAL header of the fragmented unit in progress.
    let mut fragment: Option<u8> = None;
    for &payload in payloads {
        let &indicator = payload.first().ok_or(VideoError::Malformed)?;
        if indicator & 0x80 != 0 {
            return Err(VideoError::Malformed);
        }
        match indicator & 0x1F {
            1..=23 => {
                if fragment.is_some() {
                    return Err(VideoError::Incomplete);
                }
                push_nal(out, payload)?;
            }
            TYPE_STAP_A => {
                if fragment.is_some() {
                    return Err(VideoError::Incomplete);
                }
                let mut rest = &payload[1..];
                if rest.is_empty() {
                    return Err(VideoError::Malformed);
                }
                while !rest.is_empty() {
                    let size = rest.get(..2).ok_or(VideoError::Malformed)?;
                    let len = usize::from(u16::from_be_bytes([size[0], size[1]]));
                    let nal = rest.get(2..2 + len).ok_or(VideoError::Malformed)?;
                    if !nal.first().is_some_and(|&header| valid_nal_header(header)) {
                        return Err(VideoError::Malformed);
                    }
                    push_nal(out, nal)?;
                    rest = &rest[2 + len..];
                }
            }
            TYPE_FU_A => {
                let &fu_header = payload.get(1).ok_or(VideoError::Malformed)?;
                let body = payload
                    .get(FU_PREFIX..)
                    .filter(|body| !body.is_empty())
                    .ok_or(VideoError::Malformed)?;
                let start = fu_header & 0x80 != 0;
                let end = fu_header & 0x40 != 0;
                let nal_type = fu_header & 0x1F;
                let header = (indicator & 0x60) | nal_type;
                if fu_header & 0x20 != 0 || !valid_nal_header(header) || (start && end) {
                    return Err(VideoError::Malformed);
                }
                if start {
                    if fragment.is_some() {
                        return Err(VideoError::Incomplete);
                    }
                    push_nal_start(out, header)?;
                    fragment = Some(header);
                } else if fragment != Some(header) {
                    // A continuation without its start, or of another NAL.
                    return Err(VideoError::Incomplete);
                }
                if out.len() + body.len() > super::MAX_FRAME_BYTES {
                    return Err(VideoError::TooLarge);
                }
                out.extend_from_slice(body);
                if end {
                    fragment = None;
                }
            }
            // STAP-B, MTAP16/24, FU-B, and the reserved types.
            _ => return Err(VideoError::Malformed),
        }
    }
    if fragment.is_some() || out.is_empty() {
        return Err(VideoError::Incomplete);
    }
    Ok(())
}

/// Whether an Annex B frame contains an IDR picture, so a decoder can start
/// from it. Start codes of either length are recognized.
pub(crate) fn is_keyframe(frame: &[u8]) -> bool {
    let mut from = 0;
    while let Some((_, begin)) = next_start_code(frame, from) {
        if frame
            .get(begin)
            .is_some_and(|&header| header & 0x1F == TYPE_IDR)
        {
            return true;
        }
        from = begin;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../../../fixtures/video/ipcm-128x96.h264");

    fn payloads_of(frame: &[u8], parts: &[Part]) -> Vec<Vec<u8>> {
        parts
            .iter()
            .map(|part| {
                let mut bytes = Vec::new();
                part.write(frame, &mut bytes);
                bytes
            })
            .collect()
    }

    fn round_trip(frame: &[u8], max_payload: usize) -> Vec<Vec<u8>> {
        let parts = packetize(frame, max_payload).unwrap();
        assert!(parts.iter().all(|part| part.len() <= max_payload));
        assert!(parts.last().unwrap().marker);
        assert_eq!(parts.iter().filter(|part| part.marker).count(), 1);
        let payloads = payloads_of(frame, &parts);
        let borrowed: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
        let mut actual = Vec::new();
        depacketize(&borrowed, &mut actual).unwrap();
        assert_eq!(actual, frame, "max payload {max_payload}");
        payloads
    }

    #[test]
    fn nal_units_are_delimited_by_long_and_short_start_codes() {
        let frame = [
            0, 0, 0, 1, 0x67, 1, 2, // SPS
            0, 0, 1, 0x68, 3, // PPS behind a short code
            0, 0, 0, 1, 0x65, 9, 0, // IDR whose last byte is a zero
            0, 0, 0, 1, 0x41, 7,
        ];
        let units = nal_units(&frame).unwrap();
        assert_eq!(
            units.iter().map(|r| &frame[r.clone()]).collect::<Vec<_>>(),
            [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 9, 0], &[0x41, 7]]
        );
    }

    #[test]
    fn ciphertext_zeros_before_a_long_start_code_stay_in_their_nal_unit() {
        // DAVE ciphertext may end in zero bytes; only the four bytes
        // 00 00 00 01 are the next start code.
        let frame = [0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 0, 0, 1, 0x41, 0xBB];
        let units = nal_units(&frame).unwrap();
        assert_eq!(&frame[units[0].clone()], [0x65, 0xAA, 0, 0]);
        assert_eq!(&frame[units[1].clone()], [0x41, 0xBB]);
    }

    #[test]
    fn frames_that_are_not_annex_b_are_refused() {
        for bad in [
            &[][..],
            &[0x65, 1, 2, 3],
            // Leading garbage before the first start code.
            &[9, 0, 0, 0, 1, 0x65],
            // An empty NAL unit.
            &[0, 0, 0, 1, 0, 0, 0, 1, 0x65],
            &[0, 0, 0, 1],
            // forbidden_zero_bit set, NAL type 0, and RTP-only types.
            &[0, 0, 0, 1, 0xE5, 1],
            &[0, 0, 0, 1, 0x60, 1],
            &[0, 0, 0, 1, 0x78, 1],
            &[0, 0, 0, 1, 0x7C, 1],
        ] {
            assert!(nal_units(bad).is_err(), "{bad:02x?}");
        }
    }

    #[test]
    fn small_nal_units_travel_alone_and_the_last_packet_carries_the_marker() {
        let frame = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 9,
        ];
        let parts = packetize(&frame, 100).unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            payloads_of(&frame, &parts),
            [vec![0x67, 1, 2], vec![0x68, 3], vec![0x65, 9, 9, 9]]
        );
        assert_eq!(
            parts.iter().map(|part| part.marker).collect::<Vec<_>>(),
            [false, false, true]
        );
        round_trip(&frame, 100);
    }

    #[test]
    fn a_large_nal_unit_is_split_into_fu_a_with_start_and_end_bits() {
        // NAL header 0x65: NRI 3, type 5; payload 1..=10.
        let mut frame = vec![0, 0, 0, 1, 0x65];
        frame.extend(1..=10u8);
        let payloads = round_trip(&frame, 6);
        // Four bytes of data per packet after the two FU bytes.
        assert_eq!(
            payloads,
            [
                vec![0x7C, 0x85, 1, 2, 3, 4],
                vec![0x7C, 0x05, 5, 6, 7, 8],
                vec![0x7C, 0x45, 9, 10],
            ]
        );
    }

    #[test]
    fn the_smallest_legal_payload_still_makes_progress() {
        let frame = [0, 0, 0, 1, 0x65, 1, 2, 3, 4, 5];
        assert_eq!(round_trip(&frame, MIN_PAYLOAD_BYTES).len(), 5);
    }

    #[test]
    fn the_fixture_keyframe_and_slices_reconstruct_exactly_at_every_size() {
        // SPS, PPS, IDR, P slice, and IDR; 55,628 bytes of real H.264.
        for max_payload in [100, 1138, 1190, 1400, 20_000, 40_000] {
            let payloads = round_trip(FIXTURE, max_payload);
            if max_payload == 1138 {
                // SPS and PPS alone, then three 18,532-byte NAL units of
                // 18,531 data bytes in 1,136-byte FU-A fragments: 17 each.
                assert_eq!(payloads.len(), 2 + 3 * 17);
            }
        }
        assert!(is_keyframe(FIXTURE));
    }

    #[test]
    fn pseudo_random_ciphertext_like_frames_round_trip() {
        // Bytes that cannot form a start code, like DAVE ciphertext.
        let mut state = 0x1234_5678_u32;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        };
        for round in 0..64 {
            let mut frame = Vec::new();
            for nal in 0..1 + round % 5 {
                frame.extend_from_slice(&LONG_START_CODE);
                frame.push(if nal % 2 == 0 { 0x65 } else { 0x41 });
                let len = 1 + usize::from(next()) * (1 + round % 9);
                let mut zeros = 0;
                for _ in 0..len {
                    let mut byte = next();
                    // Keep 00 00 00 and 00 00 01 out of the payload.
                    if byte <= 1 && zeros >= 2 {
                        byte |= 0x80;
                    }
                    zeros = if byte == 0 { zeros + 1 } else { 0 };
                    frame.push(byte);
                }
            }
            for max_payload in [50, 1138] {
                round_trip(&frame, max_payload);
            }
        }
    }

    #[test]
    fn stap_a_aggregation_and_mixed_frames_are_depacketized() {
        let mut out = Vec::new();
        depacketize(
            &[&[0x78, 0, 3, 0x67, 1, 2, 0, 2, 0x68, 3], &[0x65, 9, 8]],
            &mut out,
        )
        .unwrap();
        assert_eq!(
            out,
            [
                0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 8
            ]
        );
        assert!(is_keyframe(&out));
    }

    #[test]
    fn broken_frames_are_errors_and_never_partial_output() {
        let mut out = vec![1, 2, 3];
        for (payloads, error) in [
            // Missing the end of a fragmented NAL unit.
            (
                vec![&[0x7C, 0x85, 1][..], &[0x7C, 0x05, 2]],
                VideoError::Incomplete,
            ),
            // A continuation without its start.
            (vec![&[0x7C, 0x45, 1][..]], VideoError::Incomplete),
            // Fragments of two different NAL units.
            (
                vec![&[0x7C, 0x85, 1][..], &[0x7C, 0x41, 2]],
                VideoError::Incomplete,
            ),
            // A new NAL unit begins before the fragmented one ended.
            (
                vec![&[0x7C, 0x85, 1][..], &[0x65, 2]],
                VideoError::Incomplete,
            ),
            // Start and end together are not a fragment.
            (vec![&[0x7C, 0xC5, 1][..]], VideoError::Malformed),
            // The reserved FU bit.
            (vec![&[0x7C, 0xA5, 1][..]], VideoError::Malformed),
            // An FU with no data, an empty payload, and F set.
            (vec![&[0x7C, 0x85][..]], VideoError::Malformed),
            (vec![&[][..]], VideoError::Malformed),
            (vec![&[0xE5, 1][..]], VideoError::Malformed),
            // STAP-A with a truncated aggregate, no aggregate, and a bad unit.
            (vec![&[0x78, 0, 9, 0x65][..]], VideoError::Malformed),
            (vec![&[0x78][..]], VideoError::Malformed),
            (vec![&[0x78, 0, 1, 0x00][..]], VideoError::Malformed),
            // Interleaved and reserved packet types are refused.
            (vec![&[0x79, 0, 0][..]], VideoError::Malformed),
            (vec![&[0x7A, 0, 0][..]], VideoError::Malformed),
            (vec![&[0x7B, 0, 0][..]], VideoError::Malformed),
            (vec![&[0x7D, 0, 0][..]], VideoError::Malformed),
            (vec![&[0x7E, 0, 0][..]], VideoError::Malformed),
            // Nothing at all.
            (Vec::new(), VideoError::Incomplete),
        ] {
            assert_eq!(
                depacketize(&payloads, &mut out),
                Err(error),
                "{payloads:02x?}"
            );
            assert!(out.is_empty(), "{payloads:02x?}");
        }
    }

    #[test]
    fn frames_over_the_byte_bound_are_refused_on_both_sides() {
        let mut huge = vec![0, 0, 0, 1, 0x41];
        huge.resize(super::super::MAX_FRAME_BYTES + 1, 0x55);
        assert_eq!(nal_units(&huge), Err(VideoError::TooLarge));
        let mut out = Vec::new();
        let chunk = vec![0x41u8; 60_000];
        let mut payloads: Vec<Vec<u8>> = vec![{
            let mut first = vec![0x7C, 0x81];
            first.extend_from_slice(&chunk);
            first
        }];
        for _ in 0..40 {
            let mut middle = vec![0x7C, 0x01];
            middle.extend_from_slice(&chunk);
            payloads.push(middle);
        }
        let borrowed: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
        assert_eq!(depacketize(&borrowed, &mut out), Err(VideoError::TooLarge));
        assert!(out.is_empty());
    }

    #[test]
    fn keyframes_are_found_by_idr_nal_units_only() {
        assert!(is_keyframe(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2]));
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 1, 0, 0, 0, 1, 0x61, 2]));
        assert!(!is_keyframe(&[]));
        assert!(!is_keyframe(&[0, 0, 0, 1]));
    }
}
