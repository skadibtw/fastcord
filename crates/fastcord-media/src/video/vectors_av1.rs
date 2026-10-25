//! `fixtures/voice/av1-rtp-vectors.json`: AV1 RTP packetization, RTX, and
//! rtpsize transport vectors from an independent implementation
//! (`fixtures/voice/gen_av1_rtp_vectors.py`: AOM AV1-RTP v1.0.0, the AV1
//! bitstream specification, RFC 4588/8285, and pycryptodome). The expected
//! OBU streams are serialized from synthetic OBU objects, never decoded from
//! the generated packets. The tables cover every aggregation shape, fragment
//! and legal size-field form, loss/reorder/duplicate delivery across the
//! 16-bit sequence wrap, 44 malformed payloads, and sealed datagrams in both
//! AEAD modes.

use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use super::av1::{self, Framing};
use super::receive::{Assembled, FRAME_DEADLINE, REORDER_WAIT, ReceiveBuffer};
use super::{MAX_DATAGRAM_BYTES, MAX_FRAME_BYTES, VideoError};
use crate::crypto::{TransportCipher, TransportKey, TransportMode};
use crate::rtp::{RtpHeader, ext};

const VECTORS: &str = include_str!("../../../../fixtures/voice/av1-rtp-vectors.json");

/// Bytes an AEAD packet carries after its ciphertext: the tag and the nonce.
const SEAL_OVERHEAD: usize = 16 + 4;

fn hex_str(text: &str) -> Vec<u8> {
    assert!(
        text.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "not hexadecimal: {text}"
    );
    let (pairs, remainder) = text.as_bytes().as_chunks::<2>();
    assert!(remainder.is_empty(), "odd hex length: {text}");
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn hex(value: &Value) -> Vec<u8> {
    hex_str(value.as_str().unwrap())
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap()
}

fn int<T>(value: &Value) -> T
where
    T: TryFrom<u64>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.as_u64().unwrap()).unwrap()
}

fn vectors() -> Value {
    serde_json::from_str(VECTORS).unwrap()
}

fn frame<'a>(vectors: &'a Value, name: &str) -> &'a Value {
    vectors["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|frame| frame["name"] == name)
        .unwrap_or_else(|| panic!("no frame named {name}"))
}

fn payloads(value: &Value) -> Vec<Vec<u8>> {
    value.as_array().unwrap().iter().map(hex).collect()
}

/// The primary stream's identity: what every media packet of a frame carries.
struct Stream {
    ssrc: u32,
    payload_type: u8,
}

fn stream(vectors: &Value) -> Stream {
    Stream {
        ssrc: int(&vectors["primary_ssrc"]),
        payload_type: int(&vectors["payload_type"]),
    }
}

type Packet = (RtpHeader, Vec<u8>);

/// A frame's packets as (header, payload), headers as the vector describes.
fn frame_packets(stream: &Stream, frame: &Value) -> Vec<Packet> {
    let first: u16 = int(&frame["first_sequence"]);
    let timestamp: u32 = int(&frame["timestamp"]);
    let payloads = payloads(&frame["payloads"]);
    let markers = frame["markers"].as_array().unwrap();
    assert_eq!(payloads.len(), markers.len(), "{}", frame["name"]);
    payloads
        .into_iter()
        .zip(markers)
        .enumerate()
        .map(|(index, (payload, marker))| {
            let sequence = first.wrapping_add(u16::try_from(index).unwrap());
            let mut header = RtpHeader::new(stream.payload_type, sequence, timestamp, stream.ssrc);
            header.marker = marker.as_bool().unwrap();
            (header, payload)
        })
        .collect()
}

fn depack(payloads: &[Vec<u8>], framing: Framing) -> Result<(bool, Vec<u8>), VideoError> {
    let borrowed: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
    let mut out = Vec::new();
    let new_sequence = av1::depacketize(&borrowed, framing, &mut out)?;
    Ok((new_sequence, out))
}

/// Both framings reproduce the vector's streams, the N bit is reported, and
/// normalizing the DAVE form restores the canonical sized stream.
fn assert_depacketizes(label: &str, payloads: &[Vec<u8>], frame: &Value) {
    let (new_sequence, low) = depack(payloads, Framing::LowOverhead)
        .unwrap_or_else(|error| panic!("{label}: low-overhead: {error:?}"));
    assert_eq!(low, hex(&frame["obu_stream"]), "{label}: low-overhead");
    assert_eq!(
        new_sequence,
        frame["new_sequence"].as_bool().unwrap(),
        "{label}: N bit"
    );
    let (dave_new_sequence, dave) =
        depack(payloads, Framing::Dave).unwrap_or_else(|error| panic!("{label}: dave: {error:?}"));
    assert_eq!(dave, hex(&frame["dave_obu_stream"]), "{label}: dave");
    assert_eq!(dave_new_sequence, new_sequence, "{label}: dave N bit");
    let mut restored = Vec::new();
    av1::normalize(&dave, &mut restored)
        .unwrap_or_else(|error| panic!("{label}: normalize: {error:?}"));
    assert_eq!(restored, low, "{label}: normalize");
}

/// Neither framing accepts the payloads, and a refusal leaves no partial
/// output behind.
fn assert_refused(label: &str, payloads: &[Vec<u8>]) {
    let borrowed: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
    for (name, framing) in [
        ("low-overhead", Framing::LowOverhead),
        ("dave", Framing::Dave),
    ] {
        let mut out = vec![0xAA; 3];
        let result = av1::depacketize(&borrowed, framing, &mut out);
        assert!(result.is_err(), "{label}: {name} accepted a damaged frame");
        assert!(out.is_empty(), "{label}: {name} left partial output");
    }
}

/// Every frame `pop_frame` has ready, in order.
fn drain(buffer: &mut ReceiveBuffer, now: Instant, out: &mut Vec<Assembled>) {
    while let Some(assembled) = buffer.pop_frame(now) {
        out.push(assembled);
    }
}

#[test]
fn the_vector_document_has_the_expected_shape() {
    let vectors = vectors();
    assert_eq!(int::<u64>(&vectors["schema_version"]), 1);
    assert_eq!(int::<usize>(&vectors["max_frame_bytes"]), MAX_FRAME_BYTES);
    assert_eq!(hex(&vectors["key"]).len(), 32);
    assert_eq!(int::<u32>(&vectors["primary_ssrc"]), 13_970);
    assert_eq!(int::<u32>(&vectors["rtx_ssrc"]), 13_971);
    assert_eq!(int::<u8>(&vectors["payload_type"]), 111);
    assert_eq!(int::<u8>(&vectors["rtx_payload_type"]), 112);

    let frames = vectors["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 18);
    let mut names: Vec<&str> = frames.iter().map(|frame| text(&frame["name"])).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 18, "frame names are unique");
    for frame in frames {
        let name = text(&frame["name"]);
        let low = hex(&frame["obu_stream"]);
        let dave = hex(&frame["dave_obu_stream"]);
        assert!(!low.is_empty() && !dave.is_empty(), "{name}");
        // The canonical stream sizes every OBU, the DAVE form leaves its final
        // OBU unsized: it differs, and is at least a size byte shorter.
        assert_ne!(low, dave, "{name}");
        assert!(dave.len() < low.len(), "{name}");
        assert!(!payloads(&frame["payloads"]).is_empty(), "{name}");
        frame["new_sequence"].as_bool().unwrap();
        int::<u16>(&frame["first_sequence"]);
        int::<u32>(&frame["timestamp"]);
    }

    let cases = vectors["delivery_cases"].as_array().unwrap();
    assert_eq!(cases.len(), 8);
    for case in cases {
        let name = text(&case["name"]);
        let frame = frame(&vectors, text(&case["frame"]));
        let count = frame["payloads"].as_array().unwrap().len();
        let complete = case["expect_complete"].as_bool().unwrap();
        let order = case["arrival_order"].as_array().unwrap();
        assert!(
            order.iter().all(|index| int::<usize>(index) < count),
            "{name}"
        );
        assert_eq!(
            complete,
            case["missing_sequences"].as_array().unwrap().is_empty()
        );
        assert_eq!(
            int::<usize>(&case["expected_frame_count"]),
            usize::from(complete)
        );
        assert_eq!(
            text(&case["finish"]),
            if complete { "packet" } else { "deadline" }
        );
    }

    let malformed = vectors["malformed"].as_array().unwrap();
    assert_eq!(malformed.len(), 44);
    for case in malformed {
        let name = text(&case["name"]);
        assert!(
            matches!(text(&case["outcome"]), "reject" | "drop_incomplete"),
            "{name}"
        );
        assert!(
            matches!(text(&case["finish"]), "packet" | "deadline"),
            "{name}"
        );
        assert!(!case["expect_complete"].as_bool().unwrap(), "{name}");
        assert_eq!(int::<usize>(&case["expected_frame_count"]), 0, "{name}");
        assert!(!text(&case["reason"]).is_empty(), "{name}");
        let packets = case["packets"].as_array().unwrap();
        assert!(!packets.is_empty(), "{name}");
        for packet in packets {
            int::<u16>(&packet["sequence"]);
            int::<u32>(&packet["timestamp"]);
            packet["marker"].as_bool().unwrap();
            hex(&packet["payload"]);
        }
    }

    let transport = &vectors["transport"];
    assert_eq!(
        frame(&vectors, text(&transport["frame"]))["payloads"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let modes = transport["modes"].as_array().unwrap();
    assert_eq!(modes.len(), 2);
    for mode in modes {
        assert!(TransportMode::from_wire(text(&mode["mode"])).is_some());
        let packets = mode["packets"].as_array().unwrap();
        assert_eq!(packets.len(), 8);
        for (index, packet) in packets.iter().enumerate() {
            assert_eq!(
                text(&packet["kind"]),
                if index < 5 { "media" } else { "rtx" }
            );
        }
    }
    assert_eq!(transport["delivery_cases"].as_array().unwrap().len(), 7);
}

#[test]
fn every_independent_frame_depacketizes_to_the_exact_obu_stream() {
    let vectors = vectors();
    let frames = vectors["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 18);
    for frame in frames {
        let name = text(&frame["name"]);
        let payloads = payloads(&frame["payloads"]);
        let markers = frame["markers"].as_array().unwrap();
        assert_eq!(markers.len(), payloads.len(), "{name}");
        for (index, marker) in markers.iter().enumerate() {
            assert_eq!(
                marker.as_bool().unwrap(),
                index == payloads.len() - 1,
                "{name}: marker of packet {index}"
            );
        }
        // The N bit of the first aggregation header is the coded video
        // sequence flag; only the first packet may carry it.
        assert_eq!(
            payloads[0][0] & 0x08 != 0,
            frame["new_sequence"].as_bool().unwrap(),
            "{name}"
        );
        assert_depacketizes(name, &payloads, frame);
    }
}

#[tokio::test(start_paused = true)]
async fn every_frame_reassembles_through_the_receive_buffer() {
    let vectors = vectors();
    let stream = stream(&vectors);
    let frames = vectors["frames"].as_array().unwrap();
    assert!(
        frames
            .iter()
            .any(|frame| int::<u16>(&frame["first_sequence"]) == 65_533),
        "one vector crosses the 16-bit wrap"
    );
    for frame in frames {
        let name = text(&frame["name"]);
        let packets = frame_packets(&stream, frame);
        let now = Instant::now();
        let mut buffer = ReceiveBuffer::new();
        for (index, (header, payload)) in packets.iter().enumerate() {
            assert!(
                buffer.pop_frame(now).is_none(),
                "{name}: early frame at {index}"
            );
            buffer.push(now, header, payload.as_slice()).unwrap();
        }
        let assembled = buffer
            .pop_frame(now)
            .unwrap_or_else(|| panic!("{name}: no frame"));
        assert_eq!(
            assembled.timestamp,
            int::<u32>(&frame["timestamp"]),
            "{name}"
        );
        assert_eq!(assembled.payloads, payloads(&frame["payloads"]), "{name}");
        assert_depacketizes(name, &assembled.payloads, frame);
        assert!(buffer.pop_frame(now).is_none(), "{name}");
        assert!(!buffer.take_broken(), "{name}");
        assert_eq!(buffer.buffered_bytes(), 0, "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn delivery_cases_emit_only_complete_frames_exactly_once() {
    let vectors = vectors();
    let stream = stream(&vectors);
    let cases = vectors["delivery_cases"].as_array().unwrap();
    assert_eq!(cases.len(), 8);
    for case in cases {
        let name = text(&case["name"]);
        let frame = frame(&vectors, text(&case["frame"]));
        let packets = frame_packets(&stream, frame);
        let order: Vec<usize> = case["arrival_order"]
            .as_array()
            .unwrap()
            .iter()
            .map(int::<usize>)
            .collect();
        let complete = case["expect_complete"].as_bool().unwrap();
        let expected_count: usize = int(&case["expected_frame_count"]);

        let mut buffer = ReceiveBuffer::new();
        let mut emitted = Vec::new();
        let mut now = Instant::now();
        for &index in &order {
            now += Duration::from_millis(1);
            let (header, payload) = &packets[index];
            buffer.push(now, header, payload.as_slice()).unwrap();
            drain(&mut buffer, now, &mut emitted);
        }

        // Only a frame whose packets all arrived may come out, once, intact.
        let mut produced = 0;
        for assembled in &emitted {
            if depack(&assembled.payloads, Framing::LowOverhead).is_ok() {
                produced += 1;
                assert_eq!(
                    assembled.timestamp,
                    int::<u32>(&frame["timestamp"]),
                    "{name}"
                );
                assert_depacketizes(name, &assembled.payloads, frame);
            } else {
                assert_refused(name, &assembled.payloads);
            }
        }
        assert_eq!(produced, expected_count, "{name}: frames produced");
        assert_eq!(complete, produced == 1, "{name}");
        if order[0] != 0 {
            // A receiver starts at its first packet. Here that is a fragment
            // in the middle of the frame: the run up to the marker is
            // contiguous, so the buffer hands it over, and the depacketizer
            // refuses the headless fragment.
            assert_eq!(emitted.len(), 1, "{name}: headless run is assembled");
            assert!(!complete, "{name}");
        } else {
            assert_eq!(emitted.len(), expected_count, "{name}: nothing partial");
        }

        let pending = buffer.buffered_bytes() > 0;
        if text(&case["finish"]) == "deadline" {
            let late = now + FRAME_DEADLINE;
            assert!(buffer.pop_frame(late).is_none(), "{name}");
            assert_eq!(buffer.expire(late), pending, "{name}: expire");
            assert_eq!(buffer.take_broken(), pending, "{name}: broken");
            assert!(buffer.pop_frame(late).is_none(), "{name}");
        } else {
            assert!(!pending, "{name}: complete frames leave nothing behind");
            assert!(!buffer.take_broken(), "{name}: broken");
        }
        assert_eq!(buffer.buffered_bytes(), 0, "{name}: buffer empty");
    }
}

#[tokio::test(start_paused = true)]
async fn malformed_vectors_never_produce_a_frame() {
    let vectors = vectors();
    let stream = stream(&vectors);
    let cases = vectors["malformed"].as_array().unwrap();
    assert_eq!(cases.len(), 44);
    for case in cases {
        let name = text(&case["name"]);
        let finish = text(&case["finish"]);
        let mut buffer = ReceiveBuffer::new();
        let mut assembled = Vec::new();
        let mut now = Instant::now();
        for packet in case["packets"].as_array().unwrap() {
            now += Duration::from_millis(1);
            let mut header = RtpHeader::new(
                stream.payload_type,
                int(&packet["sequence"]),
                int(&packet["timestamp"]),
                stream.ssrc,
            );
            header.marker = packet["marker"].as_bool().unwrap();
            let payload = hex(&packet["payload"]);
            // Only an empty payload is refused outright; everything else is
            // the depacketizer's to judge.
            assert_eq!(
                buffer.push(now, &header, payload.as_slice()).is_err(),
                payload.is_empty(),
                "{name}: push"
            );
            drain(&mut buffer, now, &mut assembled);
        }
        for frame in &assembled {
            assert_refused(name, &frame.payloads);
        }

        let pending = buffer.buffered_bytes() > 0;
        if finish == "deadline" {
            assert!(pending, "{name}: an unfinished frame is waiting");
            assert!(assembled.is_empty(), "{name}: nothing assembled");
            let late = now + FRAME_DEADLINE;
            assert!(buffer.pop_frame(late).is_none(), "{name}");
            assert!(buffer.expire(late), "{name}: expire");
            assert!(buffer.take_broken(), "{name}: broken");
        } else {
            assert_eq!(finish, "packet", "{name}");
            assert!(!pending, "{name}: the marker packet closes the frame");
        }
        assert!(buffer.pop_frame(now + FRAME_DEADLINE).is_none(), "{name}");
        assert_eq!(buffer.buffered_bytes(), 0, "{name}: buffer empty");
    }
}

/// One opened transport packet.
struct Opened {
    /// The header as sent (the RTX stream's own for a repair).
    header: RtpHeader,
    /// The header `deliver_video_packet` pushes: a repair is unwrapped onto
    /// the primary stream with its original sequence number.
    stream_header: RtpHeader,
    /// The original sequence number of an RTX repair.
    repair: Option<u16>,
    /// The RTP payload, without extensions or the RTX original sequence.
    payload: Vec<u8>,
}

/// Opens one sealed datagram and checks everything the vector says about it.
fn open_entry(
    vectors: &Value,
    cipher: &TransportCipher,
    mode: &str,
    index: usize,
    entry: &Value,
) -> Opened {
    let label = format!("{mode} packet {index}");
    let datagram = hex(&entry["datagram"]);
    assert!(datagram.len() <= MAX_DATAGRAM_BYTES, "{label}");
    let header = RtpHeader::parse(&datagram).unwrap();
    let rtx = match text(&entry["kind"]) {
        "media" => false,
        "rtx" => true,
        other => panic!("{label}: unknown kind {other}"),
    };
    let (ssrc, payload_type) = if rtx {
        (
            int::<u32>(&vectors["rtx_ssrc"]),
            int::<u8>(&vectors["rtx_payload_type"]),
        )
    } else {
        (
            int::<u32>(&vectors["primary_ssrc"]),
            int::<u8>(&vectors["payload_type"]),
        )
    };
    assert_eq!(header.sequence, int::<u16>(&entry["sequence"]), "{label}");
    assert_eq!(header.marker, entry["marker"].as_bool().unwrap(), "{label}");
    assert_eq!(header.timestamp, int::<u32>(&entry["timestamp"]), "{label}");
    assert_eq!(header.ssrc, int::<u32>(&entry["ssrc"]), "{label}");
    assert_eq!(
        header.payload_type,
        int::<u8>(&entry["payload_type"]),
        "{label}"
    );
    assert_eq!(
        (header.ssrc, header.payload_type),
        (ssrc, payload_type),
        "{label}"
    );

    // The clear part is the AAD; the rest is ciphertext, tag, and nonce.
    let clear = hex(&entry["clear"]);
    assert_eq!(&datagram[..header.clear_len()], clear.as_slice(), "{label}");
    let plaintext = hex(&entry["plaintext"]);
    assert_eq!(
        datagram.len(),
        clear.len() + plaintext.len() + SEAL_OVERHEAD,
        "{label}"
    );
    let mut body = Vec::new();
    let nonce = cipher
        .open(&datagram, header.clear_len(), &mut body)
        .unwrap_or_else(|error| panic!("{label}: {error}"));
    assert_eq!(
        u64::from(nonce),
        entry["nonce"].as_u64().unwrap(),
        "{label}"
    );
    assert_eq!(
        &datagram[datagram.len() - 4..],
        nonce.to_be_bytes().as_slice(),
        "{label}"
    );
    assert_eq!(body, plaintext, "{label}");

    // Any flipped bit (clear header, ciphertext, tag, nonce) fails.
    let mut scratch = Vec::new();
    for position in [
        4,
        header.clear_len(),
        datagram.len() - 5,
        datagram.len() - 1,
    ] {
        let mut tampered = datagram.clone();
        tampered[position] ^= 1;
        assert!(
            cipher
                .open(&tampered, header.clear_len(), &mut scratch)
                .is_err(),
            "{label}: a flipped bit at {position} was accepted"
        );
    }

    let split = header.split_body(&body).unwrap();
    let elements: Vec<(u8, Vec<u8>)> = split
        .extensions
        .elements()
        .map(|element| {
            let element = element.unwrap();
            (element.id, element.data.to_vec())
        })
        .collect();
    let sent = hex(&entry["payload"]);
    let abs_send_time = vec![0x12, 0x34, 0x56];
    let repair = if rtx {
        assert_eq!(
            elements,
            [
                (ext::ABS_SEND_TIME, abs_send_time),
                (ext::REPAIRED_RTP_STREAM_ID, b"100".to_vec())
            ],
            "{label}"
        );
        // RFC 4588: the original sequence number, then its payload.
        let original = u16::from_be_bytes([split.payload[0], split.payload[1]]);
        assert_eq!(original, int::<u16>(&entry["original_sequence"]), "{label}");
        assert_eq!(&split.payload[2..], sent.as_slice(), "{label}");
        Some(original)
    } else {
        assert_eq!(
            elements,
            [
                (ext::ABS_SEND_TIME, abs_send_time),
                (ext::PLAYOUT_DELAY, vec![0, 0, 0]),
                (ext::VIDEO_CONTENT_TYPE, vec![1]),
                (ext::RTP_STREAM_ID, b"100".to_vec())
            ],
            "{label}"
        );
        assert_eq!(split.payload, sent.as_slice(), "{label}");
        None
    };
    let stream_header = match repair {
        Some(original) => RtpHeader {
            ssrc: int(&vectors["primary_ssrc"]),
            sequence: original,
            ..header
        },
        None => header,
    };
    Opened {
        header,
        stream_header,
        repair,
        payload: sent,
    }
}

fn open_mode(vectors: &Value, mode: &Value) -> (String, Vec<Opened>) {
    let key = TransportKey::from_slice(&hex(&vectors["key"])).unwrap();
    let name = text(&mode["mode"]).to_owned();
    let cipher = TransportCipher::new(TransportMode::from_wire(&name).unwrap(), &key);
    let entries = mode["packets"].as_array().unwrap();
    assert_eq!(entries.len(), 8, "{name}");
    let opened: Vec<Opened> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| open_entry(vectors, &cipher, &name, index, entry))
        .collect();
    for (index, packet) in opened.iter().enumerate() {
        assert_eq!(packet.repair.is_some(), index >= 5, "{name} packet {index}");
    }
    (name, opened)
}

#[test]
fn sealed_av1_datagrams_open_parse_and_reassemble_in_both_transport_modes() {
    let vectors = vectors();
    let frame = frame(&vectors, text(&vectors["transport"]["frame"]));
    let modes = vectors["transport"]["modes"].as_array().unwrap();
    assert_eq!(modes.len(), 2);
    for mode in modes {
        let (name, opened) = open_mode(&vectors, mode);
        let mut buffer = ReceiveBuffer::new();
        let now = Instant::now();
        for (index, packet) in opened.iter().enumerate() {
            match packet.repair {
                None => buffer
                    .push(now, &packet.stream_header, packet.payload.as_slice())
                    .unwrap(),
                // A repair repeats an original packet byte for byte.
                Some(original) => assert!(
                    opened[..5].iter().any(|media| {
                        media.header.sequence == original && media.payload == packet.payload
                    }),
                    "{name} packet {index}: repairs nothing"
                ),
            }
        }
        let assembled = buffer
            .pop_frame(now)
            .unwrap_or_else(|| panic!("{name}: no frame"));
        assert_eq!(
            assembled.timestamp,
            int::<u32>(&frame["timestamp"]),
            "{name}"
        );
        assert_depacketizes(&name, &assembled.payloads, frame);
        assert!(buffer.pop_frame(now).is_none(), "{name}");
        assert_eq!(buffer.buffered_bytes(), 0, "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn transport_deliveries_are_requested_repaired_or_given_up_like_the_driver() {
    let vectors = vectors();
    let transport = &vectors["transport"];
    let frame = frame(&vectors, text(&transport["frame"]));
    let retry = Duration::from_millis(40);
    let cases = transport["delivery_cases"].as_array().unwrap();
    assert_eq!(cases.len(), 7);
    for mode in transport["modes"].as_array().unwrap() {
        let (mode_name, opened) = open_mode(&vectors, mode);
        for case in cases {
            let label = format!("{mode_name}/{}", text(&case["name"]));
            let order: Vec<usize> = case["arrival_order"]
                .as_array()
                .unwrap()
                .iter()
                .map(int::<usize>)
                .collect();
            let requested: Vec<u16> = case["requested_sequences"]
                .as_array()
                .unwrap()
                .iter()
                .map(int::<u16>)
                .collect();
            let missing: Vec<u16> = case["missing_sequences"]
                .as_array()
                .unwrap()
                .iter()
                .map(int::<u16>)
                .collect();
            let complete = case["expect_complete"].as_bool().unwrap();
            let expected_count: usize = int(&case["expected_frame_count"]);
            assert_eq!(expected_count, usize::from(complete), "{label}");
            assert_eq!(
                missing,
                if complete {
                    Vec::new()
                } else {
                    requested.clone()
                },
                "{label}"
            );
            assert_eq!(
                text(&case["finish"]),
                if complete { "packet" } else { "deadline" },
                "{label}"
            );

            // Everything before the first RTX repair arrives together; the
            // repair (and whatever follows) a while later.
            let split = order
                .iter()
                .position(|&index| opened[index].repair.is_some())
                .unwrap_or(order.len());
            let start = Instant::now();
            let repaired_at = start + Duration::from_millis(30);
            let mut buffer = ReceiveBuffer::new();
            let mut emitted = Vec::new();
            for &index in &order[..split] {
                let packet = &opened[index];
                buffer
                    .push(start, &packet.stream_header, packet.payload.as_slice())
                    .unwrap();
                drain(&mut buffer, start, &mut emitted);
            }

            // A gap is only known once a later packet arrives: a lost packet
            // after the highest one received cannot be requested. Reordering
            // gets REORDER_WAIT before anything is named.
            let highest = order[..split]
                .iter()
                .copied()
                .filter(|&index| index < 5)
                .max();
            let expected_nacks: Vec<u16> = requested
                .iter()
                .copied()
                .filter(|&sequence| {
                    let index = opened[..5]
                        .iter()
                        .position(|media| media.header.sequence == sequence)
                        .unwrap();
                    assert!(
                        !order[..split].contains(&index),
                        "{label}: {sequence} is requested but arrived"
                    );
                    Some(index) < highest
                })
                .collect();
            assert!(
                buffer
                    .nacks(start + REORDER_WAIT - Duration::from_millis(1), retry)
                    .is_empty(),
                "{label}: nack before the reorder wait"
            );
            assert_eq!(
                buffer.nacks(start + REORDER_WAIT, retry),
                expected_nacks,
                "{label}: nacks"
            );

            let now = if split < order.len() {
                repaired_at
            } else {
                start
            };
            for &index in &order[split..] {
                let packet = &opened[index];
                buffer
                    .push(now, &packet.stream_header, packet.payload.as_slice())
                    .unwrap();
                drain(&mut buffer, now, &mut emitted);
            }

            // The frame comes out once, exactly, or never; repeated repairs
            // and duplicate datagrams add nothing.
            assert_eq!(emitted.len(), expected_count, "{label}: frames");
            for assembled in &emitted {
                assert_eq!(
                    assembled.timestamp,
                    int::<u32>(&frame["timestamp"]),
                    "{label}"
                );
                assert_eq!(assembled.payloads, payloads(&frame["payloads"]), "{label}");
                assert_depacketizes(&label, &assembled.payloads, frame);
            }
            let pending = buffer.buffered_bytes() > 0;
            if text(&case["finish"]) == "deadline" {
                assert!(pending, "{label}: an unfinished frame is waiting");
                let late = now + FRAME_DEADLINE;
                assert!(buffer.pop_frame(late).is_none(), "{label}");
                assert!(buffer.expire(late), "{label}: expire");
                assert!(buffer.take_broken(), "{label}: broken");
            } else {
                assert!(!pending, "{label}: nothing left behind");
                assert!(!buffer.take_broken(), "{label}: broken");
                assert!(
                    buffer.nacks(now + Duration::from_secs(1), retry).is_empty(),
                    "{label}: nothing left to request"
                );
            }
            assert!(buffer.pop_frame(now + FRAME_DEADLINE).is_none(), "{label}");
            assert_eq!(buffer.buffered_bytes(), 0, "{label}: buffer empty");
        }
    }
}
