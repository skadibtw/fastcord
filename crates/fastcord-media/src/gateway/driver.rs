//! The task behind a [`VoiceSession`](super::VoiceSession).
//!
//! - One WebSocket at a time. Before media is established a dropped socket
//!   means a fresh Identify (with fresh UDP); afterwards it means Resume, which
//!   keeps the UDP socket, SSRC, transport key, and nonce counter.
//! - The transport key exists only after Session Description and only in this
//!   task. Every RTP/RTCP datagram is sealed with it; there is no plaintext
//!   send path, and the outgoing media queue is not even read before then.
//! - Close codes that say "do not reconnect" end the task with a typed reason.
//! - Heartbeats follow v8: a nonce `t` plus `seq_ack`, the last sequence seen
//!   on any numbered JSON message or binary message.

mod discovery;
mod video;

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use rand_core::{OsRng, RngCore};
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use super::transport::{Network, Signaling, UdpLink, voice_url};
use super::wire::{self, op};
use super::{CloseReason, Counters, MediaCommand, ReceivedAudio, VoiceEvent, VoiceStatus};
use crate::crypto::{CryptoError, RTCP_CLEAR_LEN, TransportCipher, TransportKey, TransportMode};
use crate::dave::DaveSession;
use crate::rtcp::{self, Compound, ReportBlocks, RtcpPacket, SenderInfo};
use crate::rtp::{OPUS_FRAME_TICKS, OPUS_PAYLOAD_TYPE, RtpHeader, RtpSender, SequenceTracker};
use crate::session::VoiceCredentials;
use crate::ssrc::{PendingSsrc, SsrcMap, StreamKind, VideoStream};
use crate::udp::{self, Datagram, MAX_DATAGRAM};
use crate::video::{ReceivedVideo, VideoControl, VideoFrame, VideoOptions};
use video::VideoState;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// One Opus frame of the media clock, in milliseconds.
const OPUS_FRAME_MS: u128 = 20;
/// A send gap of at least this many frames (200 ms) is silence, not jitter.
const SILENCE_GAP_FRAMES: u128 = 10;
/// READY after Identify, RESUMED after Resume, Session Description after
/// Select Protocol.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const DISCOVERY_RETRY: Duration = Duration::from_secs(1);
const DISCOVERY_ATTEMPTS: u32 = 5;
const PING_INTERVAL: Duration = Duration::from_secs(5);
const REPORT_INTERVAL: Duration = Duration::from_secs(5);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_HEARTBEAT: Duration = Duration::from_secs(1);
const MAX_HEARTBEAT: Duration = Duration::from_secs(60);
/// Consecutive failed (re)connections before giving up.
const MAX_RECONNECTS: u32 = 5;
const FRAME: Duration = Duration::from_millis(20);
/// Sent as a protocol tail only when it can be protected by DAVE.
const SILENCE_FRAMES: u8 = 5;
const OPUS_SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];
const MAX_DAVE_MESSAGE: usize = 1024 * 1024;
const MAX_DAVE_AUDIO_FRAME: usize = super::MAX_OPUS_FRAME + 64;
/// Seconds from 1900 (NTP) to 1970 (Unix).
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

pub(crate) struct Options {
    pub(crate) aes_accelerated: bool,
    /// Nonce of the first sealed packet; tests start near the end.
    pub(crate) first_nonce: u32,
    /// First RTP sequence and timestamp; random when `None` (RFC 3550).
    pub(crate) rtp_start: Option<(u16, u32)>,
    /// First heartbeat nonce.
    pub(crate) nonce_seed: u64,
    /// What this connection offers for video; `None` is audio-only.
    pub(crate) video: Option<VideoOptions>,
    /// First video RTP sequence, timestamp, and RTX sequence; random when
    /// `None`.
    pub(crate) video_rtp_start: Option<(u16, u32, u16)>,
}

impl Options {
    pub(crate) fn live(aes_accelerated: bool) -> Self {
        Self {
            aes_accelerated,
            first_nonce: 0,
            rtp_start: None,
            nonce_seed: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64),
            video: None,
            video_rtp_start: None,
        }
    }

    pub(crate) fn with_video(mut self, video: VideoOptions) -> Self {
        self.video = Some(video);
        self
    }
}

pub(crate) struct Channels {
    pub(crate) status: watch::Sender<VoiceStatus>,
    pub(crate) events: mpsc::Sender<VoiceEvent>,
    pub(crate) audio: mpsc::Sender<ReceivedAudio>,
    pub(crate) media: mpsc::Receiver<MediaCommand>,
    pub(crate) queued_audio: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) leave: oneshot::Receiver<()>,
    pub(crate) counters: Arc<Counters>,
    pub(crate) video_out: mpsc::Sender<ReceivedVideo>,
    pub(crate) video_in: mpsc::Receiver<VideoFrame>,
    /// Whether a viewer wants the remote video (see `MediaSender`).
    pub(crate) wanted: watch::Receiver<VideoControl>,
}

struct Udp<L> {
    link: Arc<L>,
    server: SocketAddr,
    ssrc: u32,
    mode: TransportMode,
}

struct Media {
    cipher: TransportCipher,
    rtp: RtpSender,
    speaking: bool,
    silence_left: u8,
    /// When the last Opus packet left; see `send_rtp`.
    last_audio_at: Option<Instant>,
    packets: u32,
    octets: u32,
    reported_packets: u32,
}

/// Audio from an SSRC nobody has announced yet.
struct PendingAudio {
    sequence: u16,
    timestamp: u32,
    payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    AwaitHello,
    Identifying,
    Discovering { attempts: u32 },
    Negotiating,
    Resuming,
    Connected,
}

/// How one WebSocket connection ended.
enum End {
    /// Reconnect; `progressed` when this connection got past the handshake.
    Retry {
        progressed: bool,
    },
    Close(CloseReason),
}

/// Per-WebSocket state.
struct Conn {
    phase: Phase,
    resume: bool,
    progressed: bool,
    deadline: Option<Instant>,
    heartbeat_interval: Duration,
    heartbeat_at: Option<Instant>,
    awaiting_ack: Option<u64>,
    ping_at: Option<Instant>,
    ping: Option<(u32, Instant)>,
    report_at: Option<Instant>,
    silence_at: Option<Instant>,
}

impl Conn {
    fn retry(&self) -> End {
        End::Retry {
            progressed: self.progressed,
        }
    }

    fn next_wake(&self, pending: Option<Instant>) -> Option<Instant> {
        [
            self.deadline,
            self.heartbeat_at,
            self.ping_at,
            self.report_at,
            self.silence_at,
            pending,
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

enum UdpResult<L> {
    Bound {
        link: Arc<L>,
        server: SocketAddr,
        ssrc: u32,
        mode: TransportMode,
        preludes: u8,
    },
    Sent,
    Failed {
        relay: bool,
    },
}
type UdpWork<L> = Pin<Box<dyn Future<Output = UdpResult<L>> + Send>>;
async fn udp_work<L>(work: &mut Option<UdpWork<L>>) -> UdpResult<L> {
    match work {
        Some(work) => work.await,
        None => std::future::pending().await,
    }
}

struct Driver<S, N: Network> {
    signaling: S,
    network: Arc<N>,
    credentials: VoiceCredentials,
    options: Options,
    ch: Channels,
    seq_ack: Option<u64>,
    udp: Option<Udp<N::Udp>>,
    udp_work: Option<UdpWork<N::Udp>>,
    // At most three sockets. Keep even a completed-but-unconsumed bind alive
    // across WS cancellation so an ephemeral port cannot immediately recur.
    held_udp: Arc<Mutex<Vec<Arc<N::Udp>>>>,
    discovery: discovery::Campaign,
    media: Option<Media>,
    dave: Option<DaveSession>,
    expected_users: Vec<u64>,
    ssrcs: SsrcMap,
    sequences: HashMap<u32, SequenceTracker>,
    pending: PendingSsrc<PendingAudio>,
    heartbeat_nonce: u64,
    ping_sequence: u32,
    /// Reused buffers: decrypted bodies, clear headers, sealed datagrams.
    plain: Vec<u8>,
    clear: Vec<u8>,
    sealed: Vec<u8>,
    /// Reused buffers for video: an RTP body being sealed, an RTCP compound.
    body: Vec<u8>,
    rtcp: Vec<u8>,
    video: Option<VideoState>,
    /// The wanted-video channel's sender is gone (never while the session lives).
    wants_closed: bool,
}

pub(crate) async fn run<S: Signaling, N: Network>(
    signaling: S,
    network: N,
    credentials: VoiceCredentials,
    options: Options,
    channels: Channels,
) {
    let heartbeat_nonce = options.nonce_seed;
    let wanted = channels.wanted.borrow().clone();
    let video = options
        .video
        .clone()
        .map(|video| VideoState::new(video, wanted));
    let local_user_id = credentials.user_id.0;
    let experimental_udp = network.experimental_udp();
    let mut driver = Driver {
        signaling,
        network: Arc::new(network),
        credentials,
        options,
        ch: channels,
        seq_ack: None,
        udp: None,
        udp_work: None,
        held_udp: Arc::new(Mutex::new(Vec::with_capacity(3))),
        discovery: discovery::Campaign::new(experimental_udp),
        media: None,
        dave: None,
        expected_users: vec![local_user_id],
        ssrcs: SsrcMap::default(),
        sequences: HashMap::new(),
        pending: PendingSsrc::default(),
        heartbeat_nonce,
        ping_sequence: 0,
        plain: Vec::with_capacity(MAX_DATAGRAM),
        clear: Vec::with_capacity(64),
        sealed: Vec::with_capacity(MAX_DATAGRAM),
        body: Vec::with_capacity(MAX_DATAGRAM),
        rtcp: Vec::with_capacity(256),
        video,
        wants_closed: false,
    };
    let reason = driver.drive().await;
    // Sockets and keys go before anyone hears that the connection is closed.
    driver.reset_session();
    driver
        .held_udp
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    driver.set_status(VoiceStatus::Closed(reason));
}

async fn discovery_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1 << failures.saturating_sub(1).min(4))
}

/// What a close code from the voice server means for this connection.
fn close_code(code: u16, conn: &Conn) -> End {
    match code {
        4006 | 4009 => End::Close(CloseReason::SessionInvalid),
        4004 => End::Close(CloseReason::AuthenticationFailed),
        4011 => End::Close(CloseReason::ServerNotFound),
        4014 | 4022 => End::Close(CloseReason::Disconnected),
        4017 => End::Close(CloseReason::E2eeRequired),
        4021 => End::Close(CloseReason::RateLimited),
        // Server-side crashes: resume.
        4013 | 4015 => conn.retry(),
        4000..=4999 => End::Close(CloseReason::Protocol(Some(code))),
        _ => conn.retry(),
    }
}

fn decode<T: DeserializeOwned>(d: Option<&serde_json::value::RawValue>) -> Option<T> {
    serde_json::from_str(d?.get()).ok()
}

async fn recv_udp<L: UdpLink>(
    udp: Option<&Udp<L>>,
    buffer: &mut [u8],
) -> io::Result<(usize, SocketAddr)> {
    match udp {
        Some(udp) => udp.link.recv_from(buffer).await,
        None => std::future::pending().await,
    }
}

/// Resolves with whether the viewer's wanted-video flag changed; `false` once
/// its sender is gone.
async fn wants_changed(wanted: &mut watch::Receiver<VideoControl>) -> bool {
    wanted.changed().await.is_ok()
}

fn ntp_now() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = since.as_secs() + NTP_UNIX_OFFSET;
    let fraction = (u64::from(since.subsec_nanos()) << 32) / 1_000_000_000;
    (seconds << 32) | fraction
}

impl<S: Signaling, N: Network> Driver<S, N> {
    fn start_discovery(
        &mut self,
        conn: &mut Conn,
        server: SocketAddr,
        ssrc: u32,
        mode: TransportMode,
    ) -> Result<(), End> {
        let preludes = self
            .discovery
            .reserve_flow(Instant::now())
            .ok_or(End::Close(CloseReason::UdpUnreachable))?;
        self.udp = None;
        let network = self.network.clone();
        let held = self.held_udp.clone();
        let relay = self.network.relay_configured();
        self.udp_work = Some(Box::pin(async move {
            let Ok(link) = network.bind(server).await else {
                return UdpResult::Failed { relay };
            };
            let link = Arc::new(link);
            let mut held = held.lock().unwrap_or_else(|p| p.into_inner());
            if held.len() >= 3
                || link
                    .local_port()
                    .is_some_and(|port| held.iter().any(|old| old.local_port() == Some(port)))
            {
                return UdpResult::Failed { relay };
            }
            held.push(link.clone());
            UdpResult::Bound {
                link,
                server,
                ssrc,
                mode,
                preludes,
            }
        }));
        conn.phase = Phase::Discovering { attempts: 0 };
        conn.deadline = None;
        conn.ping_at = None;
        Ok(())
    }

    fn on_udp_work(&mut self, conn: &mut Conn, result: UdpResult<N::Udp>) -> Result<(), End> {
        self.udp_work = None;
        match result {
            UdpResult::Bound {
                link,
                server,
                ssrc,
                mode,
                preludes,
            } => {
                self.udp = Some(Udp {
                    link,
                    server,
                    ssrc,
                    mode,
                });
                self.set_status(if self.discovery.experimental {
                    VoiceStatus::DiscoveringExperimental {
                        variant: self.discovery.variant,
                    }
                } else {
                    VoiceStatus::Discovering
                });
                self.queue_discovery(conn, preludes)?;
            }
            UdpResult::Sent => conn.deadline = Some(Instant::now() + DISCOVERY_RETRY),
            UdpResult::Failed { relay } => {
                return Err(End::Close(if relay {
                    CloseReason::UdpRelayUnavailable
                } else {
                    CloseReason::Network
                }));
            }
        }
        Ok(())
    }

    fn queue_discovery(&mut self, conn: &mut Conn, preludes: u8) -> Result<(), End> {
        if !self.discovery.reserve_request(Instant::now()) {
            return Err(End::Close(CloseReason::UdpUnreachable));
        }
        let udp = self.udp.as_ref().ok_or(End::Close(CloseReason::Network))?;
        let link = udp.link.clone();
        let server = udp.server;
        let request = udp::discovery_request(udp.ssrc);
        self.udp_work = Some(Box::pin(async move {
            for _ in 0..preludes {
                let _ = link.send_to(discovery::PRELUDE, server).await;
            }
            let _ = link.send_to(&request, server).await;
            UdpResult::Sent
        }));
        let attempts = match conn.phase {
            Phase::Discovering { attempts } => attempts,
            _ => 0,
        };
        conn.phase = Phase::Discovering {
            attempts: attempts + 1,
        };
        conn.deadline = None;
        Ok(())
    }

    async fn drive(&mut self) -> CloseReason {
        let Some(url) = voice_url(&self.credentials.endpoint) else {
            return CloseReason::InvalidEndpoint;
        };
        let mut failures: u32 = 0;
        loop {
            if self.discovery.expired(Instant::now()) {
                return CloseReason::UdpUnreachable;
            }
            let resume = self.media.is_some();
            if failures > MAX_RECONNECTS {
                return CloseReason::Network;
            }
            if failures > 0 {
                self.set_status(if resume {
                    VoiceStatus::Resuming { attempt: failures }
                } else {
                    VoiceStatus::Connecting
                });
                tokio::select! {
                    biased;
                    _ = &mut self.ch.leave => return CloseReason::Left,
                    () = sleep(backoff(failures)) => {},
                    () = discovery_deadline(self.discovery.deadline()) => return CloseReason::UdpUnreachable,
                }
            } else {
                self.set_status(VoiceStatus::Connecting);
            }
            if !resume {
                self.reset_session();
            }
            let connected = tokio::select! {
                biased;
                _ = &mut self.ch.leave => return CloseReason::Left,
                connected = self.signaling.connect(&url) => connected,
                () = discovery_deadline(self.discovery.deadline()) => return CloseReason::UdpUnreachable,
            };
            let Ok(socket) = connected else {
                failures += 1;
                continue;
            };
            let end = self.connection(socket, resume).await;
            // Select Protocol is a one-way boundary for this user join. Before
            // Session Description there is no valid media Resume state; do
            // not Identify again to obtain a new experimental flow.
            if matches!(end, End::Retry { .. })
                && self.discovery.experimental
                && self.discovery.selected
                && self.media.is_none()
            {
                return CloseReason::Network;
            }
            match end {
                End::Close(reason) => return reason,
                End::Retry { progressed: true } => failures = 1,
                End::Retry { progressed: false } => failures += 1,
            }
        }
    }

    /// Forgets everything tied to one voice session, including DAVE/MLS keys.
    fn reset_session(&mut self) {
        // With the experiment off retain the original direct-only signaling
        // retry policy. An opt-in campaign never gets a new budget here.
        if !self.discovery.experimental {
            self.discovery = discovery::Campaign::new(false);
            self.held_udp
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
        }
        self.udp_work = None;
        self.udp = None;
        self.media = None;
        self.dave = None;
        self.expected_users.clear();
        self.expected_users.push(self.credentials.user_id.0);
        self.ssrcs.clear();
        self.sequences.clear();
        self.pending.clear();
        self.seq_ack = None;
        let wanted = self.ch.wanted.borrow().clone();
        self.video = self
            .options
            .video
            .clone()
            .map(|video| VideoState::new(video, wanted));
    }

    fn set_status(&self, status: VoiceStatus) {
        self.signaling.note_status(status);
        self.ch.status.send_replace(status);
    }

    fn event(&self, event: VoiceEvent) {
        // A consumer that stopped reading loses events, never the connection.
        let _ = self.ch.events.try_send(event);
    }

    fn connected_status(&self) -> VoiceStatus {
        let Some(udp) = &self.udp else {
            return VoiceStatus::Closed(CloseReason::Protocol(None));
        };
        if self.dave.as_ref().is_some_and(DaveSession::is_send_ready) {
            VoiceStatus::Connected {
                mode: udp.mode,
                ssrc: udp.ssrc,
            }
        } else {
            VoiceStatus::Rekeying {
                transition_id: self.dave.as_ref().and_then(DaveSession::transition_id),
            }
        }
    }

    async fn connection(&mut self, mut socket: S::Socket, resume: bool) -> End {
        let mut conn = Conn {
            phase: Phase::AwaitHello,
            resume,
            progressed: false,
            deadline: Some(Instant::now() + HELLO_TIMEOUT),
            heartbeat_interval: MAX_HEARTBEAT,
            heartbeat_at: None,
            awaiting_ack: None,
            ping_at: self.udp.is_some().then(|| Instant::now() + PING_INTERVAL),
            ping: None,
            report_at: None,
            silence_at: None,
        };
        let mut buffer = vec![0; MAX_DATAGRAM];
        loop {
            let media_open = conn.phase == Phase::Connected
                && self.media.is_some()
                && self.dave.as_ref().is_some_and(DaveSession::is_send_ready);
            let transport_up =
                conn.phase == Phase::Connected && self.media.is_some() && self.udp.is_some();
            self.video_gate(media_open);
            let wake = [
                conn.next_wake(self.pending.next_expiry()),
                self.discovery.deadline(),
                if transport_up {
                    self.video_wake()
                } else {
                    None
                },
            ]
            .into_iter()
            .flatten()
            .min();
            let video_open = media_open && self.video_can_send();
            let wants_open = self.video.is_some() && !self.wants_closed;
            let result = tokio::select! {
                _ = &mut self.ch.leave => {
                    let close = Message::Close(Some(CloseFrame {
                        code: CloseCode::Normal,
                        reason: "".into(),
                    }));
                    let _ = timeout(SEND_TIMEOUT, socket.send(close)).await;
                    Err(End::Close(CloseReason::Left))
                }
                work = udp_work(&mut self.udp_work) => self.on_udp_work(&mut conn, work),
                message = socket.next() => self.on_message(&mut conn, &mut socket, message).await,
                received = recv_udp(self.udp.as_ref(), &mut buffer) => match received {
                    Ok((len, from)) => {
                        self.on_datagram(&mut conn, &mut socket, &buffer[..len], from).await
                    }
                    // ICMP errors surface as receive errors on some systems;
                    // a dead route shows up as missing discovery or pings.
                    Err(error) if error.kind() == io::ErrorKind::ConnectionAborted && self.udp.as_ref().is_some_and(|udp| udp.link.is_relay()) => Err(End::Close(CloseReason::UdpRelayClosed)),
                    Err(_) => Ok(()),
                },
                command = self.ch.media.recv(), if media_open => match command {
                    Some(command) => {
                        if matches!(&command, MediaCommand::Opus(_) | MediaCommand::Soundshare(_)) {
                            self.ch.queued_audio.fetch_sub(1, Ordering::AcqRel);
                        }
                        self.on_media(&mut conn, &mut socket, command).await
                    }
                    // Every sender is gone: nothing more to send, keep receiving.
                    None => Ok(()),
                },
                frame = self.ch.video_in.recv(), if video_open => match frame {
                    Some(frame) => self.on_video_frame(&conn, &mut socket, frame).await,
                    None => Ok(()),
                },
                changed = wants_changed(&mut self.ch.wanted), if wants_open => {
                    if changed {
                        self.on_wanted_changed(&conn, &mut socket).await
                    } else {
                        self.wants_closed = true;
                        Ok(())
                    }
                }
                () = sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {
                    self.on_timers(&mut conn, &mut socket).await
                }
            };
            if let Err(end) = result {
                return end;
            }
        }
    }

    async fn send_text(conn: &Conn, socket: &mut S::Socket, text: &str) -> Result<(), End> {
        match timeout(SEND_TIMEOUT, socket.send(Message::text(text))).await {
            Ok(Ok(())) => Ok(()),
            _ => Err(conn.retry()),
        }
    }
    async fn send_binary(conn: &Conn, socket: &mut S::Socket, frame: Vec<u8>) -> Result<(), End> {
        match timeout(SEND_TIMEOUT, socket.send(Message::Binary(frame.into()))).await {
            Ok(Ok(())) => Ok(()),
            _ => Err(conn.retry()),
        }
    }

    async fn on_message(
        &mut self,
        conn: &mut Conn,
        socket: &mut S::Socket,
        message: Option<Result<Message, WsError>>,
    ) -> Result<(), End> {
        match message {
            Some(Ok(Message::Text(text))) => self.on_text(conn, socket, text.as_str()).await,
            Some(Ok(Message::Binary(bytes))) => {
                if bytes.len() > MAX_DAVE_MESSAGE {
                    Counters::add(&self.ch.counters.rejected_malformed, 1);
                    return Err(End::Close(CloseReason::DaveUnavailable));
                }
                let binary = wire::server_binary(&bytes)
                    .map_err(|_| End::Close(CloseReason::Protocol(None)))?;
                self.seq_ack = Some(u64::from(binary.sequence));
                self.on_binary(conn, socket, binary.message).await
            }
            Some(Ok(Message::Close(frame))) => Err(match frame {
                Some(frame) => close_code(frame.code.into(), conn),
                None => conn.retry(),
            }),
            Some(Ok(_)) => Ok(()),
            Some(Err(_)) | None => Err(conn.retry()),
        }
    }

    async fn on_text(
        &mut self,
        conn: &mut Conn,
        socket: &mut S::Socket,
        text: &str,
    ) -> Result<(), End> {
        let Ok(envelope) = serde_json::from_str::<wire::Envelope<'_>>(text) else {
            Counters::add(&self.ch.counters.rejected_malformed, 1);
            return Ok(());
        };
        if let Some(seq) = envelope.seq {
            self.seq_ack = Some(seq);
        }
        let now = Instant::now();
        let protocol = || End::Close(CloseReason::Protocol(None));
        match envelope.op {
            op::HELLO if conn.phase == Phase::AwaitHello => {
                let hello: wire::Hello = decode(envelope.d).ok_or_else(protocol)?;
                if !hello.heartbeat_interval.is_finite() || hello.heartbeat_interval <= 0.0 {
                    return Err(protocol());
                }
                conn.heartbeat_interval = Duration::from_secs_f64(
                    (hello.heartbeat_interval / 1000.0)
                        .clamp(MIN_HEARTBEAT.as_secs_f64(), MAX_HEARTBEAT.as_secs_f64()),
                );
                conn.heartbeat_at = Some(now + conn.heartbeat_interval);
                conn.deadline = Some(now + HANDSHAKE_TIMEOUT);
                if conn.resume {
                    let payload = wire::resume(&self.credentials, self.seq_ack);
                    Self::send_text(conn, socket, &payload).await?;
                    conn.phase = Phase::Resuming;
                } else {
                    let payload = wire::identify(&self.credentials, self.options.video.as_ref());
                    Self::send_text(conn, socket, &payload).await?;
                    conn.phase = Phase::Identifying;
                    self.set_status(VoiceStatus::Identifying);
                }
            }
            op::READY if conn.phase == Phase::Identifying => {
                let ready: wire::Ready = decode(envelope.d).ok_or_else(protocol)?;
                let mode = TransportMode::select(
                    ready.modes.iter().map(String::as_str),
                    self.options.aes_accelerated,
                )
                .ok_or(End::Close(CloseReason::NoSupportedMode))?;
                let ip: IpAddr = ready.ip.parse().map_err(|_| protocol())?;
                if ready.port == 0 || ready.ssrc == 0 {
                    return Err(protocol());
                }
                let server = SocketAddr::new(ip, ready.port);
                self.start_discovery(conn, server, ready.ssrc, mode)?;
                if let Some(video) = self.video.as_mut() {
                    video.set_layer(ready.layer());
                }
            }
            op::SESSION_DESCRIPTION if conn.phase == Phase::Negotiating => {
                let description: wire::SessionDescription =
                    decode(envelope.d).ok_or_else(protocol)?;
                let Some(udp) = &self.udp else {
                    return Err(protocol());
                };
                if description.dave_protocol_version != wire::MAX_DAVE_PROTOCOL_VERSION {
                    return Err(End::Close(if description.dave_protocol_version == 0 {
                        CloseReason::E2eeRequired
                    } else {
                        CloseReason::DaveUnavailable
                    }));
                }
                if TransportMode::from_wire(&description.mode) != Some(udp.mode)
                    || description.audio_codec != "opus"
                {
                    return Err(protocol());
                }
                let key = TransportKey::from_slice(&description.secret_key[..])
                    .map_err(|_| protocol())?;
                let mut dave = match self.credentials.dave_group_id {
                    Some(group_id) => DaveSession::new_with_group(
                        description.dave_protocol_version,
                        self.credentials.user_id.0,
                        self.credentials.channel_id.0,
                        group_id,
                    ),
                    None => DaveSession::new(
                        description.dave_protocol_version,
                        self.credentials.user_id.0,
                        self.credentials.channel_id.0,
                    ),
                }
                .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
                let key_package = dave
                    .create_key_package()
                    .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
                let (sequence, timestamp) = self
                    .options
                    .rtp_start
                    .unwrap_or_else(|| (OsRng.next_u32() as u16, OsRng.next_u32()));
                self.media = Some(Media {
                    cipher: TransportCipher::starting_at(udp.mode, &key, self.options.first_nonce),
                    rtp: RtpSender::new(udp.ssrc, OPUS_PAYLOAD_TYPE, sequence, timestamp),
                    speaking: false,
                    silence_left: 0,
                    last_audio_at: None,
                    packets: 0,
                    octets: 0,
                    reported_packets: 0,
                });
                self.dave = Some(dave);
                Self::send_binary(conn, socket, wire::dave_key_package(&key_package)).await?;
                conn.progressed = true;
                conn.phase = Phase::Connected;
                conn.deadline = None;
                conn.report_at = Some(now + REPORT_INTERVAL);
                self.set_status(VoiceStatus::Rekeying {
                    transition_id: None,
                });
                self.video_session_ready(conn, socket, description.video_codec.as_deref())
                    .await?;
            }
            op::RESUMED if conn.phase == Phase::Resuming => {
                let Some(dave) = self.dave.as_mut() else {
                    return Err(End::Close(CloseReason::DaveUnavailable));
                };
                if !dave.is_receive_ready() {
                    let key_package = dave
                        .create_key_package()
                        .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
                    Self::send_binary(conn, socket, wire::dave_key_package(&key_package)).await?;
                }
                conn.progressed = true;
                conn.phase = Phase::Connected;
                conn.deadline = None;
                conn.report_at = Some(now + REPORT_INTERVAL);
                self.set_status(self.connected_status());
                self.video_resumed(conn, socket).await?;
            }
            op::HEARTBEAT_ACK => {
                if let Some(ack) = decode::<wire::HeartbeatAck>(envelope.d)
                    && conn.awaiting_ack == Some(ack.t)
                {
                    conn.awaiting_ack = None;
                }
            }
            op::HEARTBEAT if conn.phase != Phase::AwaitHello => {
                // The server asks for a heartbeat now.
                self.heartbeat(conn, socket).await?;
            }
            op::SPEAKING => {
                if let Some(speaking) = decode::<wire::Speaking>(envelope.d)
                    && speaking.user_id != self.credentials.user_id
                {
                    self.ssrcs.speaking(speaking.user_id, speaking.ssrc);
                    self.mappings_changed();
                    self.release_pending(speaking.ssrc);
                    self.event(VoiceEvent::Speaking {
                        user_id: speaking.user_id,
                        ssrc: speaking.ssrc,
                        flags: speaking.speaking,
                    });
                }
            }
            op::VIDEO => {
                if let Some(video) = decode::<wire::Video>(envelope.d)
                    && video.user_id != self.credentials.user_id
                {
                    let streams: Vec<VideoStream> = video
                        .streams
                        .iter()
                        .map(|stream| VideoStream {
                            ssrc: stream.ssrc,
                            rtx_ssrc: stream.rtx_ssrc,
                        })
                        .collect();
                    self.ssrcs.video(video.user_id, video.audio_ssrc, &streams);
                    self.mappings_changed();
                    self.release_pending(video.audio_ssrc);
                    self.on_remote_video(conn, socket, &video).await?;
                }
            }
            op::SESSION_UPDATE => {
                if let Some(update) = decode::<wire::SessionUpdate>(envelope.d) {
                    self.on_session_update(update.video_codec.as_deref());
                }
            }
            op::MEDIA_SINK_WANTS => {
                if let Some(wants) = decode::<HashMap<String, serde_json::Value>>(envelope.d) {
                    self.on_sink_wants(conn, socket, &wants).await?;
                }
            }
            op::CLIENTS_CONNECT => {
                if let Some(connect) = decode::<wire::ClientsConnect>(envelope.d) {
                    for user_id in &connect.user_ids {
                        let user_id = user_id.0;
                        if !self.expected_users.contains(&user_id) {
                            self.expected_users.push(user_id);
                        }
                    }
                    self.event(VoiceEvent::ClientsConnected(connect.user_ids));
                } else {
                    return Err(protocol());
                }
            }
            op::CLIENT_DISCONNECT => {
                let disconnect: wire::ClientDisconnect = decode(envelope.d).ok_or_else(protocol)?;
                if disconnect.user_id == self.credentials.user_id {
                    return Err(End::Close(CloseReason::Disconnected));
                }
                self.expected_users
                    .retain(|user_id| *user_id != disconnect.user_id.0);
                self.ssrcs.remove_user(disconnect.user_id);
                self.mappings_changed();
                self.event(VoiceEvent::ClientDisconnected(disconnect.user_id));
                self.video_user_gone(conn, socket, disconnect.user_id)
                    .await?;
            }
            op::DAVE_PREPARE_TRANSITION => {
                let prepare: wire::DavePrepareTransition =
                    decode(envelope.d).ok_or_else(protocol)?;
                let ready = {
                    let Some(dave) = self.dave.as_mut() else {
                        return Err(End::Close(CloseReason::DaveUnavailable));
                    };
                    let ready = dave
                        .prepare_transition(prepare.transition_id, prepare.protocol_version)
                        .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
                    if prepare.transition_id == 0 {
                        dave.execute_transition(0)
                            .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
                        false
                    } else {
                        ready
                    }
                };
                if ready {
                    Self::send_text(
                        conn,
                        socket,
                        &wire::dave_transition_ready(prepare.transition_id),
                    )
                    .await?;
                }
                self.set_status(self.connected_status());
            }
            op::DAVE_EXECUTE_TRANSITION => {
                let transition: wire::DaveTransition = decode(envelope.d).ok_or_else(protocol)?;
                let dave = self
                    .dave
                    .as_mut()
                    .ok_or(End::Close(CloseReason::DaveUnavailable))?;
                // Transition 0 already took effect when it arrived; a late
                // execute for it is harmless.
                if transition.transition_id != 0 || dave.transition_id().is_some() {
                    dave.execute_transition(transition.transition_id)
                        .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
                }
                self.set_status(self.connected_status());
            }
            op::DAVE_PREPARE_EPOCH => {
                let prepare: wire::DavePrepareEpoch = decode(envelope.d).ok_or_else(protocol)?;
                if prepare.protocol_version != wire::MAX_DAVE_PROTOCOL_VERSION {
                    return Err(End::Close(CloseReason::DaveUnavailable));
                }
                let key_package = self
                    .dave
                    .as_mut()
                    .ok_or(End::Close(CloseReason::DaveUnavailable))?
                    .prepare_epoch(prepare.epoch)
                    .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
                if let Some(key_package) = key_package {
                    Self::send_binary(conn, socket, wire::dave_key_package(&key_package)).await?;
                }
                self.set_status(self.connected_status());
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_binary(
        &mut self,
        conn: &mut Conn,
        socket: &mut S::Socket,
        message: wire::ServerBinary<'_>,
    ) -> Result<(), End> {
        if matches!(message, wire::ServerBinary::Unknown) {
            return Ok(());
        }
        if conn.phase != Phase::Connected || self.media.is_none() {
            return Err(End::Close(CloseReason::Protocol(None)));
        }
        if self.dave.is_none() {
            return Err(End::Close(CloseReason::DaveUnavailable));
        }
        match message {
            wire::ServerBinary::ExternalSender(sender) => {
                self.dave
                    .as_mut()
                    .ok_or(End::Close(CloseReason::DaveUnavailable))?
                    .set_external_sender(sender)
                    .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
            }
            wire::ServerBinary::Proposals { operation, payload } => {
                let commit_welcome = self
                    .dave
                    .as_mut()
                    .ok_or(End::Close(CloseReason::DaveUnavailable))?
                    .process_proposals(operation, payload, &self.expected_users)
                    .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
                if let Some(commit_welcome) = commit_welcome {
                    let frame = wire::dave_commit_welcome(
                        &commit_welcome.commit,
                        commit_welcome.welcome.as_deref(),
                    );
                    Self::send_binary(conn, socket, frame).await?;
                }
            }
            wire::ServerBinary::Commit {
                transition_id,
                payload,
            } => {
                let processed = self
                    .dave
                    .as_mut()
                    .ok_or(End::Close(CloseReason::DaveUnavailable))?
                    .process_commit(transition_id, payload);
                if processed.is_err() {
                    self.recover_dave(conn, socket, transition_id).await?;
                    return Ok(());
                }
                self.after_group_change(conn, socket, transition_id).await?;
            }
            wire::ServerBinary::Welcome {
                transition_id,
                payload,
            } => {
                let processed = self
                    .dave
                    .as_mut()
                    .ok_or(End::Close(CloseReason::DaveUnavailable))?
                    .process_welcome(transition_id, payload);
                if processed.is_err() {
                    self.recover_dave(conn, socket, transition_id).await?;
                    return Ok(());
                }
                self.after_group_change(conn, socket, transition_id).await?;
            }
            wire::ServerBinary::Unknown => {}
        }
        Ok(())
    }

    /// A Commit or Welcome was applied. Transition 0 is a group
    /// (re)initialization that takes effect at once: no opcode 23 is sent and
    /// no opcode 22 follows, so sending starts now. Any other transition waits
    /// for the server's execute.
    async fn after_group_change(
        &mut self,
        conn: &mut Conn,
        socket: &mut S::Socket,
        transition_id: u16,
    ) -> Result<(), End> {
        if transition_id == 0 {
            self.dave
                .as_mut()
                .ok_or(End::Close(CloseReason::DaveUnavailable))?
                .execute_transition(0)
                .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
            self.set_status(self.connected_status());
            return Ok(());
        }
        Self::send_text(conn, socket, &wire::dave_transition_ready(transition_id)).await?;
        self.set_status(VoiceStatus::Rekeying {
            transition_id: Some(transition_id),
        });
        Ok(())
    }

    async fn recover_dave(
        &mut self,
        conn: &mut Conn,
        socket: &mut S::Socket,
        transition_id: u16,
    ) -> Result<(), End> {
        let key_package = self
            .dave
            .as_mut()
            .ok_or(End::Close(CloseReason::DaveUnavailable))?
            .recover(transition_id)
            .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
        Self::send_text(
            conn,
            socket,
            &wire::dave_invalid_commit_welcome(transition_id),
        )
        .await?;
        Self::send_binary(conn, socket, wire::dave_key_package(&key_package)).await?;
        self.set_status(VoiceStatus::Rekeying {
            transition_id: Some(transition_id),
        });
        Ok(())
    }

    async fn heartbeat(&mut self, conn: &mut Conn, socket: &mut S::Socket) -> Result<(), End> {
        let nonce = self.heartbeat_nonce;
        self.heartbeat_nonce = self.heartbeat_nonce.wrapping_add(1);
        Self::send_text(conn, socket, &wire::heartbeat(nonce, self.seq_ack)).await?;
        conn.awaiting_ack = Some(nonce);
        Ok(())
    }

    /// Sequence trackers follow the SSRC map, so they stay as bounded as it.
    fn mappings_changed(&mut self) {
        let ssrcs = &self.ssrcs;
        self.sequences
            .retain(|ssrc, _| ssrcs.owner(*ssrc).is_some());
        self.video_mappings_changed();
    }

    fn release_pending(&mut self, ssrc: u32) {
        if ssrc == 0 {
            return;
        }
        let expired = self.pending.expire(Instant::now());
        Counters::add(&self.ch.counters.dropped_unknown_ssrc, expired as u64);
        for audio in self.pending.take(ssrc) {
            self.deliver(ssrc, audio);
        }
    }

    /// Hands audio of a mapped SSRC to the consumer.
    fn deliver(&mut self, ssrc: u32, audio: PendingAudio) {
        let Some(owner) = self.ssrcs.owner(ssrc) else {
            return;
        };
        if owner.kind != StreamKind::Audio {
            Counters::add(&self.ch.counters.rejected_malformed, 1);
            return;
        }
        let Some(dave) = self.dave.as_mut() else {
            Counters::add(&self.ch.counters.audio_without_dave, 1);
            return;
        };
        let payload = match dave.decrypt_opus(owner.user_id.0, &audio.payload) {
            Ok(payload) => payload,
            Err(_) => {
                Counters::add(&self.ch.counters.audio_rejected_by_dave, 1);
                return;
            }
        };
        let sequence = self
            .sequences
            .entry(ssrc)
            .or_default()
            .extend(audio.sequence);
        let received = ReceivedAudio {
            user_id: owner.user_id,
            ssrc,
            sequence,
            timestamp: audio.timestamp,
            payload,
        };
        match self.ch.audio.try_send(received) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                Counters::add(&self.ch.counters.dropped_queue_full, 1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    async fn on_datagram(
        &mut self,
        conn: &mut Conn,
        socket: &mut S::Socket,
        packet: &[u8],
        from: SocketAddr,
    ) -> Result<(), End> {
        let counters = Arc::clone(&self.ch.counters);
        let Some(udp) = &self.udp else {
            return Ok(());
        };
        if from != udp.server {
            Counters::add(&counters.rejected_source, 1);
            return Ok(());
        }
        if packet.len() >= MAX_DATAGRAM {
            // Possibly truncated by the receive buffer.
            Counters::add(&counters.rejected_malformed, 1);
            return Ok(());
        }
        match udp::classify(packet) {
            Datagram::Discovery => {
                let Phase::Discovering { .. } = conn.phase else {
                    return Ok(());
                };
                let Ok(external) = udp::parse_discovery_response(packet, udp.ssrc) else {
                    Counters::add(&counters.rejected_malformed, 1);
                    return Ok(());
                };
                // Cancel any outstanding send before the one-way protocol
                // boundary; no laboratory datagram may leave after selection.
                self.udp_work = None;
                self.discovery.selected = true;
                let select = wire::select_protocol(external, udp.mode, self.options.video.as_ref());
                Self::send_text(conn, socket, &select).await?;
                conn.phase = Phase::Negotiating;
                conn.ping_at = Some(Instant::now() + PING_INTERVAL);
                conn.deadline = Some(Instant::now() + HANDSHAKE_TIMEOUT);
                self.set_status(VoiceStatus::Negotiating);
            }
            Datagram::Ping(sequence) => {
                if let Some((expected, sent)) = conn.ping
                    && expected == sequence
                {
                    conn.ping = None;
                    let latency = Instant::now() - sent;
                    if let Some(video) = self.video.as_mut() {
                        video.note_rtt(latency);
                    }
                    self.event(VoiceEvent::UdpLatency(latency));
                }
            }
            Datagram::Rtp => self.on_rtp(packet),
            Datagram::Rtcp => self.on_rtcp(packet),
            Datagram::Unknown => Counters::add(&counters.rejected_malformed, 1),
        }
        Ok(())
    }

    fn on_rtp(&mut self, packet: &[u8]) {
        if self.media.is_none() {
            // No key yet: nothing can be verified.
            Counters::add(&self.ch.counters.rejected_malformed, 1);
            return;
        }
        let Ok(header) = RtpHeader::parse(packet) else {
            Counters::add(&self.ch.counters.rejected_malformed, 1);
            return;
        };
        if header.payload_type != OPUS_PAYLOAD_TYPE {
            if self
                .video
                .as_ref()
                .is_some_and(|video| video.is_video_payload(header.payload_type))
            {
                self.on_video_rtp(packet, header);
            } else if self.video.is_none() || header.payload_type != video::PROBE_PAYLOAD_TYPE {
                // Only Opus and the negotiated video codecs were offered in
                // Select Protocol; payload type 96 is the server's probing.
                Counters::add(&self.ch.counters.rejected_malformed, 1);
            }
            return;
        }
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
        if body.extensions.elements().any(|element| element.is_err()) {
            Counters::add(&counters.rejected_malformed, 1);
            return;
        }
        if body.payload.is_empty() || body.payload.len() > MAX_DAVE_AUDIO_FRAME {
            Counters::add(&counters.rejected_malformed, 1);
            return;
        }
        Counters::add(&counters.rtp_received, 1);
        let audio = PendingAudio {
            sequence: header.sequence,
            timestamp: header.timestamp,
            payload: body.payload.to_vec(),
        };
        if self.ssrcs.owner(header.ssrc).is_some() {
            self.deliver(header.ssrc, audio);
        } else {
            let bytes = audio.payload.len();
            let dropped = self.pending.push(Instant::now(), header.ssrc, bytes, audio);
            Counters::add(&self.ch.counters.dropped_unknown_ssrc, dropped as u64);
        }
    }

    fn on_rtcp(&mut self, packet: &[u8]) {
        let counters = &self.ch.counters;
        let (Some(media), Some(_)) = (&self.media, &self.udp) else {
            Counters::add(&counters.rejected_malformed, 1);
            return;
        };
        if packet.len() < RTCP_CLEAR_LEN {
            Counters::add(&counters.rejected_malformed, 1);
            return;
        }
        match media.cipher.open(packet, RTCP_CLEAR_LEN, &mut self.plain) {
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
        let mut clear = std::mem::take(&mut self.clear);
        clear.clear();
        clear.extend_from_slice(&packet[..RTCP_CLEAR_LEN]);
        clear.extend_from_slice(&self.plain);
        for item in Compound::new(&clear) {
            match item {
                Ok(RtcpPacket::ReceiverReport(report)) => self.on_report_blocks(report.reports),
                Ok(RtcpPacket::SenderReport(report)) => {
                    self.on_remote_sender_report(report.ssrc, report.info);
                    self.on_report_blocks(report.reports);
                }
                Ok(RtcpPacket::Feedback(feedback)) => self.on_video_feedback(&feedback),
                Ok(_) => {}
                Err(_) => {
                    Counters::add(&self.ch.counters.rejected_malformed, 1);
                    break;
                }
            }
        }
        self.clear = clear;
    }

    /// Reception reports: those about our audio SSRC are surfaced as events,
    /// those about our video layer steer its congestion control.
    fn on_report_blocks(&mut self, blocks: ReportBlocks<'_>) {
        let Some(audio_ssrc) = self.udp.as_ref().map(|udp| udp.ssrc) else {
            return;
        };
        for block in blocks.iter() {
            if block.ssrc == audio_ssrc {
                self.event(VoiceEvent::ReceptionReport {
                    fraction_lost: block.fraction_lost,
                    cumulative_lost: block.cumulative_lost,
                    jitter: block.jitter,
                });
            } else {
                self.on_video_report(&block);
            }
        }
    }

    async fn on_media(
        &mut self,
        conn: &mut Conn,
        socket: &mut S::Socket,
        command: MediaCommand,
    ) -> Result<(), End> {
        let Some(media) = &mut self.media else {
            return Ok(());
        };
        let soundshare = matches!(command, MediaCommand::Soundshare(_));
        match command {
            MediaCommand::Opus(frame) | MediaCommand::Soundshare(frame) => {
                let flags = if soundshare {
                    wire::speaking::SOUNDSHARE
                } else {
                    wire::speaking::VOICE
                };
                media.silence_left = 0;
                conn.silence_at = None;
                if !media.speaking {
                    // Announce before the first audible packet.
                    media.speaking = true;
                    let ssrc = media.rtp.ssrc();
                    Self::send_text(conn, socket, &wire::speaking(flags, ssrc)).await?;
                }
                self.send_rtp(&frame).await
            }
            MediaCommand::EndSpeech => {
                if media.speaking && media.silence_left == 0 {
                    media.silence_left = SILENCE_FRAMES;
                    conn.silence_at = Some(Instant::now());
                }
                Ok(())
            }
            MediaCommand::VideoKeyframe(ssrc) => {
                if let Some(video) = self.video.as_mut() {
                    video.want_remote_keyframe(ssrc, Instant::now());
                }
                Ok(())
            }
        }
    }

    /// Seals one Opus payload as the next RTP packet and sends it.
    async fn send_rtp(&mut self, payload: &[u8]) -> Result<(), End> {
        // davey intentionally bypasses DAVE for the synthesized Opus silence
        // sentinel. Never put that transport-only frame on the wire.
        if payload == OPUS_SILENCE {
            return Ok(());
        }
        let Some(dave) = self.dave.as_mut() else {
            return Err(End::Close(CloseReason::DaveUnavailable));
        };
        if !dave.is_send_ready() {
            Counters::add(&self.ch.counters.audio_waiting_for_keys, 1);
            return Ok(());
        }
        let encrypted = dave
            .encrypt_opus(payload)
            .map_err(|_| End::Close(CloseReason::DaveUnavailable))?;
        let payload = encrypted.as_ref();
        let (Some(media), Some(udp)) = (&mut self.media, &self.udp) else {
            return Ok(());
        };
        // The media clock keeps running while nothing is sent. Without this a
        // receiver sees contiguous timestamps after a pause, reads the pause
        // as network delay and grows its jitter buffer by it (seconds of lag
        // after a few silences). Only real gaps count, not delivery jitter.
        let now = Instant::now();
        if let Some(last) = media.last_audio_at {
            let frames = (now - last).as_millis() / OPUS_FRAME_MS;
            if frames >= SILENCE_GAP_FRAMES {
                let skipped = u32::try_from(frames - 1).unwrap_or(u32::MAX / OPUS_FRAME_TICKS);
                media.rtp.skip(skipped.saturating_mul(OPUS_FRAME_TICKS));
            }
        }
        media.last_audio_at = Some(now);
        let header = media.rtp.next_header(OPUS_FRAME_TICKS);
        self.clear.clear();
        header.write(&mut self.clear);
        media
            .cipher
            .seal(&self.clear, payload, &mut self.sealed)
            .map_err(|_| End::Close(CloseReason::KeyExhausted))?;
        media.packets = media.packets.wrapping_add(1);
        media.octets = media.octets.wrapping_add(payload.len() as u32);
        // UDP is best effort; a lost packet is the receiver's concern.
        if udp.link.send_to(&self.sealed, udp.server).await.is_ok() {
            Counters::add(&self.ch.counters.rtp_sent, 1);
        }
        Ok(())
    }

    async fn send_sender_report(&mut self) -> Result<(), End> {
        let (Some(media), Some(udp)) = (&mut self.media, &self.udp) else {
            return Ok(());
        };
        if media.packets == media.reported_packets {
            return Ok(());
        }
        media.reported_packets = media.packets;
        let info = SenderInfo {
            ntp_timestamp: ntp_now(),
            rtp_timestamp: media.rtp.next_timestamp(),
            packet_count: media.packets,
            octet_count: media.octets,
        };
        rtcp::write_sender_report(udp.ssrc, info, &mut self.clear);
        let (clear, body) = self.clear.split_at(RTCP_CLEAR_LEN);
        media
            .cipher
            .seal(clear, body, &mut self.sealed)
            .map_err(|_| End::Close(CloseReason::KeyExhausted))?;
        let _ = udp.link.send_to(&self.sealed, udp.server).await;
        Ok(())
    }

    async fn on_timers(&mut self, conn: &mut Conn, socket: &mut S::Socket) -> Result<(), End> {
        let now = Instant::now();
        if self.discovery.expired(now) {
            return Err(End::Close(CloseReason::UdpUnreachable));
        }
        if conn.deadline.is_some_and(|at| at <= now) {
            match conn.phase {
                Phase::Discovering { attempts } => {
                    if self.udp_work.is_some() {
                        conn.deadline = None;
                    } else if attempts >= DISCOVERY_ATTEMPTS {
                        if !self.discovery.advance() {
                            return Err(End::Close(CloseReason::UdpUnreachable));
                        }
                        let udp = self
                            .udp
                            .as_ref()
                            .ok_or(End::Close(CloseReason::UdpUnreachable))?;
                        let target = (udp.server, udp.ssrc, udp.mode);
                        self.start_discovery(conn, target.0, target.1, target.2)?;
                    } else {
                        self.queue_discovery(conn, 0)?;
                    }
                }
                Phase::Connected => conn.deadline = None,
                // No HELLO, READY, RESUMED, or Session Description in time.
                _ => return Err(conn.retry()),
            }
        }
        if conn.heartbeat_at.is_some_and(|at| at <= now) {
            if conn.awaiting_ack.is_some() {
                // The previous heartbeat was never acknowledged: zombie.
                return Err(conn.retry());
            }
            conn.heartbeat_at = Some(now + conn.heartbeat_interval);
            self.heartbeat(conn, socket).await?;
        }
        if conn.ping_at.is_some_and(|at| at <= now) {
            conn.ping_at = Some(now + PING_INTERVAL);
            if let Some(udp) = &self.udp {
                self.ping_sequence = self.ping_sequence.wrapping_add(1);
                let sequence = self.ping_sequence;
                let _ = udp
                    .link
                    .send_to(&udp::ping_request(sequence), udp.server)
                    .await;
                conn.ping = Some((sequence, now));
            }
        }
        if conn.report_at.is_some_and(|at| at <= now) {
            conn.report_at = Some(now + REPORT_INTERVAL);
            self.send_sender_report().await?;
            self.send_video_sender_reports(now).await?;
        }
        if conn.silence_at.is_some_and(|at| at <= now) {
            self.send_rtp(&OPUS_SILENCE).await?;
            let finished = match &mut self.media {
                Some(media) => {
                    media.silence_left = media.silence_left.saturating_sub(1);
                    media.silence_left == 0
                }
                None => true,
            };
            if finished {
                conn.silence_at = None;
                if let Some(media) = &mut self.media {
                    media.speaking = false;
                    let ssrc = media.rtp.ssrc();
                    Self::send_text(conn, socket, &wire::speaking(0, ssrc)).await?;
                }
            } else {
                conn.silence_at = conn.silence_at.map(|at| at + FRAME);
            }
        }
        let expired = self.pending.expire(now);
        Counters::add(&self.ch.counters.dropped_unknown_ssrc, expired as u64);
        if conn.phase == Phase::Connected {
            self.on_video_timers(now).await?;
        }
        Ok(())
    }
}
