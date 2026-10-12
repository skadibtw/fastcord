//! Test-only AV1 RTP packetizer and IVF reader. fastcord never sends AV1, so
//! the tests need a stand-in for the official client that puts DAVE-sealed AV1
//! frames on the wire, and a genuine encoder's temporal units to seal.

use super::av1::{self, Framing};

/// The genuine libaom stream the AV1 tests share (`fixtures/video/`): three
/// 35×27 pictures, a keyframe starting the coded video sequence and two inter
/// frames, one temporal unit each (temporal delimiter, then sized OBUs).
pub(crate) const FIXTURE: &[u8] =
    include_bytes!("../../../../fixtures/video/av1-lossless-420-8.ivf");

/// One RTP payload and whether it ends the frame (the marker bit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Part {
    pub(crate) payload: Vec<u8>,
    pub(crate) marker: bool,
}

/// The temporal units of an IVF file (its 32-byte file header and 12-byte frame
/// headers removed).
pub(crate) fn ivf_packets(ivf: &[u8]) -> Vec<&[u8]> {
    assert_eq!(&ivf[..4], b"DKIF");
    assert_eq!(&ivf[8..12], b"AV01");
    let header = usize::from(u16::from_le_bytes([ivf[6], ivf[7]]));
    let mut rest = &ivf[header..];
    let mut packets = Vec::new();
    while !rest.is_empty() {
        let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        packets.push(&rest[12..12 + len]);
        rest = &rest[12 + len..];
    }
    packets
}

/// The fixture's three temporal units.
pub(crate) fn fixture_units() -> Vec<&'static [u8]> {
    let units = ivf_packets(FIXTURE);
    assert_eq!(units.len(), 3);
    units
}

/// What a viewer receives for one fixture temporal unit: the same OBUs without
/// the two-byte temporal delimiter (`12 00`), which RTP never carries. The
/// encoder wrote minimal OBU sizes, so no other byte changes.
pub(crate) fn without_delimiter(unit: &[u8]) -> &[u8] {
    unit.strip_prefix(&[0x12, 0x00])
        .expect("a temporal delimiter leads")
}

fn leb(mut value: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

fn read_leb(bytes: &[u8]) -> (usize, usize) {
    let mut value = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        value |= usize::from(byte & 0x7F) << (7 * index);
        if byte & 0x80 == 0 {
            return (value, index + 1);
        }
    }
    panic!("unterminated LEB128");
}

/// Each OBU of `frame` as one RTP element: its header with `obu_has_size_field`
/// cleared, the extension byte, and the payload (AV1 RTP §4.4). Accepts sized
/// OBUs and a final unsized OBU, which is how DAVE writes a sealed frame.
fn elements(frame: &[u8]) -> Vec<Vec<u8>> {
    let mut elements = Vec::new();
    let mut rest = frame;
    while !rest.is_empty() {
        let header = rest[0];
        let header_len = 1 + usize::from(header & 4 != 0);
        let (payload_len, size_len) = if header & 2 != 0 {
            read_leb(&rest[header_len..])
        } else {
            (rest.len() - header_len, 0)
        };
        let start = header_len + size_len;
        let mut element = vec![header & !2];
        if header_len == 2 {
            element.push(rest[1]);
        }
        element.extend_from_slice(&rest[start..start + payload_len]);
        elements.push(element);
        rest = &rest[start + payload_len..];
    }
    elements
}

/// Splits one frame into RTP payloads of at most `max_payload` bytes: every
/// element is length-prefixed (W = 0), as many whole elements as fit share a
/// packet, and one that does not fit is fragmented (Y/Z). `new_sequence` sets N
/// on the first packet, as libwebrtc does for a keyframe.
pub(crate) fn packetize(frame: &[u8], max_payload: usize, new_sequence: bool) -> Vec<Part> {
    assert!(max_payload >= 8);
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    let mut header = if new_sequence { 0x08 } else { 0 };
    let mut body: Vec<u8> = Vec::new();
    for element in elements(frame) {
        let mut rest = &element[..];
        loop {
            let space = max_payload - 1 - body.len();
            let prefix = leb(rest.len());
            if prefix.len() + rest.len() <= space {
                body.extend_from_slice(&prefix);
                body.extend_from_slice(rest);
                break;
            }
            if space < 2 {
                // No room for a length byte and one byte of data.
                payloads.push([&[header][..], &body].concat());
                header = 0;
                body.clear();
                continue;
            }
            let mut chunk = space - 1;
            while leb(chunk).len() + chunk > space {
                chunk -= 1;
            }
            body.extend_from_slice(&leb(chunk));
            body.extend_from_slice(&rest[..chunk]);
            payloads.push([&[header | 0x40][..], &body].concat());
            header = 0x80;
            body.clear();
            rest = &rest[chunk..];
        }
    }
    if !body.is_empty() {
        payloads.push([&[header][..], &body].concat());
    }
    let last = payloads.len() - 1;
    payloads
        .into_iter()
        .enumerate()
        .map(|(index, payload)| Part {
            payload,
            marker: index == last,
        })
        .collect()
}

#[test]
fn the_fixture_is_three_genuine_temporal_units_starting_with_a_delimiter() {
    let units = fixture_units();
    for unit in &units {
        assert_eq!(&unit[..2], &[0x12, 0x00]);
    }
    // The first carries a sequence header (OBU type 1), the others do not.
    assert_eq!(units[0][2] >> 3 & 15, 1);
    assert_ne!(units[1][2] >> 3 & 15, 1);
    assert_eq!(without_delimiter(units[0]).len(), units[0].len() - 2);
}

#[test]
fn the_test_packetizer_round_trips_through_the_depacketizer_at_every_size() {
    for (index, unit) in fixture_units().into_iter().enumerate() {
        let expected = without_delimiter(unit);
        for max_payload in [8, 9, 10, 17, 40, 100, 129, 300, 1138] {
            let parts = packetize(unit, max_payload, index == 0);
            assert!(parts.iter().all(|part| part.payload.len() <= max_payload));
            assert_eq!(parts.iter().filter(|part| part.marker).count(), 1);
            assert!(parts.last().unwrap().marker);
            let payloads: Vec<&[u8]> = parts.iter().map(|part| part.payload.as_slice()).collect();
            let mut out = Vec::new();
            assert_eq!(
                av1::depacketize(&payloads, Framing::LowOverhead, &mut out),
                Ok(index == 0)
            );
            assert_eq!(out, expected, "unit {index} at {max_payload}");
            // The DAVE boundary leaves the final OBU unsized; sizes come back
            // after authentication.
            let mut sealed_shape = Vec::new();
            av1::depacketize(&payloads, Framing::Dave, &mut sealed_shape).unwrap();
            assert_ne!(sealed_shape, out);
            let mut restored = Vec::new();
            av1::normalize(&sealed_shape, &mut restored).unwrap();
            assert_eq!(restored, expected);
        }
    }
}

#[test]
fn small_payloads_fragment_and_large_ones_aggregate() {
    let unit = fixture_units()[0];
    assert!(packetize(unit, 40, true).len() > 20);
    // The sequence header and frame OBUs share one packet when they fit.
    assert_eq!(packetize(unit, 4000, true).len(), 1);
    let parts = packetize(unit, 40, true);
    assert_eq!(parts[0].payload[0] & 0xC8, 0x48, "N and Y on the first");
    assert!(parts[1..].iter().all(|part| part.payload[0] & 0x08 == 0));
    assert!(
        parts[1..]
            .iter()
            .all(|part| part.payload[0] & 0x80 != 0 || part.payload[0] == 0)
    );
}
