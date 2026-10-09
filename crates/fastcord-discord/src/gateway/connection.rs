//! The Gateway connection state machine (SPEC §4.2).
//!
//! `Connecting -> AwaitHello -> Identifying/Resuming -> Ready -> Reconnecting`,
//! plus the terminal `AuthenticationRequired` and `Stopped`. One task owns the
//! socket, the zlib context, heartbeats, and the session; it never touches
//! state owned by the reducer, only hands it ordered events.
//!
//! Design notes:
//! - Delivery to the consumer is a `select!` branch beside the heartbeat timer,
//!   so a stalled consumer applies backpressure to the socket but can never stop
//!   heartbeats. While events are waiting for the consumer the task is not
//!   reading, so missing ACKs are not held against the server.
//! - Events already decoded are always delivered, in order, before the task
//!   reconnects, so the sequence used to Resume is exactly what was delivered.
//! - Resume survives connect failures (a network outage says nothing about the
//!   session); only the server (opcode 9, close codes 4003/4007/4009) ends it.
//! - Opcode 4 is sent only while Ready, and only when the wanted voice state
//!   differs from what this session last sent. That record survives
//!   reconnects: a Resume keeps the server's voice state, a fresh READY
//!   starts without one.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use fastcord_model::VoiceStateRequest;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use zeroize::Zeroizing;

use super::url::GatewayUrl;

use super::compression::{InflateError, Inflater};
use super::decode::{Decoded, decode_dispatch, is_ready_family};
use super::event::{ConnectionState, ReconnectReason, StopReason};
use super::event::{Dispatch, GatewayEvent};
use super::outbox::Outbox;
use super::pacing::{JitterSource, SEND_WINDOW, SendBudget, backoff_delay, invalid_session_delay};
use super::profile::{ClientProperties, HostOs};
use super::stream::StreamCommand;
use super::subscription::{CONTROL_RESERVE, Subscriber, SubscriptionTarget};
use super::transport::{DiscoverError, Transport};
use super::voice;
use super::wire::{self, Envelope, Hello, op};
use crate::UserToken;

const HELLO_TIMEOUT: Duration = Duration::from_secs(20);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// A connection that stayed Ready this long counts as healthy: the next
/// reconnect starts the backoff over instead of continuing it.
const STABLE_AFTER: Duration = Duration::from_secs(30);
/// Heartbeat intervals outside this range are treated as hostile or broken.
const MIN_HEARTBEAT: Duration = Duration::from_secs(1);
const MAX_HEARTBEAT: Duration = Duration::from_secs(300);
/// Decoded events allowed to wait for the consumer before the socket is no
/// longer read.
const OUTBOX_LIMIT: usize = 4;
/// Consecutive Invalid Session answers to Identify before giving up.
const MAX_IDENTIFY_REJECTIONS: u32 = 5;
const STATE_COST: usize = 64;

/// Why the whole task ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Terminal {
    /// The handle was dropped.
    Shutdown,
    AuthenticationRequired,
    Stop(StopReason),
}

enum Interrupt {
    Shutdown,
    AuthenticationRequired,
}

impl From<Interrupt> for Terminal {
    fn from(interrupt: Interrupt) -> Self {
        match interrupt {
            Interrupt::Shutdown => Self::Shutdown,
            Interrupt::AuthenticationRequired => Self::AuthenticationRequired,
        }
    }
}

type InterruptFuture = Pin<Box<dyn Future<Output = Interrupt> + Send>>;

fn make_interrupt<T: Transport>(
    transport: Arc<T>,
    shutdown: oneshot::Receiver<()>,
) -> InterruptFuture {
    Box::pin(async move {
        tokio::select! {
            biased;
            _ = shutdown => Interrupt::Shutdown,
            () = transport.authentication_required() => Interrupt::AuthenticationRequired,
        }
    })
}

async fn interruptible<F: Future>(
    interrupt: &mut InterruptFuture,
    future: F,
) -> Result<F::Output, Terminal> {
    tokio::select! {
        biased;
        reason = interrupt.as_mut() => Err(reason.into()),
        output = future => Ok(output),
    }
}

/// Server-side session state, kept in memory only.
struct Session {
    id: String,
    resume_url: Option<GatewayUrl>,
    /// Last dispatch sequence received and delivered.
    seq: Option<u64>,
}

struct Driver<T: Transport, J: JitterSource> {
    transport: Arc<T>,
    token: Arc<UserToken>,
    properties: ClientProperties,
    jitter: J,
    interrupt: InterruptFuture,
    outbox: Outbox,
    subscriptions: watch::Receiver<SubscriptionTarget>,
    subscriber: Subscriber,
    session: Option<Session>,
    identify_rejections: u32,
    stream: mpsc::Receiver<StreamCommand>,
    stream_pending: Option<StreamCommand>,
    /// The last opcode 4 this session accepted from us, across reconnects.
    voice: watch::Receiver<Option<VoiceStateRequest>>,
    voice_sent: Option<VoiceStateRequest>,
}

/// What the consumer wants the session to hold, as the handles set them.
pub(crate) struct Wanted {
    pub(crate) subscriptions: watch::Receiver<SubscriptionTarget>,
    pub(crate) voice: watch::Receiver<Option<VoiceStateRequest>>,
    pub(crate) stream: mpsc::Receiver<StreamCommand>,
}

pub(crate) async fn run<T: Transport, J: JitterSource>(
    transport: Arc<T>,
    token: Arc<UserToken>,
    locale: String,
    jitter: J,
    outbox: Outbox,
    wanted: Wanted,
    shutdown: oneshot::Receiver<()>,
) {
    let Wanted {
        subscriptions,
        voice,
        stream,
    } = wanted;
    let interrupt = make_interrupt(Arc::clone(&transport), shutdown);
    let mut driver = Driver {
        transport,
        token,
        // Replaced by `drive` once the build number is resolved.
        properties: ClientProperties::web(HostOs::current(), &locale, 0),
        jitter,
        interrupt,
        outbox,
        subscriptions,
        subscriber: Subscriber::new(),
        session: None,
        identify_rejections: 0,
        stream,
        stream_pending: None,
        voice,
        voice_sent: None,
    };
    let terminal = match drive(&mut driver, &locale).await {
        Err(terminal) => terminal,
        Ok(never) => match never {},
    };
    finish(driver.outbox, terminal).await;
}

async fn finish(mut outbox: Outbox, terminal: Terminal) {
    let state = match terminal {
        // The handle (and with it the receiver) is gone: nothing to tell.
        Terminal::Shutdown => return,
        Terminal::AuthenticationRequired => ConnectionState::AuthenticationRequired,
        Terminal::Stop(reason) => ConnectionState::Stopped(reason),
    };
    outbox.push(GatewayEvent::State(state), STATE_COST);
    let _ = outbox.flush().await;
}

async fn drive<T: Transport, J: JitterSource>(
    driver: &mut Driver<T, J>,
    locale: &str,
) -> Result<Infallible, Terminal> {
    // The profile is fixed here, once, and reused by every Identify.
    let build = interruptible(&mut driver.interrupt, driver.transport.build_number()).await?;
    driver.properties = ClientProperties::web(HostOs::current(), locale, build.value);
    let mut failures = 0u32;
    let mut base_url: Option<GatewayUrl> = None;
    loop {
        driver.outbox.push(
            GatewayEvent::State(ConnectionState::Connecting { attempt: failures }),
            STATE_COST,
        );
        let resume_url = driver
            .session
            .as_ref()
            .and_then(|session| session.resume_url.clone());
        let url = match resume_url.or_else(|| base_url.clone()) {
            Some(url) => url,
            None => {
                match interruptible(&mut driver.interrupt, driver.transport.discover()).await? {
                    Ok(text) => {
                        let Some(url) = GatewayUrl::parse(&text) else {
                            return Err(Terminal::Stop(StopReason::InvalidGatewayUrl));
                        };
                        base_url = Some(url.clone());
                        url
                    }
                    Err(DiscoverError::AuthenticationRequired) => {
                        return Err(Terminal::AuthenticationRequired);
                    }
                    Err(DiscoverError::Failed) => {
                        failures = failures.saturating_add(1);
                        wait(driver, failures, ReconnectReason::DiscoveryFailed).await?;
                        continue;
                    }
                }
            }
        };
        let socket =
            match interruptible(&mut driver.interrupt, driver.transport.connect(&url)).await? {
                Ok(socket) => socket,
                Err(_) => {
                    failures = failures.saturating_add(1);
                    if driver.session.is_none() {
                        // Ask Discord for a fresh address after a failed connect.
                        base_url = None;
                    }
                    wait(driver, failures, ReconnectReason::ConnectFailed).await?;
                    continue;
                }
            };
        let ended = connection(driver, socket).await?;
        if !ended.resume {
            driver.session = None;
        }
        failures = if ended.healthy {
            0
        } else {
            failures.saturating_add(1)
        };
        wait(driver, failures, ended.reason).await?;
    }
}

fn reconnect_delay(
    reason: ReconnectReason,
    failures: u32,
    jitter: &mut impl JitterSource,
) -> Duration {
    match reason {
        ReconnectReason::InvalidSession { resumable: false } => {
            invalid_session_delay(jitter.unit())
        }
        // A connection that was healthy before it dropped resumes at once.
        _ if failures == 0 => Duration::ZERO,
        _ => backoff_delay(failures, jitter.unit()),
    }
}

/// Announces the wait, delivers everything still pending, then sleeps.
async fn wait<T: Transport, J: JitterSource>(
    driver: &mut Driver<T, J>,
    failures: u32,
    reason: ReconnectReason,
) -> Result<(), Terminal> {
    let delay = reconnect_delay(reason, failures, &mut driver.jitter);
    driver.outbox.push(
        GatewayEvent::State(ConnectionState::Reconnecting {
            attempt: failures,
            delay,
            reason,
        }),
        STATE_COST,
    );
    interruptible(&mut driver.interrupt, driver.outbox.flush())
        .await?
        .map_err(|_| Terminal::Shutdown)?;
    interruptible(&mut driver.interrupt, sleep(delay)).await?;
    Ok(())
}

struct Ended {
    reason: ReconnectReason,
    /// Whether the session may be resumed on the next connection.
    resume: bool,
    healthy: bool,
}

type End = Result<(ReconnectReason, bool), Terminal>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sent {
    Nothing,
    Identify,
    Resume,
}

struct Conn {
    hello_seen: bool,
    interval: Duration,
    /// A heartbeat sent by the interval timer has not been acknowledged yet.
    ack_pending: bool,
    sent: Sent,
    ready_since: Option<Instant>,
}

enum Action {
    Nothing,
    Hello(Duration),
    HeartbeatRequested,
    Ready,
    Resumed,
    Reconnect(ReconnectReason, bool),
    Terminal(Terminal),
}

async fn send<S>(socket: &mut S, text: &str) -> bool
where
    S: futures_util::Sink<Message, Error = WsError> + Unpin,
{
    matches!(
        timeout(SEND_TIMEOUT, socket.send(Message::text(text))).await,
        Ok(Ok(()))
    )
}

/// Enough future timed heartbeats for a complete send window. HELLO intervals
/// are already clamped to the supported nonzero range.
fn heartbeat_reserve(interval: Duration) -> usize {
    SEND_WINDOW.as_nanos().div_ceil(interval.as_nanos()) as usize
}

/// The heartbeat reserve plus room for other control traffic (opcode 4,
/// answers to server heartbeat requests), which optional traffic leaves alone.
fn control_reserve(interval: Duration) -> usize {
    CONTROL_RESERVE + heartbeat_reserve(interval)
}

enum SubscriptionSendError {
    Outbound,
    Network,
}

/// Sends what brings the server's subscriptions to `target`. Optional traffic
/// leaves room for future heartbeats and yields between batches once the next
/// heartbeat is due. A rejected plan is never sent or committed.
async fn flush_subscriptions<S>(
    socket: &mut S,
    subscriber: &mut Subscriber,
    target: &SubscriptionTarget,
    budget: &mut SendBudget,
    interval: Duration,
    heartbeat_deadline: Instant,
) -> Result<(), SubscriptionSendError>
where
    S: futures_util::Sink<Message, Error = WsError> + Unpin,
{
    let reserve = control_reserve(interval);
    for batch in subscriber
        .plan(target)
        .map_err(|_| SubscriptionSendError::Outbound)?
    {
        let now = Instant::now();
        if now >= heartbeat_deadline {
            // Keep the pending deadline: the biased loop sends the heartbeat
            // first, then returns to the remaining subscription work.
            return Ok(());
        }
        if budget.remaining(now) <= reserve {
            subscriber.defer(now);
            return Ok(());
        }
        budget.record(now);
        if !send(socket, &batch.frame).await {
            return Err(SubscriptionSendError::Network);
        }
        subscriber.commit(batch);
    }
    subscriber.settle();
    Ok(())
}

enum VoiceFlush {
    Done,
    /// The send ceiling has no room above the heartbeat reserve before this.
    RetryAt(Instant),
    Network,
}

/// Sends the wanted voice state unless this session already holds it. It may
/// use the control reserve that subscriptions leave alone, but never the room
/// kept for a full window of timed heartbeats.
async fn flush_voice<S>(
    socket: &mut S,
    wanted: Option<VoiceStateRequest>,
    sent: &mut Option<VoiceStateRequest>,
    budget: &mut SendBudget,
    interval: Duration,
) -> VoiceFlush
where
    S: futures_util::Sink<Message, Error = WsError> + Unpin,
{
    let Some(request) = wanted.filter(|_| voice::needs_send(wanted, *sent)) else {
        return VoiceFlush::Done;
    };
    let now = Instant::now();
    let reserve = heartbeat_reserve(interval);
    if budget.remaining(now) <= reserve {
        return VoiceFlush::RetryAt(budget.free_at(now, reserve));
    }
    budget.record(now);
    if !send(socket, &wire::voice_state_update(&request)).await {
        return VoiceFlush::Network;
    }
    *sent = Some(request);
    VoiceFlush::Done
}
enum StreamFlush {
    Done,
    RetryAt(Instant),
    Network,
    Outbound,
}

/// Stream signals are optional control traffic and leave a complete heartbeat
/// send window available, just like opcode 4.
async fn flush_stream<S>(
    socket: &mut S,
    command: &StreamCommand,
    budget: &mut SendBudget,
    interval: Duration,
) -> StreamFlush
where
    S: futures_util::Sink<Message, Error = WsError> + Unpin,
{
    let now = Instant::now();
    let reserve = heartbeat_reserve(interval);
    if budget.remaining(now) <= reserve {
        return StreamFlush::RetryAt(budget.free_at(now, reserve));
    }
    let Ok(payload) = wire::stream_signal(command) else {
        return StreamFlush::Outbound;
    };
    budget.record(now);
    if !send(socket, &payload).await {
        return StreamFlush::Network;
    }
    StreamFlush::Done
}

async fn connection<T: Transport, J: JitterSource>(
    driver: &mut Driver<T, J>,
    mut socket: T::Socket,
) -> Result<Ended, Terminal> {
    let Driver {
        transport,
        token,
        properties,
        jitter,
        interrupt,
        outbox,
        subscriptions,
        subscriber,
        session,
        identify_rejections,
        stream,
        stream_pending,
        voice,
        voice_sent,
    } = driver;
    let mut conn = Conn {
        hello_seen: false,
        interval: MAX_HEARTBEAT,
        ack_pending: false,
        sent: Sent::Nothing,
        ready_since: None,
    };
    let mut inflater = Inflater::new();
    subscriber.pause();
    // Cleared once every handle is gone: no further changes can arrive.
    let mut watching = true;
    let mut voice_watching = true;
    let mut stream_watching = true;
    // Stream opcodes, like voice-state requests, are gated on READY/RESUMED.
    let mut voice_live = false;
    let mut stream_live = false;
    // The pending command belongs to the driver across socket replacements.
    let mut voice_due: Option<Instant> = None;
    let mut stream_due: Option<Instant> = None;
    let mut budget = SendBudget::new();
    // HELLO deadline until HELLO arrives, then the next heartbeat.
    let mut timer = Instant::now() + HELLO_TIMEOUT;
    outbox.push(GatewayEvent::State(ConnectionState::AwaitHello), STATE_COST);

    let end: End = loop {
        tokio::select! {
            biased;
            reason = interrupt.as_mut() => break Err(reason.into()),
            () = sleep_until(timer) => {
                if !conn.hello_seen {
                    break Ok((ReconnectReason::HelloTimeout, true));
                }
                let now = Instant::now();
                // Not reading while events wait for the consumer: no ACK could
                // have been observed, so that is not the server's fault.
                if conn.ack_pending && outbox.is_empty() {
                    break Ok((ReconnectReason::HeartbeatTimeout, true));
                }
                budget.record(now);
                conn.ack_pending = true;
                timer = now + conn.interval;
                let seq = session.as_ref().and_then(|s| s.seq);
                if !send(&mut socket, &wire::heartbeat(seq)).await {
                    break Ok((ReconnectReason::Network, true));
                }
            }
            delivered = outbox.deliver_one(), if !outbox.is_empty() => {
                if delivered.is_err() {
                    break Err(Terminal::Shutdown);
                }
            }
            changed = voice.changed(), if voice_watching => {
                match changed {
                    Ok(()) => voice_due = Some(Instant::now()),
                    Err(_) => voice_watching = false,
                }
            }
            () = sleep_until(voice_due.unwrap_or(timer)), if voice_live && voice_due.is_some() => {
                // The newest wanted state wins; earlier ones were never sent.
                let wanted = *voice.borrow_and_update();
                voice_due = None;
                match flush_voice(&mut socket, wanted, voice_sent, &mut budget, conn.interval).await {
                    VoiceFlush::Done => {}
                    VoiceFlush::RetryAt(at) => voice_due = Some(at),
                    VoiceFlush::Network => break Ok((ReconnectReason::Network, true)),
                }
            }
            command = stream.recv(), if stream_live && stream_watching && stream_pending.is_none() => {
                match command {
                    Some(command) => {
                        *stream_pending = Some(command);
                        stream_due = Some(Instant::now());
                    }
                    None => stream_watching = false,
                }
            }
            () = sleep_until(stream_due.unwrap_or(timer)), if stream_live && stream_due.is_some() => {
                let result = flush_stream(
                    &mut socket,
                    stream_pending.as_ref().expect("due stream command is present"),
                    &mut budget,
                    conn.interval,
                ).await;
                match result {
                    StreamFlush::Done => {
                        *stream_pending = None;
                        stream_due = None;
                    }
                    StreamFlush::RetryAt(at) => stream_due = Some(at),
                    StreamFlush::Network => break Ok((ReconnectReason::Network, true)),
                    StreamFlush::Outbound => break Err(Terminal::Stop(StopReason::StreamSignalTooLarge)),
                }
            }
            changed = subscriptions.changed(), if watching => {
                match changed {
                    Ok(()) => subscriber.changed(Instant::now()),
                    Err(_) => watching = false,
                }
            }
            () = sleep_until(subscriber.deadline().unwrap_or(timer)), if subscriber.deadline().is_some() => {
                // Take the latest wanted state; the guard must not live across
                // the send.
                let target = subscriptions.borrow_and_update().clone();
                match flush_subscriptions(
                    &mut socket,
                    subscriber,
                    &target,
                    &mut budget,
                    conn.interval,
                    timer,
                ).await {
                    Ok(()) => {}
                    Err(SubscriptionSendError::Network) => {
                        break Ok((ReconnectReason::Network, true));
                    }
                    Err(SubscriptionSendError::Outbound) => {
                        break Err(Terminal::Stop(StopReason::SubscriptionTooLarge));
                    }
                }
            }
            received = socket.next(), if outbox.len() < OUTBOX_LIMIT => {
                let message = match received {
                    None => break Ok((ReconnectReason::Closed(None), true)),
                    Some(Err(error)) => break error_end(&error),
                    Some(Ok(message)) => message,
                };
                let complete = match message {
                    Message::Binary(bytes) => match inflater.push_compressed(&bytes) {
                        Ok(complete) => complete,
                        Err(error) => break inflate_end(error),
                    },
                    Message::Text(text) => match inflater.set_plain(text.as_bytes()) {
                        Ok(()) => true,
                        Err(error) => break inflate_end(error),
                    },
                    Message::Close(frame) => {
                        break close_end(frame.map(|f| u16::from(f.code)), &**transport);
                    }
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => false,
                };
                if !complete {
                    continue;
                }
                let action = on_message(
                    &mut conn,
                    session,
                    outbox,
                    identify_rejections,
                    inflater.message(),
                    Instant::now(),
                );
                inflater.finish_message();
                match action {
                    Action::Nothing => {}
                    Action::Ready => {
                        let now = Instant::now();
                        subscriber.start(now);
                        // A new session holds no voice state of ours.
                        *voice_sent = None;
                        voice_live = true;
                        stream_live = true;
                        voice_due = Some(now);
                        stream_due = stream_pending.as_ref().map(|_| now);
                    }
                    Action::Resumed => {
                        let now = Instant::now();
                        subscriber.resume(now);
                        // The resumed session kept its accepted state.
                        voice_live = true;
                        stream_live = true;
                        voice_due = Some(now);
                        stream_due = stream_pending.as_ref().map(|_| now);
                    }
                    Action::Hello(interval) => {
                        conn.interval = interval;
                        timer = Instant::now() + interval.mul_f64(jitter.unit());
                        let resuming = session.as_ref().and_then(|s| s.seq.map(|seq| (s, seq)));
                        let (payload, sent, state) = match resuming {
                            Some((session, seq)) => (
                                wire::resume(token.expose_secret(), &session.id, seq),
                                Sent::Resume,
                                ConnectionState::Resuming,
                            ),
                            None => (
                                wire::identify(token.expose_secret(), properties),
                                Sent::Identify,
                                ConnectionState::Identifying,
                            ),
                        };
                        // A token that cannot fit an Identify is not a token.
                        let Ok(payload) = payload else {
                            break Err(Terminal::AuthenticationRequired);
                        };
                        let payload = Zeroizing::new(payload);
                        outbox.push(GatewayEvent::State(state), STATE_COST);
                        conn.sent = sent;
                        budget.record(Instant::now());
                        if !send(&mut socket, &payload).await {
                            break Ok((ReconnectReason::Network, true));
                        }
                    }
                    Action::HeartbeatRequested => {
                        let now = Instant::now();
                        // A server that asks for heartbeats in a flood must not
                        // push this connection over the send ceiling.
                        if budget.has_capacity(now) {
                            budget.record(now);
                            let seq = session.as_ref().and_then(|s| s.seq);
                            if !send(&mut socket, &wire::heartbeat(seq)).await {
                                break Ok((ReconnectReason::Network, true));
                            }
                        }
                    }
                    Action::Reconnect(reason, resume) => break Ok((reason, resume)),
                    Action::Terminal(terminal) => break Err(terminal),
                }
            }
        }
    };

    if matches!(
        end,
        Err(Terminal::Shutdown | Terminal::AuthenticationRequired)
    ) {
        // Closing with 1000 ends the session, so the account goes offline. This
        // also covers logout, where the account's REST work is stopped just
        // before the handle is dropped and either may be observed first. After
        // a server-sent close (4004) this fails at once and harmlessly.
        let goodbye = Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        }));
        let _ = timeout(CLOSE_TIMEOUT, socket.send(goodbye)).await;
    }
    let healthy = conn
        .ready_since
        .is_some_and(|since| since.elapsed() >= STABLE_AFTER);
    end.map(|(reason, resume)| Ended {
        reason,
        resume,
        healthy,
    })
}

fn error_end(error: &WsError) -> End {
    match error {
        WsError::Capacity(_) => Err(Terminal::Stop(StopReason::EventTooLarge)),
        WsError::ConnectionClosed | WsError::AlreadyClosed => {
            Ok((ReconnectReason::Closed(None), true))
        }
        _ => Ok((ReconnectReason::Network, true)),
    }
}

fn inflate_end(error: InflateError) -> End {
    match error {
        InflateError::EventTooLarge => Err(Terminal::Stop(StopReason::EventTooLarge)),
        InflateError::Corrupt => Ok((ReconnectReason::Protocol, true)),
    }
}

/// What a close code means for the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CloseAction {
    /// Reconnect and Resume.
    Resume,
    /// The session is gone: reconnect and Identify.
    Identify,
    AuthenticationRequired,
    /// Reconnecting cannot help.
    Fatal(StopReason),
}

pub(crate) fn close_action(code: Option<u16>) -> CloseAction {
    match code {
        Some(4004) => CloseAction::AuthenticationRequired,
        // Not authenticated / invalid seq / timed out; also a clean close,
        // which ends the session.
        Some(1000 | 1001 | 4003 | 4007 | 4009) => CloseAction::Identify,
        // Our payload or version was rejected, or sharding/intents (which a
        // user client never sends): repeating cannot succeed.
        Some(code @ (4001 | 4002 | 4010..=4014 | 4016)) => {
            CloseAction::Fatal(StopReason::Rejected(code))
        }
        Some(4015) => CloseAction::Fatal(StopReason::TooManySessions),
        // 4000 unknown, 4005, 4008 rate limited, abnormal closure, anything new.
        _ => CloseAction::Resume,
    }
}

fn close_end<T: Transport>(code: Option<u16>, transport: &T) -> End {
    match close_action(code) {
        CloseAction::Resume => Ok((ReconnectReason::Closed(code), true)),
        CloseAction::Identify => Ok((ReconnectReason::Closed(code), false)),
        CloseAction::AuthenticationRequired => {
            // Tear down every authenticated worker of this account.
            transport.stop_authenticated_work();
            Err(Terminal::AuthenticationRequired)
        }
        CloseAction::Fatal(reason) => Err(Terminal::Stop(reason)),
    }
}

/// Decodes one complete event and updates the session and outbox. Pure with
/// respect to I/O: anything that needs the socket is returned as an [`Action`].
fn on_message(
    conn: &mut Conn,
    session: &mut Option<Session>,
    outbox: &mut Outbox,
    identify_rejections: &mut u32,
    message: &[u8],
    now: Instant,
) -> Action {
    let Ok(envelope) = serde_json::from_slice::<Envelope<'_>>(message) else {
        return Action::Reconnect(ReconnectReason::Protocol, true);
    };
    match envelope.op {
        op::DISPATCH => on_dispatch(
            conn,
            session,
            outbox,
            identify_rejections,
            &envelope,
            message.len(),
            now,
        ),
        op::HEARTBEAT => Action::HeartbeatRequested,
        op::HEARTBEAT_ACK => {
            conn.ack_pending = false;
            Action::Nothing
        }
        op::RECONNECT => Action::Reconnect(ReconnectReason::ServerRequested, true),
        op::INVALID_SESSION => {
            let resumable = envelope.d.is_some_and(|d| d.get().trim() == "true");
            if resumable {
                return Action::Reconnect(ReconnectReason::InvalidSession { resumable }, true);
            }
            if conn.sent == Sent::Identify {
                *identify_rejections += 1;
                if *identify_rejections >= MAX_IDENTIFY_REJECTIONS {
                    return Action::Terminal(Terminal::Stop(StopReason::IdentifyRejected));
                }
            }
            Action::Reconnect(ReconnectReason::InvalidSession { resumable }, false)
        }
        op::HELLO => {
            if conn.hello_seen {
                return Action::Nothing;
            }
            let hello = envelope
                .d
                .and_then(|d| serde_json::from_str::<Hello>(d.get()).ok());
            let Some(hello) = hello else {
                return Action::Reconnect(ReconnectReason::Protocol, true);
            };
            conn.hello_seen = true;
            Action::Hello(
                Duration::from_millis(hello.heartbeat_interval).clamp(MIN_HEARTBEAT, MAX_HEARTBEAT),
            )
        }
        _ => Action::Nothing,
    }
}

fn on_dispatch(
    conn: &mut Conn,
    session: &mut Option<Session>,
    outbox: &mut Outbox,
    identify_rejections: &mut u32,
    envelope: &Envelope<'_>,
    frame_bytes: usize,
    now: Instant,
) -> Action {
    let (Some(name), Some(data)) = (envelope.t.as_deref(), envelope.d) else {
        return Action::Nothing;
    };
    let sequence = envelope.s;
    // Estimated consumer-side memory: typed entities are denser than JSON text
    // but carry allocation overhead.
    let cost = frame_bytes.saturating_mul(2).saturating_add(256);

    // Replay overlap: a sequence already delivered must not be applied twice.
    if let (Some(sequence), Some(last)) = (sequence, session.as_ref().and_then(|s| s.seq))
        && sequence <= last
        && name != "READY"
    {
        return Action::Nothing;
    }

    match decode_dispatch(name, data.get()) {
        Decoded::Ready(decoded) => {
            let decoded = *decoded;
            *session = Some(Session {
                id: decoded.ready.session_id.as_str().to_owned(),
                // An unusable resume address falls back to discovery.
                resume_url: GatewayUrl::parse(&decoded.resume_gateway_url),
                seq: sequence,
            });
            *identify_rejections = 0;
            conn.ready_since = Some(now);
            outbox.push(
                GatewayEvent::Dispatch {
                    sequence: sequence.unwrap_or(0),
                    event: Dispatch::Ready(Box::new(decoded.ready)),
                },
                cost,
            );
            outbox.push(GatewayEvent::State(ConnectionState::Ready), STATE_COST);
            match decoded.required_action {
                Some(action) => {
                    Action::Terminal(Terminal::Stop(StopReason::ActionRequired(action)))
                }
                None => Action::Ready,
            }
        }
        Decoded::Event(event) => {
            // Before READY there is no session to attach events to.
            let Some(active) = session.as_mut() else {
                return Action::Nothing;
            };
            if sequence.is_some() {
                active.seq = sequence;
            }
            let resumed = matches!(event, Dispatch::Resumed);
            outbox.push(
                GatewayEvent::Dispatch {
                    sequence: sequence.unwrap_or(0),
                    event,
                },
                cost,
            );
            if resumed {
                conn.ready_since = Some(now);
                outbox.push(GatewayEvent::State(ConnectionState::Ready), STATE_COST);
            }
            if resumed {
                Action::Resumed
            } else {
                Action::Nothing
            }
        }
        Decoded::Unhandled => {
            if let (Some(active), Some(_)) = (session.as_mut(), sequence) {
                active.seq = sequence;
            }
            Action::Nothing
        }
        Decoded::Malformed => {
            if is_ready_family(name) {
                // Never continue with silently incomplete account state.
                return Action::Terminal(Terminal::Stop(StopReason::MalformedReady));
            }
            if let (Some(active), Some(_)) = (session.as_mut(), sequence) {
                active.seq = sequence;
            }
            Action::Nothing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(f64);

    impl JitterSource for Fixed {
        fn unit(&mut self) -> f64 {
            self.0
        }
    }

    #[test]
    fn subscription_reserve_accounts_for_the_hello_heartbeat_interval() {
        assert_eq!(control_reserve(MIN_HEARTBEAT), CONTROL_RESERVE + 60);
        assert_eq!(
            control_reserve(Duration::from_millis(2_500)),
            CONTROL_RESERVE + 24
        );
        assert_eq!(
            control_reserve(Duration::from_secs(40)),
            CONTROL_RESERVE + 2
        );
        assert_eq!(control_reserve(MAX_HEARTBEAT), CONTROL_RESERVE + 1);
    }

    #[test]
    fn close_codes_map_to_the_documented_semantics() {
        use CloseAction::*;
        assert_eq!(close_action(Some(4004)), AuthenticationRequired);
        for code in [4003, 4007, 4009, 1000, 1001] {
            assert_eq!(close_action(Some(code)), Identify, "{code}");
        }
        for code in [4000, 4005, 4008, 1006, 4999, 1011] {
            assert_eq!(close_action(Some(code)), Resume, "{code}");
        }
        assert_eq!(close_action(None), Resume);
        for code in [4001, 4002, 4010, 4011, 4012, 4013, 4014] {
            assert_eq!(close_action(Some(code)), Fatal(StopReason::Rejected(code)));
        }
        assert_eq!(close_action(Some(4015)), Fatal(StopReason::TooManySessions));
    }

    #[test]
    fn reconnect_delays_follow_the_reason_and_the_failure_streak() {
        let mut jitter = Fixed(1.0);
        // A healthy connection that dropped resumes immediately.
        for reason in [
            ReconnectReason::ServerRequested,
            ReconnectReason::HeartbeatTimeout,
            ReconnectReason::Closed(Some(4000)),
            ReconnectReason::Network,
            ReconnectReason::InvalidSession { resumable: true },
        ] {
            assert_eq!(reconnect_delay(reason, 0, &mut jitter), Duration::ZERO);
            assert_eq!(
                reconnect_delay(reason, 3, &mut jitter),
                Duration::from_secs(4)
            );
        }
        // Non-resumable Invalid Session always waits 1 to 5 s before Identify.
        let invalid = ReconnectReason::InvalidSession { resumable: false };
        assert_eq!(
            reconnect_delay(invalid, 0, &mut Fixed(0.0)),
            Duration::from_secs(1)
        );
        assert_eq!(
            reconnect_delay(invalid, 9, &mut Fixed(0.0)),
            Duration::from_secs(1)
        );
    }
}
