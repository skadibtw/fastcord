//! One voice connection: the v8 voice Gateway, the UDP media socket, and the
//! transport encryption between them (SPEC §6.1–6.2).
//!
//! [`connect`] spawns a task that owns the WebSocket, the UDP socket, and the
//! transport key for one [`Generation`]. It runs Identify, IP discovery,
//! Select Protocol, and Session Description, then carries encrypted RTP/RTCP.
//! Outgoing media is accepted into a small bounded queue at any time but only
//! leaves, encrypted and after a Speaking announcement, once the transport
//! key exists; nothing unencrypted is ever sent. Received audio is decrypted,
//! parsed, and attributed to a user through SSRC mappings.
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fastcord_model::Snowflake;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::crypto::{TransportMode, aes_accelerated};
use crate::session::{Generation, VoiceCredentials};
pub use wire::speaking;

/// Encoded frames waiting for the transport key: 100 ms of 20 ms Opus. More is
/// refused rather than released late.
pub const MEDIA_QUEUE_FRAMES: usize = 5;
/// The largest Opus packet (RFC 6716 §3.2.1).
pub const MAX_OPUS_FRAME: usize = 1275;
/// Received audio packets waiting for the consumer, each at most
/// [`MAX_OPUS_FRAME`] bytes of payload; newer packets are dropped when full.
pub const AUDIO_QUEUE_PACKETS: usize = 64;
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
            Self::E2eeRequired => {
                f.write_str("this call requires end-to-end encryption (DAVE), not yet supported")
            }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaSendError {
    /// The queue is full (the key is not ready yet, or the sender outpaces
    /// the network): the frame was dropped.
    Full,
    /// Empty or larger than one Opus packet.
    InvalidFrame,
    /// The connection is closed.
    Closed,
}

impl fmt::Display for MediaSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Full => "voice media queue is full",
            Self::InvalidFrame => "not a valid Opus frame",
            Self::Closed => "voice connection is closed",
        })
    }
}

impl std::error::Error for MediaSendError {}

pub(crate) enum MediaCommand {
    Opus(Vec<u8>),
    EndSpeech,
}

/// The sending half of a voice connection's audio. Never blocks.
#[derive(Clone)]
pub struct MediaSender {
    queue: mpsc::Sender<MediaCommand>,
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
        self.push(MediaCommand::Opus(frame))
    }

    /// Ends a speech burst: five Opus silence frames, then speaking off.
    pub fn end_speech(&self) -> Result<(), MediaSendError> {
        self.push(MediaCommand::EndSpeech)
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

/// Starts a voice connection on the current Tokio runtime.
pub fn connect(credentials: VoiceCredentials) -> VoiceSession {
    let options = driver::Options::live(aes_accelerated());
    start(
        transport::LiveSignaling,
        transport::LiveNetwork,
        credentials,
        options,
    )
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
    let (media_tx, media_rx) = mpsc::channel(MEDIA_QUEUE_FRAMES);
    let (leave_tx, leave_rx) = oneshot::channel();
    let counters = Arc::new(Counters::default());
    let channels = driver::Channels {
        status: status_tx,
        events: events_tx,
        audio: audio_tx,
        media: media_rx,
        leave: leave_rx,
        counters: Arc::clone(&counters),
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
        media: MediaSender { queue: media_tx },
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
