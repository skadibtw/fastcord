//! UI-side view of the account's Gateway connection. The protocol, session,
//! heartbeats, and reconnects live in `fastcord_discord::gateway::Gateway`; the
//! account's normalized state lives in `fastcord_discord::state::Store`, which
//! the worker below owns and is the only writer of (the reducer). This module
//! turns the ordered events into that state plus the few status changes the
//! screen shows, and is dropped, with the connection and the state it owns,
//! when the account screen is left (which closes the Gateway session cleanly).

mod voice_audio;

use voice_audio::VoiceAudioState;

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastcord_discord::gateway::{
    ConnectionState, Dispatch, Gateway, GatewayEvent, ReconnectReason, StopReason,
};
use fastcord_discord::state::Store;
use fastcord_discord::state::navigation::{Navigation, NavigationSnapshot};
use fastcord_media::{Correlation, JoinCorrelator};
use fastcord_model::{Snowflake, VoiceStateRequest};
use tokio::sync::{mpsc, watch};

use crate::composer::Composer;
use crate::history::{History, Intent};
use crate::outbox::{OpId, Reply, Slots};
use crate::timeline;
use crate::variable_list::{Measurement, Viewport};
use crate::virtual_list::Window;
use fastcord_discord::{RestClient, UserToken};
use iced::futures::{Stream, stream};
use iced::task::Handle;

/// What READY said about the account, for display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub guilds: usize,
    pub unavailable: usize,
    pub direct_messages: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayStatus {
    Connecting {
        attempt: u32,
    },
    SigningIn,
    Resuming,
    Ready(Counts),
    Reconnecting {
        attempt: u32,
        delay: Duration,
        reason: ReconnectReason,
    },
    /// Terminal: the token is no longer accepted.
    AuthenticationRequired,
    /// Terminal: reconnecting cannot help.
    Stopped(StopReason),
}

impl GatewayStatus {
    pub fn describe(&self) -> String {
        match self {
            Self::Connecting { attempt: 0 } => "Connecting to Discord…".to_owned(),
            Self::Connecting { attempt } => {
                format!("Connecting to Discord… (attempt {})", attempt + 1)
            }
            Self::SigningIn => "Connected; signing in…".to_owned(),
            Self::Resuming => "Connected; resuming your session…".to_owned(),
            Self::Ready(counts) => {
                let mut text = format!(
                    "Connected to Discord: {} servers, {} direct messages.",
                    counts.guilds, counts.direct_messages
                );
                if counts.unavailable > 0 {
                    text.push_str(&format!(" {} servers are unavailable.", counts.unavailable));
                }
                text
            }
            Self::Reconnecting {
                attempt,
                delay,
                reason,
            } => format!(
                "Connection lost ({}). Reconnecting in {} (attempt {attempt}).",
                describe_reason(*reason),
                describe_delay(*delay)
            ),
            Self::AuthenticationRequired => {
                "Discord no longer accepts this login. Log in again.".to_owned()
            }
            Self::Stopped(reason) => format!("Disconnected: {reason}"),
        }
    }
}

fn describe_reason(reason: ReconnectReason) -> &'static str {
    match reason {
        ReconnectReason::ServerRequested => "Discord asked to reconnect",
        ReconnectReason::HeartbeatTimeout => "Discord stopped responding",
        ReconnectReason::HelloTimeout => "Discord did not greet the connection",
        ReconnectReason::Protocol => "unreadable data",
        ReconnectReason::InvalidSession { .. } => "session expired",
        ReconnectReason::Closed(_) => "closed by Discord",
        ReconnectReason::Network => "network error",
        ReconnectReason::ConnectFailed => "could not connect",
        ReconnectReason::DiscoveryFailed => "could not reach Discord",
    }
}

/// "now" / "2 seconds": static text, so no per-second redraw timer is needed.
fn describe_delay(delay: Duration) -> String {
    match delay.as_secs_f64() {
        seconds if seconds < 0.5 => "a moment".to_owned(),
        seconds if seconds < 1.5 => "1 second".to_owned(),
        seconds => format!("{} seconds", seconds.round() as u64),
    }
}

/// Folds ordered Gateway events into the single-writer reducer. Navigation
/// commands are also validated here, never against a stale UI snapshot.
#[derive(Default)]
struct Tracker {
    store: Store,
    history: History,
    navigation: Navigation,
    navigation_dirty: bool,
}

impl Tracker {
    /// What the store holds, for the connected message.
    fn counts(&self) -> Counts {
        Counts {
            guilds: self.store.guild_ids().len(),
            unavailable: self.store.unavailable_guilds().count(),
            direct_messages: self.store.private_channels().count(),
        }
    }

    fn apply(&mut self, event: GatewayEvent) -> Option<GatewayStatus> {
        match event {
            GatewayEvent::State(state) => Some(match state {
                ConnectionState::Connecting { attempt } => GatewayStatus::Connecting { attempt },
                ConnectionState::AwaitHello | ConnectionState::Identifying => {
                    GatewayStatus::SigningIn
                }
                ConnectionState::Resuming => GatewayStatus::Resuming,
                ConnectionState::Ready => GatewayStatus::Ready(self.counts()),
                ConnectionState::Reconnecting {
                    attempt,
                    delay,
                    reason,
                } => GatewayStatus::Reconnecting {
                    attempt,
                    delay,
                    reason,
                },
                ConnectionState::AuthenticationRequired => GatewayStatus::AuthenticationRequired,
                ConnectionState::Stopped(reason) => GatewayStatus::Stopped(reason),
            }),
            dispatch @ GatewayEvent::Dispatch { .. } => {
                if let GatewayEvent::Dispatch { event, .. } = &dispatch {
                    self.history.apply(event);
                }
                let relevant = !matches!(
                    &dispatch,
                    GatewayEvent::Dispatch {
                        event: Dispatch::MessageCreate(_)
                            | Dispatch::MessageUpdate(_)
                            | Dispatch::MessageDelete(_)
                            | Dispatch::MessageDeleteBulk(_),
                        ..
                    }
                );
                let changes = self.store.apply(dispatch);
                self.navigation_dirty =
                    (relevant && !changes.is_empty()) || self.store.limit_exceeded();
                if self.navigation_dirty {
                    self.navigation.reconcile(&self.store);
                }
                self.store
                    .limit_exceeded()
                    .then_some(GatewayStatus::Stopped(StopReason::StateTooLarge))
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum SelectionIntent {
    Guild(Snowflake),
    Channel(Snowflake, Snowflake),
}

#[derive(Clone, Debug, Default)]
struct Request {
    revision: u64,
    selection: Option<SelectionIntent>,
    guilds: Window,
    channels: Window,
    timeline: crate::history::Request,
}

#[derive(Default)]
struct Latest {
    snapshot: Arc<NavigationSnapshot>,
    timeline: Arc<timeline::Snapshot>,
    status: Option<GatewayStatus>,
    notified: bool,
}

/// What one consumed notification carries to the UI.
pub struct Consumed {
    pub navigation: Arc<NavigationSnapshot>,
    pub timeline: Arc<timeline::Snapshot>,
    pub status: Option<GatewayStatus>,
}

/// An explicit, ordered user action on the outbox or on one of the user's
/// messages. Unlike the coalesced [`Request`], none of these may be dropped or
/// merged, so they travel through a small bounded queue of their own.
pub enum Command {
    Send {
        channel: Snowflake,
        content: String,
        everyone: bool,
        reply: Option<Reply>,
    },
    Retry(OpId),
    Discard(OpId),
    /// Save new text for one of the user's messages.
    Edit {
        channel: Snowflake,
        message: Snowflake,
        content: String,
    },
    /// Delete one of the user's messages (the user confirmed it).
    Delete {
        channel: Snowflake,
        message: Snowflake,
    },
    RetryChange {
        channel: Snowflake,
        message: Snowflake,
    },
    DismissChange {
        channel: Snowflake,
        message: Snowflake,
    },
}

// Message text never reaches a log.
impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Send {
                channel, everyone, ..
            } => f
                .debug_struct("Send")
                .field("channel", channel)
                .field("everyone", everyone)
                .finish_non_exhaustive(),
            Self::Retry(id) => f.debug_tuple("Retry").field(id).finish(),
            Self::Discard(id) => f.debug_tuple("Discard").field(id).finish(),
            Self::Edit {
                channel, message, ..
            } => f
                .debug_struct("Edit")
                .field("channel", channel)
                .field("message", message)
                .finish_non_exhaustive(),
            Self::Delete { channel, message } => f
                .debug_struct("Delete")
                .field("channel", channel)
                .field("message", message)
                .finish(),
            Self::RetryChange { channel, message } => f
                .debug_struct("RetryChange")
                .field("channel", channel)
                .field("message", message)
                .finish(),
            Self::DismissChange { channel, message } => f
                .debug_struct("DismissChange")
                .field("channel", channel)
                .field("message", message)
                .finish(),
        }
    }
}

/// Commands waiting for the worker; a full queue refuses the action instead of
/// buffering without bound (the composer then keeps the user's text). Sends
/// are further limited by the outbox [`Slots`].
const COMMAND_QUEUE: usize = 32;

/// One coalesced request and one bounded presentation window per account.
/// Notifications carry no rows; an unconsumed notification prevents more from
/// entering iced's queue while the slot continues replacing stale snapshots.
#[derive(Clone)]
pub struct NavigationBridge {
    requests: watch::Sender<Request>,
    voice_requests: watch::Sender<Option<VoiceStateRequest>>,
    voice_audio: watch::Sender<VoiceAudioState>,
    latest: Arc<Mutex<Latest>>,
    commands: mpsc::Sender<Command>,
    command_receiver: Arc<Mutex<Option<mpsc::Receiver<Command>>>>,
    outbox_slots: Slots,
}

impl NavigationBridge {
    pub fn new() -> Self {
        let (requests, _) = watch::channel(Request::default());
        let (voice_requests, _) = watch::channel(None);
        let (voice_audio, _) = watch::channel(VoiceAudioState::Idle);
        let (commands, receiver) = mpsc::channel(COMMAND_QUEUE);
        Self {
            requests,
            voice_requests,
            voice_audio,
            latest: Arc::default(),
            commands,
            command_receiver: Arc::new(Mutex::new(Some(receiver))),
            outbox_slots: Slots::default(),
        }
    }

    /// The audio side of the current call, for the voice controls (milestone 21).
    #[expect(
        dead_code,
        reason = "shown by the voice controls added in milestone 21"
    )]
    pub fn voice_audio_state(&self) -> VoiceAudioState {
        *self.voice_audio.borrow()
    }

    /// Requests a voice join, move, or leave. This is the application path
    /// used by explicit voice controls (milestone 21); no audio connection
    /// starts beforehand.
    #[expect(
        dead_code,
        reason = "called by the voice controls added in milestone 21"
    )]
    pub fn request_voice_state(&self, request: VoiceStateRequest) {
        self.voice_requests.send_replace(Some(request));
    }

    /// The worker's end of the command queue; there is only one.
    pub(crate) fn take_commands(&self) -> Option<mpsc::Receiver<Command>> {
        self.command_receiver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Queues a message for sending. Returns whether it was accepted; on
    /// `false` (the outbox is full or the worker is gone) nothing was queued
    /// and the caller must keep the text.
    pub fn send_message(
        &self,
        channel: Snowflake,
        content: String,
        everyone: bool,
        reply: Option<Reply>,
    ) -> bool {
        if !self.outbox_slots.reserve() {
            return false;
        }
        let queued = self
            .commands
            .try_send(Command::Send {
                channel,
                content,
                everyone,
                reply,
            })
            .is_ok();
        if !queued {
            self.outbox_slots.release();
        }
        queued
    }

    pub fn retry_send(&self, id: OpId) -> bool {
        self.commands.try_send(Command::Retry(id)).is_ok()
    }

    pub fn discard_send(&self, id: OpId) -> bool {
        self.commands.try_send(Command::Discard(id)).is_ok()
    }

    /// Queues new text for one of the user's messages. Returns whether it was
    /// accepted; on `false` nothing was queued and the caller keeps the text.
    pub fn edit_message(&self, channel: Snowflake, message: Snowflake, content: String) -> bool {
        self.commands
            .try_send(Command::Edit {
                channel,
                message,
                content,
            })
            .is_ok()
    }

    /// Queues the confirmed deletion of one of the user's messages.
    pub fn delete_message(&self, channel: Snowflake, message: Snowflake) -> bool {
        self.commands
            .try_send(Command::Delete { channel, message })
            .is_ok()
    }

    pub fn retry_change(&self, channel: Snowflake, message: Snowflake) -> bool {
        self.commands
            .try_send(Command::RetryChange { channel, message })
            .is_ok()
    }

    pub fn dismiss_change(&self, channel: Snowflake, message: Snowflake) -> bool {
        self.commands
            .try_send(Command::DismissChange { channel, message })
            .is_ok()
    }

    fn publish(
        &self,
        snapshot: NavigationSnapshot,
        timeline: timeline::Snapshot,
        status: Option<GatewayStatus>,
    ) -> bool {
        let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        let status_changed = status
            .as_ref()
            .is_some_and(|status| latest.status.as_ref() != Some(status));
        let snapshot_changed = *latest.snapshot != snapshot;
        let timeline_changed = *latest.timeline != timeline;
        if !status_changed && !snapshot_changed && !timeline_changed {
            return false;
        }
        if snapshot_changed {
            latest.snapshot = Arc::new(snapshot);
        }
        if timeline_changed {
            latest.timeline = Arc::new(timeline);
        }
        if let Some(status) = status {
            latest.status = Some(status);
        }
        if latest.notified {
            false
        } else {
            latest.notified = true;
            true
        }
    }

    pub fn consume(&self) -> Consumed {
        let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        latest.notified = false;
        Consumed {
            navigation: Arc::clone(&latest.snapshot),
            timeline: Arc::clone(&latest.timeline),
            status: latest.status.clone(),
        }
    }

    /// Atomically takes the request, leaving the one-shot parts (the newest
    /// viewport and measured heights) consumed so they cannot be replayed
    /// over a later scroll correction.
    fn take_request(&self) -> Request {
        let mut taken = Request::default();
        self.requests.send_if_modified(|request| {
            taken = request.clone();
            request.timeline.viewport = None;
            request.timeline.measurements.clear();
            false
        });
        taken
    }

    pub fn select_guild(&self, id: Snowflake) {
        self.requests.send_modify(|request| {
            request.revision = request.revision.wrapping_add(1);
            request.selection = Some(SelectionIntent::Guild(id));
            request.channels = Window::default();
        });
    }

    pub fn select_channel(&self, guild: Snowflake, channel: Snowflake) {
        self.requests.send_modify(|request| {
            request.revision = request.revision.wrapping_add(1);
            request.selection = Some(SelectionIntent::Channel(guild, channel));
        });
    }

    pub fn viewport(&self, guilds: bool, window: Window) {
        self.requests.send_if_modified(|request| {
            let old = if guilds {
                &mut request.guilds
            } else {
                &mut request.channels
            };
            if *old == window {
                return false;
            }
            *old = window;
            true
        });
    }

    pub fn timeline_viewport(&self, channel: Snowflake, viewport: Viewport) {
        self.requests.send_modify(|request| {
            request.timeline.viewport = Some((channel, viewport));
        });
    }

    pub fn timeline_measured(&self, channel: Snowflake, measurement: Measurement) {
        self.requests.send_modify(|request| {
            request.timeline.measure(channel, measurement);
        });
    }

    pub fn timeline_intent(&self, intent: Intent) {
        self.requests.send_modify(|request| {
            request.timeline.revision = request.timeline.revision.wrapping_add(1);
            request.timeline.intent = Some(intent);
        });
    }
}

struct Worker {
    start: Option<(RestClient, Arc<UserToken>, String)>,
    rest: RestClient,
    gateway: Option<Gateway>,
    tracker: Tracker,
    bridge: NavigationBridge,
    requests: watch::Receiver<Request>,
    voice_requests: watch::Receiver<Option<VoiceStateRequest>>,
    commands: mpsc::Receiver<Command>,
    revision: u64,
    voice_correlator: Option<JoinCorrelator>,
    voice_task: Option<tokio::task::JoinHandle<()>>,
    voice_cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Worker {
    /// Applies one explicit outbox or message action.
    fn command(&mut self, command: Command) {
        let history = &mut self.tracker.history;
        match command {
            Command::Send {
                channel,
                content,
                everyone,
                reply,
            } => {
                history.send(&self.rest, channel, &content, everyone, reply);
            }
            Command::Retry(id) => history.retry_send(&self.rest, id),
            Command::Discard(id) => history.discard_send(id),
            Command::Edit {
                channel,
                message,
                content,
            } => history.edit_message(&self.rest, channel, message, &content),
            Command::Delete { channel, message } => {
                history.delete_message(&self.rest, channel, message);
            }
            Command::RetryChange { channel, message } => {
                history.retry_change(&self.rest, channel, message);
            }
            Command::DismissChange { channel, message } => {
                history.dismiss_change(channel, message);
            }
        }
    }

    fn request_voice_state(&mut self, gateway: &Gateway) -> bool {
        let Some(request) = *self.voice_requests.borrow_and_update() else {
            return false;
        };
        if let Some(channel) = request.channel_id {
            if let Some(correlator) = self.voice_correlator.as_mut() {
                correlator.join(request.guild_id, channel);
            }
        } else {
            if let Some(correlator) = self.voice_correlator.as_mut() {
                correlator.leave();
            }
        }
        gateway.voice_state().request(request);
        true
    }

    fn voice_dispatch(&mut self, event: &GatewayEvent) -> Option<Correlation> {
        let GatewayEvent::Dispatch { event, .. } = event else {
            return None;
        };
        match event {
            Dispatch::Ready(ready) => {
                self.voice_correlator = Some(JoinCorrelator::new(ready.user.id));
                if let Some(request) = *self.voice_requests.borrow()
                    && let Some(channel) = request.channel_id
                {
                    self.voice_correlator
                        .as_mut()
                        .unwrap()
                        .join(request.guild_id, channel);
                }
            }
            Dispatch::VoiceStateUpdate(update) => {
                if let Some(correlator) = self.voice_correlator.as_mut() {
                    let correlation = correlator.voice_state(&update.state);
                    return Some(correlation);
                }
            }
            Dispatch::VoiceServerUpdate(update) => {
                if let Some(correlator) = self.voice_correlator.as_mut() {
                    let correlation = correlator.voice_server(update);
                    return Some(correlation);
                }
            }
            _ => {}
        }
        None
    }

    /// Ends the current call's audio: asks it to leave gracefully (speech ended, voice session
    /// closed, devices released) and waits for that, aborting only if it takes too long.
    async fn stop_voice(&mut self) {
        if let Some(tx) = self.voice_cancel_tx.take() {
            let _ = tx.send(());
        }
        if let Some(mut task) = self.voice_task.take()
            && tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .is_err()
        {
            task.abort();
            let _ = task.await;
            self.bridge.voice_audio.send_replace(VoiceAudioState::Idle);
        }
    }

    async fn handle_correlation(&mut self, correlation: Correlation) {
        match correlation {
            Correlation::Connect(credentials) => {
                // The previous call must have released its audio devices before this one
                // opens them: `stop_voice` returns only after that task has ended.
                self.stop_voice().await;
                self.bridge.voice_audio.send_replace(VoiceAudioState::Idle);
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.voice_cancel_tx = Some(tx);
                self.voice_task = Some(tokio::spawn(voice_audio::run(
                    credentials,
                    rx,
                    self.bridge.voice_audio.clone(),
                )));
            }
            Correlation::Ended | Correlation::Reallocating => self.stop_voice().await,
            Correlation::Ignored | Correlation::Waiting => {}
        }
    }

    fn refresh(
        &mut self,
        gateway: &Gateway,
        request: Request,
        status: Option<GatewayStatus>,
    ) -> bool {
        if request.revision != self.revision {
            match request.selection {
                Some(SelectionIntent::Guild(guild)) => {
                    self.tracker
                        .navigation
                        .select_guild(&self.tracker.store, guild);
                }
                Some(SelectionIntent::Channel(guild, channel))
                    if self.tracker.navigation.selected_guild() == Some(guild) =>
                {
                    self.tracker
                        .navigation
                        .select_channel(&self.tracker.store, channel);
                }
                _ => {}
            }
            self.revision = request.revision;
        }
        let target = self
            .tracker
            .navigation
            .subscription_target(&self.tracker.store);
        self.tracker.store.focus(&target);
        gateway.subscriptions().update(|current| *current = target);
        let navigation = self.tracker.navigation.snapshot(
            &self.tracker.store,
            request.guilds.start,
            request.guilds.count,
            request.channels.start,
            request.channels.count,
        );
        // History follows the validated selection: permission loss closes it,
        // and nothing is fetched until a readable channel is selected.
        self.tracker
            .history
            .refresh(&self.rest, &navigation, &request.timeline);
        let timeline = self.tracker.history.snapshot();
        self.bridge.publish(navigation, timeline, status)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Abort the task if it exists; proper await happens in status_stream before Drop
        if let Some(task) = self.voice_task.take() {
            task.abort();
        }
    }
}

/// Starts only when iced polls this account-owned stream. Dropping the panel
/// aborts it and releases the socket, store, presentation slot, and controls.
pub fn status_stream(
    rest: RestClient,
    token: Arc<UserToken>,
    locale: String,
    bridge: NavigationBridge,
) -> impl Stream<Item = ()> {
    let requests = bridge.requests.subscribe();
    let voice_requests = bridge.voice_requests.subscribe();
    let rest_client = rest.clone();
    // There is one worker per bridge; a second one would simply get no commands.
    let commands = bridge.take_commands().unwrap_or_else(|| mpsc::channel(1).1);
    let worker = Worker {
        start: Some((rest, token, locale)),
        rest: rest_client,
        gateway: None,
        tracker: Tracker {
            history: History::new(bridge.outbox_slots.clone()),
            ..Tracker::default()
        },
        bridge,
        requests,
        voice_requests,
        commands,
        revision: 0,
        voice_correlator: None,
        voice_task: None,
        voice_cancel_tx: None,
    };
    stream::unfold(worker, |mut worker| async move {
        let mut gateway = match worker.gateway.take() {
            Some(gateway) => gateway,
            None => {
                let (rest, token, locale) = worker.start.take()?;
                Gateway::start(rest, token, locale)
            }
        };
        loop {
            let (status, requested) = tokio::select! {
                event = gateway.next_event() => {
                    let event = event?;
                    let correlation = worker.voice_dispatch(&event);
                    let status = worker.tracker.apply(event);
                    if let Some(correlation) = correlation {
                        worker.handle_correlation(correlation).await;
                    }
                    (status, false)
                }
                changed = worker.requests.changed() => {
                    changed.ok()?;
                    (None, true)
                }
                changed = worker.voice_requests.changed() => {
                    changed.ok()?;
                    worker.stop_voice().await;
                    if worker.request_voice_state(&gateway) {
                        (None, true)
                    } else {
                        (None, false)
                    }
                }
                completed = worker.tracker.history.next_completed() => {
                    let rejected = worker.tracker.history.complete(&worker.rest, completed);
                    (rejected.then_some(GatewayStatus::AuthenticationRequired), false)
                }
                Some(command) = worker.commands.recv() => {
                    worker.command(command);
                    (None, false)
                }
            };
            if !requested
                && !worker.tracker.navigation_dirty
                && !worker.tracker.history.dirty()
                && status.is_none()
            {
                continue;
            }
            worker.tracker.navigation_dirty = false;
            let request = worker.bridge.take_request();
            let terminal = matches!(
                status,
                Some(GatewayStatus::Stopped(_)) | Some(GatewayStatus::AuthenticationRequired)
            );
            let notified = worker.refresh(&gateway, request, status);
            if terminal {
                worker.start = None;
                worker.stop_voice().await;
                return notified.then_some(((), worker));
            }
            if notified {
                worker.gateway = Some(gateway);
                return Some(((), worker));
            }
        }
    })
}

/// Messages from an earlier session (identified by `id`) are ignored.
pub struct GatewayPanel {
    pub id: u64,
    pub status: GatewayStatus,
    pub navigation: Arc<NavigationSnapshot>,
    pub timeline: Arc<timeline::Snapshot>,
    pub controls: NavigationBridge,
    /// The draft and composer state; local to the UI until the user sends.
    pub composer: Composer,
    /// The user's message (channel, message) whose deletion waits for confirmation.
    pub confirm_delete: Option<(Snowflake, Snowflake)>,
    _worker: Handle,
}

impl fmt::Debug for GatewayPanel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayPanel")
            .field("id", &self.id)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl GatewayPanel {
    pub fn new(id: u64, worker: Handle, controls: NavigationBridge) -> Self {
        Self {
            id,
            status: GatewayStatus::Connecting { attempt: 0 },
            navigation: Arc::default(),
            timeline: Arc::default(),
            controls,
            confirm_delete: None,
            composer: Composer::default(),
            _worker: worker.abort_on_drop(),
        }
    }

    /// Takes the worker's latest snapshots. Drafts and editing follow the open
    /// channel; a deletion waiting for confirmation does not survive leaving it.
    pub fn consume(&mut self) -> Option<GatewayStatus> {
        let consumed = self.controls.consume();
        self.navigation = consumed.navigation;
        self.timeline = consumed.timeline;
        self.composer.select(self.timeline.channel_id);
        if self
            .confirm_delete
            .is_some_and(|(channel, _)| self.timeline.channel_id != Some(channel))
        {
            self.confirm_delete = None;
        }
        consumed.status
    }

    /// What the timeline shows about rows the UI is working on.
    pub fn interaction(&self) -> timeline::Interaction {
        timeline::Interaction {
            editing: self.composer.editing(),
            replying: self.composer.replying(),
            confirming: self
                .confirm_delete
                .filter(|(channel, _)| self.timeline.channel_id == Some(*channel))
                .map(|(_, message)| message),
        }
    }

    /// Applies a timeline action for the open channel. Reply selects a loaded
    /// target locally; Edit moves text into the composer; Delete waits for
    /// confirmation before it is queued. Returns whether to focus the editor.
    pub fn message_action(&mut self, event: timeline::Event) -> bool {
        use timeline::Event;
        let open = self.timeline.channel_id;
        let queued = match event {
            Event::Edit {
                channel_id,
                message_id,
            } if open == Some(channel_id) => {
                self.confirm_delete = None;
                return self.composer.begin_edit(&self.timeline, message_id);
            }
            Event::Reply {
                channel_id,
                message_id,
            } if open == Some(channel_id) && self.timeline.can_send => {
                self.confirm_delete = None;
                return self.composer.begin_reply(&self.timeline, message_id);
            }
            Event::Delete {
                channel_id,
                message_id,
            } if open == Some(channel_id) => {
                self.confirm_delete = Some((channel_id, message_id));
                true
            }
            Event::ConfirmDelete {
                channel_id,
                message_id,
            } if self.confirm_delete == Some((channel_id, message_id)) => {
                self.confirm_delete = None;
                if self.composer.editing() == Some(message_id) {
                    self.composer.cancel_edit();
                }
                self.controls.delete_message(channel_id, message_id)
            }
            Event::CancelDelete => {
                self.confirm_delete = None;
                true
            }
            Event::RetryChange {
                channel_id,
                message_id,
            } => self.controls.retry_change(channel_id, message_id),
            Event::DismissChange {
                channel_id,
                message_id,
            } => self.controls.dismiss_change(channel_id, message_id),
            _ => true,
        };
        if !queued {
            self.composer.refused();
        }
        false
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;

    #[test]
    fn requests_coalesce_without_queueing_navigation_or_viewports() {
        let bridge = NavigationBridge::new();
        let requests = bridge.requests.subscribe();
        for id in 0..10_000 {
            bridge.select_guild(Snowflake(id));
            bridge.viewport(
                true,
                Window {
                    start: id as usize,
                    count: 39,
                },
            );
        }
        let request = requests.borrow().clone();
        assert_eq!(request.revision, 10_000);
        assert!(matches!(
            request.selection,
            Some(SelectionIntent::Guild(Snowflake(9_999)))
        ));
        assert_eq!(request.guilds.start, 9_999);
        assert_eq!(request.channels, Window::default());
    }

    #[test]
    fn at_most_one_notification_is_queued_and_consumption_gets_latest() {
        let bridge = NavigationBridge::new();
        let mut snapshot = NavigationSnapshot::default();
        snapshot.guilds.total = 1;
        let timeline = timeline::Snapshot::default;
        assert!(bridge.publish(snapshot.clone(), timeline(), None));
        for total in 2..10_000 {
            snapshot.guilds.total = total;
            assert!(!bridge.publish(snapshot.clone(), timeline(), None));
        }
        assert_eq!(bridge.consume().navigation.guilds.total, 9_999);
        assert!(!bridge.publish(snapshot.clone(), timeline(), None));
        snapshot.guilds.total = 10_000;
        assert!(bridge.publish(
            snapshot,
            timeline(),
            Some(GatewayStatus::AuthenticationRequired)
        ));
        let consumed = bridge.consume();
        assert_eq!(consumed.navigation.guilds.total, 10_000);
        assert_eq!(consumed.status, Some(GatewayStatus::AuthenticationRequired));
    }

    #[test]
    fn timeline_snapshots_replace_in_the_latest_slot_with_one_notification() {
        let bridge = NavigationBridge::new();
        let navigation = NavigationSnapshot::default();
        let mut snapshot = timeline::Snapshot {
            loading: true,
            ..timeline::Snapshot::default()
        };
        assert!(bridge.publish(navigation.clone(), snapshot.clone(), None));
        snapshot.loading = false;
        snapshot.has_older = true;
        assert!(!bridge.publish(navigation.clone(), snapshot, None));
        let consumed = bridge.consume();
        assert!(!consumed.timeline.loading);
        assert!(consumed.timeline.has_older);
        // Nothing changed: no notification, even though the slot was read.
        assert!(!bridge.publish(navigation, consumed.timeline.as_ref().clone(), None));
    }

    #[test]
    fn one_shot_timeline_inputs_are_taken_once_and_measurements_coalesce() {
        use crate::variable_list::Measurement;
        let bridge = NavigationBridge::new();
        let channel = Snowflake(5);
        for height in 0..1_000 {
            bridge.timeline_measured(
                channel,
                Measurement {
                    id: Snowflake(9),
                    revision: 1,
                    width_bucket: 3,
                    height: height as f32,
                },
            );
        }
        for id in 0..1_000u64 {
            bridge.timeline_measured(
                channel,
                Measurement {
                    id: Snowflake(id),
                    revision: 1,
                    width_bucket: 3,
                    height: 50.0,
                },
            );
        }
        bridge.timeline_viewport(channel, Viewport::default());
        let request = bridge.take_request();
        assert!(request.timeline.measurements.len() <= 128);
        assert!(request.timeline.viewport.is_some());
        let again = bridge.take_request();
        assert!(again.timeline.measurements.is_empty());
        assert!(again.timeline.viewport.is_none());
    }

    #[test]
    fn sends_are_ordered_unmerged_and_bounded_by_outbox_slots() {
        use crate::outbox::MAX_OUTBOX;
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        assert!(bridge.take_commands().is_none(), "one worker end");
        for n in 0..MAX_OUTBOX {
            assert!(bridge.send_message(Snowflake(1), format!("message {n}"), false, None));
        }
        // However stale the UI's view, a full outbox refuses and keeps nothing.
        assert!(!bridge.send_message(Snowflake(1), "one too many".to_owned(), true, None,));
        for n in 0..MAX_OUTBOX {
            match commands.try_recv().unwrap() {
                Command::Send { content, .. } => assert_eq!(content, format!("message {n}")),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(commands.try_recv().is_err());
        // The worker frees slots as operations leave the outbox.
        bridge.outbox_slots.release();
        assert!(bridge.send_message(Snowflake(1), "fits again".to_owned(), false, None));
        // Retry and discard are never merged either; Debug never shows text.
        assert!(bridge.retry_send(OpId(3)));
        assert!(bridge.discard_send(OpId(3)));
        let shown = format!("{:?}", commands.try_recv().unwrap());
        assert!(!shown.contains("fits") && shown.contains("Send"));
        assert!(matches!(commands.try_recv(), Ok(Command::Retry(OpId(3)))));
        assert!(matches!(commands.try_recv(), Ok(Command::Discard(OpId(3)))));
        // A gone worker refuses the send and returns its slot.
        drop(commands);
        bridge.outbox_slots.release();
        let used = bridge.outbox_slots.used();
        assert!(!bridge.send_message(Snowflake(1), "lost?".to_owned(), false, None));
        assert_eq!(bridge.outbox_slots.used(), used);
    }

    #[test]
    fn message_changes_queue_in_order_and_never_show_their_text() {
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let (c, m) = (Snowflake(500), Snowflake(7));
        assert!(bridge.edit_message(c, m, "secret words".to_owned()));
        assert!(bridge.delete_message(c, m));
        assert!(bridge.retry_change(c, m));
        assert!(bridge.dismiss_change(c, m));
        let shown = format!("{:?}", commands.try_recv().unwrap());
        assert!(
            shown.contains("Edit") && !shown.contains("secret"),
            "{shown}"
        );
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Delete { channel, message }) if (channel, message) == (c, m)
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::RetryChange { .. })
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::DismissChange { .. })
        ));
        // A full queue refuses rather than buffering without bound.
        for _ in 0..COMMAND_QUEUE {
            assert!(bridge.delete_message(c, m));
        }
        assert!(!bridge.edit_message(c, m, "more".to_owned()));
        assert!(!bridge.delete_message(c, m));
    }

    fn panel(bridge: &NavigationBridge) -> GatewayPanel {
        let (_, handle) = iced::Task::<()>::none().abortable();
        let mut panel = GatewayPanel::new(1, handle, bridge.clone());
        let mine = |id: u64, own: bool| timeline::Row {
            own,
            ..timeline::Row::new(
                Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "id": id.to_string(),
                        "channel_id": "500",
                        "author": {"id": "42", "username": "alt"},
                        "content": "hello",
                        "timestamp": "2026-10-08T09:00:00.000000+00:00",
                    }))
                    .unwrap(),
                ),
                1,
            )
        };
        panel.timeline = Arc::new(timeline::Snapshot {
            channel_id: Some(Snowflake(500)),
            rows: vec![mine(7, true), mine(8, false)],
            can_send: true,
            ..timeline::Snapshot::default()
        });
        panel.composer.select(Some(Snowflake(500)));
        panel
    }

    #[test]
    fn deleting_requires_the_confirmation_of_the_same_message() {
        use timeline::Event;
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let mut panel = panel(&bridge);
        let (channel_id, message_id) = (Snowflake(500), Snowflake(7));
        // A confirmation nobody asked for, or for another message, does nothing.
        panel.message_action(Event::ConfirmDelete {
            channel_id,
            message_id,
        });
        assert!(commands.try_recv().is_err());
        panel.message_action(Event::Delete {
            channel_id,
            message_id,
        });
        assert_eq!(panel.interaction().confirming, Some(message_id));
        assert!(commands.try_recv().is_err(), "asking is not deleting");
        panel.message_action(Event::ConfirmDelete {
            channel_id,
            message_id: Snowflake(8),
        });
        assert!(commands.try_recv().is_err());
        panel.message_action(Event::CancelDelete);
        assert_eq!(panel.interaction().confirming, None);
        // Asked, then confirmed: exactly one deletion.
        panel.message_action(Event::Delete {
            channel_id,
            message_id,
        });
        panel.message_action(Event::ConfirmDelete {
            channel_id,
            message_id,
        });
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Delete { message, .. }) if message == message_id
        ));
        assert!(commands.try_recv().is_err());
        assert_eq!(panel.interaction().confirming, None);
        // A pending confirmation does not survive leaving the channel.
        panel.message_action(Event::Delete {
            channel_id,
            message_id,
        });
        bridge.publish(
            NavigationSnapshot::default(),
            timeline::Snapshot::default(),
            None,
        );
        panel.consume();
        assert_eq!(panel.confirm_delete, None);
    }

    #[test]
    fn edit_moves_only_the_users_message_into_the_composer() {
        use timeline::Event;
        let bridge = NavigationBridge::new();
        let _commands = bridge.take_commands().unwrap();
        let mut panel = panel(&bridge);
        let channel_id = Snowflake(500);
        let foreign = Event::Edit {
            channel_id,
            message_id: Snowflake(8),
        };
        assert!(!panel.message_action(foreign), "not the user's message");
        let other_channel = Event::Edit {
            channel_id: Snowflake(501),
            message_id: Snowflake(7),
        };
        assert!(!panel.message_action(other_channel));
        assert_eq!(panel.interaction().editing, None);
        panel.message_action(Event::Delete {
            channel_id,
            message_id: Snowflake(7),
        });
        let edit = Event::Edit {
            channel_id,
            message_id: Snowflake(7),
        };
        assert!(panel.message_action(edit), "the editor takes focus");
        assert_eq!(
            panel.interaction(),
            timeline::Interaction {
                editing: Some(Snowflake(7)),
                replying: None,
                confirming: None,
            }
        );
    }

    #[test]
    fn reply_selection_uses_the_loaded_row_and_requires_send_permission() {
        use timeline::Event;

        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let mut panel = panel(&bridge);
        assert!(panel.message_action(Event::Reply {
            channel_id: Snowflake(500),
            message_id: Snowflake(8),
        }));
        assert_eq!(panel.interaction().replying, Some(Snowflake(8)));
        assert!(commands.try_recv().is_err(), "reply selection is local");

        panel.composer.cancel_reply();
        panel.timeline = Arc::new(timeline::Snapshot {
            can_send: false,
            ..(*panel.timeline).clone()
        });
        assert!(!panel.message_action(Event::Reply {
            channel_id: Snowflake(500),
            message_id: Snowflake(8),
        }));
        assert!(panel.composer.replying().is_none());
    }
}

#[cfg(test)]
mod tests {
    use fastcord_discord::gateway::{Dispatch, GuildDelete, Ready, SessionId};
    use fastcord_model::{Channel, ChannelKind, Guild, Snowflake, User};

    use super::*;

    fn user() -> User {
        User {
            id: Snowflake(1),
            username: "alt_fixture".to_owned(),
            global_name: None,
            avatar: None,
            bot: false,
        }
    }

    fn ready_event(guilds: usize, unavailable: usize, dms: usize) -> GatewayEvent {
        let guild = |id| Guild {
            id: Snowflake(id),
            name: String::new(),
            icon: None,
            owner_id: None,
            roles: Vec::new(),
            channels: Vec::new(),
            members: Vec::new(),
            member_count: 0,
        };
        let dm = |id| Channel {
            id: Snowflake(id),
            kind: ChannelKind::Dm,
            guild_id: None,
            name: None,
            position: None,
            parent_id: None,
            permission_overwrites: Vec::new(),
            recipients: Vec::new(),
            recipient_ids: Vec::new(),
            last_message_id: None,
        };
        GatewayEvent::Dispatch {
            sequence: 1,
            event: Dispatch::Ready(Box::new(Ready {
                session_id: SessionId::new("fixture-session".to_owned()),
                user: user(),
                users: vec![user()],
                guilds: (0..guilds as u64).map(guild).collect(),
                unavailable_guilds: (0..unavailable as u64).map(Snowflake).collect(),
                private_channels: (0..dms as u64).map(dm).collect(),
            })),
        }
    }

    #[test]
    fn states_map_to_statuses_and_ready_reports_the_counts_from_ready() {
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker.apply(GatewayEvent::State(ConnectionState::Connecting {
                attempt: 0
            })),
            Some(GatewayStatus::Connecting { attempt: 0 })
        );
        for state in [ConnectionState::AwaitHello, ConnectionState::Identifying] {
            assert_eq!(
                tracker.apply(GatewayEvent::State(state)),
                Some(GatewayStatus::SigningIn)
            );
        }
        // READY itself is not a status change; the Ready state that follows is.
        assert_eq!(tracker.apply(ready_event(3, 1, 2)), None);
        let status = tracker
            .apply(GatewayEvent::State(ConnectionState::Ready))
            .unwrap();
        assert_eq!(
            status,
            GatewayStatus::Ready(Counts {
                guilds: 3,
                unavailable: 1,
                direct_messages: 2
            })
        );
        assert!(status.describe().starts_with("Connected to Discord"));
        assert_eq!(
            status.describe(),
            "Connected to Discord: 3 servers, 2 direct messages. 1 servers are unavailable."
        );
    }

    #[test]
    fn reconnecting_is_never_shown_as_connected_and_resume_keeps_the_counts() {
        let mut tracker = Tracker::default();
        let _ = tracker.apply(ready_event(2, 0, 1));
        let lost = tracker
            .apply(GatewayEvent::State(ConnectionState::Reconnecting {
                attempt: 2,
                delay: Duration::from_millis(1_500),
                reason: ReconnectReason::HeartbeatTimeout,
            }))
            .unwrap();
        assert!(!lost.describe().contains("Connected"));
        assert_eq!(
            lost.describe(),
            "Connection lost (Discord stopped responding). Reconnecting in 2 seconds (attempt 2)."
        );
        assert_eq!(
            tracker.apply(GatewayEvent::State(ConnectionState::Resuming)),
            Some(GatewayStatus::Resuming)
        );
        // RESUMED is replay bookkeeping, not a status; the counts survive it.
        assert_eq!(
            tracker.apply(GatewayEvent::Dispatch {
                sequence: 9,
                event: Dispatch::Resumed
            }),
            None
        );
        assert_eq!(
            tracker.apply(GatewayEvent::State(ConnectionState::Ready)),
            Some(GatewayStatus::Ready(Counts {
                guilds: 2,
                unavailable: 0,
                direct_messages: 1
            }))
        );
    }

    #[test]
    fn a_new_ready_replaces_the_old_counts() {
        let mut tracker = Tracker::default();
        let _ = tracker.apply(ready_event(5, 0, 5));
        let _ = tracker.apply(ready_event(1, 0, 0));
        assert_eq!(
            tracker.apply(GatewayEvent::State(ConnectionState::Ready)),
            Some(GatewayStatus::Ready(Counts {
                guilds: 1,
                unavailable: 0,
                direct_messages: 0
            }))
        );
    }
    #[test]
    fn the_worker_applies_events_to_its_store_and_the_counts_follow_it() {
        let mut tracker = Tracker::default();
        let _ = tracker.apply(ready_event(3, 0, 1));
        // A guild is left while connected: the next connected message agrees.
        let _ = tracker.apply(GatewayEvent::Dispatch {
            sequence: 2,
            event: Dispatch::GuildDelete(GuildDelete {
                id: Snowflake(1),
                unavailable: false,
            }),
        });
        // A repeated delete (replay) is harmless.
        let _ = tracker.apply(GatewayEvent::Dispatch {
            sequence: 3,
            event: Dispatch::GuildDelete(GuildDelete {
                id: Snowflake(1),
                unavailable: false,
            }),
        });
        assert_eq!(
            tracker.apply(GatewayEvent::State(ConnectionState::Ready)),
            Some(GatewayStatus::Ready(Counts {
                guilds: 2,
                unavailable: 0,
                direct_messages: 1
            }))
        );
    }

    #[test]
    fn required_state_over_the_cap_is_a_visible_terminal_failure() {
        let mut tracker = Tracker {
            store: Store::with_budget(1),
            ..Tracker::default()
        };
        let status = tracker.apply(ready_event(3, 0, 1)).unwrap();
        assert_eq!(status, GatewayStatus::Stopped(StopReason::StateTooLarge));
        assert!(status.describe().contains("12 MiB"));
        assert!(!status.describe().contains("Connected"));
        assert!(tracker.store.bytes() <= tracker.store.budget());
    }

    #[test]
    fn terminal_states_explain_themselves() {
        let mut tracker = Tracker::default();
        let auth = tracker
            .apply(GatewayEvent::State(ConnectionState::AuthenticationRequired))
            .unwrap();
        assert!(auth.describe().contains("Log in again"));
        let stopped = tracker
            .apply(GatewayEvent::State(ConnectionState::Stopped(
                StopReason::ActionRequired("REQUIRE_VERIFIED_EMAIL".to_owned()),
            )))
            .unwrap();
        assert!(stopped.describe().contains("official Discord client"));
        assert!(!stopped.describe().contains("Connected"));
    }

    #[test]
    fn delays_are_described_without_a_countdown() {
        assert_eq!(describe_delay(Duration::ZERO), "a moment");
        assert_eq!(describe_delay(Duration::from_millis(750)), "1 second");
        assert_eq!(describe_delay(Duration::from_secs(32)), "32 seconds");
    }
}
