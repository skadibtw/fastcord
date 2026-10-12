//! AV1 through the real driver (SPEC §8.3, milestone 26), with the same rig as
//! the H.264 tests above: an in-process voice server plays the SFU and an
//! official client whose AV1 temporal units (the genuine libaom stream in
//! `fixtures/video/`) are sealed by a real MLS/DAVE session, packetized per the
//! AOM AV1 RTP payload format, and sealed again with the transport key. Paused
//! time throughout.

use super::*;
use crate::video::av1_support::{Part, fixture_units, packetize, without_delimiter};

/// The offered AV1 payload types (`CodecOffer::av1`).
const AV1_PT: u8 = 101;
const AV1_RTX_PT: u8 = 102;

/// The bytes a viewer must receive for fixture temporal unit `index`: its OBUs
/// without the temporal delimiter RTP never carries, every OBU sized.
fn plain(index: usize) -> Vec<u8> {
    without_delimiter(fixture_units()[index]).to_vec()
}

impl Viewer {
    /// One temporal unit sealed by the streamer's DAVE session.
    fn seal_av1(&mut self, unit: &[u8]) -> Vec<u8> {
        self.dave
            .peer
            .encrypt_video(VideoCodec::Av1, unit)
            .unwrap()
            .into_owned()
    }

    /// `parts` as one frame of RTP on `payload_type`, each datagram sealed with
    /// the transport key: the frame's packets share a timestamp and take
    /// consecutive sequence numbers.
    fn forward_parts(&mut self, parts: &[Part], payload_type: u8) -> Vec<Forwarded> {
        let timestamp = self.next_timestamp;
        self.next_timestamp = self.next_timestamp.wrapping_add(3000);
        parts
            .iter()
            .map(|part| {
                let sequence = self.next_sequence;
                self.next_sequence = self.next_sequence.wrapping_add(1);
                let datagram = rtp_datagram(
                    &mut self.server,
                    payload_type,
                    VIDEO_SSRC,
                    sequence,
                    timestamp,
                    part.marker,
                    &[
                        (ext::ABS_SEND_TIME, &[1, 2, 3]),
                        (ext::RTP_STREAM_ID, b"100"),
                    ],
                    &part.payload,
                );
                Forwarded {
                    sequence,
                    timestamp,
                    marker: part.marker,
                    payload: part.payload.clone(),
                    datagram,
                }
            })
            .collect()
    }

    /// Already-sealed (or deliberately plain) AV1 bytes as one RTP frame.
    fn forward_av1_bytes(
        &mut self,
        frame: &[u8],
        new_sequence: bool,
        max_payload: usize,
    ) -> Vec<Forwarded> {
        let parts = packetize(frame, max_payload, new_sequence);
        self.forward_parts(&parts, AV1_PT)
    }

    /// One fixture temporal unit as an official client sends it: DAVE seals the
    /// whole unit, then it is split into RTP payloads of at most `max_payload`
    /// bytes with the N bit set on a keyframe.
    fn forward_av1(
        &mut self,
        unit: &[u8],
        new_sequence: bool,
        max_payload: usize,
    ) -> Vec<Forwarded> {
        let sealed = self.seal_av1(unit);
        self.forward_av1_bytes(&sealed, new_sequence, max_payload)
    }

    /// [`forward_av1`](Self::forward_av1) and sends every packet.
    fn send_av1(&mut self, unit: &[u8], new_sequence: bool, max_payload: usize) -> Vec<Forwarded> {
        let packets = self.forward_av1(unit, new_sequence, max_payload);
        self.send(&packets);
        packets
    }

    /// The RTX datagram that repairs `packet` on the AV1 RTX payload type.
    fn rtx_av1(&mut self, packet: &Forwarded) -> Vec<u8> {
        let mut payload = packet.sequence.to_be_bytes().to_vec();
        payload.extend_from_slice(&packet.payload);
        let sequence = self.next_rtx;
        self.next_rtx = self.next_rtx.wrapping_add(1);
        rtp_datagram(
            &mut self.server,
            AV1_RTX_PT,
            RTX_SSRC,
            sequence,
            packet.timestamp,
            packet.marker,
            &[
                (ext::ABS_SEND_TIME, &[1, 2, 3]),
                (ext::REPAIRED_RTP_STREAM_ID, b"100"),
            ],
            &payload,
        )
    }
}

/// M26: a viewer offers AV1 for decoding only, next to H.264, and opens real
/// AV1 sealed by a real DAVE group: packets that beat the announcement are held,
/// frames of every packetization (aggregated, fragmented, one packet; across the
/// 16-bit sequence wrap) arrive as the exact OBU stream, with sizes restored,
/// the codec, the keyframe flag, and the RTP timestamp.
#[tokio::test(start_paused = true)]
async fn a_viewer_offers_av1_decode_and_opens_dave_sealed_av1_exactly() {
    let mut rig = start_viewer().await;
    assert_eq!(rig.identify["d"]["video"], true);
    let codecs = rig.select["d"]["codecs"].as_array().unwrap();
    assert_eq!(codecs.len(), 3, "opus, H.264 and AV1");
    assert_eq!(
        codecs[1],
        json!({"name": "H264", "type": "video", "priority": 1000, "payload_type": 103,
               "rtx_payload_type": 104, "encode": false, "decode": true})
    );
    assert_eq!(
        codecs[2],
        json!({"name": "AV1", "type": "video", "priority": 2000, "payload_type": 101,
               "rtx_payload_type": 102, "encode": false, "decode": true})
    );

    let units = fixture_units();
    // Packets that beat the announcement wait for it (100 ms, bounded) and then
    // go through. This keyframe's sequence numbers cross the 16-bit wrap.
    let first = rig.forward_av1(units[0], true, 100);
    assert!(first.len() > 10 && first.last().unwrap().marker);
    assert!(first.iter().all(|packet| packet.datagram.len() <= 1200));
    assert!(first.iter().any(|packet| packet.sequence > 65_000));
    assert!(first.iter().any(|packet| packet.sequence < 100));
    rig.send(&first);
    rig.announce_stream().await;
    let frame = next_video(&mut rig.harness).await;
    assert_eq!(
        (frame.user_id, frame.ssrc, frame.codec, frame.keyframe),
        (ALICE, VIDEO_SSRC, VideoCodec::Av1, true)
    );
    assert_eq!(frame.timestamp, 1_000_000);
    assert_eq!(frame.data, plain(0), "opened, joined, and sized again");

    let mut packets = first.len();
    for (index, max_payload, timestamp) in [(1, 150, 1_003_000), (2, 1138, 1_006_000)] {
        let frame_packets = rig.forward_av1(units[index], false, max_payload);
        assert!(frame_packets.iter().all(|p| p.datagram.len() <= 1200));
        if max_payload == 1138 {
            assert_eq!(frame_packets.len(), 1, "a whole sealed frame in one packet");
        } else {
            assert!(frame_packets.len() > 5);
        }
        packets += frame_packets.len();
        rig.send(&frame_packets);
        let frame = next_video(&mut rig.harness).await;
        assert_eq!(
            (frame.codec, frame.keyframe, frame.timestamp),
            (VideoCodec::Av1, false, timestamp)
        );
        assert_eq!(frame.data, plain(index), "unit {index}");
    }
    let stats = rig.harness.session.stats();
    assert_eq!(stats.video_frames_received, 3);
    assert_eq!(stats.video_frames_dropped, 0);
    assert_eq!(stats.rejected_authentication, 0);
    assert_eq!(stats.keyframe_requests_sent, 0);
    assert_eq!(stats.video_packets_received, packets as u64);
}

/// M26: AV1 that did not go through DAVE is never delivered however well the
/// transport authenticates it, a dependent frame never starts a stream (its
/// frame header is encrypted, so only the RTP layer's N bit says "keyframe"),
/// and only a keyframe resumes it.
#[tokio::test(start_paused = true)]
async fn av1_without_dave_or_without_a_keyframe_is_never_delivered() {
    let mut rig = start_viewer().await;
    rig.announce_stream().await;
    let units = fixture_units();
    rig.send_av1(units[0], true, 200);
    assert_eq!(next_video(&mut rig.harness).await.data, plain(0));

    // The plain OBUs, packetized and sealed by the transport only.
    let unsealed = rig.forward_av1_bytes(&plain(1), false, 200);
    rig.send(&unsealed);
    let feedback = rig.rtcp_until(|feedback| !feedback.plis.is_empty()).await;
    assert_eq!(feedback.plis, [VIDEO_SSRC]);
    assert_no_video(&mut rig.harness).await;
    let stats = rig.harness.session.stats();
    assert_eq!(stats.video_frames_received, 1);
    assert_eq!(stats.video_frames_dropped, 1);
    assert_eq!(stats.rejected_authentication, 1);
    assert_eq!(stats.keyframe_requests_sent, 1);

    // While the stream waits, a correctly sealed inter frame is not even opened.
    rig.send_av1(units[1], false, 200);
    assert_no_video(&mut rig.harness).await;
    let stats = rig.harness.session.stats();
    assert_eq!(stats.video_frames_dropped, 2);
    assert_eq!(stats.rejected_authentication, 1);
    // A keyframe whose first packet lacks the N bit does not restart it: nothing
    // inside the sealed frame can be trusted to say what it is.
    rig.send_av1(units[0], false, 200);
    assert_no_video(&mut rig.harness).await;
    assert_eq!(rig.harness.session.stats().video_frames_dropped, 3);
    // A keyframe with it does, and the frames after it continue normally.
    rig.send_av1(units[0], true, 200);
    let frame = next_video(&mut rig.harness).await;
    assert!(frame.keyframe);
    assert_eq!(frame.data, plain(0));
    rig.send_av1(units[1], false, 200);
    let frame = next_video(&mut rig.harness).await;
    assert!(!frame.keyframe);
    assert_eq!(frame.data, plain(1));
    assert_eq!(rig.harness.session.stats().video_frames_received, 3);
}

/// M26: a lost AV1 packet (inside a frame, at its marker, or a keyframe's first
/// packet carrying the N bit) is requested with a Generic NACK after the
/// reordering grace and repaired by RTX on the AV1 RTX payload type into the
/// exact frame, an unrepaired loss is abandoned with a PLI and no partial frame,
/// and only a keyframe resumes the stream.
#[tokio::test(start_paused = true)]
async fn lost_av1_packets_are_nacked_and_repaired_by_rtx_or_restart_at_a_keyframe() {
    let mut rig = start_viewer().await;
    rig.announce_stream().await;
    let units = fixture_units();
    rig.send_av1(units[0], true, 120);
    assert_eq!(next_video(&mut rig.harness).await.data, plain(0));

    // A packet in the middle of the next frame is lost.
    let frame = rig.forward_av1(units[1], false, 120);
    let lost = frame[3].clone();
    for packet in frame
        .iter()
        .filter(|packet| packet.sequence != lost.sequence)
    {
        rig.udp.send(packet.datagram.clone());
    }
    let sent_at = Instant::now();
    let feedback = rig.rtcp_until(|feedback| !feedback.nacks.is_empty()).await;
    assert_eq!(feedback.nacks, [(VIDEO_SSRC, vec![lost.sequence])]);
    assert!(Instant::now() - sent_at >= Duration::from_millis(10));
    assert_no_video(&mut rig.harness).await;
    let repair = rig.rtx_av1(&lost);
    rig.udp.send(repair);
    let repaired = next_video(&mut rig.harness).await;
    assert_eq!(repaired.data, plain(1));
    assert!(!repaired.keyframe);
    let stats = rig.harness.session.stats();
    assert_eq!(stats.nack_sequences_sent, 1);
    assert_eq!(stats.keyframe_requests_sent, 0);

    // The marker packet of a frame is lost: nothing shows the hole until the
    // next frame arrives, then it is asked for, and both frames come out in
    // order once it is repaired.
    let frame = rig.forward_av1(units[2], false, 120);
    let tail = frame.last().unwrap().clone();
    assert!(tail.marker);
    for packet in &frame[..frame.len() - 1] {
        rig.udp.send(packet.datagram.clone());
    }
    assert_no_video(&mut rig.harness).await;
    rig.send_av1(units[1], false, 120);
    let feedback = rig.rtcp_until(|feedback| !feedback.nacks.is_empty()).await;
    assert_eq!(feedback.nacks, [(VIDEO_SSRC, vec![tail.sequence])]);
    let repair = rig.rtx_av1(&tail);
    rig.udp.send(repair);
    assert_eq!(next_video(&mut rig.harness).await.data, plain(2));
    assert_eq!(next_video(&mut rig.harness).await.data, plain(1));

    // The first packet of a keyframe is lost: its repair carries the N bit that
    // makes the frame a keyframe, so the repaired frame restarts nothing wrongly
    // and is delivered as one.
    let frame = rig.forward_av1(units[0], true, 120);
    assert_eq!(frame[0].payload[0] & 0x08, 0x08);
    let lost = frame[0].clone();
    for packet in &frame[1..] {
        rig.udp.send(packet.datagram.clone());
    }
    let feedback = rig.rtcp_until(|feedback| !feedback.nacks.is_empty()).await;
    assert_eq!(feedback.nacks, [(VIDEO_SSRC, vec![lost.sequence])]);
    let repair = rig.rtx_av1(&lost);
    rig.udp.send(repair);
    let frame = next_video(&mut rig.harness).await;
    assert!(frame.keyframe);
    assert_eq!(frame.data, plain(0));
    assert_eq!(rig.harness.session.stats().keyframe_requests_sent, 0);

    // This time a packet is never repaired: NACKs repeat, then the frame is
    // abandoned after its deadline, a keyframe is requested, and nothing partial
    // is delivered.
    let frame = rig.forward_av1(units[1], false, 120);
    let missing = frame[2].sequence;
    for packet in frame.iter().filter(|packet| packet.sequence != missing) {
        rig.udp.send(packet.datagram.clone());
    }
    let started = Instant::now();
    let feedback = rig.rtcp_until(|feedback| !feedback.plis.is_empty()).await;
    assert_eq!(feedback.plis, [VIDEO_SSRC]);
    assert!(Instant::now() - started >= Duration::from_millis(200));
    assert_no_video(&mut rig.harness).await;
    let stats = rig.harness.session.stats();
    assert!(stats.nack_sequences_sent >= 3, "{stats:?}");
    assert_eq!(stats.video_frames_dropped, 1);
    assert_eq!(stats.keyframe_requests_sent, 1);

    // A complete inter frame cannot be decoded without its reference and is not
    // delivered; the next keyframe is.
    rig.send_av1(units[1], false, 120);
    assert_no_video(&mut rig.harness).await;
    rig.send_av1(units[0], true, 120);
    let recovered = next_video(&mut rig.harness).await;
    assert!(recovered.keyframe);
    assert_eq!(recovered.data, plain(0));
}

/// M26: the decoder side (a consumer that dropped a frame or could not decode
/// one) asks for a keyframe through `MediaSender::request_video_keyframe`: the
/// stream is suppressed (inter frames that authenticate are refused) and a PLI
/// goes out; a request for no stream, or for SSRC 0, changes nothing.
#[tokio::test(start_paused = true)]
async fn a_consumer_keyframe_request_suppresses_dependent_av1_frames_and_sends_a_pli() {
    let mut rig = start_viewer().await;
    rig.announce_stream().await;
    let units = fixture_units();
    rig.send_av1(units[0], true, 200);
    assert_eq!(next_video(&mut rig.harness).await.data, plain(0));
    rig.send_av1(units[1], false, 200);
    assert_eq!(next_video(&mut rig.harness).await.data, plain(1));

    let media = rig.harness.session.media();
    assert_eq!(
        media.request_video_keyframe(0),
        Err(MediaSendError::InvalidFrame)
    );
    // A stream nobody receives: accepted, and nothing happens for a long while.
    assert_eq!(media.request_video_keyframe(6_000), Ok(()));
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(rig.harness.session.stats().keyframe_requests_sent, 0);
    rig.send_av1(units[2], false, 200);
    assert_eq!(next_video(&mut rig.harness).await.data, plain(2));

    // The stream we receive: a PLI, and nothing dependent gets through.
    assert_eq!(media.request_video_keyframe(VIDEO_SSRC), Ok(()));
    let feedback = rig.rtcp_until(|feedback| !feedback.plis.is_empty()).await;
    assert_eq!(feedback.plis, [VIDEO_SSRC]);
    rig.send_av1(units[1], false, 200);
    assert_no_video(&mut rig.harness).await;
    rig.send_av1(units[0], true, 200);
    let frame = next_video(&mut rig.harness).await;
    assert!(frame.keyframe);
    assert_eq!(frame.data, plain(0));
    rig.send_av1(units[2], false, 200);
    assert_eq!(next_video(&mut rig.harness).await.data, plain(2));
    let stats = rig.harness.session.stats();
    assert_eq!(stats.keyframe_requests_sent, 1);
    assert_eq!(stats.video_frames_dropped, 1);
}

/// M26: the sender may change codec on a stream. Each switch replaces the
/// receiver (its depacketizer and DAVE transform follow the payload type), so a
/// dependent frame of the new codec is refused with a PLI and only that codec's
/// keyframe is delivered, as the codec it is.
#[tokio::test(start_paused = true)]
async fn a_codec_switch_restarts_at_a_keyframe_of_the_new_codec_each_way() {
    let mut rig = start_viewer().await;
    rig.announce_stream().await;
    let [h264_key, h264_delta, h264_key_again] = access_units();
    let units = fixture_units();

    rig.send_frame(h264_key, false);
    let frame = next_video(&mut rig.harness).await;
    assert_eq!((frame.codec, frame.keyframe), (VideoCodec::H264, true));
    assert_eq!(frame.data, h264_key);

    // The same SSRC now carries AV1: an inter frame cannot start it.
    rig.send_av1(units[1], false, 200);
    assert_no_video(&mut rig.harness).await;
    let feedback = rig.rtcp_until(|feedback| !feedback.plis.is_empty()).await;
    assert_eq!(feedback.plis, [VIDEO_SSRC]);
    rig.send_av1(units[0], true, 200);
    let frame = next_video(&mut rig.harness).await;
    assert_eq!((frame.codec, frame.keyframe), (VideoCodec::Av1, true));
    assert_eq!(frame.data, plain(0));
    rig.send_av1(units[1], false, 200);
    let frame = next_video(&mut rig.harness).await;
    assert_eq!((frame.codec, frame.keyframe), (VideoCodec::Av1, false));
    assert_eq!(frame.data, plain(1));

    // And back to H.264: its delta frame is refused, its IDR is delivered.
    rig.send_frame(h264_delta, false);
    assert_no_video(&mut rig.harness).await;
    rig.send_frame(h264_key_again, false);
    let frame = next_video(&mut rig.harness).await;
    assert_eq!((frame.codec, frame.keyframe), (VideoCodec::H264, true));
    assert_eq!(frame.data, h264_key_again);
    let stats = rig.harness.session.stats();
    assert_eq!(stats.video_frames_received, 4);
    assert_eq!(stats.video_frames_dropped, 2);
    assert_eq!(stats.rejected_authentication, 0);
}

/// M26: damaged AV1 never reaches the consumer and never panics: a flipped bit
/// in the sealed frame under a valid transport tag fails DAVE, malformed RTP
/// payloads are dropped (or counted malformed when empty), AV1 bytes under the
/// H.264 payload type are refused, and an N-bit keyframe resumes the stream.
#[tokio::test(start_paused = true)]
async fn tampered_and_malformed_av1_is_dropped_and_a_keyframe_recovers() {
    let mut rig = start_viewer().await;
    rig.announce_stream().await;
    let units = fixture_units();
    rig.send_av1(units[0], true, 200);
    assert_eq!(next_video(&mut rig.harness).await.data, plain(0));

    // One flipped ciphertext bit: the transport authenticates, DAVE refuses.
    let mut sealed = rig.seal_av1(units[1]);
    sealed[10] ^= 1;
    let packets = rig.forward_av1_bytes(&sealed, false, 200);
    rig.send(&packets);
    let feedback = rig.rtcp_until(|feedback| !feedback.plis.is_empty()).await;
    assert_eq!(feedback.plis, [VIDEO_SSRC]);
    assert_no_video(&mut rig.harness).await;
    let stats = rig.harness.session.stats();
    assert_eq!(stats.rejected_authentication, 1);
    assert_eq!(stats.video_frames_dropped, 1);

    // A payload the AV1 RTP format forbids (a reserved aggregation-header bit),
    // complete as far as the transport can tell: dropped by the depacketizer,
    // before DAVE is asked.
    let malformed = rig.forward_parts(
        &[Part {
            payload: vec![0x01, 0x30, 0x01],
            marker: true,
        }],
        AV1_PT,
    );
    rig.send(&malformed);
    assert_no_video(&mut rig.harness).await;
    let stats = rig.harness.session.stats();
    assert_eq!(stats.video_frames_dropped, 2);
    assert_eq!(stats.rejected_authentication, 1);
    // No payload at all is malformed at the datagram.
    let before = stats.rejected_malformed;
    let empty = rig.forward_parts(
        &[Part {
            payload: Vec::new(),
            marker: true,
        }],
        AV1_PT,
    );
    rig.send(&empty);
    assert_no_video(&mut rig.harness).await;
    assert_eq!(rig.harness.session.stats().rejected_malformed, before + 1);

    // AV1 bytes under the H.264 payload type: the H.264 receiver refuses them
    // (as a depacketizer error or as a failed DAVE open), whatever they look like.
    let sealed = rig.seal_av1(units[0]);
    let parts = packetize(&sealed, 200, true);
    let as_h264 = rig.forward_parts(&parts, VIDEO_PT);
    rig.send(&as_h264);
    assert_no_video(&mut rig.harness).await;
    assert!(rig.harness.session.stats().video_frames_dropped >= 3);

    // A real keyframe restarts the stream, as AV1 again.
    rig.send_av1(units[0], true, 200);
    let frame = next_video(&mut rig.harness).await;
    assert_eq!((frame.codec, frame.keyframe), (VideoCodec::Av1, true));
    assert_eq!(frame.data, plain(0));
    rig.send_av1(units[1], false, 200);
    assert_eq!(next_video(&mut rig.harness).await.data, plain(1));
}

/// M26: a payload type is only valid on its own kind of SSRC: AV1 media on the
/// RTX SSRC and AV1 RTX on the primary SSRC are counted malformed and change
/// nothing, and the stream keeps working.
#[tokio::test(start_paused = true)]
async fn av1_payload_types_must_match_the_kind_of_their_ssrc() {
    let mut rig = start_viewer().await;
    rig.announce_stream().await;
    let before = rig.harness.session.stats().rejected_malformed;
    // RTX payload: the original sequence number, then the original payload.
    let rtx_on_primary = rtp_datagram(
        &mut rig.server,
        AV1_RTX_PT,
        VIDEO_SSRC,
        1,
        1,
        true,
        &[],
        &[0x00, 0x01, 0x10, 0x30, 0x01],
    );
    let media_on_rtx = rtp_datagram(
        &mut rig.server,
        AV1_PT,
        RTX_SSRC,
        2,
        1,
        true,
        &[],
        &[0x10, 0x30, 0x01],
    );
    rig.udp.send(rtx_on_primary);
    rig.udp.send(media_on_rtx);
    assert_no_video(&mut rig.harness).await;
    assert_eq!(
        rig.harness.session.stats().rejected_malformed - before,
        2,
        "{:?}",
        rig.harness.session.stats()
    );
    let units = fixture_units();
    rig.send_av1(units[0], true, 200);
    assert_eq!(next_video(&mut rig.harness).await.data, plain(0));
}

/// M26: an owner (which encodes H.264 only) still offers AV1 for decoding, so a
/// peer's AV1 can reach it, and never offers to encode it; a server that selects
/// AV1 for its sending leaves nothing sent (M25 covers that outcome).
#[tokio::test(start_paused = true)]
async fn an_owner_offers_av1_for_decoding_only() {
    let rig = start_owner().await;
    let codecs = rig.select["d"]["codecs"].as_array().unwrap();
    assert_eq!(codecs.len(), 3);
    assert_eq!(
        codecs[1],
        json!({"name": "H264", "type": "video", "priority": 1000, "payload_type": 103,
               "rtx_payload_type": 104, "encode": true, "decode": true})
    );
    assert_eq!(
        codecs[2],
        json!({"name": "AV1", "type": "video", "priority": 2000, "payload_type": 101,
               "rtx_payload_type": 102, "encode": false, "decode": true})
    );
}
