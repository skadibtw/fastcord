//! `fixtures/voice/h264-rtp-vectors.json`: packetization, RTX, and rtpsize
//! transport vectors from an independent implementation
//! (`fixtures/voice/gen_h264_rtp_vectors.py`, RFC 6184/4588 and pycryptodome),
//! including STAP-A aggregation, which fastcord receives but never sends.
//! They exercise the fragmented, keyframe, reorder, and loss paths with exact
//! byte comparisons.

use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use super::h264;
use super::receive::{FRAME_DEADLINE, REORDER_WAIT, ReceiveBuffer};
use crate::crypto::{TransportCipher, TransportKey, TransportMode};
use crate::rtp::{RtpHeader, ext};

const VECTORS: &str = include_str!("../../../../fixtures/voice/h264-rtp-vectors.json");

fn hex(value: &Value) -> Vec<u8> {
    let text = value.as_str().unwrap();
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
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
        .unwrap()
}

fn payloads(frame: &Value) -> Vec<Vec<u8>> {
    frame["payloads"]
        .as_array()
        .unwrap()
        .iter()
        .map(hex)
        .collect()
}

fn join(payloads: &[Vec<u8>]) -> Result<Vec<u8>, super::VideoError> {
    let borrowed: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
    let mut out = Vec::new();
    h264::depacketize(&borrowed, &mut out)?;
    Ok(out)
}

#[test]
fn every_independent_vector_depacketizes_to_the_exact_frame() {
    let vectors = vectors();
    let frames = vectors["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 4);
    for frame in frames {
        assert_eq!(
            join(&payloads(frame)).unwrap(),
            hex(&frame["annexb"]),
            "{}",
            frame["name"]
        );
    }
}

#[test]
fn our_packetizer_reproduces_the_independent_payloads_byte_for_byte() {
    let vectors = vectors();
    for name in [
        "keyframe-single-nal-and-fu-a",
        "delta-single-nal",
        "delta-fu-a",
    ] {
        let frame = frame(&vectors, name);
        let annexb = hex(&frame["annexb"]);
        let max_payload = frame["max_payload"].as_u64().unwrap() as usize;
        let parts = h264::packetize(&annexb, max_payload).unwrap();
        let ours: Vec<Vec<u8>> = parts
            .iter()
            .map(|part| {
                let mut payload = Vec::new();
                part.write(&annexb, &mut payload);
                payload
            })
            .collect();
        assert_eq!(ours, payloads(frame), "{name}");
        assert_eq!(parts.iter().filter(|part| part.marker).count(), 1);
        assert!(parts.last().unwrap().marker);
    }
    // The aggregating vector is what libwebrtc-style senders emit: our
    // packetizer packs differently, but both carry the same frame.
    let stap = frame(&vectors, "keyframe-stap-a-and-fu-a");
    let annexb = hex(&stap["annexb"]);
    assert_eq!(payloads(stap)[0][0] & 0x1F, 24);
    let ours = h264::packetize(&annexb, 1138).unwrap();
    assert!(ours.len() > payloads(stap).len());
}

#[test]
fn sealed_datagrams_open_parse_and_reassemble_in_both_transport_modes() {
    let vectors = vectors();
    let key = TransportKey::from_slice(&hex(&vectors["key"])).unwrap();
    let transport = &vectors["transport"];
    let expected = hex(&frame(&vectors, transport["frame"].as_str().unwrap())["annexb"]);
    let primary_ssrc = vectors["primary_ssrc"].as_u64().unwrap() as u32;
    let rtx_ssrc = vectors["rtx_ssrc"].as_u64().unwrap() as u32;
    for mode in transport["modes"].as_array().unwrap() {
        let name = mode["mode"].as_str().unwrap();
        let cipher = TransportCipher::new(TransportMode::from_wire(name).unwrap(), &key);
        let mut buffer = ReceiveBuffer::new();
        let now = Instant::now();
        let mut payloads = Vec::new();
        for entry in mode["packets"].as_array().unwrap() {
            let datagram = hex(&entry["datagram"]);
            assert!(datagram.len() <= super::MAX_DATAGRAM_BYTES, "{name}");
            let header = RtpHeader::parse(&datagram).unwrap();
            assert_eq!(header.sequence as u64, entry["sequence"].as_u64().unwrap());
            assert_eq!(header.marker, entry["marker"].as_bool().unwrap());
            let mut body = Vec::new();
            let nonce = cipher
                .open(&datagram, header.clear_len(), &mut body)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(u64::from(nonce), entry["nonce"].as_u64().unwrap());
            let split = header.split_body(&body).unwrap();
            let sent = hex(&entry["payload"]);
            match entry["kind"].as_str().unwrap() {
                "media" => {
                    assert_eq!((header.ssrc, header.payload_type), (primary_ssrc, 103));
                    let ids: Vec<u8> = split
                        .extensions
                        .elements()
                        .map(|element| element.unwrap().id)
                        .collect();
                    assert_eq!(
                        ids,
                        [
                            ext::ABS_SEND_TIME,
                            ext::PLAYOUT_DELAY,
                            ext::VIDEO_CONTENT_TYPE,
                            ext::RTP_STREAM_ID
                        ]
                    );
                    assert_eq!(split.extensions.find(ext::RTP_STREAM_ID), Some(&b"100"[..]));
                    assert_eq!(split.payload, sent);
                    buffer.push(now, &header, split.payload).unwrap();
                    payloads.push(sent);
                }
                "rtx" => {
                    assert_eq!((header.ssrc, header.payload_type), (rtx_ssrc, 104));
                    assert_eq!(
                        split.extensions.find(ext::REPAIRED_RTP_STREAM_ID),
                        Some(&b"100"[..])
                    );
                    // RFC 4588: the original sequence number, then its payload.
                    let original = u16::from_be_bytes([split.payload[0], split.payload[1]]);
                    assert_eq!(
                        u64::from(original),
                        entry["original_sequence"].as_u64().unwrap()
                    );
                    assert_eq!(&split.payload[2..], sent);
                    assert!(payloads.contains(&sent));
                }
                other => panic!("{other}"),
            }
        }
        let assembled = buffer.pop_frame(now).unwrap();
        assert_eq!(assembled.timestamp, 90_000 + 3000);
        assert_eq!(join(&assembled.payloads).unwrap(), expected, "{name}");
    }
}

/// The keyframe vector's media packets as (header, payload), plus its RTX
/// repair of the second packet as (header with the original sequence, payload).
type Packet = (RtpHeader, Vec<u8>);

fn keyframe_packets() -> (Vec<Packet>, Packet, Vec<u8>) {
    let vectors = vectors();
    let frame = frame(&vectors, "keyframe-stap-a-and-fu-a");
    let timestamp = frame["timestamp"].as_u64().unwrap() as u32;
    let first = frame["first_sequence"].as_u64().unwrap() as u16;
    let payloads = payloads(frame);
    let media: Vec<(RtpHeader, Vec<u8>)> = payloads
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            let mut header = RtpHeader::new(
                103,
                first.wrapping_add(index as u16),
                timestamp,
                vectors["primary_ssrc"].as_u64().unwrap() as u32,
            );
            header.marker = index == payloads.len() - 1;
            (header, payload.clone())
        })
        .collect();
    let repaired = media[1].clone();
    (media, repaired, hex(&frame["annexb"]))
}

#[tokio::test(start_paused = true)]
async fn a_lost_fragment_is_requested_and_repaired_by_rtx_into_the_exact_frame() {
    let (media, (repaired_header, repaired_payload), expected) = keyframe_packets();
    let now = Instant::now();
    let retry = Duration::from_millis(40);
    let mut buffer = ReceiveBuffer::new();
    for (index, (header, payload)) in media.iter().enumerate() {
        if index != 1 {
            buffer.push(now, header, payload).unwrap();
        }
    }
    assert!(buffer.pop_frame(now).is_none());
    // Reordering gets its 10 ms; after that the gap is named.
    assert!(
        buffer
            .nacks(now + REORDER_WAIT - Duration::from_millis(1), retry)
            .is_empty()
    );
    assert_eq!(
        buffer.nacks(now + REORDER_WAIT, retry),
        [repaired_header.sequence]
    );
    // The RTX packet arrives, unwrapped onto the primary stream.
    buffer
        .push(
            now + Duration::from_millis(30),
            &repaired_header,
            &repaired_payload,
        )
        .unwrap();
    let assembled = buffer.pop_frame(now + Duration::from_millis(30)).unwrap();
    assert_eq!(join(&assembled.payloads).unwrap(), expected);
    assert!(!buffer.take_broken());
    assert_eq!(buffer.buffered_bytes(), 0);
}

#[tokio::test(start_paused = true)]
async fn reordering_across_the_sequence_wrap_and_duplicates_change_nothing() {
    let vectors = vectors();
    let frame = frame(&vectors, "keyframe-single-nal-and-fu-a");
    let timestamp = frame["timestamp"].as_u64().unwrap() as u32;
    let first = frame["first_sequence"].as_u64().unwrap() as u16;
    assert_eq!(first, 65_533, "the vector crosses the 16-bit wrap");
    let payloads = payloads(frame);
    let expected = hex(&frame["annexb"]);
    let packet = |index: usize| {
        let mut header = RtpHeader::new(103, first.wrapping_add(index as u16), timestamp, 7);
        header.marker = index == payloads.len() - 1;
        (header, payloads[index].clone())
    };
    // The first packet leads (a receiver starts at its first packet); the
    // rest arrive shuffled, with duplicates.
    let now = Instant::now();
    let mut buffer = ReceiveBuffer::new();
    for index in [0, 3, 1, 4, 2, 2, 0, 4] {
        let (header, payload) = packet(index);
        buffer.push(now, &header, &payload).unwrap();
    }
    assert_eq!(
        join(&buffer.pop_frame(now).unwrap().payloads).unwrap(),
        expected
    );
    assert!(buffer.pop_frame(now).is_none());
    assert_eq!(buffer.buffered_bytes(), 0);
}

#[tokio::test(start_paused = true)]
async fn an_unrepaired_loss_never_yields_a_partial_frame() {
    let (media, _, _) = keyframe_packets();
    let now = Instant::now();
    let mut buffer = ReceiveBuffer::new();
    for (index, (header, payload)) in media.iter().enumerate() {
        if index != 2 {
            buffer.push(now, header, payload).unwrap();
        }
    }
    assert!(buffer.pop_frame(now + FRAME_DEADLINE).is_none());
    assert!(buffer.expire(now + FRAME_DEADLINE));
    // The caller must wait for a keyframe, and nothing is left buffered.
    assert!(buffer.take_broken());
    assert!(buffer.pop_frame(now + FRAME_DEADLINE).is_none());
    assert_eq!(buffer.buffered_bytes(), 0);
    // FU-A fragments carry no sequence of their own: only the buffer's
    // contiguity rule above keeps a frame with a hole away from the
    // depacketizer.
}
