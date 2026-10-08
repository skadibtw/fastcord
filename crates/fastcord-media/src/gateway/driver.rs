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

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
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
use crate::rtcp::{self, Compound, RtcpPacket, SenderInfo};
use crate::rtp::{OPUS_FRAME_TICKS, OPUS_PAYLOAD_TYPE, RtpHeader, RtpSender, SequenceTracker};
use crate::session::VoiceCredentials;
use crate::ssrc::{PendingSsrc, SsrcMap, StreamKind, VideoStream};
use crate::udp::{self, Datagram, MAX_DATAGRAM};

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
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
/// Sent before stopping so receivers do not interpolate across the gap.
const SILENCE_FRAMES: u8 = 5;
const OPUS_SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];
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
        }
    }
}

pub(crate) struct Channels {
    pub(crate) status: watch::Sender<VoiceStatus>,
    pub(crate) events: mpsc::Sender<VoiceEvent>,
    pub(crate) audio: mpsc::Sender<ReceivedAudio>,
    pub(crate) media: mpsc::Receiver<MediaCommand>,
    pub(crate) leave: oneshot::Receiver<()>,
    pub(crate) counters: Arc<Counters>,
}

struct Udp<L> {
    link: L,
    server: SocketAddr,
    ssrc: u32,
    mode: TransportMode,
}

struct Media {
    cipher: TransportCipher,
    rtp: RtpSender,
    speaking: bool,
    silence_left: u8,
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

struct Driver<S, N: Network> {
    signaling: S,
    network: N,
    credentials: VoiceCredentials,
    options: Options,
    ch: Channels,
    seq_ack: Option<u64>,
    udp: Option<Udp<N::Udp>>,
    media: Option<Media>,
    ssrcs: SsrcMap,
    sequences: HashMap<u32, SequenceTracker>,
    pending: PendingSsrc<PendingAudio>,
    heartbeat_nonce: u64,
    ping_sequence: u32,
    /// Reused buffers: decrypted bodies, clear headers, sealed datagrams.
    plain: Vec<u8>,
    clear: Vec<u8>,
    sealed: Vec<u8>,
}

pub(crate) async fn run<S: Signaling, N: Network>(
    signaling: S,
    network: N,
    credentials: VoiceCredentials,
    options: Options,
    channels: Channels,
) {
    let heartbeat_nonce = options.nonce_seed;
    let mut driver = Driver {
        signaling,
        network,
        credentials,
        options,
        ch: channels,
        seq_ack: None,
        udp: None,
        media: None,
        ssrcs: SsrcMap::default(),
        sequences: HashMap::new(),
        pending: PendingSsrc::default(),
        heartbeat_nonce,
        ping_sequence: 0,
        plain: Vec::with_capacity(MAX_DATAGRAM),
        clear: Vec::with_capacity(64),
        sealed: Vec::with_capacity(MAX_DATAGRAM),
    };
    let reason = driver.drive().await;
    // Sockets and keys go before anyone hears that the connection is closed.
    driver.reset_session();
    driver.ch.status.send_replace(VoiceStatus::Closed(reason));
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

fn ntp_now() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = since.as_secs() + NTP_UNIX_OFFSET;
    let fraction = (u64::from(since.subsec_nanos()) << 32) / 1_000_000_000;
    (seconds << 32) | fraction
}

impl<S: Signaling, N: Network> Driver<S, N> {
    async fn drive(&mut self) -> CloseReason {
        let Some(url) = voice_url(&self.credentials.endpoint) else {
            return CloseReason::InvalidEndpoint;
        };
        let mut failures: u32 = 0;
        loop {
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
                    () = sleep(backoff(failures)) => {}
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
            };
            let Ok(socket) = connected else {
                failures += 1;
                continue;
            };
            match self.connection(socket, resume).await {
                End::Close(reason) => return reason,
                End::Retry { progressed: true } => failures = 1,
                End::Retry { progressed: false } => failures += 1,
            }
        }
    }

    /// Forgets everything tied to one voice session: UDP, key, SSRCs.
    fn reset_session(&mut self) {
        self.udp = None;
        self.media = None;
        self.ssrcs.clear();
        self.sequences.clear();
        self.pending.clear();
        self.seq_ack = None;
    }

    fn set_status(&self, status: VoiceStatus) {
        self.ch.status.send_replace(status);
    }

    fn event(&self, event: VoiceEvent) {
        // A consumer that stopped reading loses events, never the connection.
        let _ = self.ch.events.try_send(event);
    }

    fn connected_status(&self) -> VoiceStatus {
        match &self.udp {
            Some(udp) => VoiceStatus::Connected {
                mode: udp.mode,
                ssrc: udp.ssrc,
            },
            None => VoiceStatus::Closed(CloseReason::Protocol(None)),
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
            let wake = conn.next_wake(self.pending.next_expiry());
            let media_open = conn.phase == Phase::Connected && self.media.is_some();
            let result = tokio::select! {
                _ = &mut self.ch.leave => {
                    let close = Message::Close(Some(CloseFrame {
                        code: CloseCode::Normal,
                        reason: "".into(),
                    }));
                    let _ = timeout(SEND_TIMEOUT, socket.send(close)).await;
                    Err(End::Close(CloseReason::Left))
                }
                message = socket.next() => self.on_message(&mut conn, &mut socket, message).await,
                received = recv_udp(self.udp.as_ref(), &mut buffer) => match received {
                    Ok((len, from)) => {
                        self.on_datagram(&mut conn, &mut socket, &buffer[..len], from).await
                    }
                    // ICMP errors surface as receive errors on some systems;
                    // a dead route shows up as missing discovery or pings.
                    Err(_) => Ok(()),
                },
                command = self.ch.media.recv(), if media_open => match command {
                    Some(command) => self.on_media(&mut conn, &mut socket, command).await,
                    // Every sender is gone: nothing more to send, keep receiving.
                    None => Ok(()),
                },
                () = sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {
                    self.on_timers(&mut conn, &mut socket).await
                }
            };
            if let Err(end) = result {
                return end;
            }
        }
    }

    async fn send_text(&self, conn: &Conn, socket: &mut S::Socket, text: &str) -> Result<(), End> {
        match timeout(SEND_TIMEOUT, socket.send(Message::text(text))).await {
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
                // Server binary messages (DAVE, milestone 18) start with a
                // sequence number that heartbeats must acknowledge.
                if bytes.len() >= 3 {
                    self.seq_ack = Some(u64::from(u16::from_be_bytes([bytes[0], bytes[1]])));
                }
                Ok(())
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
                    self.send_text(conn, socket, &payload).await?;
                    conn.phase = Phase::Resuming;
                } else {
                    let payload = wire::identify(&self.credentials);
                    self.send_text(conn, socket, &payload).await?;
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
                let link = self
                    .network
                    .bind(server)
                    .await
                    .map_err(|_| End::Close(CloseReason::Network))?;
                let _ = link
                    .send_to(&udp::discovery_request(ready.ssrc), server)
                    .await;
                self.udp = Some(Udp {
                    link,
                    server,
                    ssrc: ready.ssrc,
                    mode,
                });

                conn.phase = Phase::Discovering { attempts: 1 };
                conn.deadline = Some(now + DISCOVERY_RETRY);
                conn.ping_at = Some(now + PING_INTERVAL);
                self.set_status(VoiceStatus::Discovering);
            }
            op::SESSION_DESCRIPTION if conn.phase == Phase::Negotiating => {
                let description: wire::SessionDescription =
                    decode(envelope.d).ok_or_else(protocol)?;
                let Some(udp) = &self.udp else {
                    return Err(protocol());
                };
                // fastcord offered no DAVE version, so 0 is the only answer
                // it can honor; anything else would mean unprotected frames.
                if TransportMode::from_wire(&description.mode) != Some(udp.mode)
                    || description.dave_protocol_version != wire::MAX_DAVE_PROTOCOL_VERSION
                    || description.audio_codec != "opus"
                {
                    return Err(protocol());
                }
                let key = TransportKey::from_slice(&description.secret_key[..])
                    .map_err(|_| protocol())?;
                let (sequence, timestamp) = self
                    .options
                    .rtp_start
                    .unwrap_or_else(|| (OsRng.next_u32() as u16, OsRng.next_u32()));
                self.media = Some(Media {
                    cipher: TransportCipher::starting_at(udp.mode, &key, self.options.first_nonce),
                    rtp: RtpSender::new(udp.ssrc, OPUS_PAYLOAD_TYPE, sequence, timestamp),
                    speaking: false,
                    silence_left: 0,
                    packets: 0,
                    octets: 0,
                    reported_packets: 0,
                });
                conn.progressed = true;
                conn.phase = Phase::Connected;
                conn.deadline = None;
                conn.report_at = Some(now + REPORT_INTERVAL);
                self.set_status(self.connected_status());
            }
            op::RESUMED if conn.phase == Phase::Resuming => {
                conn.progressed = true;
                conn.phase = Phase::Connected;
                conn.deadline = None;
                conn.report_at = Some(now + REPORT_INTERVAL);
                self.set_status(self.connected_status());
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
                }
            }
            op::CLIENTS_CONNECT => {
                if let Some(connect) = decode::<wire::ClientsConnect>(envelope.d) {
                    self.event(VoiceEvent::ClientsConnected(connect.user_ids));
                }
            }
            op::CLIENT_DISCONNECT => {
                if let Some(disconnect) = decode::<wire::ClientDisconnect>(envelope.d) {
                    self.ssrcs.remove_user(disconnect.user_id);
                    self.mappings_changed();
                    self.event(VoiceEvent::ClientDisconnected(disconnect.user_id));
                }
            }
            // Session updates, client flags/platform, media sink wants, and
            // anything newer carry nothing an audio-only client acts on.
            _ => {}
        }
        Ok(())
    }

    async fn heartbeat(&mut self, conn: &mut Conn, socket: &mut S::Socket) -> Result<(), End> {
        let nonce = self.heartbeat_nonce;
        self.heartbeat_nonce = self.heartbeat_nonce.wrapping_add(1);
        self.send_text(conn, socket, &wire::heartbeat(nonce, self.seq_ack))
            .await?;
        conn.awaiting_ack = Some(nonce);
        Ok(())
    }

    /// Sequence trackers follow the SSRC map, so they stay as bounded as it.
    fn mappings_changed(&mut self) {
        let ssrcs = &self.ssrcs;
        self.sequences
            .retain(|ssrc, _| ssrcs.owner(*ssrc).is_some());
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
            payload: audio.payload,
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
                let select = wire::select_protocol(external, udp.mode);
                self.send_text(conn, socket, &select).await?;
                conn.phase = Phase::Negotiating;
                conn.deadline = Some(Instant::now() + HANDSHAKE_TIMEOUT);
                self.set_status(VoiceStatus::Negotiating);
            }
            Datagram::Ping(sequence) => {
                if let Some((expected, sent)) = conn.ping
                    && expected == sequence
                {
                    conn.ping = None;
                    self.event(VoiceEvent::UdpLatency(Instant::now() - sent));
                }
            }
            Datagram::Rtp => self.on_rtp(packet),
            Datagram::Rtcp => self.on_rtcp(packet),
            Datagram::Unknown => Counters::add(&counters.rejected_malformed, 1),
        }
        Ok(())
    }

    fn on_rtp(&mut self, packet: &[u8]) {
        let counters = &self.ch.counters;
        let Some(media) = &self.media else {
            // No key yet: nothing can be verified.
            Counters::add(&counters.rejected_malformed, 1);
            return;
        };
        let Ok(header) = RtpHeader::parse(packet) else {
            Counters::add(&counters.rejected_malformed, 1);
            return;
        };
        if header.payload_type != OPUS_PAYLOAD_TYPE {
            // Only Opus was offered in Select Protocol.
            Counters::add(&counters.rejected_malformed, 1);
            return;
        }
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
        if body.payload.is_empty() || body.payload.len() > super::MAX_OPUS_FRAME {
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
        let (Some(media), Some(udp)) = (&self.media, &self.udp) else {
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
        self.clear.clear();
        self.clear.extend_from_slice(&packet[..RTCP_CLEAR_LEN]);
        self.clear.extend_from_slice(&self.plain);
        for item in Compound::new(&self.clear) {
            let blocks = match item {
                Ok(RtcpPacket::ReceiverReport(report)) => report.reports,
                Ok(RtcpPacket::SenderReport(report)) => report.reports,
                Ok(_) => continue,
                Err(_) => {
                    Counters::add(&counters.rejected_malformed, 1);
                    break;
                }
            };
            for block in blocks.iter().filter(|block| block.ssrc == udp.ssrc) {
                self.event(VoiceEvent::ReceptionReport {
                    fraction_lost: block.fraction_lost,
                    cumulative_lost: block.cumulative_lost,
                    jitter: block.jitter,
                });
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
        match command {
            MediaCommand::Opus(frame) => {
                media.silence_left = 0;
                conn.silence_at = None;
                if !media.speaking {
                    // Announce before the first audible packet.
                    media.speaking = true;
                    let ssrc = media.rtp.ssrc();
                    self.send_text(conn, socket, &wire::speaking(wire::speaking::VOICE, ssrc))
                        .await?;
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
        }
    }

    /// Seals one Opus payload as the next RTP packet and sends it.
    async fn send_rtp(&mut self, payload: &[u8]) -> Result<(), End> {
        let (Some(media), Some(udp)) = (&mut self.media, &self.udp) else {
            return Ok(());
        };
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
        if conn.deadline.is_some_and(|at| at <= now) {
            match conn.phase {
                Phase::Discovering { attempts } => {
                    if attempts >= DISCOVERY_ATTEMPTS {
                        return Err(End::Close(CloseReason::UdpUnreachable));
                    }
                    if let Some(udp) = &self.udp {
                        let _ = udp
                            .link
                            .send_to(&udp::discovery_request(udp.ssrc), udp.server)
                            .await;
                    }
                    conn.phase = Phase::Discovering {
                        attempts: attempts + 1,
                    };
                    conn.deadline = Some(now + DISCOVERY_RETRY);
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
                    self.send_text(conn, socket, &wire::speaking(0, ssrc))
                        .await?;
                }
            } else {
                conn.silence_at = conn.silence_at.map(|at| at + FRAME);
            }
        }
        let expired = self.pending.expire(now);
        Counters::add(&self.ch.counters.dropped_unknown_ssrc, expired as u64);
        Ok(())
    }
}
