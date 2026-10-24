//! Video on one voice connection (SPEC §8.3, milestones 25–26).
//!
//! - **Receiving.** Each announced remote video SSRC gets a [`Receiver`]: its
//!   RTP (and RTX repairs, unwrapped back onto the primary SSRC) go through a
//!   bounded [`ReceiveBuffer`]; every complete frame is depacketized, opened by
//!   DAVE as a whole, and handed to the consumer. Anything lost, undecryptable,
//!   or not taken in time makes the stream wait for a keyframe, which the
//!   receiver asks for with a PLI. Missing packets are asked for with NACKs;
//!   receiver reports describe loss and jitter.
//! - **Sending.** One layer (an owner's screen share): frames are sealed by
//!   DAVE, packetized, paced, remembered for retransmission, and answered
//!   through NACK/RTX, PLI, REMB, and reception reports. The encoder is told
//!   what to do with events, never by calls into it.
//! - **Viewing.** A viewer declares what it wants with Media Sink Wants, so the
//!   server forwards only that stream.
//!
//! Everything here shares the connection's transport cipher (every datagram,
//! RTP or RTCP, takes a fresh nonce) and its DAVE session. Plain media never
//! leaves, and a frame that fails DAVE never reaches the consumer.

use std::collections::HashMap;
use std::time::Duration;

use fastcord_model::Snowflake;
use rand_core::{OsRng, RngCore};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{Conn, Driver, End, Phase, ntp_now};
use crate::crypto::{CryptoError, RTCP_CLEAR_LEN};
use crate::dave::DaveError;
use crate::gateway::transport::{Network, Signaling, UdpLink};
use crate::gateway::wire::{self, AssignedLayer, Layer};
use crate::gateway::{CloseReason, Counters, VoiceEvent};
use crate::rtcp::{self, Feedback, FeedbackKind, ReportBlock, SenderInfo};
use crate::rtp::{self, RtpHeader, ext};
use crate::ssrc::{PENDING_WINDOW, PendingSsrc, StreamKind};
use crate::video::receive::{Assembled, ReceiveBuffer, ReceptionStats};
use crate::video::send::{Packet, VideoSender};
use crate::video::{
    ReceivedVideo, VideoCodec, VideoControl, VideoError, VideoFrame, VideoInterest, VideoOptions,
};
use crate::video::{av1, h264};

/// Remote video SSRCs followed at once: the cameras of a call that are on
/// screen (`MAX_INTEREST_USERS`), each with its own bounded reassembly buffer.
/// A stream connection has one owner with one layer; more than this is a
/// server misbehaving, not a feature.
const MAX_VIDEO_RECEIVERS: usize = crate::video::MAX_INTEREST_USERS;
/// Users whose announced streams are remembered for Media Sink Wants.
const MAX_REMOTE_VIDEO_USERS: usize = 16;
/// How often a receiver report goes out while video is arriving.
const RECEIVER_REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// Picture Loss Indications for one stream are at least this far apart.
const PLI_INTERVAL: Duration = Duration::from_millis(500);
/// A stream whose first packet just arrived is given this long to produce a
/// keyframe (the server is usually sending one) before one is requested.
const INITIAL_KEYFRAME_WAIT: Duration = Duration::from_millis(250);
/// Unannounced video held for its Video event: 100 ms of a 5 Mbit/s stream.
const PENDING_VIDEO_BYTES: usize = 64 * 1024;
/// The round trip assumed before one is measured.
const DEFAULT_RTT: Duration = Duration::from_millis(50);
/// Packets one wake-up sends at most, so other work is never starved.
const MAX_PUMP: usize = 64;
/// NACKed sequence numbers answered per feedback packet.
const MAX_REPAIRS_PER_NACK: usize = 256;
/// RTP payloads are at most one Ethernet MTU.
const MAX_VIDEO_PAYLOAD: usize = 1500;
/// Reserved by Discord for probe packets (docs.discord.food, Codec Structure).
pub(super) const PROBE_PAYLOAD_TYPE: u8 = 96;
/// Playout delay extension: minimum and maximum in 10 ms units, 12 bits each.
/// Screen content asks for none. [INFERENCE: the value is a low-latency
/// default, not captured from an official client.]
const PLAYOUT_DELAY: [u8; 3] = [0, 0, 0];
/// Video content type extension: screenshare.
const CONTENT_SCREEN: u8 = 1;

/// A packet whose SSRC nobody announced yet, already authenticated.
struct PendingVideo {
    header: RtpHeader,
    payload: Vec<u8>,
}

#[derive(Clone, Copy)]
struct RemoteStream {
    ssrc: u32,
    quality: u32,
    active: bool,
}

/// One remote video stream being received.
struct Receiver {
    user_id: Snowflake,
    codec: VideoCodec,
    buffer: ReceiveBuffer,
    stats: ReceptionStats,
    /// Frames are dropped until a keyframe arrives: either nothing was decoded
    /// yet, or a frame was lost, refused, or not taken.
    awaiting_keyframe: bool,
    /// A PLI is wanted since this moment.
    pli_pending: Option<Instant>,
    last_pli: Option<Instant>,
}

impl Receiver {
    fn new(user_id: Snowflake, codec: VideoCodec, now: Instant) -> Self {
        Self {
            user_id,
            codec,
            buffer: ReceiveBuffer::new(),
            stats: ReceptionStats::new(now),
            awaiting_keyframe: true,
            pli_pending: Some(now + INITIAL_KEYFRAME_WAIT),
            last_pli: None,
        }
    }

    /// The stream is damaged or not started: wait for a keyframe and ask for
    /// one.
    fn want_keyframe(&mut self, now: Instant) {
        self.awaiting_keyframe = true;
        self.pli_pending = Some(self.pli_pending.map_or(now, |since| since.min(now)));
    }

    /// When a wanted PLI may go out.
    fn pli_at(&self) -> Option<Instant> {
        let since = self.pli_pending?;
        Some(
            self.last_pli
                .map_or(since, |last| since.max(last + PLI_INTERVAL)),
        )
    }

    fn next_wake(&self, retry: Duration) -> Option<Instant> {
        [self.buffer.next_wake(retry), self.pli_at()]
            .into_iter()
            .flatten()
            .min()
    }
}

pub(super) struct VideoState {
    options: VideoOptions,
    /// The layer the server assigned in READY (owners only).
    layer: Option<AssignedLayer>,
    sender: Option<VideoSender>,
    /// The layer was announced active (opcode 12) since it last stopped.
    announced: bool,
    /// The server wants our layer off.
    paused: bool,
    /// The next frame to send must be a keyframe: nothing was sent yet, or a
    /// frame did not leave.
    need_keyframe: bool,
    /// Whether DAVE let local media flow at the last look.
    gate_open: bool,
    receivers: HashMap<u32, Receiver>,
    pending: PendingSsrc<PendingVideo>,
    /// Streams the server announced per remote user (opcode 12).
    remote: HashMap<Snowflake, Vec<RemoteStream>>,
    /// Which remote video the viewer wants.
    interest: VideoInterest,
    /// Users shown small: their lowest active layer is wanted.
    thumbnails: Vec<Snowflake>,
    /// Frames may be sent (a camera that is on; a share always).
    sending: bool,
    wants_sent: Option<Vec<(u32, u8)>>,
    rtt: Duration,
    next_report: Option<Instant>,
}

impl VideoState {
    pub(super) fn new(options: VideoOptions, control: VideoControl) -> Self {
        Self {
            options,
            layer: None,
            sender: None,
            announced: false,
            paused: false,
            need_keyframe: true,
            gate_open: false,
            receivers: HashMap::new(),
            pending: PendingSsrc::new(PENDING_WINDOW, PENDING_VIDEO_BYTES),
            remote: HashMap::new(),
            interest: control.interest,
            thumbnails: control.thumbnails,
            sending: control.sending,
            wants_sent: None,
            rtt: DEFAULT_RTT,
            next_report: None,
        }
    }

    pub(super) fn set_layer(&mut self, layer: Option<AssignedLayer>) {
        self.layer = layer;
    }

    /// Whether `payload_type` names negotiated video: a decodable codec's
    /// media or its RTX stream.
    pub(super) fn is_video_payload(&self, payload_type: u8) -> bool {
        self.options.receive_offer(payload_type).is_some()
    }

    pub(super) fn note_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    pub(super) fn want_remote_keyframe(&mut self, ssrc: u32, now: Instant) {
        if let Some(receiver) = self.receivers.get_mut(&ssrc) {
            receiver.want_keyframe(now);
        }
    }

    /// NACK retry spacing: one round trip plus a little, within 30–100 ms.
    fn retry_interval(&self) -> Duration {
        (self.rtt + Duration::from_millis(10))
            .clamp(Duration::from_millis(30), Duration::from_millis(100))
    }

    /// The streams to ask the server for: the best active layer of each wanted
    /// user who streams, or none while the viewer does not want video.
    fn desired_wants(&self) -> Vec<(u32, u8)> {
        let users = match &self.interest {
            VideoInterest::Off => return Vec::new(),
            VideoInterest::All => None,
            VideoInterest::Users(users) => Some(users),
        };
        let mut wants: Vec<(u32, u8)> = self
            .remote
            .iter()
            .filter(|(user, _)| users.is_none_or(|users| users.contains(user)))
            .filter_map(|(user, streams)| {
                let active = streams.iter().filter(|stream| stream.active);
                if self.thumbnails.contains(user) {
                    active.min_by_key(|stream| stream.quality)
                } else {
                    active.max_by_key(|stream| stream.quality)
                }
                .map(|stream| (stream.ssrc, 100))
            })
            .collect();
        wants.sort_unstable();
        wants
    }

    /// Our layer as the Video opcode describes it.
    fn layer_wire(&self) -> Option<Layer> {
        let sender = self.sender.as_ref()?;
        Some(Layer {
            assigned: self.layer.clone()?,
            options: sender.options,
            max_bitrate: sender.options.bitrate,
        })
    }
}

fn emit(events: &mpsc::Sender<VoiceEvent>, event: VoiceEvent) {
    // A consumer that stopped reading loses events, never the connection.
    let _ = events.try_send(event);
}

/// Tells the encoder to make its next frame a keyframe, unless it was told
/// within the last 300 ms.
fn request_keyframe(events: &mpsc::Sender<VoiceEvent>, video: &mut VideoState, now: Instant) {
    if let Some(sender) = video.sender.as_mut()
        && sender.keyframe_wanted(now)
    {
        emit(events, VoiceEvent::KeyframeRequested);
    }
}

/// The 24-bit abs-send-time (6.18 fixed-point seconds) of an NTP timestamp.
fn abs_send_time(ntp: u64) -> [u8; 3] {
    let value = ((ntp >> 14) & 0x00FF_FFFF) as u32;
    [(value >> 16) as u8, (value >> 8) as u8, value as u8]
}

fn random_start() -> (u16, u32, u16) {
    (
        OsRng.next_u32() as u16,
        OsRng.next_u32(),
        OsRng.next_u32() as u16,
    )
}

impl<S: Signaling, N: Network> Driver<S, N> {
    // ---- session ----------------------------------------------------------

    /// Whether a layer is ready to take frames.
    pub(super) fn video_can_send(&self) -> bool {
        self.video
            .as_ref()
            .is_some_and(|video| video.sender.is_some())
    }

    /// Follows whether DAVE lets local media flow. Frames that waited for keys
    /// are stale and discarded; any gap restarts the stream at a keyframe.
    pub(super) fn video_gate(&mut self, open: bool) {
        let Some(video) = self.video.as_mut() else {
            return;
        };
        if video.gate_open == open {
            return;
        }
        video.gate_open = open;
        video.need_keyframe = true;
        if open {
            while self.ch.video_in.try_recv().is_ok() {}
            request_keyframe(&self.ch.events, video, Instant::now());
        }
    }

    /// The earliest moment video has work: pacing, NACKs and PLIs, frame
    /// deadlines, reports, or held packets expiring.
    pub(super) fn video_wake(&self) -> Option<Instant> {
        let video = self.video.as_ref()?;
        let retry = video.retry_interval();
        [
            video.sender.as_ref().and_then(VideoSender::next_wake),
            video.pending.next_expiry(),
            video
                .receivers
                .values()
                .filter_map(|receiver| receiver.next_wake(retry))
                .min(),
            if video.receivers.is_empty() {
                None
            } else {
                video.next_report
            },
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Session Description arrived: build the sender for an owner, or declare
    /// that a viewer sends no video, then state what a viewer wants.
    pub(super) async fn video_session_ready(
        &mut self,
        conn: &Conn,
        socket: &mut S::Socket,
        video_codec: Option<&str>,
    ) -> Result<(), End> {
        let now = Instant::now();
        let start = self.options.video_rtp_start.unwrap_or_else(random_start);
        let Some(video) = self.video.as_mut() else {
            return Ok(());
        };
        match video.options.send {
            Some(send) => {
                let offer = video_codec
                    .and_then(VideoCodec::from_wire)
                    .and_then(|codec| video.options.send_offer(codec));
                video.sender = match (offer, video.layer.as_ref()) {
                    (Some(offer), Some(layer)) => Some(VideoSender::new(
                        offer,
                        layer.ssrc,
                        layer.rtx_ssrc,
                        send,
                        start,
                        now,
                    )),
                    _ => None,
                };
                video.announced = false;
                video.paused = false;
                video.need_keyframe = true;
                let codec = video.sender.as_ref().map(|sender| sender.offer.codec);
                emit(&self.ch.events, VoiceEvent::VideoNegotiated { codec });
                if video.options.camera
                    && let Some(audio_ssrc) = self.udp.as_ref().map(|udp| udp.ssrc)
                {
                    // A camera that is off is declared like a viewer: no
                    // video SSRC until the first frame announces the layer.
                    let payload = wire::video_state(audio_ssrc, None);
                    Self::send_text(conn, socket, &payload).await?;
                }
            }
            None => {
                // A viewer states that it sends no video before it receives
                // any (docs.discord.food: "at least one Video payload before
                // sending or receiving video data").
                if let Some(audio_ssrc) = self.udp.as_ref().map(|udp| udp.ssrc) {
                    let payload = wire::video_state(audio_ssrc, None);
                    Self::send_text(conn, socket, &payload).await?;
                }
            }
        }
        self.send_wants(conn, socket).await
    }

    /// The Gateway resumed: what the server was told may be forgotten.
    pub(super) async fn video_resumed(
        &mut self,
        conn: &Conn,
        socket: &mut S::Socket,
    ) -> Result<(), End> {
        if let Some(video) = self.video.as_mut() {
            video.wants_sent = None;
        }
        self.send_wants(conn, socket).await
    }

    /// The codec the server wants us to send changed (opcode 14).
    pub(super) fn on_session_update(&mut self, video_codec: Option<&str>) {
        let now = Instant::now();
        let start = self.options.video_rtp_start.unwrap_or_else(random_start);
        let Some(video) = self.video.as_mut() else {
            return;
        };
        let (Some(send), Some(name)) = (video.options.send, video_codec) else {
            return;
        };
        let offer = VideoCodec::from_wire(name).and_then(|codec| video.options.send_offer(codec));
        let current = video.sender.as_ref().map(|sender| sender.offer.codec);
        if offer.map(|offer| offer.codec) == current {
            return;
        }
        video.sender = match (offer, video.layer.as_ref()) {
            (Some(offer), Some(layer)) => Some(VideoSender::new(
                offer,
                layer.ssrc,
                layer.rtx_ssrc,
                send,
                start,
                now,
            )),
            _ => None,
        };
        video.announced = false;
        video.need_keyframe = true;
        let codec = video.sender.as_ref().map(|sender| sender.offer.codec);
        emit(&self.ch.events, VoiceEvent::VideoNegotiated { codec });
    }

    // ---- viewing ----------------------------------------------------------

    /// Tells the server which remote streams to forward, when that changed.
    async fn send_wants(&mut self, conn: &Conn, socket: &mut S::Socket) -> Result<(), End> {
        if conn.phase != Phase::Connected {
            return Ok(());
        }
        let Some(video) = self.video.as_mut() else {
            return Ok(());
        };
        if video.options.send.is_some() && !video.options.camera {
            // Share owners send no wants: nobody streams to them. A call
            // participant is both.
            return Ok(());
        }
        let wants = video.desired_wants();
        if video.wants_sent.as_ref() == Some(&wants)
            || (wants.is_empty() && video.wants_sent.is_none())
        {
            return Ok(());
        }
        let payload = wire::media_sink_wants(&wants);
        video.wants_sent = Some(wants);
        Self::send_text(conn, socket, &payload).await
    }

    /// The application changed what it receives or whether it sends.
    pub(super) async fn on_wanted_changed(
        &mut self,
        conn: &Conn,
        socket: &mut S::Socket,
    ) -> Result<(), End> {
        let control = self.ch.wanted.borrow_and_update().clone();
        let mut stopped_layer = false;
        if let Some(video) = self.video.as_mut() {
            let now = Instant::now();
            if control.interest != VideoInterest::Off && control.interest != video.interest {
                // Video resumes mid-stream: start again at a keyframe.
                for receiver in video.receivers.values_mut() {
                    receiver.want_keyframe(now);
                }
            }
            let thumbnails_changed = video.thumbnails != control.thumbnails;
            if thumbnails_changed && video.interest != VideoInterest::Off {
                // A different layer is a different stream: it starts at a keyframe.
                for receiver in video.receivers.values_mut() {
                    receiver.want_keyframe(now);
                }
            }
            video.thumbnails = control.thumbnails;
            video.interest = control.interest;
            if video.sending != control.sending {
                video.sending = control.sending;
                // Either way the stream restarts at a keyframe.
                video.need_keyframe = true;
                if control.sending {
                    request_keyframe(&self.ch.events, video, now);
                } else {
                    stopped_layer = std::mem::take(&mut video.announced);
                }
            }
        }
        if stopped_layer {
            self.announce_video(conn, socket, false).await?;
        }
        self.send_wants(conn, socket).await
    }

    /// A remote user's video state (opcode 12): remember its streams, release
    /// packets that raced ahead of the announcement, and update the wants.
    pub(super) async fn on_remote_video(
        &mut self,
        conn: &Conn,
        socket: &mut S::Socket,
        announced: &wire::Video,
    ) -> Result<(), End> {
        let Some(video) = self.video.as_mut() else {
            return Ok(());
        };
        let streams: Vec<RemoteStream> = announced
            .streams
            .iter()
            .map(|stream| RemoteStream {
                ssrc: stream.ssrc,
                quality: stream.quality.unwrap_or(100),
                active: stream.active.unwrap_or(true),
            })
            .collect();
        let best = streams
            .iter()
            .filter(|stream| stream.active && stream.ssrc != 0)
            .max_by_key(|stream| stream.quality)
            .map(|stream| stream.ssrc);
        if streams.is_empty() {
            video.remote.remove(&announced.user_id);
        } else if video.remote.len() < MAX_REMOTE_VIDEO_USERS
            || video.remote.contains_key(&announced.user_id)
        {
            video.remote.insert(announced.user_id, streams);
        }
        emit(
            &self.ch.events,
            VoiceEvent::VideoSource {
                user_id: announced.user_id,
                ssrc: best.unwrap_or(0),
                active: best.is_some(),
            },
        );
        let held: Vec<u32> = announced
            .streams
            .iter()
            .flat_map(|stream| [Some(stream.ssrc), stream.rtx_ssrc])
            .flatten()
            .filter(|ssrc| *ssrc != 0)
            .collect();
        for ssrc in held {
            self.release_pending_video(ssrc);
        }
        self.send_wants(conn, socket).await
    }

    /// A remote user left: its streams are no longer wanted.
    pub(super) async fn video_user_gone(
        &mut self,
        conn: &Conn,
        socket: &mut S::Socket,
        user_id: Snowflake,
    ) -> Result<(), End> {
        let removed = self
            .video
            .as_mut()
            .is_some_and(|video| video.remote.remove(&user_id).is_some());
        if removed {
            self.send_wants(conn, socket).await
        } else {
            Ok(())
        }
    }

    /// Receivers follow the SSRC map: a stream that is no longer mapped to the
    /// same user is dropped with its buffered packets.
    pub(super) fn video_mappings_changed(&mut self) {
        let ssrcs = &self.ssrcs;
        if let Some(video) = self.video.as_mut() {
            video.receivers.retain(|ssrc, receiver| {
                ssrcs.owner(*ssrc).is_some_and(|owner| {
                    owner.kind == StreamKind::Video && owner.user_id == receiver.user_id
                })
            });
        }
    }

    // ---- receiving --------------------------------------------------------

    /// One datagram of negotiated video payload type: authenticate it, strip
    /// the extension block, and route it.
    pub(super) fn on_video_rtp(&mut self, packet: &[u8], header: RtpHeader) {
        let counters = &self.ch.counters;
        let Some(media) = &self.media else {
            return;
        };
        match media
            .cipher
            .open(packet, header.clear_len(), &mut self.plain)
        {
            Ok(_) => {}
            Err(CryptoError::Authentication) => {
                Counters::add(&counters.rejected_authentication, 1);
                return;
            }
            Err(_) => {
                Counters::add(&counters.rejected_malformed, 1);
                return;
            }
        }
        let Ok(body) = header.split_body(&self.plain) else {
            Counters::add(&counters.rejected_malformed, 1);
            return;
        };
        if body.extensions.elements().any(|element| element.is_err())
            || body.payload.is_empty()
            || body.payload.len() > MAX_VIDEO_PAYLOAD
        {
            Counters::add(&counters.rejected_malformed, 1);
            return;
        }
        Counters::add(&counters.rtp_received, 1);
        Counters::add(&counters.video_packets_received, 1);
        let held = PendingVideo {
            header,
            payload: body.payload.to_vec(),
        };
        let now = Instant::now();
        if self.ssrcs.owner(header.ssrc).is_some() {
            self.deliver_video_packet(now, held);
        } else if let Some(video) = self.video.as_mut() {
            let bytes = held.payload.len();
            let dropped = video.pending.push(now, header.ssrc, bytes, held);
            Counters::add(&self.ch.counters.dropped_unknown_ssrc, dropped as u64);
        }
    }

    /// The Video event named `ssrc`: its held packets go through now.
    fn release_pending_video(&mut self, ssrc: u32) {
        let now = Instant::now();
        let Some(video) = self.video.as_mut() else {
            return;
        };
        let expired = video.pending.expire(now);
        Counters::add(&self.ch.counters.dropped_unknown_ssrc, expired as u64);
        for held in video.pending.take(ssrc) {
            self.deliver_video_packet(now, held);
        }
    }

    /// Routes one authenticated packet of a mapped SSRC to its receiver; RTX
    /// repairs are unwrapped onto the primary stream.
    fn deliver_video_packet(&mut self, now: Instant, packet: PendingVideo) {
        let header = packet.header;
        let Some(owner) = self.ssrcs.owner(header.ssrc) else {
            return;
        };
        let counters = &self.ch.counters;
        let Some(video) = self.video.as_mut() else {
            return;
        };
        let Some((offer, rtx)) = video.options.receive_offer(header.payload_type) else {
            Counters::add(&counters.rejected_malformed, 1);
            return;
        };
        let (primary, media_payload, sequence) = match (owner.kind, rtx) {
            (StreamKind::Video, false) => (header.ssrc, &packet.payload[..], header.sequence),
            (StreamKind::Retransmission, true) => {
                let Some(primary) = self.ssrcs.primary_of(header.ssrc) else {
                    Counters::add(&counters.rejected_malformed, 1);
                    return;
                };
                // RFC 4588: the original sequence number, then its payload.
                let Some((original, payload)) = packet.payload.split_first_chunk::<2>() else {
                    Counters::add(&counters.rejected_malformed, 1);
                    return;
                };
                if payload.is_empty() {
                    Counters::add(&counters.rejected_malformed, 1);
                    return;
                }
                (primary, payload, u16::from_be_bytes(*original))
            }
            // Audio SSRC, or media on the wrong stream.
            _ => {
                Counters::add(&counters.rejected_malformed, 1);
                return;
            }
        };
        if video.receivers.get(&primary).is_some_and(|receiver| {
            receiver.user_id != owner.user_id || receiver.codec != offer.codec
        }) {
            video.receivers.remove(&primary);
        }
        if !video.receivers.contains_key(&primary) {
            if video.receivers.len() >= MAX_VIDEO_RECEIVERS {
                Counters::add(&counters.video_frames_dropped, 1);
                return;
            }
            video
                .receivers
                .insert(primary, Receiver::new(owner.user_id, offer.codec, now));
            video
                .next_report
                .get_or_insert(now + RECEIVER_REPORT_INTERVAL);
        }
        let Some(receiver) = video.receivers.get_mut(&primary) else {
            return;
        };
        let stream_header = RtpHeader {
            ssrc: primary,
            sequence,
            ..header
        };
        if rtx {
            receiver.stats.note_recovered();
        } else {
            receiver.stats.note_packet(now, sequence, header.timestamp);
        }
        if receiver
            .buffer
            .push(now, &stream_header, media_payload)
            .is_err()
        {
            Counters::add(&counters.video_frames_dropped, 1);
            receiver.want_keyframe(now);
        }
        if receiver.buffer.take_broken() {
            receiver.want_keyframe(now);
        }
        self.drain_receiver(now, primary);
    }

    /// Hands out every frame of `primary` that is complete.
    fn drain_receiver(&mut self, now: Instant, primary: u32) {
        loop {
            let assembled = self
                .video
                .as_mut()
                .and_then(|video| video.receivers.get_mut(&primary))
                .and_then(|receiver| receiver.buffer.pop_frame(now));
            let Some(assembled) = assembled else {
                return;
            };
            self.finish_frame(now, primary, assembled);
        }
    }

    /// One complete frame: depacketize, hold it for a keyframe if the stream is
    /// damaged, open it with DAVE, and deliver it.
    fn finish_frame(&mut self, now: Instant, primary: u32, assembled: Assembled) {
        let counters = &self.ch.counters;
        let Some(video) = self.video.as_mut() else {
            return;
        };
        let Some(receiver) = video.receivers.get_mut(&primary) else {
            return;
        };
        let payloads: Vec<&[u8]> = assembled.payloads.iter().map(Vec::as_slice).collect();
        let mut sealed = Vec::new();
        let depacketized = match receiver.codec {
            VideoCodec::H264 => {
                h264::depacketize(&payloads, &mut sealed).map(|()| h264::is_keyframe(&sealed))
            }
            VideoCodec::Av1 => av1::depacketize(&payloads, av1::Framing::Dave, &mut sealed),
        };
        let Ok(mut keyframe) = depacketized else {
            Counters::add(&counters.video_frames_dropped, 1);
            receiver.want_keyframe(now);
            return;
        };
        // AV1's frame-header payload is encrypted: N, unlike those bits,
        // describes the coded-sequence boundary at the RTP layer.
        if receiver.awaiting_keyframe && !keyframe {
            Counters::add(&counters.video_frames_dropped, 1);
            receiver.want_keyframe(now);
            return;
        }
        let opened = self
            .dave
            .as_mut()
            .ok_or(DaveError::NotReady)
            .and_then(|dave| dave.decrypt_video(receiver.user_id.0, &sealed));
        let Ok(mut plain) = opened else {
            Counters::add(&counters.rejected_authentication, 1);
            Counters::add(&counters.video_frames_dropped, 1);
            receiver.want_keyframe(now);
            return;
        };
        match receiver.codec {
            VideoCodec::H264 => keyframe = h264::is_keyframe(&plain),
            VideoCodec::Av1 => {
                // Reuse the sealed allocation; sizes are restored only after
                // authentication, never rewritten inside DAVE's AAD.
                if av1::normalize(&plain, &mut sealed).is_err() {
                    Counters::add(&counters.video_frames_dropped, 1);
                    receiver.want_keyframe(now);
                    return;
                }
                plain = sealed;
            }
        }
        let frame = ReceivedVideo {
            user_id: receiver.user_id,
            ssrc: primary,
            codec: receiver.codec,
            timestamp: assembled.timestamp,
            keyframe,
            data: plain,
        };
        match self.ch.video_out.try_send(frame) {
            Ok(()) => {
                Counters::add(&counters.video_frames_received, 1);
                if keyframe {
                    // A keyframe answers any request still waiting to be sent.
                    receiver.awaiting_keyframe = false;
                    receiver.pli_pending = None;
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                Counters::add(&counters.dropped_queue_full, 1);
                Counters::add(&counters.video_frames_dropped, 1);
                receiver.want_keyframe(now);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// The sender's report about a remote stream: echoed back in ours so the
    /// sender can measure the round trip.
    pub(super) fn on_remote_sender_report(&mut self, ssrc: u32, info: SenderInfo) {
        let now = Instant::now();
        if let Some(receiver) = self
            .video
            .as_mut()
            .and_then(|video| video.receivers.get_mut(&ssrc))
        {
            receiver
                .stats
                .note_sender_report(now, rtcp::ntp_middle(info.ntp_timestamp));
        }
    }

    /// Everything the receive side owes the server at `now`: NACKs for gaps
    /// that outlived reordering, PLIs for damaged streams, and the periodic
    /// receiver report, as one compound RTCP datagram.
    async fn flush_feedback(&mut self, now: Instant) -> Result<(), End> {
        let (Some(video), Some(media), Some(udp)) =
            (self.video.as_mut(), self.media.as_mut(), self.udp.as_ref())
        else {
            return Ok(());
        };
        if video.receivers.is_empty() {
            return Ok(());
        }
        let report_due = video.next_report.is_some_and(|at| at <= now);
        let retry = video.retry_interval();
        let mut blocks = Vec::new();
        let mut nacks: Vec<(u32, Vec<u16>)> = Vec::new();
        let mut plis = Vec::new();
        for (&ssrc, receiver) in &mut video.receivers {
            let lost = receiver.buffer.nacks(now, retry);
            if !lost.is_empty() {
                nacks.push((ssrc, lost));
            }
            if receiver.pli_at().is_some_and(|at| at <= now) {
                receiver.pli_pending = None;
                receiver.last_pli = Some(now);
                plis.push(ssrc);
            }
            if report_due && let Some(block) = receiver.stats.block(now, ssrc) {
                blocks.push(block);
            }
        }
        if report_due {
            video.next_report = Some(now + RECEIVER_REPORT_INTERVAL);
        }
        if blocks.is_empty() && nacks.is_empty() && plis.is_empty() {
            return Ok(());
        }
        // A compound starts with a report, even an empty one (RFC 3550 §6.1).
        let mut out = std::mem::take(&mut self.rtcp);
        out.clear();
        rtcp::append_receiver_report(udp.ssrc, &blocks, &mut out);
        let mut requested = 0u64;
        for (ssrc, sequences) in &nacks {
            requested += sequences.len() as u64;
            rtcp::append_generic_nack(udp.ssrc, *ssrc, sequences, &mut out);
        }
        for ssrc in &plis {
            rtcp::append_picture_loss(udp.ssrc, *ssrc, &mut out);
        }
        let (clear, body) = out.split_at(RTCP_CLEAR_LEN);
        let sealed = media
            .cipher
            .seal(clear, body, &mut self.sealed)
            .map_err(|_| End::Close(CloseReason::KeyExhausted));
        self.rtcp = out;
        sealed?;
        if udp.link.send_to(&self.sealed, udp.server).await.is_ok() {
            let counters = &self.ch.counters;
            Counters::add(&counters.nack_sequences_sent, requested);
            Counters::add(&counters.keyframe_requests_sent, plis.len() as u64);
        }
        Ok(())
    }

    // ---- sending ----------------------------------------------------------

    /// One frame from the encoder: gate, seal with DAVE, packetize, queue for
    /// pacing, and send what is due.
    pub(super) async fn on_video_frame(
        &mut self,
        conn: &Conn,
        socket: &mut S::Socket,
        frame: VideoFrame,
    ) -> Result<(), End> {
        let now = Instant::now();
        let Some(video) = self.video.as_ref() else {
            return Ok(());
        };
        if video.sender.is_none() || video.paused || !video.sending {
            Counters::add(&self.ch.counters.video_frames_refused, 1);
            return Ok(());
        }
        let keyframe = h264::is_keyframe(&frame.data);
        if video.need_keyframe && !keyframe {
            // After any gap the stream restarts at a keyframe.
            self.refuse_video_frame(now, false);
            return Ok(());
        }
        let sender = video.sender.as_ref().expect("checked above");
        if !sender.can_accept_frame(now, frame.data.len(), keyframe) {
            // A frame too large even for an empty queue is not evidence of
            // network congestion. Cutting bitrate also cuts that queue's
            // capacity, trapping every future keyframe in a downward spiral.
            let congested = sender.has_queued_packets();
            self.refuse_video_frame(now, congested);
            return Ok(());
        }
        let Some(dave) = self.dave.as_mut() else {
            return Err(End::Close(CloseReason::DaveUnavailable));
        };
        let sealed = match dave.encrypt_video(crate::video::VideoCodec::H264, &frame.data) {
            Ok(sealed) => sealed.into_owned(),
            // Never put plain media on the wire.
            Err(DaveError::PlaintextBypass) => {
                return Err(End::Close(CloseReason::DaveUnavailable));
            }
            Err(_) => {
                self.refuse_video_frame(now, false);
                return Ok(());
            }
        };
        if self.video.as_ref().is_some_and(|video| !video.announced) {
            self.announce_video(conn, socket, true).await?;
        }
        let Some(sender) = self.video.as_mut().and_then(|video| video.sender.as_mut()) else {
            return Ok(());
        };
        match sender.enqueue_frame(now, frame.timestamp, sealed, keyframe) {
            Ok(()) => {
                Counters::add(&self.ch.counters.video_frames_sent, 1);
                if let Some(video) = self.video.as_mut() {
                    video.need_keyframe = false;
                }
            }
            Err(error) => {
                let congested = error == VideoError::QueueFull && sender.has_queued_packets();
                self.refuse_video_frame(now, congested);
                return Ok(());
            }
        }
        self.pump_video(now).await
    }

    /// A frame did not leave: the stream restarts at a keyframe, and a pacer
    /// that was full also cuts the bitrate by 20% (SPEC §8.3).
    fn refuse_video_frame(&mut self, now: Instant, congested: bool) {
        Counters::add(&self.ch.counters.video_frames_refused, 1);
        let Some(video) = self.video.as_mut() else {
            return;
        };
        video.need_keyframe = true;
        if congested
            && let Some(sender) = video.sender.as_mut()
            && let Some(bitrate) = sender.congestion.reduce(now)
        {
            sender.apply_bitrate(bitrate);
            emit(&self.ch.events, VoiceEvent::VideoBitrate(bitrate));
        }
        request_keyframe(&self.ch.events, video, now);
    }

    /// Opcode 12: the layer is sending (`true`) or stopped (`false`).
    async fn announce_video(
        &mut self,
        conn: &Conn,
        socket: &mut S::Socket,
        active: bool,
    ) -> Result<(), End> {
        let (Some(audio_ssrc), Some(layer)) = (
            self.udp.as_ref().map(|udp| udp.ssrc),
            self.video.as_ref().and_then(VideoState::layer_wire),
        ) else {
            return Ok(());
        };
        let payload = wire::video_state(audio_ssrc, Some((&layer, active)));
        Self::send_text(conn, socket, &payload).await?;
        if let Some(video) = self.video.as_mut() {
            video.announced = active;
        }
        Ok(())
    }

    /// Opcode 15 from the server about our layer: how much of it is wanted.
    pub(super) async fn on_sink_wants(
        &mut self,
        conn: &Conn,
        socket: &mut S::Socket,
        wants: &HashMap<String, Value>,
    ) -> Result<(), End> {
        let now = Instant::now();
        let Some(video) = self.video.as_mut() else {
            return Ok(());
        };
        let Some(sender) = video.sender.as_mut() else {
            return Ok(());
        };
        let quality = wants
            .get(&sender.ssrc.to_string())
            .or_else(|| wants.get("any"))
            .and_then(Value::as_u64);
        let Some(quality) = quality.map(|quality| quality.min(100)) else {
            return Ok(());
        };
        // A lower quality is a lower bitrate; 100 only lifts an earlier cap.
        let ceiling = u64::from(sender.options.bitrate);
        if quality > 0
            && let Some(bitrate) = sender.congestion.limit(ceiling * quality / 100, now)
        {
            sender.apply_bitrate(bitrate);
            emit(&self.ch.events, VoiceEvent::VideoBitrate(bitrate));
        }
        if quality == 0 && !video.paused {
            video.paused = true;
            let was_announced = std::mem::take(&mut video.announced);
            emit(&self.ch.events, VoiceEvent::VideoActive(false));
            if was_announced {
                return self.announce_video(conn, socket, false).await;
            }
        } else if quality > 0 && video.paused {
            video.paused = false;
            video.need_keyframe = true;
            emit(&self.ch.events, VoiceEvent::VideoActive(true));
            request_keyframe(&self.ch.events, video, now);
        }
        Ok(())
    }

    /// Feedback about our layer: retransmit on NACK, keyframe on PLI/FIR, and
    /// obey a receiver's bitrate estimate.
    pub(super) fn on_video_feedback(&mut self, feedback: &Feedback<'_>) {
        let now = Instant::now();
        let Some(video) = self.video.as_mut() else {
            return;
        };
        let Some(sender) = video.sender.as_mut() else {
            return;
        };
        // REMB names its SSRCs inside the message; its media SSRC is zero.
        if let Some(remb) = feedback.remb() {
            if remb.applies_to(sender.ssrc)
                && let Some(bitrate) = sender.congestion.limit(remb.bitrate, now)
            {
                sender.apply_bitrate(bitrate);
                emit(&self.ch.events, VoiceEvent::VideoBitrate(bitrate));
            }
            return;
        }
        if feedback.media_ssrc != sender.ssrc {
            return;
        }
        if feedback.kind == FeedbackKind::Transport {
            sender.repair(now, feedback.nacked_sequences().take(MAX_REPAIRS_PER_NACK));
        } else if (feedback.is_picture_loss() || feedback.is_full_intra_request())
            && sender.keyframe_wanted(now)
        {
            emit(&self.ch.events, VoiceEvent::KeyframeRequested);
        }
    }

    /// A reception report block about our video layer: loss and round trip
    /// steer its bitrate.
    pub(super) fn on_video_report(&mut self, block: &ReportBlock) {
        let now = Instant::now();
        let Some(video) = self.video.as_mut() else {
            return;
        };
        let Some(sender) = video.sender.as_mut() else {
            return;
        };
        if block.ssrc != sender.ssrc {
            return;
        }
        let rtt = rtcp::round_trip(rtcp::ntp_middle(ntp_now()), block);
        if let Some(rtt) = rtt {
            video.rtt = rtt;
        }
        if let Some(bitrate) = sender.congestion.report(block.fraction_lost, rtt, now) {
            sender.apply_bitrate(bitrate);
            emit(&self.ch.events, VoiceEvent::VideoBitrate(bitrate));
        }
    }

    /// Sends the packets the pacer lets out at `now`.
    async fn pump_video(&mut self, now: Instant) -> Result<(), End> {
        for _ in 0..MAX_PUMP {
            let Some(packet) = self
                .video
                .as_mut()
                .and_then(|video| video.sender.as_mut())
                .and_then(|sender| sender.next_packet(now))
            else {
                break;
            };
            self.send_video_packet(packet).await?;
        }
        Ok(())
    }

    /// Seals one RTP packet of the layer, or one retransmission of it, with a
    /// fresh transport nonce and sends it. Extension elements travel inside
    /// the encrypted body, announced by the clear preamble.
    async fn send_video_packet(&mut self, packet: Packet) -> Result<(), End> {
        let (Some(video), Some(media), Some(udp)) =
            (self.video.as_ref(), self.media.as_mut(), self.udp.as_ref())
        else {
            return Ok(());
        };
        let (Some(sender), Some(layer)) = (video.sender.as_ref(), video.layer.as_ref()) else {
            return Ok(());
        };
        let retransmission = packet.original.is_some();
        let (ssrc, payload_type) = if retransmission {
            (sender.rtx_ssrc, sender.offer.rtx_payload_type)
        } else {
            (sender.ssrc, sender.offer.payload_type)
        };
        let send_time = abs_send_time(ntp_now());
        let rid = layer.rid.as_bytes();
        self.body.clear();
        let preamble = if retransmission {
            rtp::write_extension_block(
                &[
                    (ext::ABS_SEND_TIME, &send_time[..]),
                    (ext::REPAIRED_RTP_STREAM_ID, rid),
                ],
                &mut self.body,
            )
        } else {
            // A camera carries no screenshare content type.
            let mut elements: Vec<(u8, &[u8])> = vec![
                (ext::ABS_SEND_TIME, &send_time[..]),
                (ext::PLAYOUT_DELAY, &PLAYOUT_DELAY[..]),
            ];
            if !video.options.camera {
                elements.push((ext::VIDEO_CONTENT_TYPE, &[CONTENT_SCREEN][..]));
            }
            elements.push((ext::RTP_STREAM_ID, rid));
            rtp::write_extension_block(&elements, &mut self.body)
        };
        // Fixed IDs and short elements: this cannot fail.
        let Ok(preamble) = preamble else {
            return Ok(());
        };
        packet.write_payload(&mut self.body);
        let header = RtpHeader {
            padding: false,
            marker: packet.marker(),
            payload_type,
            sequence: packet.sequence,
            timestamp: packet.timestamp,
            ssrc,
            csrc_count: 0,
            extension: Some(preamble),
        };
        self.clear.clear();
        header.write(&mut self.clear);
        media
            .cipher
            .seal(&self.clear, &self.body, &mut self.sealed)
            .map_err(|_| End::Close(CloseReason::KeyExhausted))?;
        // UDP is best effort; a lost packet is the receiver's NACK to send.
        if udp.link.send_to(&self.sealed, udp.server).await.is_ok() {
            let counters = &self.ch.counters;
            Counters::add(&counters.rtp_sent, 1);
            if retransmission {
                Counters::add(&counters.retransmissions_sent, 1);
            } else {
                Counters::add(&counters.video_packets_sent, 1);
            }
        }
        Ok(())
    }

    /// Sender reports for the layer and, when it repaired anything, its RTX
    /// stream, every report interval while they sent (docs.discord.food).
    pub(super) async fn send_video_sender_reports(&mut self, now: Instant) -> Result<(), End> {
        let (Some(video), Some(media), Some(udp)) =
            (self.video.as_mut(), self.media.as_mut(), self.udp.as_ref())
        else {
            return Ok(());
        };
        let Some(sender) = video.sender.as_mut() else {
            return Ok(());
        };
        let Some(rtp_timestamp) = sender.rtp_timestamp_at(now) else {
            return Ok(());
        };
        let reports = [
            (
                sender.ssrc,
                sender.packets,
                sender.octets,
                sender.reported.0 != sender.packets,
            ),
            (
                sender.rtx_ssrc,
                sender.rtx_packets,
                sender.rtx_octets,
                sender.reported.1 != sender.rtx_packets,
            ),
        ];
        sender.reported = (sender.packets, sender.rtx_packets);
        let ntp_timestamp = ntp_now();
        for (ssrc, packet_count, octet_count, due) in reports {
            if !due {
                continue;
            }
            let info = SenderInfo {
                ntp_timestamp,
                rtp_timestamp,
                packet_count,
                octet_count,
            };
            rtcp::write_sender_report(ssrc, info, &mut self.clear);
            let (clear, body) = self.clear.split_at(RTCP_CLEAR_LEN);
            media
                .cipher
                .seal(clear, body, &mut self.sealed)
                .map_err(|_| End::Close(CloseReason::KeyExhausted))?;
            let _ = udp.link.send_to(&self.sealed, udp.server).await;
        }
        Ok(())
    }

    // ---- timers -----------------------------------------------------------

    /// Everything video does on a wake-up: expire held packets and overdue
    /// frames, deliver what completed, send feedback, and pace out packets.
    pub(super) async fn on_video_timers(&mut self, now: Instant) -> Result<(), End> {
        if self.media.is_none() || self.udp.is_none() {
            return Ok(());
        }
        let Some(video) = self.video.as_mut() else {
            return Ok(());
        };
        let expired = video.pending.expire(now);
        Counters::add(&self.ch.counters.dropped_unknown_ssrc, expired as u64);
        let ssrcs: Vec<u32> = video.receivers.keys().copied().collect();
        for ssrc in ssrcs {
            self.drain_receiver(now, ssrc);
            if let Some(receiver) = self
                .video
                .as_mut()
                .and_then(|video| video.receivers.get_mut(&ssrc))
            {
                if receiver.buffer.expire(now) {
                    Counters::add(&self.ch.counters.video_frames_dropped, 1);
                }
                if receiver.buffer.take_broken() {
                    receiver.want_keyframe(now);
                }
            }
            self.drain_receiver(now, ssrc);
        }
        self.flush_feedback(now).await?;
        self.pump_video(now).await
    }
}
