//! UI-side view of the account's Gateway connection. The protocol, session,
//! heartbeats, and reconnects live in `fastcord_discord::gateway::Gateway`; the
//! account's normalized state lives in `fastcord_discord::state::Store`, which
//! the worker below owns and is the only writer of (the reducer). This module
//! turns the ordered events into that state plus the few status changes the
//! screen shows, and is dropped, with the connection and the state it owns,
//! when the account screen is left (which closes the Gateway session cleanly).

mod stream_audio;
mod voice_audio;

pub use voice_audio::VoiceAudioState;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastcord_discord::gateway::{
    ConnectionState, Dispatch, Gateway, GatewayEvent, ReconnectReason, StopReason,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceParticipant {
    pub user_id: Snowflake,
    pub ssrc: Option<u32>,
    pub speaking: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserVolume {
    pub user_id: Snowflake,
    pub percent: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VoiceControls {
    pub self_muted: bool,
    pub deafened: bool,
    pub server_muted: bool,
    pub server_deafened: bool,
    pub participants: Vec<VoiceParticipant>,
    pub volumes: Vec<UserVolume>,
}
impl VoiceControls {
    pub fn capture_enabled(&self) -> bool {
        !self.self_muted && !self.deafened && !self.server_muted
    }

    pub fn output_enabled(&self) -> bool {
        !self.deafened && !self.server_deafened
    }
    pub fn volume_percent(&self, user_id: Snowflake) -> u16 {
        self.volumes
            .iter()
            .find(|volume| volume.user_id == user_id)
            .map_or(100, |volume| volume.percent)
    }
}
use fastcord_discord::state::Store;
use fastcord_discord::state::navigation::{Navigation, NavigationSnapshot};
use fastcord_media::{Correlation, JoinCorrelator, StreamCorrelation, StreamCorrelator};
use fastcord_model::{
    Channel, Snowflake, StreamCreateRequest, StreamKey, StreamType, VoiceStateRequest,
};
use tokio::sync::{mpsc, watch};

use crate::attachments::AttachmentCache;
use crate::composer::Composer;
use crate::history::{Completed, History, Intent};
use crate::outbox::{OpId, Reply, Slots};
use crate::timeline;
use crate::timeline::AttachmentView;
use crate::variable_list::{Measurement, Viewport};
use crate::virtual_list::Window;
use fastcord_discord::{PrivateChannelError, RestClient, UserToken};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttachmentAction {
    Download,
    Open,
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
    PrivateChannel(Snowflake),
}

#[derive(Clone, Debug, Default)]
struct Request {
    revision: u64,
    selection: Option<SelectionIntent>,
    guilds: Window,
    channels: Window,
    private_channels: Window,
    timeline: crate::history::Request,
}

#[derive(Default)]
struct Latest {
    snapshot: Arc<NavigationSnapshot>,
    timeline: Arc<timeline::Snapshot>,
    status: Option<GatewayStatus>,
    private_notice: Option<String>,
    notified: bool,
    private_notice_pending: bool,
}

/// What one consumed notification carries to the UI.
pub struct Consumed {
    pub navigation: Arc<NavigationSnapshot>,
    pub timeline: Arc<timeline::Snapshot>,
    pub status: Option<GatewayStatus>,
    pub private_notice: Option<String>,
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
    OpenPrivate(Vec<Snowflake>),
    #[allow(dead_code)]
    Stream(StreamIntent),
}

/// An explicit Go Live control action. Video capture and encoding are not
/// started by these signaling intents.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum StreamIntent {
    Create(StreamCreateRequest),
    Watch(String),
    Unwatch(String),
    PauseResume { stream_key: String, paused: bool },
    Ping(String),
}

fn send_or_retain_stream_intent(
    pending: &mut Option<StreamIntent>,
    intent: StreamIntent,
    send: impl FnOnce(&StreamIntent) -> bool,
) -> bool {
    if send(&intent) {
        true
    } else {
        *pending = Some(intent);
        false
    }
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
            Self::OpenPrivate(_) => f.write_str("OpenPrivate([selected recipients])"),
            Self::Stream(intent) => f.debug_tuple("Stream").field(intent).finish(),
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
    voice_controls: watch::Sender<VoiceControls>,
    account_id: Arc<Mutex<Option<Snowflake>>>,
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
        let (voice_controls, _) = watch::channel(VoiceControls::default());
        let (commands, receiver) = mpsc::channel(COMMAND_QUEUE);
        Self {
            requests,
            voice_requests,
            voice_audio,
            voice_controls,
            account_id: Arc::new(Mutex::new(None)),
            latest: Arc::default(),
            commands,
            command_receiver: Arc::new(Mutex::new(Some(receiver))),
            outbox_slots: Slots::default(),
        }
    }

    /// Current audio state for the voice controls.
    pub fn voice_audio_state(&self) -> VoiceAudioState {
        *self.voice_audio.borrow()
    }
    pub fn voice_controls(&self) -> VoiceControls {
        self.voice_controls.borrow().clone()
    }
    pub fn set_account_id(&self, account_id: Snowflake) {
        *self.account_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(account_id);
    }

    pub fn set_self_muted(&self, self_muted: bool) {
        self.voice_controls
            .send_modify(|controls| controls.self_muted = self_muted);
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.voice_controls
            .send_modify(|controls| controls.deafened = deafened);
    }

    pub fn set_volume_percent(&self, user_id: Snowflake, percent: u16) -> bool {
        let percent = percent.min(200);
        self.voice_controls.send_modify(|controls| {
            if let Some(volume) = controls.volumes.iter_mut().find(|v| v.user_id == user_id) {
                volume.percent = percent;
            } else if controls.volumes.len() < 256 {
                controls.volumes.push(UserVolume { user_id, percent });
            } else {
                controls.volumes.remove(0);
                controls.volumes.push(UserVolume { user_id, percent });
            }
        });
        true
    }

    pub fn set_volume_settings(&self, volumes: Vec<UserVolume>) {
        let volumes: Vec<_> = volumes
            .into_iter()
            .take(256)
            .map(|volume| UserVolume {
                user_id: volume.user_id,
                percent: volume.percent.min(200),
            })
            .collect();
        self.voice_controls
            .send_modify(|controls| controls.volumes = volumes);
    }

    /// Requests a voice join, move, or leave. No audio connection starts beforehand.
    ///
    /// Joining the channel this account is already joined to (or still joining) is a
    /// no-op: the Gateway would not resend an identical opcode 4, so tearing the call
    /// down locally would leave it dead while Discord still shows the user connected.
    /// Returns whether the request was queued.
    pub fn request_voice_state(&self, request: VoiceStateRequest) -> bool {
        if let Some(channel) = request.channel_id
            && matches!(
                *self.voice_audio.borrow(),
                VoiceAudioState::Connecting | VoiceAudioState::Running
            )
            && self.voice_requests.borrow().is_some_and(|current| {
                current.guild_id == request.guild_id && current.channel_id == Some(channel)
            })
        {
            return false;
        }
        self.voice_audio
            .send_replace(if request.channel_id.is_some() {
                VoiceAudioState::Connecting
            } else {
                VoiceAudioState::Idle
            });
        self.voice_requests.send_replace(Some(request));
        true
    }
    /// Starts a local Go Live stream intent through main-Gateway opcode 18.
    #[allow(dead_code)]
    pub fn create_stream(&self, request: StreamCreateRequest) -> bool {
        self.stream_intent(StreamIntent::Create(request))
    }

    /// Watches the named Go Live stream through opcode 20.
    #[allow(dead_code)]
    pub fn watch_stream(&self, stream_key: impl Into<String>) -> bool {
        let stream_key = stream_key.into();
        StreamKey::from_wire(&stream_key)
            .is_some_and(|_| self.stream_intent(StreamIntent::Watch(stream_key)))
    }

    /// Stops watching (or ends an owned stream) through opcode 19.
    #[allow(dead_code)]
    pub fn unwatch_stream(&self, stream_key: impl Into<String>) -> bool {
        let stream_key = stream_key.into();
        StreamKey::from_wire(&stream_key)
            .is_some_and(|_| self.stream_intent(StreamIntent::Unwatch(stream_key)))
    }

    #[allow(dead_code)]
    pub fn pause_stream(&self, stream_key: impl Into<String>, paused: bool) -> bool {
        let stream_key = stream_key.into();
        StreamKey::from_wire(&stream_key)
            .is_some_and(|_| self.stream_intent(StreamIntent::PauseResume { stream_key, paused }))
    }

    #[allow(dead_code)]
    pub fn ping_stream(&self, stream_key: impl Into<String>) -> bool {
        let stream_key = stream_key.into();
        StreamKey::from_wire(&stream_key)
            .is_some_and(|_| self.stream_intent(StreamIntent::Ping(stream_key)))
    }

    fn stream_intent(&self, intent: StreamIntent) -> bool {
        self.commands.try_send(Command::Stream(intent)).is_ok()
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
            private_notice: latest.private_notice.clone(),
        }
    }
    fn set_private_notice(&self, notice: String) {
        let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        latest.private_notice = Some(notice);
        latest.notified = true;
        latest.private_notice_pending = true;
    }
    fn clear_private_notice(&self) {
        let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        if latest.private_notice.take().is_some() {
            latest.notified = true;
            latest.private_notice_pending = true;
        }
    }
    fn clear_private_notice_on_selection_change(&self, selection: Option<Snowflake>) {
        let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        let previous = latest
            .snapshot
            .private_selection
            .as_ref()
            .map(|selection| selection.channel_id);
        if previous != selection && latest.private_notice.take().is_some() {
            latest.notified = true;
            latest.private_notice_pending = true;
        }
    }
    fn take_private_notice_pending(&self) -> bool {
        let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut latest.private_notice_pending)
    }

    /// Asks for a redraw because a voice watch changed. Coalesced like every other
    /// notification: `false` when one is already waiting to be consumed.
    fn notify_voice(&self) -> bool {
        let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        !std::mem::replace(&mut latest.notified, true)
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
        self.clear_private_notice();
        self.requests.send_modify(|request| {
            request.revision = request.revision.wrapping_add(1);
            request.selection = Some(SelectionIntent::Guild(id));
            request.channels = Window::default();
        });
    }

    pub fn select_channel(&self, guild: Snowflake, channel: Snowflake) {
        self.clear_private_notice();
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
    pub fn private_viewport(&self, window: Window) {
        self.requests.send_if_modified(|request| {
            if request.private_channels == window {
                return false;
            }
            request.private_channels = window;
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
    pub fn select_private_channel(&self, channel: Snowflake) {
        self.clear_private_notice();
        self.requests.send_modify(|request| {
            request.revision = request.revision.wrapping_add(1);
            request.selection = Some(SelectionIntent::PrivateChannel(channel));
        });
    }

    pub fn open_private_channel(&self, recipients: Vec<Snowflake>) -> bool {
        if recipients.is_empty()
            || recipients.len() > fastcord_discord::MAX_PRIVATE_RECIPIENTS
            || recipients
                .iter()
                .enumerate()
                .any(|(index, id)| recipients[..index].contains(id))
        {
            return false;
        }
        self.commands
            .try_send(Command::OpenPrivate(recipients))
            .is_ok()
    }
}

type PrivateListFuture = Pin<Box<dyn Future<Output = (u64, Result<Vec<Channel>, String>)> + Send>>;
type PrivateOpenFuture =
    Pin<Box<dyn Future<Output = (u64, Result<Channel, PrivateChannelError>)> + Send>>;

enum WorkerInput {
    Request,
    VoiceRequest,
    VoiceAudio,
    VoiceControls,
    History(Completed),
    Command(Command),
    PrivateList(u64, Result<Vec<Channel>, String>),
    PrivateOpen(u64, Result<Channel, PrivateChannelError>),
}

struct Worker {
    start: Option<(RestClient, Arc<UserToken>, String)>,
    rest: RestClient,
    gateway: Option<Gateway>,
    tracker: Tracker,
    bridge: NavigationBridge,
    requests: watch::Receiver<Request>,
    voice_requests: watch::Receiver<Option<VoiceStateRequest>>,
    voice_audio: watch::Receiver<VoiceAudioState>,
    voice_controls: watch::Receiver<VoiceControls>,
    commands: mpsc::Receiver<Command>,
    voice_flags: (bool, bool),
    revision: u64,
    voice_correlator: Option<JoinCorrelator>,
    voice_task: Option<tokio::task::JoinHandle<()>>,
    voice_cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,
    private_list_generation: u64,
    private_list_loaded_generation: Option<u64>,
    private_list_task: Option<PrivateListFuture>,
    private_open_task: Option<PrivateOpenFuture>,
    pending_private_opens: VecDeque<Vec<Snowflake>>,
    stream_correlator: Option<StreamCorrelator>,
    stream_tasks: HashMap<StreamKey, tokio::task::JoinHandle<()>>,
    stream_intents: StreamIntentState,
    pending_stream_signal: Option<StreamIntent>,
}

fn stream_request_matches_key(
    request: &StreamCreateRequest,
    key: &StreamKey,
    owner_id: Snowflake,
) -> bool {
    match (request.stream_type, key) {
        (
            StreamType::Guild,
            StreamKey::Guild {
                guild_id,
                channel_id,
                owner_id: stream_owner,
            },
        ) => {
            request.guild_id == Some(*guild_id)
                && request.channel_id == *channel_id
                && *stream_owner == owner_id
        }
        (
            StreamType::Call,
            StreamKey::Call {
                channel_id,
                owner_id: stream_owner,
            },
        ) => {
            request.guild_id.is_none()
                && request.channel_id == *channel_id
                && *stream_owner == owner_id
        }
        _ => false,
    }
}

fn stream_request_matches_location(request: &StreamCreateRequest, key: &StreamKey) -> bool {
    match (request.stream_type, key) {
        (
            StreamType::Guild,
            StreamKey::Guild {
                guild_id,
                channel_id,
                ..
            },
        ) => request.guild_id == Some(*guild_id) && request.channel_id == *channel_id,
        (StreamType::Call, StreamKey::Call { channel_id, .. }) => {
            request.guild_id.is_none() && request.channel_id == *channel_id
        }
        _ => false,
    }
}

fn stream_key_owner(key: &StreamKey) -> Snowflake {
    match key {
        StreamKey::Guild { owner_id, .. } | StreamKey::Call { owner_id, .. } => *owner_id,
    }
}

#[derive(Default)]
struct StreamIntentState {
    user_id: Option<Snowflake>,
    active: HashSet<StreamKey>,
    creates: VecDeque<StreamCreateRequest>,
    cancelled: HashSet<StreamKey>,
}

impl StreamIntentState {
    fn ready(&mut self, user_id: Snowflake) {
        self.user_id = Some(user_id);
    }

    fn record_create(&mut self, request: &StreamCreateRequest) {
        let user_id = self.user_id;
        self.cancelled.retain(|key| {
            !stream_request_matches_location(request, key)
                || user_id.is_some_and(|user_id| stream_key_owner(key) != user_id)
        });
        self.creates.push_back(request.clone());
    }

    fn watch(&mut self, key: StreamKey) {
        self.cancelled.remove(&key);
        self.active.insert(key);
    }

    fn retire(&mut self, key: &StreamKey) {
        self.active.remove(key);
        self.cancelled.insert(key.clone());
        let user_id = self.user_id;
        self.creates.retain(|request| {
            !stream_request_matches_location(request, key)
                || user_id.is_some_and(|user_id| stream_key_owner(key) != user_id)
        });
    }

    fn is_active(&self, key: &StreamKey) -> bool {
        self.active.contains(key)
    }

    fn admit(&mut self, key: &StreamKey) -> bool {
        if self.cancelled.contains(key) {
            return false;
        }
        if self.is_active(key) {
            return true;
        }
        let Some(user_id) = self.user_id else {
            return false;
        };
        let Some(index) = self
            .creates
            .iter()
            .position(|request| stream_request_matches_key(request, key, user_id))
        else {
            return false;
        };
        self.creates.remove(index);
        self.active.insert(key.clone());
        true
    }
}

impl Worker {
    fn begin_private_list_fetch(&mut self) -> u64 {
        self.private_list_generation = self.private_list_generation.wrapping_add(1);
        self.private_list_loaded_generation = None;
        self.private_list_task = None;
        self.private_open_task = None;
        self.pending_private_opens.clear();
        self.bridge.clear_private_notice();
        self.private_list_generation
    }

    fn finish_private_list_fetch(
        &mut self,
        generation: u64,
        result: Result<Vec<Channel>, String>,
    ) -> Option<GatewayStatus> {
        if generation != self.private_list_generation
            || self.private_list_loaded_generation == Some(generation)
        {
            return None;
        }
        match result {
            Ok(channels) => {
                for channel in channels {
                    self.tracker.store.apply(GatewayEvent::Dispatch {
                        sequence: 0,
                        event: Dispatch::ChannelCreate(Box::new(channel)),
                    });
                }
                self.tracker.navigation_dirty = true;
                self.private_list_loaded_generation = Some(generation);
                self.tracker
                    .store
                    .limit_exceeded()
                    .then_some(GatewayStatus::Stopped(StopReason::StateTooLarge))
            }
            Err(error) => {
                self.bridge
                    .set_private_notice(format!("Could not refresh direct conversations: {error}"));
                self.tracker.navigation_dirty = true;
                None
            }
        }
    }
}

impl Worker {
    fn apply_gateway_event(
        &mut self,
        event: GatewayEvent,
        start_private_list: impl FnOnce(&mut Self),
    ) -> Option<GatewayStatus> {
        let ready = matches!(
            &event,
            GatewayEvent::Dispatch {
                event: Dispatch::Ready(_),
                ..
            }
        );
        let status = self.tracker.apply(event);
        if ready {
            start_private_list(self);
        }
        status
    }
    fn start_private_list_fetch_with(
        &mut self,
        future: impl Future<Output = Result<Vec<Channel>, String>> + Send + 'static,
    ) -> u64 {
        let generation = self.begin_private_list_fetch();
        self.private_list_task = Some(Box::pin(async move { (generation, future.await) }));
        generation
    }

    fn start_private_list_fetch(&mut self) -> u64 {
        let rest = self.rest.clone();
        self.start_private_list_fetch_with(async move {
            rest.private_channels()
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn start_private_open(&mut self, recipients: Vec<Snowflake>) {
        let generation = self.private_list_generation;
        let rest = self.rest.clone();
        self.private_open_task = Some(Box::pin(async move {
            (generation, rest.open_private_channel(&recipients).await)
        }));
    }
    fn queue_private_open(&mut self, recipients: Vec<Snowflake>) {
        if self.private_open_task.is_some() {
            self.pending_private_opens.push_back(recipients);
        } else {
            self.start_private_open(recipients);
        }
    }

    fn start_next_private_open(&mut self) {
        if self.private_open_task.is_none()
            && let Some(recipients) = self.pending_private_opens.pop_front()
        {
            self.start_private_open(recipients);
        }
    }
    fn finish_private_open(
        &mut self,
        generation: u64,
        result: Result<Channel, PrivateChannelError>,
    ) -> Option<GatewayStatus> {
        if generation != self.private_list_generation {
            return None;
        }
        match result {
            Ok(channel) => {
                let channel_id = channel.id;
                self.tracker.store.apply(GatewayEvent::Dispatch {
                    sequence: 0,
                    event: Dispatch::ChannelCreate(Box::new(channel)),
                });
                self.tracker.navigation.reconcile(&self.tracker.store);
                self.tracker
                    .navigation
                    .select_private_channel(&self.tracker.store, channel_id);
                self.tracker.navigation_dirty = true;
            }
            Err(error) => {
                self.bridge
                    .set_private_notice(format!("Could not open direct conversation: {error:?}"));
                self.tracker.navigation_dirty = true;
            }
        }
        self.tracker
            .store
            .limit_exceeded()
            .then_some(GatewayStatus::Stopped(StopReason::StateTooLarge))
    }

    async fn next_input(&mut self) -> Option<WorkerInput> {
        tokio::select! {
            changed = self.requests.changed() => {
                changed.ok().map(|()| WorkerInput::Request)
            }
            changed = self.voice_requests.changed() => {
                changed.ok().map(|()| WorkerInput::VoiceRequest)
            }
            changed = self.voice_audio.changed() => {
                changed.ok().map(|()| WorkerInput::VoiceAudio)
            }
            changed = self.voice_controls.changed() => {
                changed.ok().map(|()| WorkerInput::VoiceControls)
            }
            completed = self.tracker.history.next_completed() => {
                Some(WorkerInput::History(completed))
            }
            Some(command) = self.commands.recv(),
                if self.pending_private_opens.len() < COMMAND_QUEUE
                    && self.pending_stream_signal.is_none() =>
            {
                Some(WorkerInput::Command(command))
            }
            completed = async {
                match self.private_list_task.as_mut() {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            } => {
                self.private_list_task = None;
                Some(WorkerInput::PrivateList(completed.0, completed.1))
            }
            completed = async {
                match self.private_open_task.as_mut() {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            } => {
                self.private_open_task = None;
                Some(WorkerInput::PrivateOpen(completed.0, completed.1))
            }
        }
    }
}

impl Worker {
    /// Applies one explicit Go Live control action.
    fn stream_command(&mut self, gateway: &Gateway, intent: StreamIntent) {
        match &intent {
            StreamIntent::Create(_) => {}
            StreamIntent::Watch(key) => {
                if let Some(key) = StreamKey::from_wire(key) {
                    self.stream_intents.watch(key);
                } else {
                    return;
                }
            }
            StreamIntent::Unwatch(key) => {
                if let Some(key_value) = StreamKey::from_wire(key) {
                    self.stream_intents.retire(&key_value);
                    if let Some(correlator) = self.stream_correlator.as_mut() {
                        correlator.retire(key);
                    }
                    self.stop_stream_key(key);
                } else {
                    return;
                }
            }
            StreamIntent::PauseResume { stream_key, .. } | StreamIntent::Ping(stream_key) => {
                if StreamKey::from_wire(stream_key).is_none() {
                    return;
                }
            }
        }
        self.queue_stream_intent(gateway, intent);
    }

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
            Command::OpenPrivate(_) => {}
            Command::Stream(_) => unreachable!("stream commands are handled above"),
        }
    }

    fn send_stream_intent(gateway: &Gateway, intent: &StreamIntent) -> bool {
        let streams = gateway.streams();
        match intent {
            StreamIntent::Create(request) => streams.create(request.clone()).is_ok(),
            StreamIntent::Watch(key) => streams.watch(key.clone()).is_ok(),
            StreamIntent::Unwatch(key) => streams.delete(key.clone()).is_ok(),
            StreamIntent::PauseResume { stream_key, paused } => {
                streams.pause_resume(stream_key.clone(), *paused).is_ok()
            }
            StreamIntent::Ping(key) => streams.ping(key.clone()).is_ok(),
        }
    }
    fn queue_stream_intent(&mut self, gateway: &Gateway, intent: StreamIntent) {
        let sent = send_or_retain_stream_intent(
            &mut self.pending_stream_signal,
            intent.clone(),
            |intent| Self::send_stream_intent(gateway, intent),
        );
        if sent {
            self.record_stream_intent(&intent);
        }
    }

    fn record_stream_intent(&mut self, intent: &StreamIntent) {
        if let StreamIntent::Create(request) = intent {
            self.stream_intents.record_create(request);
        }
    }

    /// An audio failure leaves the Gateway voice state too. Otherwise Discord keeps the
    /// user in the channel, an identical re-join is never resent, and the UI could not
    /// recover from the failed engine.
    fn voice_audio_changed(&mut self) -> Option<VoiceStateRequest> {
        if *self.voice_audio.borrow_and_update() != VoiceAudioState::EngineFailed {
            return None;
        }
        let request = *self.voice_requests.borrow();
        if let Some(request) = request
            && request.channel_id.is_some()
        {
            let leave = VoiceStateRequest::leave(request.guild_id);
            self.bridge.voice_requests.send_replace(Some(leave));
            if let Some(correlator) = self.voice_correlator.as_mut() {
                correlator.leave();
            }
            return Some(leave);
        }
        None
    }

    /// A local mute or deafen change is sent to Discord with the current voice state.
    fn voice_controls_changed(&mut self, gateway: &Gateway) {
        let flags = {
            let controls = self.voice_controls.borrow_and_update();
            (controls.self_muted, controls.deafened)
        };
        if flags == self.voice_flags {
            return;
        }
        self.voice_flags = flags;
        if matches!(
            *self.voice_audio.borrow(),
            VoiceAudioState::Connecting | VoiceAudioState::Running
        ) && let Some(request) = *self.voice_requests.borrow()
            && let Some(channel) = request.channel_id
        {
            gateway.voice_state().request(VoiceStateRequest::join(
                request.guild_id,
                channel,
                flags.0,
                flags.1,
            ));
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
            self.bridge
                .voice_audio
                .send_replace(VoiceAudioState::Connecting);
        } else {
            if let Some(correlator) = self.voice_correlator.as_mut() {
                correlator.leave();
            }
        }
        self.voice_flags = (request.self_mute, request.self_deaf);
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
                if *self
                    .bridge
                    .account_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    == Some(update.state.user_id)
                {
                    self.bridge.voice_controls.send_modify(|controls| {
                        controls.server_muted = update.state.mute;
                        controls.server_deafened = update.state.deaf;
                    });
                }
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
    fn stream_dispatch(&mut self, event: &GatewayEvent) -> Option<StreamCorrelation> {
        let GatewayEvent::Dispatch { event, .. } = event else {
            return None;
        };
        match event {
            Dispatch::Ready(ready) => {
                self.stop_all_streams();
                self.stream_correlator = Some(StreamCorrelator::new(
                    ready.user.id,
                    ready.session_id.as_str(),
                ));
                self.stream_intents.ready(ready.user.id);
            }
            Dispatch::StreamCreate(created) => {
                let Some(key) = StreamKey::from_wire(&created.stream_key) else {
                    return Some(StreamCorrelation::Ignored);
                };
                if !self.admit_stream_key(&key) {
                    return Some(StreamCorrelation::Ignored);
                }
                self.stop_stream_key(&key.to_wire());
                return self.stream_correlator.as_mut().map(|c| c.create(created));
            }
            Dispatch::StreamServerUpdate(update) => {
                let Some(key) = StreamKey::from_wire(&update.stream_key) else {
                    return Some(StreamCorrelation::Ignored);
                };
                if !self.admit_stream_key(&key) {
                    return Some(StreamCorrelation::Ignored);
                }
                return self
                    .stream_correlator
                    .as_mut()
                    .map(|c| c.server_update(update));
            }
            Dispatch::StreamDelete(deleted) => {
                let Some(key) = StreamKey::from_wire(&deleted.stream_key) else {
                    return Some(StreamCorrelation::Ignored);
                };
                if !self.admit_stream_key(&key) {
                    return Some(StreamCorrelation::Ignored);
                }
                self.stream_intents.retire(&key);
                if let Some(correlator) = self.stream_correlator.as_mut() {
                    let correlation = correlator.delete(&deleted.stream_key);
                    if matches!(&correlation, StreamCorrelation::Ignored) {
                        self.stop_stream_key(&deleted.stream_key);
                    }
                    return Some(correlation);
                }
                self.stop_stream_key(&deleted.stream_key);
                return Some(StreamCorrelation::Ignored);
            }
            Dispatch::StreamUpdate(update)
                if self.stream_correlator.as_ref().is_none_or(|c| {
                    StreamKey::from_wire(&update.stream_key).is_none_or(|key| {
                        !self.stream_intents.is_active(&key) || !c.is_active(&key)
                    })
                }) =>
            {
                return Some(StreamCorrelation::Ignored);
            }
            Dispatch::StreamUpdate(_) => {}
            _ => {}
        }
        None
    }

    fn admit_stream_key(&mut self, key: &StreamKey) -> bool {
        self.stream_intents.admit(key)
    }

    async fn handle_stream_correlation(&mut self, correlation: StreamCorrelation) {
        match correlation {
            StreamCorrelation::Connect(credentials) => {
                let key = credentials.key.clone();
                self.stop_stream_key(&key.to_wire());
                self.stream_tasks
                    .insert(key, tokio::spawn(stream_audio::run(credentials)));
            }
            StreamCorrelation::Reallocating { key, .. } | StreamCorrelation::Ended { key, .. } => {
                self.stop_stream_key(&key.to_wire())
            }
            StreamCorrelation::Invalid { key: Some(key), .. } => {
                self.stop_stream_key(&key.to_wire())
            }
            StreamCorrelation::Invalid { key: None, .. }
            | StreamCorrelation::Ignored
            | StreamCorrelation::Waiting => {}
        }
    }

    fn stop_stream_key(&mut self, wire_key: &str) {
        let Some(key) = StreamKey::from_wire(wire_key) else {
            return;
        };
        if let Some(task) = self.stream_tasks.remove(&key) {
            task.abort();
        }
    }

    fn stop_all_streams(&mut self) {
        for (_, task) in self.stream_tasks.drain() {
            task.abort();
        }
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
                self.bridge
                    .voice_audio
                    .send_replace(VoiceAudioState::Connecting);
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.voice_cancel_tx = Some(tx);
                self.voice_task = Some(tokio::spawn(voice_audio::run(
                    credentials,
                    rx,
                    self.bridge.voice_audio.clone(),
                    self.bridge.voice_controls.subscribe(),
                    self.bridge.voice_controls.clone(),
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
                Some(SelectionIntent::PrivateChannel(channel)) => {
                    self.tracker
                        .navigation
                        .select_private_channel(&self.tracker.store, channel);
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
        let navigation = self.tracker.navigation.snapshot_with_private(
            &self.tracker.store,
            request.guilds.start,
            request.guilds.count,
            request.channels.start,
            request.channels.count,
            request.private_channels.start
                ..request
                    .private_channels
                    .start
                    .saturating_add(request.private_channels.count),
        );
        self.bridge.clear_private_notice_on_selection_change(
            navigation
                .private_selection
                .as_ref()
                .map(|selection| selection.channel_id),
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
        for (_, task) in self.stream_tasks.drain() {
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
    let voice_audio = bridge.voice_audio.subscribe();
    let voice_controls = bridge.voice_controls.subscribe();
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
        voice_audio,
        voice_controls,
        commands,
        voice_flags: (false, false),
        revision: 0,
        voice_correlator: None,
        voice_task: None,
        voice_cancel_tx: None,
        private_list_generation: 0,
        private_list_task: None,
        private_open_task: None,
        pending_private_opens: VecDeque::new(),
        private_list_loaded_generation: None,
        stream_correlator: None,
        stream_tasks: HashMap::new(),
        stream_intents: StreamIntentState::default(),
        pending_stream_signal: None,
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
            let mut voice_changed = false;
            let (status, requested) = tokio::select! {
                event = gateway.next_event() => {
                    let event = event?;
                    let correlation = worker.voice_dispatch(&event);
                    let stream_correlation = worker.stream_dispatch(&event);
                    let status = worker.apply_gateway_event(event, |worker| {
                        worker.start_private_list_fetch();
                    });
                    if let Some(correlation) = correlation {
                        worker.handle_correlation(correlation).await;
                    }
                    if let Some(correlation) = stream_correlation {
                        worker.handle_stream_correlation(correlation).await;
                    }
                    (status, false)
                }
                input = worker.next_input() => {
                    let input = input?;
                    match input {
                        WorkerInput::Request => (None, true),
                        WorkerInput::VoiceAudio => {
                            if let Some(leave) = worker.voice_audio_changed() {
                                gateway.voice_state().request(leave);
                            }
                            voice_changed = true;
                            (None, false)
                        }
                        WorkerInput::VoiceControls => {
                            worker.voice_controls_changed(&gateway);
                            voice_changed = true;
                            (None, false)
                        }
                        WorkerInput::VoiceRequest => {
                            worker.stop_voice().await;
                            if worker.request_voice_state(&gateway) {
                                (None, true)
                            } else {
                                (None, false)
                            }
                        }
                        WorkerInput::History(completed) => {
                            let rejected = worker.tracker.history.complete(&worker.rest, completed);
                            (rejected.then_some(GatewayStatus::AuthenticationRequired), false)
                        }
                        WorkerInput::Command(Command::OpenPrivate(recipients)) => {
                            worker.queue_private_open(recipients);
                            (None, false)
                        }
                        WorkerInput::Command(Command::Stream(intent)) => {
                            worker.stream_command(&gateway, intent);
                            (None, false)
                        }
                        WorkerInput::Command(command) => {
                            worker.command(command);
                            (None, false)
                        }
                        WorkerInput::PrivateList(generation, result) => {
                            let status = worker.finish_private_list_fetch(generation, result);
                            (status, false)
                        }
                        WorkerInput::PrivateOpen(generation, result) => {
                            let status = worker.finish_private_open(generation, result);
                            worker.start_next_private_open();
                            (status, false)
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(10)),
                    if worker.pending_stream_signal.is_some() =>
                {
                    if let Some(intent) = worker.pending_stream_signal.take() {
                        let sent = send_or_retain_stream_intent(
                            &mut worker.pending_stream_signal,
                            intent.clone(),
                            |intent| Worker::send_stream_intent(&gateway, intent),
                        );
                        if sent {
                            worker.record_stream_intent(&intent);
                        }
                    }
                    (None, false)
                }
            };
            if !requested
                && !worker.tracker.navigation_dirty
                && !worker.tracker.history.dirty()
                && status.is_none()
            {
                // Voice controls are read straight from the bridge's watches, so a
                // voice-only change needs a (coalesced) redraw but no snapshot rebuild.
                if voice_changed && worker.bridge.notify_voice() {
                    worker.gateway = Some(gateway);
                    return Some(((), worker));
                }
                continue;
            }
            worker.tracker.navigation_dirty = false;
            let request = worker.bridge.take_request();
            let terminal = matches!(
                status,
                Some(GatewayStatus::Stopped(_)) | Some(GatewayStatus::AuthenticationRequired)
            );
            let snapshot_notification = worker.refresh(&gateway, request, status);
            let notice_notification = worker.bridge.take_private_notice_pending();
            let notified = notice_notification || snapshot_notification;
            if terminal {
                worker.start = None;
                worker.stop_voice().await;
                worker.stop_all_streams();
                return notified.then_some(((), worker));
            }
            if notified {
                worker.gateway = Some(gateway);
                return Some(((), worker));
            }
        }
    })
}

#[derive(Debug, Default)]
pub struct PrivateConversationUi {
    pub recipients: String,
    pub notice: Option<String>,
}

/// Messages from an earlier session (identified by `id`) are ignored.
pub struct GatewayPanel {
    pub id: u64,
    pub status: GatewayStatus,
    pub navigation: Arc<NavigationSnapshot>,
    pub timeline: Arc<timeline::Snapshot>,
    pub controls: NavigationBridge,
    pub voice_guild: Option<Snowflake>,
    /// The draft and composer state; local to the UI until the user sends.
    pub composer: Composer,
    /// The user's message (channel, message) whose deletion waits for confirmation.
    pub confirm_delete: Option<(Snowflake, Snowflake)>,
    pub private: Box<PrivateConversationUi>,
    _worker: Handle,
    pub attachments: Option<AttachmentCache>,
    /// Render state for the account's cached/downloaded attachment previews.
    pub attachment_views: HashMap<crate::attachments::AttachmentKey, AttachmentView>,
    /// Abortable native download/open workers, keyed by attachment and action.
    pub attachment_action_workers:
        HashMap<(crate::attachments::AttachmentKey, AttachmentAction), Handle>,
    /// Ready thumbnails, at most four, oldest first; each is at most a 360x270
    /// RGBA buffer (under 400 KiB) shared with the decoded-image cache.
    pub attachment_ready_order: VecDeque<crate::attachments::AttachmentKey>,
    /// Abortable workers for active preview loads; bounded to four per account.
    pub attachment_workers: HashMap<crate::attachments::AttachmentKey, Handle>,
    /// The larger view of one image, if open; dropping it cancels its decode.
    pub attachment_viewer: Option<crate::attachment_ui::AttachmentViewer>,
    pub attachment_notice: Option<&'static str>,
}

impl fmt::Debug for GatewayPanel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayPanel")
            .field("id", &self.id)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Whether `key` names an attachment of a message among the open channel's
/// built timeline rows. Work for anything else is cancelled.
fn is_attachment_in(
    timeline: &timeline::Snapshot,
    key: &crate::attachments::AttachmentKey,
) -> bool {
    timeline.channel_id == Some(key.channel)
        && timeline.rows.iter().any(|row| {
            row.message.id == key.message
                && row
                    .message
                    .attachments
                    .iter()
                    .any(|attachment| attachment.id == key.attachment)
        })
}

impl GatewayPanel {
    pub fn new(id: u64, worker: Handle, controls: NavigationBridge) -> Self {
        Self {
            id,
            status: GatewayStatus::Connecting { attempt: 0 },
            navigation: Arc::default(),
            timeline: Arc::default(),
            controls,
            private: Box::default(),
            voice_guild: None,
            composer: Composer::default(),
            confirm_delete: None,
            _worker: worker.abort_on_drop(),
            attachments: None,
            attachment_views: HashMap::new(),
            attachment_workers: HashMap::new(),
            attachment_action_workers: HashMap::new(),
            attachment_ready_order: VecDeque::new(),
            attachment_viewer: None,
            attachment_notice: None,
        }
    }
    pub fn with_attachment_cache(mut self, attachments: Option<AttachmentCache>) -> Self {
        self.attachments = attachments;
        self
    }

    /// Takes the worker's latest snapshots. Drafts and editing follow the open
    /// channel; a deletion waiting for confirmation does not survive leaving it.
    pub fn consume(&mut self) -> Option<GatewayStatus> {
        let consumed = self.controls.consume();
        self.navigation = consumed.navigation;
        self.timeline = consumed.timeline;
        let open_channel = self.timeline.channel_id;
        let timeline = &self.timeline;
        let still_loaded =
            |key: &crate::attachments::AttachmentKey| is_attachment_in(timeline, key);
        self.attachment_views.retain(|key, _| still_loaded(key));
        self.attachment_workers.retain(|key, _| still_loaded(key));
        self.attachment_action_workers
            .retain(|(key, _), _| still_loaded(key));
        let attachment_views = &self.attachment_views;
        self.attachment_ready_order.retain(|key| {
            still_loaded(key) && matches!(attachment_views.get(key), Some(AttachmentView::Ready(_)))
        });
        // The larger view is modal and stays while its row scrolls out of the
        // built window, but never outlives its channel.
        if self
            .attachment_viewer
            .as_ref()
            .is_some_and(|viewer| Some(viewer.key.channel) != open_channel)
        {
            self.attachment_viewer = None;
        }
        self.private.notice = consumed.private_notice;
        self.composer.select(self.timeline.channel_id);
        if self
            .confirm_delete
            .is_some_and(|(channel, _)| self.timeline.channel_id != Some(channel))
        {
            self.confirm_delete = None;
        }
        consumed.status
    }

    /// Whether the attachment's message is still among the rows being built.
    pub fn is_attachment_loaded(&self, key: &crate::attachments::AttachmentKey) -> bool {
        is_attachment_in(&self.timeline, key)
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
    fn explicit_stream_intents_reach_the_bounded_worker_queue_in_order() {
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let key = "guild:41:127:1";
        assert!(bridge.create_stream(StreamCreateRequest::guild(Snowflake(41), Snowflake(127),)));
        assert!(bridge.watch_stream(key));
        assert!(bridge.pause_stream(key, true));
        assert!(bridge.ping_stream(key));
        assert!(bridge.unwatch_stream(key));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Stream(StreamIntent::Create(_)))
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Stream(StreamIntent::Watch(actual))) if actual == key
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Stream(StreamIntent::PauseResume { stream_key, paused }))
                if stream_key == key && paused
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Stream(StreamIntent::Ping(actual))) if actual == key
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Stream(StreamIntent::Unwatch(actual))) if actual == key
        ));
        assert!(commands.try_recv().is_err());
    }
    #[test]
    fn a_full_gateway_signal_queue_retains_the_accepted_app_intent_for_retry() {
        let mut pending = None;
        let intent = StreamIntent::Unwatch("guild:41:127:1".into());
        assert!(!send_or_retain_stream_intent(&mut pending, intent, |_| {
            false
        },));
        assert!(matches!(
            pending.as_ref(),
            Some(StreamIntent::Unwatch(key)) if key == "guild:41:127:1"
        ));
        let retry = pending.take().unwrap();
        assert!(send_or_retain_stream_intent(&mut pending, retry, |_| true,));
        assert!(pending.is_none());
    }
    #[test]
    fn local_unwatch_cancels_all_pending_creates_and_gates_late_event_orders() {
        let request = StreamCreateRequest::guild(Snowflake(41), Snowflake(127));
        let key = StreamKey::from_wire("guild:41:127:1").unwrap();
        let mut intents = StreamIntentState::default();
        intents.ready(Snowflake(1));
        intents.record_create(&request);
        intents.record_create(&request);
        intents.retire(&key);
        assert!(intents.creates.is_empty());
        // Each of these represents one side of the event pair arriving late.
        assert!(!intents.admit(&key));
        assert!(!intents.admit(&key));

        let mut before_ready = StreamIntentState::default();
        before_ready.record_create(&request);
        before_ready.retire(&key);
        before_ready.ready(Snowflake(1));
        assert!(!before_ready.admit(&key));

        // A later explicit create is the only owner intent that clears retirement.
        intents.record_create(&request);
        assert!(intents.admit(&key));
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
    fn attachment_work_follows_the_built_rows_and_the_viewer_follows_its_channel() {
        use crate::attachments::AttachmentKey;
        use timeline::AttachmentView;
        let bridge = NavigationBridge::new();
        let mut panel = panel(&bridge);
        let with_attachment = |channel: u64, message: u64| {
            let message: fastcord_model::Message = serde_json::from_value(serde_json::json!({
                "id": message.to_string(),
                "channel_id": channel.to_string(),
                "author": {"id": "42", "username": "alt"},
                "content": "",
                "timestamp": "2026-10-08T09:00:00.000000+00:00",
                "attachments": [{
                    "id": "9", "filename": "cat.png", "size": 10,
                    "url": "https://cdn.example/cat.png"
                }],
            }))
            .unwrap();
            timeline::Snapshot {
                channel_id: Some(Snowflake(channel)),
                rows: vec![timeline::Row::new(Arc::new(message), 1)],
                ..timeline::Snapshot::default()
            }
        };
        let key = |channel: u64, message: u64| AttachmentKey {
            account: Snowflake(42),
            channel: Snowflake(channel),
            message: Snowflake(message),
            attachment: Snowflake(9),
        };
        let handle = || iced::Task::<()>::none().abortable().1.abort_on_drop();
        let attachment = with_attachment(500, 7).rows[0].message.attachments[0].clone();

        bridge.publish(NavigationSnapshot::default(), with_attachment(500, 7), None);
        panel.consume();
        for message in [7, 8] {
            let key = key(500, message);
            panel.attachment_views.insert(key, AttachmentView::Loading);
            panel.attachment_workers.insert(key, handle());
            panel
                .attachment_action_workers
                .insert((key, AttachmentAction::Open), handle());
        }
        panel.attachment_viewer = Some(crate::attachment_ui::AttachmentViewer::for_test(
            key(500, 8),
            attachment,
        ));
        // Message 8 is no longer among the built rows: its previews and workers
        // are cancelled; the modal view stays while the channel is open.
        bridge.publish(NavigationSnapshot::default(), with_attachment(500, 7), None);
        panel.consume();
        assert_eq!(panel.attachment_views.len(), 1);
        assert!(panel.attachment_views.contains_key(&key(500, 7)));
        assert_eq!(panel.attachment_workers.len(), 1);
        assert_eq!(panel.attachment_action_workers.len(), 1);
        assert!(panel.is_attachment_loaded(&key(500, 7)));
        assert!(!panel.is_attachment_loaded(&key(500, 8)));
        assert!(panel.attachment_viewer.is_some());
        // Leaving the channel cancels everything, the viewer included.
        bridge.publish(NavigationSnapshot::default(), with_attachment(501, 7), None);
        panel.consume();
        assert!(panel.attachment_views.is_empty());
        assert!(panel.attachment_workers.is_empty());
        assert!(panel.attachment_action_workers.is_empty());
        assert!(panel.attachment_viewer.is_none());
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
    fn worker() -> Worker {
        let bridge = NavigationBridge::new();
        let rest =
            RestClient::new(UserToken::new("offline-fixture-credential".to_owned())).unwrap();
        Worker {
            start: None,
            rest: rest.clone(),
            gateway: None,
            tracker: Tracker::default(),
            requests: bridge.requests.subscribe(),
            voice_requests: bridge.voice_requests.subscribe(),
            voice_audio: bridge.voice_audio.subscribe(),
            voice_controls: bridge.voice_controls.subscribe(),
            voice_flags: (false, false),
            commands: bridge.take_commands().unwrap(),
            revision: 0,
            voice_correlator: None,
            voice_task: None,
            voice_cancel_tx: None,
            private_list_generation: 0,
            private_list_loaded_generation: None,
            private_list_task: None,
            private_open_task: None,
            pending_private_opens: VecDeque::new(),
            stream_correlator: None,
            stream_tasks: HashMap::new(),
            stream_intents: StreamIntentState::default(),
            pending_stream_signal: None,
            bridge,
        }
    }
    fn private_channel(id: u64) -> Channel {
        Channel {
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
        }
    }

    #[test]
    fn private_list_fetch_failure_retries_on_the_next_ready_generation() {
        let mut worker = worker();
        worker
            .bridge
            .set_private_notice("previous session failed".to_owned());
        worker.begin_private_list_fetch();
        assert!(worker.bridge.consume().private_notice.is_none());
        worker.tracker.apply(ready_event(0, 0, 0));
        let first_generation = worker.begin_private_list_fetch();
        assert_eq!(
            worker.finish_private_list_fetch(first_generation, Err("offline".to_owned())),
            None
        );
        assert_eq!(worker.private_list_loaded_generation, None);
        assert!(
            worker
                .bridge
                .consume()
                .private_notice
                .as_deref()
                .unwrap()
                .contains("offline")
        );

        worker.tracker.apply(ready_event(0, 0, 0));
        let second_generation = worker.begin_private_list_fetch();
        assert_ne!(first_generation, second_generation);
        assert_eq!(
            worker.finish_private_list_fetch(second_generation, Ok(vec![private_channel(300)]),),
            None
        );
        assert_eq!(
            worker.private_list_loaded_generation,
            Some(second_generation)
        );
        assert!(
            worker
                .tracker
                .store
                .private_channel(Snowflake(300))
                .is_some()
        );
        assert!(worker.bridge.consume().private_notice.is_none());
    }

    #[tokio::test]
    async fn late_list_failure_marks_the_worker_dirty_for_notice_publication() {
        let mut worker = worker();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        worker.apply_gateway_event(ready_event(0, 0, 0), |worker| {
            worker.start_private_list_fetch_with(async move { receiver.await.unwrap() });
        });
        // READY's snapshot was already published before its list request failed.
        worker.tracker.navigation_dirty = false;

        sender.send(Err("offline".to_owned())).unwrap();
        let WorkerInput::PrivateList(generation, result) = worker.next_input().await.unwrap()
        else {
            panic!("expected the stalled list completion");
        };
        worker.finish_private_list_fetch(generation, result);
        assert!(
            worker.tracker.navigation_dirty,
            "the worker must publish the new notice even after READY was already consumed"
        );
    }

    #[test]
    fn late_private_list_result_cannot_overwrite_a_new_ready_generation() {
        let mut worker = worker();
        worker.tracker.apply(ready_event(0, 0, 0));
        let stale_generation = worker.begin_private_list_fetch();
        worker.tracker.apply(ready_event(0, 0, 0));
        let current_generation = worker.begin_private_list_fetch();

        worker.finish_private_list_fetch(stale_generation, Ok(vec![private_channel(301)]));
        assert!(
            worker
                .tracker
                .store
                .private_channel(Snowflake(301))
                .is_none()
        );
        assert_eq!(
            worker.finish_private_open(stale_generation, Ok(private_channel(303))),
            None
        );
        assert!(
            worker
                .tracker
                .store
                .private_channel(Snowflake(303))
                .is_none()
        );
        worker.finish_private_list_fetch(current_generation, Ok(vec![private_channel(302)]));
        assert!(
            worker
                .tracker
                .store
                .private_channel(Snowflake(302))
                .is_some()
        );
        assert_eq!(
            worker.private_list_loaded_generation,
            Some(current_generation)
        );
    }
    #[tokio::test]
    async fn stalled_private_list_does_not_block_ready_voice_or_commands() {
        let mut worker = worker();
        worker.rest.stop_authenticated_work();
        let (old_sender, old_receiver) = tokio::sync::oneshot::channel();
        let generation = worker.apply_gateway_event(ready_event(0, 0, 0), |worker| {
            worker.start_private_list_fetch_with(async move { old_receiver.await.unwrap() });
        });
        assert_eq!(generation, None);
        let old_generation = worker.private_list_generation;

        assert!(worker.bridge.open_private_channel(vec![Snowflake(90)]));
        assert!(worker.bridge.send_message(
            Snowflake(300),
            "fixture command".to_owned(),
            false,
            None
        ));
        worker
            .bridge
            .voice_requests
            .send_replace(Some(VoiceStateRequest::join(
                Some(Snowflake(1)),
                Snowflake(2),
                false,
                false,
            )));

        let (mut saw_open, mut saw_send, mut saw_voice, mut saw_open_result) =
            (false, false, false, false);
        for _ in 0..8 {
            match worker.next_input().await.unwrap() {
                WorkerInput::Command(Command::OpenPrivate(recipients)) => {
                    worker.queue_private_open(recipients);
                    saw_open = true;
                }
                WorkerInput::Command(command @ Command::Send { .. }) => {
                    worker.command(command);
                    saw_send = true;
                }
                WorkerInput::VoiceRequest => saw_voice = true,
                WorkerInput::PrivateOpen(generation, result) => {
                    worker.finish_private_open(generation, result);
                    worker.start_next_private_open();
                    saw_open_result = true;
                }
                WorkerInput::History(completed) => {
                    worker.tracker.history.complete(&worker.rest, completed);
                }
                _ => panic!("unexpected worker input while list REST is stalled"),
            }
            if saw_open && saw_send && saw_voice && saw_open_result {
                break;
            }
        }
        assert!(saw_open && saw_send && saw_voice && saw_open_result);
        assert!(worker.tracker.history.dirty());

        let (new_sender, new_receiver) = tokio::sync::oneshot::channel();
        worker.apply_gateway_event(ready_event(0, 0, 0), |worker| {
            worker.start_private_list_fetch_with(async move { new_receiver.await.unwrap() });
        });
        assert_ne!(worker.private_list_generation, old_generation);
        assert!(
            old_sender.send(Ok(vec![private_channel(301)])).is_err(),
            "a later READY cancels the stale pending REST future"
        );

        new_sender.send(Ok(vec![private_channel(302)])).unwrap();
        loop {
            match worker.next_input().await.unwrap() {
                WorkerInput::PrivateList(generation, result) => {
                    worker.finish_private_list_fetch(generation, result);
                    break;
                }
                WorkerInput::History(completed) => {
                    worker.tracker.history.complete(&worker.rest, completed);
                }
                _ => panic!("unexpected worker input before current list completion"),
            }
        }
        assert!(
            worker
                .tracker
                .store
                .private_channel(Snowflake(302))
                .is_some()
        );
        assert!(
            worker
                .tracker
                .store
                .private_channel(Snowflake(301))
                .is_none()
        );
    }

    #[test]
    fn changing_conversation_clears_a_stale_private_notice() {
        let bridge = NavigationBridge::new();
        bridge.set_private_notice("request failed".to_owned());
        bridge.select_private_channel(Snowflake(300));
        assert!(bridge.consume().private_notice.is_none());
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
    #[test]
    fn mute_and_deafen_precedence_preserves_self_mute() {
        let mut controls = VoiceControls::default();
        assert!(controls.capture_enabled());
        assert!(controls.output_enabled());
        controls.deafened = true;
        assert!(!controls.capture_enabled());
        assert!(!controls.output_enabled());
        controls.deafened = false;
        assert!(controls.capture_enabled());
        controls.self_muted = true;
        assert!(!controls.capture_enabled());
        assert!(controls.output_enabled());
        controls.self_muted = false;
        controls.server_muted = true;
        assert!(!controls.capture_enabled());
        assert!(controls.output_enabled());
        controls.server_muted = false;
        controls.server_deafened = true;
        assert!(controls.capture_enabled());
        assert!(!controls.output_enabled());
    }
    #[test]
    fn volume_controls_are_bounded_and_mute_deafen_state_is_independent() {
        let bridge = NavigationBridge::new();
        bridge.set_self_muted(true);
        bridge.set_deafened(true);
        assert!(bridge.voice_controls().self_muted);
        assert!(bridge.voice_controls().deafened);
        bridge.set_deafened(false);
        assert!(bridge.voice_controls().self_muted);
        for user in 0..256 {
            assert!(bridge.set_volume_percent(Snowflake(user), user as u16 % 201));
        }
        assert!(bridge.set_volume_percent(Snowflake(1_000), 200));
        let controls = bridge.voice_controls();
        assert_eq!(controls.volumes.len(), 256);
        assert_eq!(controls.volume_percent(Snowflake(1_000)), 200);
        assert_eq!(controls.volume_percent(Snowflake(0)), 100);
    }

    fn join(channel: u64) -> VoiceStateRequest {
        VoiceStateRequest::join(Some(Snowflake(7)), Snowflake(channel), false, false)
    }

    #[test]
    fn selecting_the_joined_voice_channel_again_keeps_the_session() {
        let bridge = NavigationBridge::new();
        let mut requests = bridge.voice_requests.subscribe();
        assert!(bridge.request_voice_state(join(70)));
        assert_eq!(bridge.voice_audio_state(), VoiceAudioState::Connecting);
        requests.borrow_and_update();
        assert!(!bridge.request_voice_state(join(70)));
        assert_eq!(bridge.voice_audio_state(), VoiceAudioState::Connecting);
        assert!(!requests.has_changed().unwrap());
        bridge.voice_audio.send_replace(VoiceAudioState::Running);
        requests.borrow_and_update();

        // The Gateway would not resend an identical opcode 4, so the worker must not be
        // told to tear the call down and wait for credentials that never come.
        assert!(!bridge.request_voice_state(join(70)));
        assert_eq!(bridge.voice_audio_state(), VoiceAudioState::Running);
        assert!(!requests.has_changed().unwrap());

        // A different channel is a real move.
        bridge.request_voice_state(join(71));
        assert_eq!(bridge.voice_audio_state(), VoiceAudioState::Connecting);
        assert!(requests.has_changed().unwrap());

        // A failed engine left the Gateway voice state, so the same channel joins again.
        bridge
            .voice_audio
            .send_replace(VoiceAudioState::EngineFailed);
        requests.borrow_and_update();
        bridge.request_voice_state(join(71));
        assert_eq!(bridge.voice_audio_state(), VoiceAudioState::Connecting);
        assert!(requests.has_changed().unwrap());
    }

    #[test]
    fn a_failed_engine_leaves_the_gateway_voice_state() {
        let mut worker = worker();
        worker.bridge.request_voice_state(join(70));
        worker.voice_requests.borrow_and_update();
        worker
            .bridge
            .voice_audio
            .send_replace(VoiceAudioState::Running);
        assert_eq!(worker.voice_audio_changed(), None);
        assert_eq!(
            worker.voice_requests.borrow().unwrap().channel_id,
            Some(Snowflake(70))
        );

        worker
            .bridge
            .voice_audio
            .send_replace(VoiceAudioState::EngineFailed);
        assert_eq!(
            worker.voice_audio_changed(),
            Some(VoiceStateRequest::leave(Some(Snowflake(7))))
        );
        let request = worker.voice_requests.borrow().unwrap();
        assert_eq!(request.guild_id, Some(Snowflake(7)));
        assert_eq!(request.channel_id, None);
        // The failure stays visible; only the Gateway voice state is left.
        assert_eq!(
            worker.bridge.voice_audio_state(),
            VoiceAudioState::EngineFailed
        );
    }

    fn participant(user: u64, speaking: bool) -> VoiceParticipant {
        VoiceParticipant {
            user_id: Snowflake(user),
            ssrc: Some(user as u32),
            speaking,
        }
    }

    /// Races participant, server-restriction, and UI writes to the shared controls watch.
    /// Each writer must retain the fields owned by the other two tasks.
    fn race_with_publisher(bridge: &NavigationBridge, ui: impl FnOnce(&NavigationBridge) + Send) {
        let start = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut speaking = false;
                start.wait();
                for _ in 0..200_000 {
                    speaking = !speaking;
                    let participants = HashMap::from([
                        (Snowflake(1), participant(1, speaking)),
                        (Snowflake(2), participant(2, !speaking)),
                    ]);
                    voice_audio::publish_participants(&bridge.voice_controls, &participants);
                }
            });
            scope.spawn(|| {
                start.wait();
                for _ in 0..200_000 {
                    bridge.voice_controls.send_modify(|controls| {
                        controls.server_muted = true;
                        controls.server_deafened = true;
                    });
                    let controls = bridge.voice_controls();
                    assert!(controls.server_muted, "server mute was overwritten");
                    assert!(controls.server_deafened, "server deafen was overwritten");
                }
            });
            start.wait();
            ui(bridge);
        });
    }

    #[test]
    fn a_speaking_publish_never_overwrites_a_concurrent_mute() {
        let bridge = NavigationBridge::new();
        race_with_publisher(&bridge, |bridge| {
            for _ in 0..200_000 {
                bridge.set_self_muted(true);
                assert!(bridge.voice_controls().self_muted, "mute was overwritten");
                bridge.set_self_muted(false);
                bridge.set_deafened(true);
                assert!(bridge.voice_controls().deafened, "deafen was overwritten");
                bridge.set_deafened(false);
            }
        });
    }

    #[test]
    fn a_participants_publish_never_overwrites_a_concurrent_volume_change() {
        let bridge = NavigationBridge::new();
        race_with_publisher(&bridge, |bridge| {
            for round in 0..200_000u32 {
                let percent = (round % 200) as u16;
                bridge.set_volume_percent(Snowflake(2), percent);
                let controls = bridge.voice_controls();
                assert_eq!(
                    controls.volume_percent(Snowflake(2)),
                    percent,
                    "volume was overwritten"
                );
            }
        });
        assert_eq!(bridge.voice_controls().participants.len(), 2);
    }
}
