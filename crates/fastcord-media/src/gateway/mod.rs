//! One voice connection: the v8 voice Gateway, the UDP media socket, and the
//! transport encryption between them (SPEC §6.1–6.2).
//!
//! [`connect`] spawns a task that owns the WebSocket, the UDP socket, and the
//! transport key for one [`Generation`]. It runs Identify, IP discovery,
//! Select Protocol, and Session Description, then carries encrypted RTP/RTCP.
//! Outgoing media is accepted into a small bounded queue at any time but only
//! leaves, encrypted and after a Speaking announcement, once the transport
//! key exists; nothing unencrypted is ever sent. Received audio is decrypted,
//! parsed, and attributed to a user through SSRC mappings. A connection created
//! with [`connect_with_video`] also negotiates H.264 send/receive and AV1 receive
//! (see [`crate::video`]): frames cross DAVE whole, before packetization when
//! sending and after reassembly when receiving.
//!
//! Leaving is [`VoiceSession::leave`] (or dropping the session): the socket
//! closes, and the sockets, keys, and task go away. The main Gateway's
//! channel-null opcode 4 is the caller's job (`fastcord-discord`), as is
//! correlating the join ([`JoinCorrelator`](crate::session::JoinCorrelator)).

mod driver;
mod transport;
pub(crate) mod wire;

#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use fastcord_model::Snowflake;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::crypto::{TransportMode, aes_accelerated};
use crate::session::{Generation, VoiceCredentials};
use crate::video::{
    MAX_FRAME_BYTES, MAX_INTEREST_USERS, ReceivedVideo, VideoCodec, VideoControl, VideoError,
    VideoFrame, VideoInterest, VideoOptions,
};
pub use transport::set_handshake_identity;
pub use wire::speaking;

/// Encoded frames waiting for the transport key: 100 ms of 20 ms Opus. More is
/// refused rather than released late.
pub const MEDIA_QUEUE_FRAMES: usize = 5;
/// The largest Opus packet (RFC 6716 §3.2.1).
pub const MAX_OPUS_FRAME: usize = 1275;
/// Received audio packets waiting for the consumer, each at most
/// [`MAX_OPUS_FRAME`] bytes of payload; newer packets are dropped when full.
pub const AUDIO_QUEUE_PACKETS: usize = 64;
/// Whole video frames waiting in either direction. A frame is at most
/// [`MAX_FRAME_BYTES`], so each queue holds at most 6 MiB; what does not fit is
/// dropped, and the stream restarts at its next keyframe.
pub const VIDEO_QUEUE_FRAMES: usize = 3;
/// Received video frames waiting for the consumer on a call connection, which
/// carries up to [`MAX_INTEREST_USERS`] cameras at once (at most 16 MiB).
pub const CALL_VIDEO_QUEUE_FRAMES: usize = 8;
const EVENT_QUEUE: usize = 64;
/// How long [`VoiceSession::leave`] waits for the close handshake.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the connection is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceStatus {
    /// Opening the voice WebSocket.
    Connecting,
    /// Identify sent, waiting for READY.
    Identifying,
    /// Asking the voice server for our external UDP address.
    Discovering,
    /// Select Protocol sent, waiting for the transport key.
    Negotiating,
    /// Transport is established, but DAVE has not executed a key-ready
    /// transition yet. Local media remains paused.
    Rekeying {
        transition_id: Option<u16>,
    },
    /// Media flows.
    Connected {
        mode: TransportMode,
        ssrc: u32,
    },
    /// The WebSocket dropped; resuming keeps UDP and keys.
    Resuming {
        attempt: u32,
    },
    Closed(CloseReason),
}

/// Why a voice connection ended. Only [`CloseReason::Left`] was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Left,
    /// Kicked, moved away, channel deleted, or call ended (4014/4022).
    Disconnected,
    /// The voice session is gone (4006/4009); join again for new credentials.
    SessionInvalid,
    /// The voice token was rejected (4004).
    AuthenticationFailed,
    /// The channel requires DAVE end-to-end encryption (4017).
    E2eeRequired,
    /// DAVE negotiation or authenticated frame processing failed closed.
    DaveUnavailable,
    /// 4021.
    RateLimited,
    /// 4011.
    ServerNotFound,
    /// The voice server did not answer UDP IP discovery: UDP is blocked on
    /// this network (no relay fallback exists).
    UdpUnreachable,
    /// READY offered no transport mode fastcord implements.
    NoSupportedMode,
    /// Every transport nonce of this key was used; a new session is needed.
    KeyExhausted,
    /// The endpoint in VOICE_SERVER_UPDATE is not a plain `host[:port]`.
    InvalidEndpoint,
    /// Reconnecting failed repeatedly.
    Network,
    /// The server broke the protocol, or closed with this protocol-error code.
    Protocol(Option<u16>),
}

impl fmt::Display for CloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Left => f.write_str("left voice"),
            Self::Disconnected => f.write_str("disconnected from voice"),
            Self::SessionInvalid => f.write_str("voice session expired"),
            Self::AuthenticationFailed => f.write_str("voice server rejected the session"),
            Self::E2eeRequired => f.write_str("this call requires end-to-end encryption (DAVE)"),
            Self::DaveUnavailable => f.write_str("voice could not establish DAVE encryption"),
            Self::RateLimited => f.write_str("voice server rate limit reached"),
            Self::ServerNotFound => f.write_str("voice server not found"),
            Self::UdpUnreachable => f.write_str("UDP to the voice server is blocked"),
            Self::NoSupportedMode => f.write_str("no supported voice encryption mode"),
            Self::KeyExhausted => f.write_str("voice encryption key exhausted"),
            Self::InvalidEndpoint => f.write_str("invalid voice server endpoint"),
            Self::Network => f.write_str("voice connection lost"),
            Self::Protocol(Some(code)) => write!(f, "voice protocol error ({code})"),
            Self::Protocol(None) => f.write_str("voice protocol error"),
        }
    }
}

/// Voice Gateway events about other participants and link quality.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceEvent {
    ClientsConnected(Vec<Snowflake>),
    ClientDisconnected(Snowflake),
    /// A participant's speaking flags ([`speaking`]) and audio SSRC.
    Speaking {
        user_id: Snowflake,
        ssrc: u32,
        flags: u32,
    },
    /// Round trip of a UDP ping.
    UdpLatency(Duration),
    /// The server's reception report about our audio.
    ReceptionReport {
        /// Packets lost since the previous report, in 1/256 units.
        fraction_lost: u8,
        cumulative_lost: i32,
        /// In 48 kHz RTP timestamp units.
        jitter: u32,
    },
    /// Video negotiation finished for a connection that offered to send: the
    /// codec the server chose for it, or `None` when it chose one fastcord
    /// cannot send or assigned no layer (nothing will be sent).
    VideoNegotiated {
        codec: Option<VideoCodec>,
    },
    /// A participant's video as the server announced it: the primary SSRC of
    /// its best active stream, `0` once it stopped.
    VideoSource {
        user_id: Snowflake,
        ssrc: u32,
        active: bool,
    },
    /// The encoder must make the next frame a keyframe: a receiver or the
    /// server asked for one, or a frame did not leave.
    KeyframeRequested,
    /// The bitrate the encoder should now aim for, in bits per second,
    /// after congestion feedback or a server or receiver limit.
    VideoBitrate(u32),
    /// The server wants our video off (`false`) or on again (`true`). While
    /// off, frames are discarded and the encoder may pause.
    VideoActive(bool),
}

/// One decrypted Opus packet from a known participant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedAudio {
    pub user_id: Snowflake,
    pub ssrc: u32,
    /// The RTP sequence extended across 16-bit wraps.
    pub sequence: u64,
    pub timestamp: u32,
    pub payload: Vec<u8>,
}
/// One item from a voice session's participant-event and audio streams.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoiceMessage {
    Event(VoiceEvent),
    Audio(ReceivedAudio),
    /// One complete, DAVE-opened video frame.
    Video(ReceivedVideo),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaSendError {
    /// The queue is full (the key is not ready yet, or the sender outpaces
    /// the network): the frame was dropped.
    Full,
    /// Empty or larger than one Opus packet (audio), or empty or over the
    /// 2 MiB frame bound (video).
    InvalidFrame,
    /// The connection is closed.
    Closed,
}

impl fmt::Display for MediaSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Full => "voice media queue is full",
            Self::InvalidFrame => "not a valid audio or video frame",
            Self::Closed => "voice connection is closed",
        })
    }
}

impl std::error::Error for MediaSendError {}

pub(crate) enum MediaCommand {
    Opus(Vec<u8>),
    /// A stream-audio frame: like `Opus`, announced with the SOUNDSHARE speaking flag.
    Soundshare(Vec<u8>),
    EndSpeech,
    VideoKeyframe(u32),
}

/// Stores `value` and says whether it differed.
fn replace<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        return false;
    }
    *slot = value;
    true
}

/// The sending/control half of a voice connection. Never blocks.
#[derive(Clone)]
pub struct MediaSender {
    queue: mpsc::Sender<MediaCommand>,
    queued_audio: Arc<AtomicUsize>,
    video: mpsc::Sender<VideoFrame>,
    wanted: Arc<watch::Sender<VideoControl>>,
}

impl fmt::Debug for MediaSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MediaSender")
    }
}

impl MediaSender {
    /// Queues one 20 ms Opus frame. The connection announces speaking before
    /// the first frame after silence.
    pub fn send_opus(&self, frame: Vec<u8>) -> Result<(), MediaSendError> {
        if frame.is_empty() || frame.len() > MAX_OPUS_FRAME {
            return Err(MediaSendError::InvalidFrame);
        }
        self.push_audio(MediaCommand::Opus(frame))
    }

    /// Queues one 20 ms Opus frame of shared application or system audio. A
    /// stream connection announces speaking with the SOUNDSHARE flag (and no
    /// voice flag) before the first frame after silence; [`end_speech`]
    /// ends the burst.
    ///
    /// [`end_speech`]: Self::end_speech
    pub fn send_soundshare(&self, frame: Vec<u8>) -> Result<(), MediaSendError> {
        if frame.is_empty() || frame.len() > MAX_OPUS_FRAME {
            return Err(MediaSendError::InvalidFrame);
        }
        self.push_audio(MediaCommand::Soundshare(frame))
    }

    /// Ends a speech burst: five Opus silence frames, then speaking off.
    pub fn end_speech(&self) -> Result<(), MediaSendError> {
        self.push(MediaCommand::EndSpeech)
    }

    /// Queues one encoded H.264 frame (Annex B, one access unit, not yet
    /// sealed by DAVE). The connection seals, packetizes, and paces it; frames
    /// wait only while DAVE keys are not ready, at most [`VIDEO_QUEUE_FRAMES`],
    /// and `Full` means the frame was dropped. After any dropped frame the
    /// stream must resume at a keyframe, which the connection requests with
    /// [`VoiceEvent::KeyframeRequested`].
    pub fn send_video(&self, frame: VideoFrame) -> Result<(), MediaSendError> {
        if frame.data.is_empty() || frame.data.len() > MAX_FRAME_BYTES {
            return Err(MediaSendError::InvalidFrame);
        }
        self.video.try_send(frame).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => MediaSendError::Full,
            mpsc::error::TrySendError::Closed(_) => MediaSendError::Closed,
        })
    }

    /// Whether this viewer wants the remote video at all. A watch connection
    /// starts wanting it; turn it off while the stream is hidden or minimized
    /// (the server then stops forwarding it) and on again to resume.
    pub fn set_video_wanted(&self, wanted: bool) {
        let interest = if wanted {
            VideoInterest::All
        } else {
            VideoInterest::Off
        };
        self.wanted
            .send_if_modified(|control| replace(&mut control.interest, interest));
    }

    /// Asks for the best stream of only these users (a call connection: the
    /// camera tiles that are on screen). An empty list wants nothing; more
    /// than [`MAX_INTEREST_USERS`] is cut to the first ones.
    pub fn set_video_users(&self, users: &[Snowflake]) {
        let interest = if users.is_empty() {
            VideoInterest::Off
        } else {
            VideoInterest::Users(users.iter().copied().take(MAX_INTEREST_USERS).collect())
        };
        self.wanted
            .send_if_modified(|control| replace(&mut control.interest, interest));
    }

    /// Marks the users whose tile is small (at most [`MAX_INTEREST_USERS`]):
    /// for each, the lowest active layer the server announced is wanted
    /// instead of the best one. A user with a single layer is unaffected.
    pub fn set_video_thumbnails(&self, users: &[Snowflake]) {
        let users: Vec<Snowflake> = users.iter().copied().take(MAX_INTEREST_USERS).collect();
        self.wanted
            .send_if_modified(|control| replace(&mut control.thumbnails, users));
    }

    /// Lets this connection's own layer send (`true`) or announces it stopped
    /// (`false`): a camera turned on or off. Frames offered while off are
    /// refused.
    pub fn set_video_sending(&self, sending: bool) {
        self.wanted
            .send_if_modified(|control| replace(&mut control.sending, sending));
    }

    /// A decoder or bounded decode queue lost a frame of `ssrc`: suppress
    /// dependent frames and ask its sender for a keyframe (PLI).
    pub fn request_video_keyframe(&self, ssrc: u32) -> Result<(), MediaSendError> {
        if ssrc == 0 {
            return Err(MediaSendError::InvalidFrame);
        }
        self.push(MediaCommand::VideoKeyframe(ssrc))
    }

    fn push_audio(&self, command: MediaCommand) -> Result<(), MediaSendError> {
        if self.queue.is_closed() {
            return Err(MediaSendError::Closed);
        }
        let mut queued = self.queued_audio.load(Ordering::Acquire);
        loop {
            if queued >= MEDIA_QUEUE_FRAMES {
                return Err(MediaSendError::Full);
            }
            match self.queued_audio.compare_exchange_weak(
                queued,
                queued + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => queued = current,
            }
        }
        self.queue.try_send(command).map_err(|error| {
            self.queued_audio.fetch_sub(1, Ordering::AcqRel);
            match error {
                mpsc::error::TrySendError::Full(_) => MediaSendError::Full,
                mpsc::error::TrySendError::Closed(_) => MediaSendError::Closed,
            }
        })
    }
    fn push(&self, command: MediaCommand) -> Result<(), MediaSendError> {
        self.queue.try_send(command).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => MediaSendError::Full,
            mpsc::error::TrySendError::Closed(_) => MediaSendError::Closed,
        })
    }
}

/// Packet counters of one connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransportStats {
    pub rtp_sent: u64,
    pub rtp_received: u64,
    /// Failed transport authentication.
    pub rejected_authentication: u64,
    /// Malformed or unexpected datagrams and RTP/RTCP headers.
    pub rejected_malformed: u64,
    /// Datagrams not from the voice server's address.
    pub rejected_source: u64,
    /// Media from SSRCs never announced within the hold window.
    pub dropped_unknown_ssrc: u64,
    /// Received audio the consumer did not take in time.
    pub dropped_queue_full: u64,
    /// Video RTP packets (media and retransmissions) accepted from the server.
    pub video_packets_received: u64,
    /// Complete frames opened by DAVE and handed to the consumer.
    pub video_frames_received: u64,
    /// Frames given up on: lost past the deadline, refused by DAVE, waiting for
    /// a keyframe, or not taken by the consumer in time.
    pub video_frames_dropped: u64,
    /// Video RTP packets sent, retransmissions not included.
    pub video_packets_sent: u64,
    /// Frames accepted for sending.
    pub video_frames_sent: u64,
    /// Frames that did not leave: paused, not a keyframe after a gap, pacer
    /// full, or refused by DAVE.
    pub video_frames_refused: u64,
    /// RTX packets sent in answer to NACKs.
    pub retransmissions_sent: u64,
    /// Sequence numbers requested with NACKs.
    pub nack_sequences_sent: u64,
    /// Picture Loss Indications sent.
    pub keyframe_requests_sent: u64,
    /// Opus frames not sent because DAVE had no send key yet.
    pub audio_waiting_for_keys: u64,
}

#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) rtp_sent: AtomicU64,
    pub(crate) rtp_received: AtomicU64,
    pub(crate) rejected_authentication: AtomicU64,
    pub(crate) rejected_malformed: AtomicU64,
    pub(crate) rejected_source: AtomicU64,
    pub(crate) dropped_unknown_ssrc: AtomicU64,
    pub(crate) dropped_queue_full: AtomicU64,
    pub(crate) video_packets_received: AtomicU64,
    pub(crate) video_frames_received: AtomicU64,
    pub(crate) video_frames_dropped: AtomicU64,
    pub(crate) video_packets_sent: AtomicU64,
    pub(crate) video_frames_sent: AtomicU64,
    pub(crate) video_frames_refused: AtomicU64,
    pub(crate) retransmissions_sent: AtomicU64,
    pub(crate) nack_sequences_sent: AtomicU64,
    pub(crate) keyframe_requests_sent: AtomicU64,
    pub(crate) audio_waiting_for_keys: AtomicU64,
}

impl Counters {
    pub(crate) fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    fn snapshot(&self) -> TransportStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        TransportStats {
            rtp_sent: get(&self.rtp_sent),
            rtp_received: get(&self.rtp_received),
            rejected_authentication: get(&self.rejected_authentication),
            rejected_malformed: get(&self.rejected_malformed),
            rejected_source: get(&self.rejected_source),
            dropped_unknown_ssrc: get(&self.dropped_unknown_ssrc),
            dropped_queue_full: get(&self.dropped_queue_full),
            video_packets_received: get(&self.video_packets_received),
            video_frames_received: get(&self.video_frames_received),
            video_frames_dropped: get(&self.video_frames_dropped),
            video_packets_sent: get(&self.video_packets_sent),
            video_frames_sent: get(&self.video_frames_sent),
            video_frames_refused: get(&self.video_frames_refused),
            retransmissions_sent: get(&self.retransmissions_sent),
            nack_sequences_sent: get(&self.nack_sequences_sent),
            keyframe_requests_sent: get(&self.keyframe_requests_sent),
            audio_waiting_for_keys: get(&self.audio_waiting_for_keys),
        }
    }
}

/// A running voice connection. Dropping it cancels the connection at once;
/// [`leave`](Self::leave) closes it politely first.
pub struct VoiceSession {
    generation: Generation,
    status: watch::Receiver<VoiceStatus>,
    events: mpsc::Receiver<VoiceEvent>,
    audio: mpsc::Receiver<ReceivedAudio>,
    video: mpsc::Receiver<ReceivedVideo>,
    media: MediaSender,
    counters: Arc<Counters>,
    leave: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl fmt::Debug for VoiceSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceSession")
            .field("generation", &self.generation)
            .field("status", &*self.status.borrow())
            .finish_non_exhaustive()
    }
}

/// Starts an audio-only voice connection on the current Tokio runtime.
pub fn connect(credentials: VoiceCredentials) -> VoiceSession {
    let options = driver::Options::live(aes_accelerated());
    start(
        transport::LiveSignaling,
        transport::LiveNetwork,
        credentials,
        options,
    )
}

/// Starts a connection that also offers video (SPEC §8.3): a stream viewer
/// ([`VideoOptions::watch`]) or owner ([`VideoOptions::share`]). Fails before
/// anything is sent if the offer is inconsistent.
pub fn connect_with_video(
    credentials: VoiceCredentials,
    video: VideoOptions,
) -> Result<VoiceSession, VideoError> {
    video.validate()?;
    let options = driver::Options::live(aes_accelerated()).with_video(video);
    Ok(start(
        transport::LiveSignaling,
        transport::LiveNetwork,
        credentials,
        options,
    ))
}

fn start<S: transport::Signaling, N: transport::Network>(
    signaling: S,
    network: N,
    credentials: VoiceCredentials,
    options: driver::Options,
) -> VoiceSession {
    let generation = credentials.generation;
    let (status_tx, status) = watch::channel(VoiceStatus::Connecting);
    let (events_tx, events) = mpsc::channel(EVENT_QUEUE);
    let (audio_tx, audio) = mpsc::channel(AUDIO_QUEUE_PACKETS);
    let call_video = options.video.as_ref().is_some_and(|video| video.camera);
    let (video_out_tx, video) = mpsc::channel(if call_video {
        CALL_VIDEO_QUEUE_FRAMES
    } else {
        VIDEO_QUEUE_FRAMES
    });
    let (video_in_tx, video_in_rx) = mpsc::channel(VIDEO_QUEUE_FRAMES);
    // A viewer connection exists because the user chose to watch: it wants the
    // stream from the start. An owner sends from the start. A call connection
    // wants and sends nothing until the application says so.
    let (wanted_tx, wanted_rx) = watch::channel(options.video.as_ref().map_or_else(
        VideoControl::default,
        |video| VideoControl {
            interest: if video.send.is_none() && !video.camera {
                VideoInterest::All
            } else {
                VideoInterest::Off
            },
            sending: video.send.is_some() && !video.camera,
            thumbnails: Vec::new(),
        },
    ));
    let queued_audio = Arc::new(AtomicUsize::new(0));
    let (media_tx, media_rx) = mpsc::channel(MEDIA_QUEUE_FRAMES + 1);
    let (leave_tx, leave_rx) = oneshot::channel();
    let counters = Arc::new(Counters::default());
    let channels = driver::Channels {
        status: status_tx,
        events: events_tx,
        audio: audio_tx,
        media: media_rx,
        queued_audio: Arc::clone(&queued_audio),
        leave: leave_rx,
        counters: Arc::clone(&counters),
        video_out: video_out_tx,
        video_in: video_in_rx,
        wanted: wanted_rx,
    };
    let task = tokio::spawn(driver::run(
        signaling,
        network,
        credentials,
        options,
        channels,
    ));
    VoiceSession {
        generation,
        status,
        events,
        audio,
        video,
        media: MediaSender {
            queue: media_tx,
            queued_audio,
            video: video_in_tx,
            wanted: Arc::new(wanted_tx),
        },
        counters,
        leave: Some(leave_tx),
        task,
    }
}

impl VoiceSession {
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    pub fn status(&self) -> VoiceStatus {
        *self.status.borrow()
    }

    /// A receiver of status changes, for a UI subscription.
    pub fn watch_status(&self) -> watch::Receiver<VoiceStatus> {
        self.status.clone()
    }

    /// The next participant/link event; `None` once the connection ended.
    pub async fn next_event(&mut self) -> Option<VoiceEvent> {
        self.events.recv().await
    }

    /// The next received audio packet; `None` once the connection ended.
    pub async fn next_audio(&mut self) -> Option<ReceivedAudio> {
        self.audio.recv().await
    }

    /// The next received video frame; `None` once the connection ended.
    pub async fn next_video(&mut self) -> Option<ReceivedVideo> {
        self.video.recv().await
    }

    /// Waits for the next event, audio packet, or video frame without requiring
    /// simultaneous mutable borrows.
    pub async fn next_message(&mut self) -> Option<VoiceMessage> {
        loop {
            let events_done = self.events.is_closed() && self.events.is_empty();
            let audio_done = self.audio.is_closed() && self.audio.is_empty();
            let video_done = self.video.is_closed() && self.video.is_empty();
            if events_done && audio_done && video_done {
                return None;
            }
            tokio::select! {
                event = self.events.recv(), if !events_done => {
                    if let Some(event) = event {
                        return Some(VoiceMessage::Event(event));
                    }
                }
                audio = self.audio.recv(), if !audio_done => {
                    if let Some(audio) = audio {
                        return Some(VoiceMessage::Audio(audio));
                    }
                }
                video = self.video.recv(), if !video_done => {
                    if let Some(video) = video {
                        return Some(VoiceMessage::Video(video));
                    }
                }
            }
        }
    }

    pub fn media(&self) -> MediaSender {
        self.media.clone()
    }

    pub fn stats(&self) -> TransportStats {
        self.counters.snapshot()
    }

    /// Closes the voice WebSocket normally and waits (briefly) for the task
    /// to drop its sockets and keys.
    pub async fn leave(mut self) {
        if let Some(leave) = self.leave.take() {
            let _ = leave.send(());
        }
        if tokio::time::timeout(LEAVE_TIMEOUT, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}

impl Drop for VoiceSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}
