//! Video through the real driver (SPEC §8.3, milestone 25): an in-process voice
//! server plays the SFU and, for a viewer, an official client's DAVE-sealed
//! H.264 stream; for an owner it opens, checks, and answers what the driver
//! sends. Real MLS/DAVE sessions, both directions, paused time.

use std::time::Duration;

use futures_util::FutureExt;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::*;
use crate::rtcp::{self, Compound, RtcpPacket};
use crate::rtp::{self, ext};
use crate::video::h264;
use crate::video::{ReceivedVideo, SendOptions, VideoCodec, VideoFrame, VideoOptions};

const FIXTURE: &[u8] = include_bytes!("../../../../../fixtures/video/ipcm-128x96.h264");
const VIDEO_SSRC: u32 = 5_000;
const RTX_SSRC: u32 = 5_001;
const VIDEO_PT: u8 = 103;
const RTX_PT: u8 = 104;
const OWN_VIDEO_SSRC: u32 = 13_960;
const OWN_RTX_SSRC: u32 = 13_961;
const MODE: TransportMode = TransportMode::Aes256GcmRtpSize;

/// The fixture's access units: SPS+PPS+IDR, a P slice, and a lone IDR.
fn access_units() -> [&'static [u8]; 3] {
    [
        &FIXTURE[..18_556],
        &FIXTURE[18_556..37_092],
        &FIXTURE[37_092..],
    ]
}

async fn negotiate_video(harness: &mut Harness, ws: &mut Ws, ready: Value) -> (Udp, Value, Value) {
    ws.hello(QUIET_HEARTBEAT_MS).await;
    let identify = ws.recv_op(0).await;
    ws.send(ready).await;
    let mut udp = harness.udp().await;
    let discovery = udp.recv().await;
    udp.answer_discovery(&discovery);
    let select = ws.recv_op(1).await;
    (udp, identify, select)
}

async fn next_video(harness: &mut Harness) -> ReceivedVideo {
    timeout(Duration::from_secs(600), harness.session.next_video())
        .await
        .expect("no video frame")
        .expect("the connection ended")
}

async fn assert_no_video(harness: &mut Harness) {
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(harness.session.next_video().now_or_never().is_none());
}

/// One RTP datagram from the server: extension block and payload sealed
/// together under the transport key.
#[allow(clippy::too_many_arguments)]
fn rtp_datagram(
    cipher: &mut TransportCipher,
    payload_type: u8,
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    marker: bool,
    elements: &[(u8, &[u8])],
    payload: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    let preamble = rtp::write_extension_block(elements, &mut body).unwrap();
    body.extend_from_slice(payload);
    let header = RtpHeader {
        marker,
        extension: Some(preamble),
        ..RtpHeader::new(payload_type, sequence, timestamp, ssrc)
    };
    let mut clear = Vec::new();
    header.write(&mut clear);
    let mut out = Vec::new();
    cipher.seal(&clear, &body, &mut out).unwrap();
    out
}

fn sealed_rtcp(cipher: &mut TransportCipher, compound: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    cipher
        .seal(
            &compound[..RTCP_CLEAR_LEN],
            &compound[RTCP_CLEAR_LEN..],
            &mut out,
        )
        .unwrap();
    out
}

/// A datagram the client sent, opened and reassembled as one compound.
fn open_rtcp(cipher: &TransportCipher, datagram: &[u8]) -> Vec<u8> {
    let mut plain = Vec::new();
    cipher
        .open(datagram, RTCP_CLEAR_LEN, &mut plain)
        .expect("client RTCP must authenticate with the session key");
    let mut compound = datagram[..RTCP_CLEAR_LEN].to_vec();
    compound.extend_from_slice(&plain);
    compound
}

/// What a client compound asked for.
#[derive(Debug, Default)]
struct Feedback {
    sender_ssrc: u32,
    blocks: Vec<rtcp::ReportBlock>,
    nacks: Vec<(u32, Vec<u16>)>,
    plis: Vec<u32>,
    sender_reports: Vec<(u32, rtcp::SenderInfo)>,
}

fn parse_rtcp(compound: &[u8]) -> Feedback {
    let mut feedback = Feedback::default();
    for packet in Compound::new(compound) {
        match packet.expect("well-formed client RTCP") {
            RtcpPacket::ReceiverReport(report) => {
                feedback.sender_ssrc = report.ssrc;
                feedback.blocks.extend(report.reports.iter());
            }
            RtcpPacket::SenderReport(report) => {
                feedback.sender_reports.push((report.ssrc, report.info));
            }
            RtcpPacket::Feedback(item) => {
                if item.kind == rtcp::FeedbackKind::Transport && item.format == rtcp::GENERIC_NACK {
                    feedback
                        .nacks
                        .push((item.media_ssrc, item.nacked_sequences().collect()));
                }
                if item.is_picture_loss() {
                    feedback.plis.push(item.media_ssrc);
                }
            }
            RtcpPacket::Other { .. } => {}
        }
    }
    feedback
}

/// One RTP datagram of ours, opened.
struct Opened {
    header: RtpHeader,
    elements: Vec<(u8, Vec<u8>)>,
    payload: Vec<u8>,
    nonce: u32,
    datagram_len: usize,
    at: Instant,
}

fn open_video(cipher: &TransportCipher, datagram: &[u8]) -> Opened {
    let header = RtpHeader::parse(datagram).unwrap();
    let mut body = Vec::new();
    let nonce = cipher
        .open(datagram, header.clear_len(), &mut body)
        .expect("client RTP must authenticate with the session key");
    let split = header.split_body(&body).unwrap();
    Opened {
        header,
        elements: split
            .extensions
            .elements()
            .map(|element| {
                let element = element.unwrap();
                (element.id, element.data.to_vec())
            })
            .collect(),
        payload: split.payload.to_vec(),
        nonce,
        datagram_len: datagram.len(),
        at: Instant::now(),
    }
}

fn join(packets: &[&Opened]) -> Vec<u8> {
    let payloads: Vec<&[u8]> = packets.iter().map(|p| p.payload.as_slice()).collect();
    let mut out = Vec::new();
    h264::depacketize(&payloads, &mut out).unwrap();
    out
}

// ---- viewer ---------------------------------------------------------------

/// One RTP packet of a forwarded frame.
#[derive(Clone)]
struct Forwarded {
    sequence: u16,
    timestamp: u32,
    marker: bool,
    payload: Vec<u8>,
    datagram: Vec<u8>,
}

struct Viewer {
    harness: Harness,
    ws: Ws,
    udp: Udp,
    dave: TestDaveGroup,
    /// The server's sending side of the transport.
    server: TransportCipher,
    /// Opens what the client sends.
    client: TransportCipher,
    identify: Value,
    select: Value,
    next_sequence: u16,
    next_timestamp: u32,
    next_rtx: u16,
    _signaling: FakeSignaling,
}

async fn start_viewer() -> Viewer {
    let (mut harness, signaling) = Harness::start(Options {
        video: Some(VideoOptions::watch()),
        ..options()
    });
    let mut ws = signaling.accept().await;
    let (udp, identify, select) = negotiate_video(&mut harness, &mut ws, ready(&BOTH_MODES)).await;
    ws.send(session_description("aead_aes256_gcm_rtpsize"))
        .await;
    let dave = install_test_dave_group(&harness, &mut ws).await;
    let key = TransportKey::from_slice(&KEY).unwrap();
    Viewer {
        harness,
        ws,
        udp,
        dave,
        server: TransportCipher::starting_at(MODE, &key, 500),
        client: TransportCipher::new(MODE, &key),
        identify,
        select,
        next_sequence: 65_530,
        next_timestamp: 1_000_000,
        next_rtx: 40,
        _signaling: signaling,
    }
}

impl Viewer {
    /// The server announces ALICE's stream (opcode 12) and, as a viewer, the
    /// client asks for it with Media Sink Wants.
    async fn announce(&mut self, streams: Value) {
        self.ws
            .send(json!({"op": 12, "d": {
                "user_id": ALICE.0.to_string(), "audio_ssrc": 0, "streams": streams
            }}))
            .await;
    }

    async fn announce_stream(&mut self) {
        self.announce(json!([{
            "ssrc": VIDEO_SSRC, "rtx_ssrc": RTX_SSRC, "rid": "100", "quality": 100, "active": true
        }]))
        .await;
        assert_eq!(
            self.harness.event().await,
            VoiceEvent::VideoSource {
                user_id: ALICE,
                ssrc: VIDEO_SSRC,
                active: true
            }
        );
        assert_eq!(
            self.ws.recv_op(15).await,
            json!({"op": 15, "d": {"5000": 100, "any": 0}})
        );
    }

    /// One frame as the SFU forwards an official client's stream: sealed by
    /// DAVE (unless `plain`), split into RTP, each packet sealed with the
    /// transport key.
    fn forward(&mut self, frame: &[u8], plain: bool) -> Vec<Forwarded> {
        let sealed = if plain {
            frame.to_vec()
        } else {
            self.dave.peer.encrypt_h264(frame).unwrap().into_owned()
        };
        self.forward_sealed(&sealed)
    }

    fn forward_sealed(&mut self, sealed: &[u8]) -> Vec<Forwarded> {
        let parts = h264::packetize(sealed, 1138).unwrap();
        let timestamp = self.next_timestamp;
        self.next_timestamp = self.next_timestamp.wrapping_add(3000);
        parts
            .iter()
            .map(|part| {
                let mut payload = Vec::new();
                part.write(sealed, &mut payload);
                let sequence = self.next_sequence;
                self.next_sequence = self.next_sequence.wrapping_add(1);
                let datagram = rtp_datagram(
                    &mut self.server,
                    VIDEO_PT,
                    VIDEO_SSRC,
                    sequence,
                    timestamp,
                    part.marker,
                    &[
                        (ext::ABS_SEND_TIME, &[1, 2, 3]),
                        (ext::RTP_STREAM_ID, b"100"),
                    ],
                    &payload,
                );
                Forwarded {
                    sequence,
                    timestamp,
                    marker: part.marker,
                    payload,
                    datagram,
                }
            })
            .collect()
    }

    fn send(&self, packets: &[Forwarded]) {
        for packet in packets {
            self.udp.send(packet.datagram.clone());
        }
    }

    /// Forwards a frame and sends all of its packets.
    fn send_frame(&mut self, frame: &[u8], plain: bool) -> Vec<Forwarded> {
        let packets = self.forward(frame, plain);
        self.send(&packets);
        packets
    }

    /// The RTX datagram that repairs `packet` (RFC 4588).
    fn rtx_of(&mut self, packet: &Forwarded) -> Vec<u8> {
        let mut payload = packet.sequence.to_be_bytes().to_vec();
        payload.extend_from_slice(&packet.payload);
        let sequence = self.next_rtx;
        self.next_rtx = self.next_rtx.wrapping_add(1);
        rtp_datagram(
            &mut self.server,
            RTX_PT,
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

    /// The next RTCP compound the client sends.
    async fn next_rtcp(&mut self) -> Feedback {
        loop {
            let datagram = self.udp.recv().await;
            if rtcp::is_rtcp(&datagram) {
                return parse_rtcp(&open_rtcp(&self.client, &datagram));
            }
        }
    }

    async fn rtcp_until(&mut self, wanted: impl Fn(&Feedback) -> bool) -> Feedback {
        for _ in 0..200 {
            let feedback = self.next_rtcp().await;
            assert_eq!(feedback.sender_ssrc, SSRC, "{feedback:?}");
            if wanted(&feedback) {
                return feedback;
            }
        }
        panic!("the awaited feedback never came");
    }
}

#[tokio::test(start_paused = true)]
async fn a_viewer_declares_video_wants_the_stream_and_opens_dave_sealed_h264() {
    let mut rig = start_viewer().await;
    // What the viewer offered: video support, no layer, decode only.
    assert_eq!(rig.identify["d"]["video"], true);
    assert!(rig.identify["d"].get("streams").is_none());
    assert_eq!(
        rig.select["d"]["codecs"][1],
        json!({"name": "H264", "type": "video", "priority": 1000, "payload_type": 103,
               "rtx_payload_type": 104, "encode": false, "decode": true})
    );
    // It stated that it sends no video before it received any.
    assert_eq!(
        rig.ws.1,
        [json!({"op": 12, "d": {
            "audio_ssrc": SSRC, "video_ssrc": 0, "rtx_ssrc": 0, "streams": []
        }})]
    );

    // Packets that beat the announcement wait for it (100 ms, bounded) and
    // then go through.
    let [keyframe, delta, key_again] = access_units();
    let first = rig.forward(keyframe, false);
    assert!(first.len() > 10 && first.last().unwrap().marker);
    assert!(first.iter().all(|packet| packet.datagram.len() <= 1200));
    rig.send(&first);
    rig.announce_stream().await;
    let frame = next_video(&mut rig.harness).await;
    assert_eq!(
        (frame.user_id, frame.ssrc, frame.codec, frame.keyframe),
        (ALICE, VIDEO_SSRC, VideoCodec::H264, true)
    );
    assert_eq!(frame.timestamp, 1_000_000);
    assert_eq!(frame.data, keyframe, "decrypted and depacketized exactly");

    // The next frame is a delta, delivered as such, across the sequence wrap.
    let second = rig.forward(delta, false);
    assert!(second.iter().any(|packet| packet.sequence < 100));
    rig.send(&second);
    let frame = next_video(&mut rig.harness).await;
    assert_eq!((frame.keyframe, frame.timestamp), (false, 1_003_000));
    assert_eq!(frame.data, delta);

    // Media that never went through DAVE is never delivered, however well
    // the transport authenticates it: the stream waits for a keyframe.
    let unsealed = rig.forward(delta, true);
    rig.send(&unsealed);
    let feedback = rig.rtcp_until(|feedback| !feedback.plis.is_empty()).await;
    assert_eq!(feedback.plis, [VIDEO_SSRC]);
    assert_no_video(&mut rig.harness).await;
    let stats = rig.harness.session.stats();
    assert_eq!(stats.video_frames_received, 2);
    assert_eq!(stats.video_frames_dropped, 1);
    assert_eq!(stats.rejected_authentication, 1);
    assert_eq!(stats.keyframe_requests_sent, 1);
    assert_eq!(
        stats.video_packets_received,
        (first.len() + second.len() + unsealed.len()) as u64
    );

    // A keyframe restarts it; a delta still waiting does not count.
    rig.send_frame(delta, false);
    assert_no_video(&mut rig.harness).await;
    rig.send_frame(key_again, false);
    let frame = next_video(&mut rig.harness).await;
    assert!(frame.keyframe);
    assert_eq!(frame.data, key_again);
}

#[tokio::test(start_paused = true)]
async fn a_lost_packet_is_nacked_and_repaired_by_rtx_and_an_unrepairable_loss_restarts_at_a_keyframe()
 {
    let mut rig = start_viewer().await;
    rig.announce_stream().await;
    let [keyframe, delta, _] = access_units();
    rig.send_frame(keyframe, false);
    assert_eq!(next_video(&mut rig.harness).await.data, keyframe);

    // One packet of the next frame is lost.
    let frame = rig.forward(delta, false);
    let lost = frame[4].clone();
    for packet in frame
        .iter()
        .filter(|packet| packet.sequence != lost.sequence)
    {
        rig.udp.send(packet.datagram.clone());
    }
    let sent_at = Instant::now();
    let feedback = rig.rtcp_until(|feedback| !feedback.nacks.is_empty()).await;
    assert_eq!(feedback.nacks, [(VIDEO_SSRC, vec![lost.sequence])]);
    // Reordering got its 10 ms first.
    assert!(Instant::now() - sent_at >= Duration::from_millis(10));
    assert_no_video(&mut rig.harness).await;
    // The repair arrives on the RTX SSRC and completes the exact frame.
    let repair = rig.rtx_of(&lost);
    rig.udp.send(repair);
    let repaired = next_video(&mut rig.harness).await;
    assert_eq!(repaired.data, delta);
    assert!(!repaired.keyframe);
    let stats = rig.harness.session.stats();
    assert_eq!(stats.nack_sequences_sent, 1);
    assert_eq!(stats.keyframe_requests_sent, 0);

    // This time the packet is never repaired: NACKs repeat, then the frame is
    // abandoned after its deadline and a keyframe is requested.
    let frame = rig.forward(delta, false);
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

    // A complete delta frame cannot be decoded without its reference and is
    // not delivered; the next keyframe is.
    rig.send_frame(delta, false);
    assert_no_video(&mut rig.harness).await;
    rig.send_frame(keyframe, false);
    let recovered = next_video(&mut rig.harness).await;
    assert!(recovered.keyframe);
    assert_eq!(recovered.data, keyframe);
    assert_eq!(rig.harness.session.stats().video_frames_received, 3);
}

#[tokio::test(start_paused = true)]
async fn receiver_reports_carry_loss_and_the_sender_report_echo() {
    let mut rig = start_viewer().await;
    rig.announce_stream().await;
    let [keyframe, delta, _] = access_units();
    // The sender's report names its NTP time; the client echoes it.
    let mut report = Vec::new();
    rtcp::write_sender_report(
        VIDEO_SSRC,
        rtcp::SenderInfo {
            ntp_timestamp: 0x0000_1234_5678_0000,
            rtp_timestamp: 1_000_000,
            packet_count: 1,
            octet_count: 1,
        },
        &mut report,
    );
    rig.send_frame(keyframe, false);
    next_video(&mut rig.harness).await;
    rig.udp.send(sealed_rtcp(&mut rig.server, &report));
    // Two packets of the next frame are lost and then repaired late.
    let frame = rig.forward(delta, false);
    for packet in &frame[..frame.len() - 1] {
        if ![2, 7].contains(
            &(frame
                .iter()
                .position(|p| p.sequence == packet.sequence)
                .unwrap()),
        ) {
            rig.udp.send(packet.datagram.clone());
        }
    }
    let feedback = rig.rtcp_until(|feedback| !feedback.blocks.is_empty()).await;
    let block = feedback.blocks[0];
    assert_eq!(block.ssrc, VIDEO_SSRC);
    assert_eq!(block.last_sender_report, 0x1234_5678);
    assert!(block.delay_since_last_sender_report > 0);
    // The marker packet and two holes: the report counts the highest sequence
    // received (the last but one), with the loss since the report began.
    assert_eq!(block.cumulative_lost, 2);
    assert_eq!(
        block.highest_sequence & 0xFFFF,
        u32::from(frame[frame.len() - 2].sequence)
    );
}

#[tokio::test(start_paused = true)]
async fn wants_follow_the_viewer_the_best_layer_and_a_leaving_streamer() {
    let mut rig = start_viewer().await;
    rig.announce(json!([
        {"ssrc": 6_000, "rtx_ssrc": 6_001, "rid": "50", "quality": 50, "active": true},
        {"ssrc": 6_002, "rtx_ssrc": 6_003, "rid": "100", "quality": 100, "active": true},
        {"ssrc": 6_004, "rtx_ssrc": 6_005, "rid": "200", "quality": 150, "active": false}
    ]))
    .await;
    assert_eq!(
        rig.harness.event().await,
        VoiceEvent::VideoSource {
            user_id: ALICE,
            ssrc: 6_002,
            active: true
        }
    );
    // The best active layer only; the rest stay off.
    assert_eq!(
        rig.ws.recv_op(15).await,
        json!({"op": 15, "d": {"6002": 100, "any": 0}})
    );
    let media = rig.harness.session.media();
    media.set_video_wanted(false);
    assert_eq!(rig.ws.recv_op(15).await, json!({"op": 15, "d": {"any": 0}}));
    media.set_video_wanted(true);
    assert_eq!(
        rig.ws.recv_op(15).await,
        json!({"op": 15, "d": {"6002": 100, "any": 0}})
    );
    // The streamer stops: nothing is wanted, and its state is forgotten.
    rig.announce(json!([])).await;
    assert_eq!(
        rig.harness.event().await,
        VoiceEvent::VideoSource {
            user_id: ALICE,
            ssrc: 0,
            active: false
        }
    );
    assert_eq!(rig.ws.recv_op(15).await, json!({"op": 15, "d": {"any": 0}}));
    // A streamer that leaves is dropped the same way.
    rig.announce(json!([{"ssrc": 6_000, "rtx_ssrc": 6_001, "quality": 100, "active": true}]))
        .await;
    rig.harness.event().await;
    assert_eq!(
        rig.ws.recv_op(15).await,
        json!({"op": 15, "d": {"6000": 100, "any": 0}})
    );
    rig.ws
        .send(json!({"op": 13, "d": {"user_id": ALICE.0.to_string()}}))
        .await;
    assert_eq!(rig.ws.recv_op(15).await, json!({"op": 15, "d": {"any": 0}}));
}

#[tokio::test(start_paused = true)]
async fn video_that_arrives_unannounced_or_from_the_wrong_stream_is_never_attributed() {
    let mut rig = start_viewer().await;
    let [keyframe, ..] = access_units();
    // Nobody announced SSRC 5000: held, then dropped after 100 ms.
    let held = rig.forward(keyframe, false);
    rig.send(&held);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_no_video(&mut rig.harness).await;
    assert!(rig.harness.session.stats().dropped_unknown_ssrc > 0);
    // An announcement now finds nothing waiting.
    rig.announce_stream().await;
    assert_no_video(&mut rig.harness).await;
    // Payload types nobody negotiated (96 is the server's probing) are ignored
    // or rejected, and audio SSRCs carry no video.
    let before = rig.harness.session.stats().rejected_malformed;
    let probe = rtp_datagram(&mut rig.server, 96, VIDEO_SSRC, 1, 1, true, &[], b"probe");
    let unknown = rtp_datagram(&mut rig.server, 111, VIDEO_SSRC, 2, 1, true, &[], b"other");
    rig.udp.send(probe);
    rig.udp.send(unknown);
    // A tampered datagram fails the transport tag.
    let mut tampered = rig.forward(keyframe, false)[0].datagram.clone();
    let last = tampered.len() - 10;
    tampered[last] ^= 1;
    rig.udp.send(tampered);
    tokio::time::sleep(Duration::from_millis(10)).await;
    let stats = rig.harness.session.stats();
    // The probe (payload type 96) is ignored; the unnegotiated one is not.
    assert_eq!(stats.rejected_malformed - before, 1, "{stats:?}");
    assert_eq!(stats.rejected_authentication, 1);
    assert_no_video(&mut rig.harness).await;
}

// ---- owner ----------------------------------------------------------------

struct Owner {
    harness: Harness,
    ws: Ws,
    udp: Udp,
    dave: TestDaveGroup,
    server: TransportCipher,
    client: TransportCipher,
    identify: Value,
    select: Value,
    _signaling: FakeSignaling,
}

fn layer_ready() -> Value {
    let mut ready = ready(&BOTH_MODES);
    ready["d"]["streams"] = json!([{
        "type": "video", "ssrc": OWN_VIDEO_SSRC, "rtx_ssrc": OWN_RTX_SSRC,
        "rid": "100", "quality": 100, "active": false
    }]);
    ready
}

async fn start_owner_with(ready: Value, description: Value) -> Owner {
    let (mut harness, signaling) = Harness::start(Options {
        video: Some(VideoOptions::share(SendOptions::SCREEN_720P30)),
        video_rtp_start: Some((65_530, 450_000, 900)),
        ..options()
    });
    let mut ws = signaling.accept().await;
    let (udp, identify, select) = negotiate_video(&mut harness, &mut ws, ready).await;
    ws.send(description).await;
    let dave = install_test_dave_group(&harness, &mut ws).await;
    let key = TransportKey::from_slice(&KEY).unwrap();
    Owner {
        harness,
        ws,
        udp,
        dave,
        server: TransportCipher::starting_at(MODE, &key, 900),
        client: TransportCipher::new(MODE, &key),
        identify,
        select,
        _signaling: signaling,
    }
}

async fn start_owner() -> Owner {
    start_owner_with(
        layer_ready(),
        session_description("aead_aes256_gcm_rtpsize"),
    )
    .await
}

fn frame_of(data: &[u8], millis: u64) -> VideoFrame {
    VideoFrame {
        data: data.to_vec(),
        timestamp: Duration::from_millis(millis),
    }
}

impl Owner {
    /// Sends `frame` and collects the media packets of it that leave, up to its
    /// marker, with the time each arrived.
    async fn media_packets(&mut self) -> Vec<Opened> {
        let mut packets = Vec::new();
        loop {
            let datagram = self.udp.recv().await;
            if rtcp::is_rtcp(&datagram) {
                continue;
            }
            let packet = open_video(&self.client, &datagram);
            assert_eq!(packet.header.payload_type, VIDEO_PT);
            let marker = packet.header.marker;
            packets.push(packet);
            if marker {
                return packets;
            }
        }
    }

    /// The event queue's next event that is not a keyframe request or pre-
    /// negotiation noise `skip` names.
    async fn event_after(&mut self, skip: &[VoiceEvent]) -> VoiceEvent {
        loop {
            let event = self.harness.event().await;
            if !skip.contains(&event) {
                return event;
            }
        }
    }

    fn server_feedback(&mut self, packets: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let mut compound = Vec::new();
        // A compound starts with a report (the server's SSRC is 1).
        rtcp::append_receiver_report(1, &[], &mut compound);
        packets(&mut compound);
        sealed_rtcp(&mut self.server, &compound)
    }
}

#[tokio::test(start_paused = true)]
async fn an_owner_negotiates_seals_packetizes_and_paces_a_screen_layer() {
    let mut rig = start_owner().await;
    // The offer: video, one screen layer with its limits, H.264 both ways.
    assert_eq!(rig.identify["d"]["video"], true);
    assert_eq!(
        rig.identify["d"]["streams"],
        json!([{
            "type": "screen", "rid": "100", "quality": 100,
            "max_bitrate": 2_500_000, "max_framerate": 30,
            "max_resolution": {"type": "fixed", "width": 1280, "height": 720}
        }])
    );
    assert_eq!(
        rig.select["d"]["codecs"][1],
        json!({"name": "H264", "type": "video", "priority": 1000, "payload_type": 103,
               "rtx_payload_type": 104, "encode": true, "decode": true})
    );
    assert_eq!(
        rig.harness.event().await,
        VoiceEvent::VideoNegotiated {
            codec: Some(VideoCodec::H264)
        }
    );
    // Opening media starts the stream at a keyframe.
    assert_eq!(rig.harness.event().await, VoiceEvent::KeyframeRequested);
    // An owner states nothing about what it wants to receive.
    assert!(rig.ws.1.is_empty());

    let [keyframe, delta, _] = access_units();
    let media = rig.harness.session.media();
    // A delta before any keyframe is refused, not sent.
    media.send_video(frame_of(delta, 0)).unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(rig.harness.session.stats().video_frames_refused, 1);
    assert!(rig.udp.drain().iter().all(|datagram| is_ping(datagram)));

    media.send_video(frame_of(keyframe, 100)).unwrap();
    // The layer is announced before the first packet (opcode 12).
    assert_eq!(
        rig.ws.recv_op(12).await,
        json!({"op": 12, "d": {
            "audio_ssrc": SSRC, "video_ssrc": OWN_VIDEO_SSRC, "rtx_ssrc": OWN_RTX_SSRC,
            "streams": [{
                "type": "video", "rid": "100", "ssrc": OWN_VIDEO_SSRC, "active": true,
                "quality": 100, "rtx_ssrc": OWN_RTX_SSRC, "max_bitrate": 2_500_000,
                "max_framerate": 30,
                "max_resolution": {"type": "fixed", "width": 1280, "height": 720}
            }]
        }})
    );
    let packets = rig.media_packets().await;
    assert!(packets.len() > 10);
    // Sequence and timestamp start where the test put them, wrap, and the
    // timestamp is shared by the whole frame; only the last packet has the marker.
    for (index, packet) in packets.iter().enumerate() {
        assert_eq!(packet.header.ssrc, OWN_VIDEO_SSRC);
        assert_eq!(packet.header.sequence, 65_530u16.wrapping_add(index as u16));
        assert_eq!(packet.header.timestamp, 450_000);
        assert_eq!(packet.header.marker, index == packets.len() - 1);
        assert_eq!(packet.nonce, index as u32, "fresh, consecutive nonces");
        assert!(packet.datagram_len <= 1200, "{}", packet.datagram_len);
        // abs-send-time, playout delay, screenshare content type, and the RID.
        let ids: Vec<u8> = packet.elements.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            [
                ext::ABS_SEND_TIME,
                ext::PLAYOUT_DELAY,
                ext::VIDEO_CONTENT_TYPE,
                ext::RTP_STREAM_ID
            ]
        );
        assert_eq!(packet.elements[1].1, [0, 0, 0]);
        assert_eq!(packet.elements[2].1, [1]);
        assert_eq!(packet.elements[3].1, b"100");
    }
    assert!(packets[0].header.sequence > packets[packets.len() - 1].header.sequence);
    // Paced: a keyframe of this size leaves over tens of milliseconds, not at once.
    let spread = packets[packets.len() - 1].at - packets[0].at;
    assert!(
        spread >= Duration::from_millis(10) && spread <= Duration::from_millis(60),
        "{spread:?}"
    );
    // DAVE sealed before packetization: the viewer's DAVE session opens the
    // reassembled frame, and the wire shows none of the frame's content.
    let refs: Vec<&Opened> = packets.iter().collect();
    let sealed = join(&refs);
    assert_ne!(sealed, keyframe);
    assert_eq!(rig.dave.peer.decrypt_h264(ME.0, &sealed).unwrap(), keyframe);
    let window = &keyframe[9_000..9_064];
    assert!(
        sealed
            .windows(window.len())
            .all(|candidate| candidate != window)
    );
    let stats = rig.harness.session.stats();
    assert_eq!(stats.video_frames_sent, 1);
    assert_eq!(stats.video_packets_sent, packets.len() as u64);

    // The next frame continues the stream, one clock tick per source tick.
    media.send_video(frame_of(delta, 133)).unwrap();
    let next = rig.media_packets().await;
    assert_eq!(next[0].header.timestamp, 450_000 + 33 * 90);
    assert_eq!(
        next[0].header.sequence,
        65_530u16.wrapping_add(packets.len() as u16)
    );
    let refs: Vec<&Opened> = next.iter().collect();
    assert_eq!(
        rig.dave.peer.decrypt_h264(ME.0, &join(&refs)).unwrap(),
        delta
    );
}

#[tokio::test(start_paused = true)]
async fn an_owner_answers_nacks_with_rtx_under_fresh_nonces_and_obeys_feedback() {
    let mut rig = start_owner().await;
    assert_eq!(
        rig.event_after(&[]).await,
        VoiceEvent::VideoNegotiated {
            codec: Some(VideoCodec::H264)
        }
    );
    assert_eq!(rig.harness.event().await, VoiceEvent::KeyframeRequested);
    let [keyframe, delta, _] = access_units();
    let media = rig.harness.session.media();
    media.send_video(frame_of(keyframe, 0)).unwrap();
    let packets = rig.media_packets().await;
    let last_nonce = packets.last().unwrap().nonce;

    // The server reports the second and third packets lost (the third wraps
    // the sequence number space).
    let (first, second) = (&packets[5], &packets[6]);
    assert_eq!(first.header.sequence, 65_535);
    assert_eq!(second.header.sequence, 0, "the sequence numbers wrap here");
    let feedback = rig.server_feedback(|compound| {
        rtcp::append_generic_nack(
            1,
            OWN_VIDEO_SSRC,
            &[first.header.sequence, second.header.sequence, 3_000],
            compound,
        );
    });
    rig.udp.send(feedback);
    let mut repairs = Vec::new();
    for _ in 0..2 {
        let datagram = rig.udp.recv().await;
        repairs.push(open_video(&rig.client, &datagram));
    }
    for (repair, original) in repairs.iter().zip([first, second]) {
        assert_eq!(
            (repair.header.payload_type, repair.header.ssrc),
            (RTX_PT, OWN_RTX_SSRC)
        );
        // RFC 4588: the original sequence number, then its payload; the
        // timestamp and marker are the original's.
        assert_eq!(
            u16::from_be_bytes([repair.payload[0], repair.payload[1]]),
            original.header.sequence
        );
        assert_eq!(&repair.payload[2..], original.payload);
        assert_eq!(repair.header.timestamp, original.header.timestamp);
        assert_eq!(repair.header.marker, original.header.marker);
        let ids: Vec<u8> = repair.elements.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, [ext::ABS_SEND_TIME, ext::REPAIRED_RTP_STREAM_ID]);
        // Every retransmission takes a nonce never used before.
        assert!(repair.nonce > last_nonce, "{} {}", repair.nonce, last_nonce);
        assert_ne!(repair.nonce, original.nonce);
    }
    assert_eq!(repairs[0].header.sequence, 900);
    assert_eq!(repairs[1].header.sequence, 901);
    assert_eq!(repairs[1].nonce, repairs[0].nonce + 1);
    assert_eq!(rig.harness.session.stats().retransmissions_sent, 2);

    // Asking again repeats it with yet another nonce and RTX sequence number;
    // an unknown sequence number and a packet past the 500 ms window do not.
    let again = rig.server_feedback(|compound| {
        rtcp::append_generic_nack(1, OWN_VIDEO_SSRC, &[first.header.sequence], compound);
    });
    rig.udp.send(again);
    let repeated = open_video(&rig.client, &rig.udp.recv().await);
    assert_eq!(repeated.header.sequence, 902);
    assert!(repeated.nonce > repairs[1].nonce);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let late = rig.server_feedback(|compound| {
        rtcp::append_generic_nack(1, OWN_VIDEO_SSRC, &[first.header.sequence], compound);
    });
    rig.udp.send(late);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        rig.udp
            .drain()
            .iter()
            .all(|datagram| is_ping(datagram) || rtcp::is_rtcp(datagram)),
        "an expired packet was retransmitted"
    );
    assert_eq!(rig.harness.session.stats().retransmissions_sent, 3);

    // A picture loss becomes one keyframe request, however often it repeats.
    let pli = |rig: &mut Owner| {
        rig.server_feedback(|compound| rtcp::append_picture_loss(1, OWN_VIDEO_SSRC, compound))
    };
    let (one, two) = (pli(&mut rig), pli(&mut rig));
    rig.udp.send(one);
    rig.udp.send(two);
    assert_eq!(rig.harness.event().await, VoiceEvent::KeyframeRequested);
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(rig.harness.session.next_event().now_or_never().is_none());

    // A receiver's bitrate estimate is honored at once; so is heavy loss; both
    // reach the encoder as events.
    let remb = rig
        .server_feedback(|compound| rtcp::append_remb(1, 1_000_000, &[OWN_VIDEO_SSRC], compound));
    rig.udp.send(remb);
    assert_eq!(
        rig.harness.event().await,
        VoiceEvent::VideoBitrate(1_000_000)
    );
    let loss = rig.server_feedback(|compound| {
        compound.clear();
        rtcp::append_receiver_report(
            1,
            &[rtcp::ReportBlock {
                ssrc: OWN_VIDEO_SSRC,
                fraction_lost: 40,
                cumulative_lost: 50,
                highest_sequence: 65_535,
                jitter: 3,
                last_sender_report: 0,
                delay_since_last_sender_report: 0,
            }],
            compound,
        );
    });
    rig.udp.send(loss);
    assert_eq!(rig.harness.event().await, VoiceEvent::VideoBitrate(800_000));
    // The delta after it still flows, now paced for the lower rate.
    media.send_video(frame_of(delta, 100)).unwrap();
    let next = rig.media_packets().await;
    assert_eq!(
        next[0].header.sequence,
        packets.last().unwrap().header.sequence.wrapping_add(1)
    );

    // Sender reports follow while media flows: the layer, and its RTX stream
    // because it repaired something.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let mut reports = Vec::new();
    for datagram in rig.udp.drain() {
        if rtcp::is_rtcp(&datagram) {
            reports.extend(parse_rtcp(&open_rtcp(&rig.client, &datagram)).sender_reports);
        }
    }
    let primary = reports
        .iter()
        .find(|(ssrc, _)| *ssrc == OWN_VIDEO_SSRC)
        .unwrap();
    assert_eq!(primary.1.packet_count as usize, packets.len() + next.len());
    assert!(primary.1.octet_count > 18_000);
    let rtx = reports
        .iter()
        .find(|(ssrc, _)| *ssrc == OWN_RTX_SSRC)
        .unwrap();
    assert_eq!(rtx.1.packet_count, 3);
    assert!(primary.1.ntp_timestamp > 0 && primary.1.rtp_timestamp >= 450_000);
}

#[tokio::test(start_paused = true)]
async fn an_owner_pauses_resumes_and_cuts_its_rate_when_the_pacer_is_full() {
    let mut rig = start_owner().await;
    rig.event_after(&[]).await;
    assert_eq!(rig.harness.event().await, VoiceEvent::KeyframeRequested);
    let [keyframe, delta, _] = access_units();
    let media = rig.harness.session.media();
    media.send_video(frame_of(keyframe, 0)).unwrap();
    rig.ws.recv_op(12).await;
    rig.media_packets().await;

    // The server wants the layer off (quality 0 for our SSRC).
    rig.ws
        .send(json!({"op": 15, "d": {"13960": 0, "pixelCounts": {}}}))
        .await;
    assert_eq!(rig.harness.event().await, VoiceEvent::VideoActive(false));
    let stopped = rig.ws.recv_op(12).await;
    assert_eq!(stopped["d"]["streams"][0]["active"], false);
    let refused = rig.harness.session.stats().video_frames_refused;
    media.send_video(frame_of(delta, 33)).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        rig.harness.session.stats().video_frames_refused,
        refused + 1
    );
    assert!(
        rig.udp
            .drain()
            .iter()
            .all(|d| is_ping(d) || rtcp::is_rtcp(d))
    );

    // Back on: the stream restarts at a keyframe and is announced again.
    tokio::time::sleep(Duration::from_millis(400)).await;
    rig.ws.send(json!({"op": 15, "d": {"any": 100}})).await;
    assert_eq!(rig.harness.event().await, VoiceEvent::VideoActive(true));
    assert_eq!(rig.harness.event().await, VoiceEvent::KeyframeRequested);
    media.send_video(frame_of(delta, 66)).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        rig.udp
            .drain()
            .iter()
            .all(|d| is_ping(d) || rtcp::is_rtcp(d))
    );
    media.send_video(frame_of(keyframe, 99)).unwrap();
    assert_eq!(rig.ws.recv_op(12).await["d"]["streams"][0]["active"], true);
    rig.media_packets().await;

    // A lower quality is a lower bitrate.
    rig.ws.send(json!({"op": 15, "d": {"13960": 40}})).await;
    assert_eq!(
        rig.harness.event().await,
        VoiceEvent::VideoBitrate(1_000_000)
    );

    // A frame the 100 ms pacer cannot hold is refused whole; the sequence
    // numbers do not skip, the rate drops by 20%, and a keyframe is asked for.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut huge = vec![0, 0, 0, 1, 0x65];
    huge.extend((0..200_000u32).map(|i| 0x80 | (i % 0x7F) as u8));
    media.send_video(frame_of(&huge, 200)).unwrap();
    assert_eq!(rig.harness.event().await, VoiceEvent::VideoBitrate(800_000));
    assert_eq!(rig.harness.event().await, VoiceEvent::KeyframeRequested);
    media.send_video(frame_of(delta, 233)).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        rig.udp
            .drain()
            .iter()
            .all(|d| is_ping(d) || rtcp::is_rtcp(d))
    );
    // The keyframe that follows continues the numbering without a gap.
    tokio::time::sleep(Duration::from_millis(400)).await;
    media.send_video(frame_of(keyframe, 266)).unwrap();
    let packets = rig.media_packets().await;
    assert_eq!(packets[0].header.timestamp, 450_000 + 266 * 90);
}

#[tokio::test(start_paused = true)]
async fn an_owner_without_a_layer_or_codec_sends_nothing_and_a_session_update_can_fix_it() {
    // READY assigned no layer: nothing can be sent.
    let mut rig = start_owner_with(
        ready(&BOTH_MODES),
        session_description("aead_aes256_gcm_rtpsize"),
    )
    .await;
    assert_eq!(
        rig.harness.event().await,
        VoiceEvent::VideoNegotiated { codec: None }
    );
    let [keyframe, ..] = access_units();
    rig.harness
        .session
        .media()
        .send_video(frame_of(keyframe, 0))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(rig.udp.drain().iter().all(|d| is_ping(d)));
    assert_eq!(rig.harness.session.stats().video_packets_sent, 0);

    // The server selects a codec fastcord cannot send; a later Session Update
    // names H.264 and sending starts.
    let mut description = session_description("aead_aes256_gcm_rtpsize");
    description["d"]["video_codec"] = json!("AV1");
    let mut rig = start_owner_with(layer_ready(), description).await;
    assert_eq!(
        rig.harness.event().await,
        VoiceEvent::VideoNegotiated { codec: None }
    );
    rig.ws
        .send(json!({"op": 14, "d": {"video_codec": "H264", "keyframe_interval": 5000}}))
        .await;
    assert_eq!(
        rig.harness.event().await,
        VoiceEvent::VideoNegotiated {
            codec: Some(VideoCodec::H264)
        }
    );
    rig.harness
        .session
        .media()
        .send_video(frame_of(keyframe, 0))
        .unwrap();
    rig.ws.recv_op(12).await;
    let packets = rig.media_packets().await;
    assert!(packets.len() > 10);
    // Back to a codec it cannot send: the stream stops being sendable.
    rig.ws
        .send(json!({"op": 14, "d": {"video_codec": "VP9"}}))
        .await;
    assert_eq!(
        rig.event_after(&[VoiceEvent::KeyframeRequested]).await,
        VoiceEvent::VideoNegotiated { codec: None }
    );
}

#[tokio::test(start_paused = true)]
async fn video_waits_for_dave_keys_and_never_leaves_in_the_clear() {
    let (mut harness, signaling) = Harness::start(Options {
        video: Some(VideoOptions::share(SendOptions::SCREEN_720P30)),
        ..options()
    });
    let mut ws = signaling.accept().await;
    let (mut udp, _, _) = negotiate_video(&mut harness, &mut ws, layer_ready()).await;
    ws.send(session_description("aead_aes256_gcm_rtpsize"))
        .await;
    harness
        .wait_status(|status| matches!(status, VoiceStatus::Rekeying { .. }))
        .await;
    let [keyframe, ..] = access_units();
    let media = harness.session.media();
    // Frames wait in a three-frame queue; the fourth is refused whole.
    for _ in 0..3 {
        media.send_video(frame_of(keyframe, 0)).unwrap();
    }
    assert_eq!(
        media.send_video(frame_of(keyframe, 0)),
        Err(MediaSendError::Full)
    );
    assert_eq!(
        media.send_video(frame_of(&[], 0)),
        Err(MediaSendError::InvalidFrame)
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let sent = udp.drain();
    assert!(
        sent.iter().all(|datagram| is_ping(datagram)),
        "{}",
        sent.len()
    );
    assert_eq!(harness.session.stats().video_packets_sent, 0);
    // Keys arrive: what waited is stale and discarded; only a fresh frame leaves.
    let dave = install_test_dave_group(&harness, &mut ws).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(udp.drain().iter().all(|datagram| is_ping(datagram)));
    media.send_video(frame_of(keyframe, 500)).unwrap();
    let key = TransportKey::from_slice(&KEY).unwrap();
    let client = TransportCipher::new(MODE, &key);
    let mut packets = Vec::new();
    loop {
        let datagram = udp.recv().await;
        if rtcp::is_rtcp(&datagram) {
            continue;
        }
        let packet = open_video(&client, &datagram);
        let marker = packet.header.marker;
        packets.push(packet);
        if marker {
            break;
        }
    }
    let mut dave = dave;
    let refs: Vec<&Opened> = packets.iter().collect();
    assert_eq!(
        dave.peer.decrypt_h264(ME.0, &join(&refs)).unwrap(),
        keyframe
    );
    assert_eq!(packets[0].nonce, 0);
    drop(ws);
}

#[tokio::test(start_paused = true)]
async fn an_audio_only_connection_offers_and_declares_no_video() {
    let (mut harness, signaling) = Harness::start(options());
    let mut ws = signaling.accept().await;
    let (_, select) = negotiate(&mut harness, &mut ws, &BOTH_MODES).await;
    assert_eq!(
        select["d"]["codecs"],
        json!([{"name": "opus", "type": "audio", "priority": 1000, "payload_type": 120}])
    );
    ws.send(session_description("aead_aes256_gcm_rtpsize"))
        .await;
    install_test_dave_group(&harness, &mut ws).await;
    // No video declaration, no wants, and no video media is accepted.
    assert!(ws.1.is_empty());
    assert_eq!(
        harness
            .session
            .media()
            .send_video(frame_of(access_units()[0], 0)),
        Ok(())
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.session.stats().video_frames_sent, 0);
    harness.session.media().set_video_wanted(true);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(ws.try_recv().is_none());
}
