//! UI-side view of the account's Gateway connection. The protocol, session,
//! heartbeats, and reconnects live in `fastcord_discord::gateway::Gateway`; the
//! account's normalized state lives in `fastcord_discord::state::Store`, which
//! the worker below owns and is the only writer of (the reducer). This module
//! turns the ordered events into that state plus the few status changes the
//! screen shows, and is dropped, with the connection and the state it owns,
//! when the account screen is left (which closes the Gateway session cleanly).

mod camera;
mod share_audio;
mod stream_audio;
mod stream_share;
mod stream_speaker;
mod stream_video;
#[cfg(test)]
pub(crate) use stream_video::MAX_QUEUED_BYTES as MAX_QUEUED_VIDEO_BYTES;
pub(crate) use stream_video::open_h264;
mod voice_audio;

pub use camera::{
    CameraChoice, CameraControls, CameraList, CameraStatus, CameraTiles, TileVisibility, TilesView,
    can_send as can_send_camera, describe as describe_camera, list_cameras, preview_caption,
};
pub use share_audio::AudioOffer;
pub use stream_share::{SharePicker, ShareView};
pub use voice_audio::VoiceAudioState;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fastcord_video::effect::Effect;

use crate::i18n::{tr, trf, trn};

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

/// The Stage side of the current call, derived by the reducer from the voice
/// states, the channel's permissions and its live session. `Default` outside a
/// Stage channel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StageControls {
    /// The call is in a Stage channel.
    pub active: bool,
    /// The account is in the audience, or its own voice state is not known yet:
    /// the microphone stays closed until the server says it may speak.
    pub suppressed: bool,
    /// The account has a raised hand.
    pub requested: bool,
    pub permissions: fastcord_discord::StagePermissions,
    /// The live session's topic; `None` while no session runs.
    pub topic: Option<String>,
    pub guild: Option<Snowflake>,
    pub channel: Option<Snowflake>,
    /// Who sits where: speakers, then raised hands, then the audience; at most
    /// [`MAX_VOICE_NAMES`].
    pub seats: Vec<(Snowflake, StageSeat)>,
}

impl StageControls {
    pub fn seat(&self, user: Snowflake) -> Option<StageSeat> {
        self.seats
            .iter()
            .find(|(id, _)| *id == user)
            .map(|(_, s)| *s)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VoiceControls {
    pub stage: StageControls,
    /// Names of the embedded Activities running in the call's channel (read-only awareness,
    /// issue #102; empty string when unnamed), at most `MAX_PER_CHANNEL`.
    pub activities: Vec<String>,
    pub self_muted: bool,
    pub deafened: bool,
    pub server_muted: bool,
    pub server_deafened: bool,
    pub participants: Vec<VoiceParticipant>,
    /// Display names of the participants and stream owners the state store
    /// knows, at most [`MAX_VOICE_NAMES`]; resolved by the reducer's worker.
    pub names: Vec<(Snowflake, String)>,
    /// Avatar hashes of the same users (those that have one), for the call bar.
    pub avatars: Vec<(Snowflake, String)>,
    pub streams: Arc<[StreamOffer]>,
    pub volumes: Vec<UserVolume>,
    pub input_device: fastcord_audio::DeviceChoice,
    pub output_device: fastcord_audio::DeviceChoice,
    pub input_generation: u64,
    pub output_generation: u64,
    pub input_error: Option<fastcord_audio::DeviceErrorKind>,
    pub output_error: Option<fastcord_audio::DeviceErrorKind>,
    pub noise_suppression: fastcord_audio::NoiseSuppression,
    pub noise_generation: u64,
    /// The running engine's actual state; `None` outside a call.
    pub noise_status: Option<fastcord_audio::NoiseSuppressionStatus>,
    /// Echo cancellation is on unless this is set (so the derived default is on).
    pub echo_cancellation_off: bool,
    /// Playback underrun/concealment counters of the running engine; `None` outside a call.
    pub audio_stats: Option<fastcord_audio::EngineStats>,
    /// The microphone is sending a speech burst now (the account's own
    /// speaking ring); flips only at the start and end of a burst.
    pub self_speaking: bool,
    /// The call connection's packet counters, kept only while the audio
    /// dialog is open (so an idle call never redraws for them).
    pub transport_stats: Option<fastcord_media::gateway::TransportStats>,
    /// The latest UDP round trip to the voice server in whole milliseconds;
    /// `None` until the first ping answer and outside a call.
    pub voice_ping_ms: Option<u32>,
    /// Manual and automatic microphone gain (the saved preference).
    pub input_gain: fastcord_audio::InputGain,
    /// The audio dialog is open, so the running engine measures the microphone level.
    pub diagnostics_open: bool,
    /// The latest microphone level and echo-canceller state; `Some` only while the dialog is
    /// open during a call.
    pub input_diagnostics: Option<fastcord_audio::InputDiagnostics>,
    /// How the microphone is opened to the call (the saved preference).
    pub input_mode: InputMode,
    /// The channel withholds "use voice activity", so push-to-talk is forced.
    pub vad_forbidden: bool,
    /// The joined guild voice or Stage channel's `bitrate` as the server reports it; 0 in
    /// a DM call or while unknown. [`Self::send_bitrate`] is what the microphone uses.
    pub channel_bitrate: u32,
    /// The push-to-talk key is held now.
    pub ptt_down: bool,
    /// Counts forced releases (focus, listener, device or session loss); the call worker
    /// drops a held push-to-talk at once, with no release delay, when it changes.
    pub ptt_cancel: u64,
    /// The push-to-mute key is held now.
    pub push_to_mute_down: bool,
    /// How long push-to-talk keeps transmitting after the key is released.
    pub ptt_release_ms: u16,
    /// Voice-activity threshold as `-n` dBFS; `None` is automatic (nothing is withheld).
    pub sensitivity: Option<u8>,
    /// The camera of the current call: what the user asked for and what the call
    /// reports back (issue #84).
    pub camera: CameraControls,
    /// Soundboard plays heard in the current call, oldest first, at most [`SOUND_CUES`]
    /// (issue #87). The UI plays each `seq` it has not seen; older ones are dropped.
    pub sound_cues: Vec<SoundCue>,
    /// Counts plays and catalog changes; every cue and change carries the value it set.
    pub sound_seq: u64,
    /// The latest soundboard catalog change: the server and the `sound_seq` it set.
    pub sound_catalog_change: Option<(Snowflake, u64)>,
    /// The latest sticker change: the server and the `sound_seq` it set.
    pub sticker_change: Option<(Snowflake, u64)>,
}

/// Most soundboard plays kept for the UI between two looks; a burst keeps the newest.
pub const SOUND_CUES: usize = 4;

/// Someone played a server sound in the call the account is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoundCue {
    pub seq: u64,
    pub user: Snowflake,
    pub sound: Snowflake,
    /// The sound's own volume, 0 to 100.
    pub volume_percent: u8,
}

/// A Go Live offer in the parent voice channel; at most 32, keys only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamOffer {
    pub key: String,
    pub owner: Snowflake,
}
/// Most names (and so participants plus stream offers) a snapshot carries.
pub const MAX_VOICE_NAMES: usize = 64;
const MAX_NAME_CHARS: usize = 48;

impl VoiceControls {
    /// The server nickname or display name, or `User NNNN` while unknown.
    pub fn display_name(&self, user: Snowflake) -> String {
        self.names
            .iter()
            .find(|(id, _)| *id == user)
            .map_or_else(|| trf("User {}", &[&(user.0 % 10_000)]), |(_, n)| n.clone())
    }

    pub fn avatar_key(&self, user: Snowflake) -> crate::avatars::AvatarKey {
        let hash = self.avatars.iter().find(|(id, _)| *id == user);
        crate::avatars::AvatarKey::user(user, hash.map(|(_, hash)| hash.as_str()))
    }

    /// The microphone encoder's target: the channel's bitrate, clamped because it is
    /// network input, or 64 kbps for DM calls and channels without one.
    pub fn send_bitrate(&self) -> u32 {
        match self.channel_bitrate {
            0 => fastcord_audio::DEFAULT_BITRATE,
            bitrate => bitrate.clamp(8_000, 384_000),
        }
    }

    /// The mode in force: a channel without "use voice activity" always needs push-to-talk.
    pub fn effective_mode(&self) -> InputMode {
        if self.vad_forbidden {
            InputMode::PushToTalk
        } else {
            self.input_mode
        }
    }

    /// The one microphone gate. Stage audience (suppression), a server mute, self mute,
    /// deafen or held push-to-mute always closes it; push-to-talk mode additionally needs `ptt_active` (the key
    /// held, or inside its release delay).
    pub fn capture_enabled_with(&self, ptt_active: bool) -> bool {
        !self.stage.suppressed
            && !self.server_muted
            && !self.self_muted
            && !self.deafened
            && !self.push_to_mute_down
            && (self.effective_mode() == InputMode::VoiceActivity || ptt_active)
    }

    pub fn capture_enabled(&self) -> bool {
        self.capture_enabled_with(self.ptt_down)
    }

    /// The sensitivity threshold in force: only voice activity is gated by level.
    pub fn effective_sensitivity(&self) -> Option<u8> {
        (self.effective_mode() == InputMode::VoiceActivity)
            .then_some(self.sensitivity)
            .flatten()
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
use fastcord_discord::UploadFile;
use fastcord_discord::state::Store;
use fastcord_discord::state::navigation::StageSeat;
use fastcord_discord::state::navigation::{Navigation, NavigationSnapshot};
use fastcord_media::{Correlation, JoinCorrelator, StreamCorrelation, StreamCorrelator};
use fastcord_model::{
    Channel, Emoji, Snowflake, StreamCreateRequest, StreamKey, StreamType, VoiceStateRequest,
};
use tokio::sync::{mpsc, watch};

use crate::attachments::AttachmentCache;
use crate::composer::{Candidate, CandidateKind, Composer};
use crate::history::{Carried, Completed, History, Intent};
use crate::outbox::{OpId, Reply, Slots};
use crate::rich;
use crate::shortcuts::InputMode;
use crate::timeline;
use crate::timeline::AttachmentView;
use crate::variable_list::{Measurement, Viewport};
use crate::virtual_list::Window;
use crate::{emoji, emoji_picker};
use fastcord_discord::{
    Clock, MonotonicClock, PrivateChannelError, RestClient, TypingSend, TypingTable, UserToken,
};
use fastcord_platform::capture::CaptureSource;
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
            Self::Connecting { attempt: 0 } => tr("Connecting to Discord…").to_owned(),
            Self::Connecting { attempt } => {
                trf("Connecting to Discord… (attempt {})", &[&(attempt + 1)])
            }
            Self::SigningIn => tr("Connected; signing in…").to_owned(),
            Self::Resuming => tr("Connected; resuming your session…").to_owned(),
            Self::Ready(counts) => {
                let mut text = trf(
                    "Connected to Discord: {} servers, {} direct messages.",
                    &[&counts.guilds, &counts.direct_messages],
                );
                if counts.unavailable > 0 {
                    text.push(' ');
                    text.push_str(&trf("{} servers are unavailable.", &[&counts.unavailable]));
                }
                text
            }
            Self::Reconnecting {
                attempt,
                delay,
                reason,
            } => trf(
                "Connection lost ({}). Reconnecting in {} (attempt {}).",
                &[
                    &tr(describe_reason(*reason)),
                    &describe_delay(*delay),
                    attempt,
                ],
            ),
            Self::AuthenticationRequired => {
                tr("Discord no longer accepts this login. Log in again.").to_owned()
            }
            Self::Stopped(reason) => trf("Disconnected: {}", &[&tr(&reason.to_string())]),
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
        seconds if seconds < 0.5 => tr("a moment").to_owned(),
        seconds if seconds < 1.5 => tr("1 second").to_owned(),
        seconds => trn(seconds.round() as u64, "{} second", "{} seconds"),
    }
}

/// Opens `channel` of `guild`. A link, notification or search result may name
/// another server (or arrive from the DMs), so that server is opened first;
/// the channel is then checked against it as for any click.
fn open_guild_channel(
    navigation: &mut Navigation,
    store: &Store,
    guild: Snowflake,
    channel: Snowflake,
) {
    if navigation.selected_guild() == Some(guild) || navigation.select_guild(store, guild) {
        navigation.select_channel(store, channel);
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
    /// Who else is typing; expires on its own one-shot timer (see `Worker::typing_wait`).
    typing: TypingTable,
    /// The typing line changed: the snapshot is rebuilt.
    typing_dirty: bool,
    /// AutoMod rule changes the Gateway reported (issue #96).
    automod: fastcord_discord::AutoModFeed,
    clock: MonotonicClock,
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
                let automod_before = self.automod.latest();
                if let GatewayEvent::Dispatch { event, .. } = &dispatch {
                    self.history.apply(event);
                    self.apply_typing(event);
                    if let Dispatch::AutoModeration(change) = event {
                        self.automod.push((**change).clone());
                    }
                }
                let relevant = !matches!(
                    &dispatch,
                    GatewayEvent::Dispatch {
                        event: Dispatch::MessageCreate(_)
                            | Dispatch::MessageUpdate(_)
                            | Dispatch::MessageDelete(_)
                            | Dispatch::MessageDeleteBulk(_)
                            | Dispatch::MessageReactionAdd(_)
                            | Dispatch::MessageReactionRemove(_)
                            | Dispatch::MessageReactionRemoveAll(_)
                            | Dispatch::MessageReactionRemoveEmoji(_),
                        ..
                    }
                );
                if let GatewayEvent::Dispatch {
                    event: Dispatch::MessageCreate(message),
                    ..
                } = &dispatch
                {
                    let muted = self.store.is_muted(message.channel_id, message.guild_id);
                    // One rule for every consumer: ignored/blocked authors and
                    // message requests never raise a toast.
                    // "Nothing" is the account's own level for the place: not
                    // even a mention raises a toast. Suppressed @everyone and
                    // role mentions are not mentions.
                    let nothing = self
                        .store
                        .notify_level(message.channel_id, message.guild_id)
                        == fastcord_discord::gateway::NotifyLevel::Nothing;
                    let suppressed = nothing
                        || self.store.suppression(message.author.id).is_some()
                        || self
                            .store
                            .private_channel(message.channel_id)
                            .is_some_and(Channel::is_pending_request);
                    let me = self.store.current_user().map(|u| u.id);
                    let own_status =
                        me.and_then(|me| self.store.presences().get(&me).map(|p| p.status));
                    crate::notify::on_message(
                        message,
                        me,
                        muted,
                        suppressed,
                        self.store.notifies_account(message),
                        own_status == Some(fastcord_model::Status::Dnd),
                    );
                    crate::speech::on_message(
                        message,
                        me,
                        self.history.open_channel(),
                        own_status,
                        muted,
                        suppressed,
                    );
                }
                let changes = self.store.apply(dispatch);
                self.navigation_dirty = (relevant && !changes.is_empty())
                    || changes.read
                    || self.automod.latest() != automod_before
                    || self.store.limit_exceeded();
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

impl Tracker {
    /// Keeps the typing table: a started user shows (never the account's own),
    /// and a sent message, a deleted channel, or a new session clears.
    /// Every channel is recorded, so switching shows its typers at once, but
    /// only the open channel's changes rebuild the snapshot (#153).
    fn apply_typing(&mut self, event: &Dispatch) {
        let now = self.clock.now();
        let open = self.history.open_channel();
        let changed = match event {
            Dispatch::TypingStart(start) => {
                if self.store.current_user().map(|u| u.id) == Some(start.user_id) {
                    false
                } else {
                    self.typing.start(now, start.channel_id, start.user_id);
                    open == Some(start.channel_id)
                }
            }
            Dispatch::MessageCreate(message) => {
                self.typing.stop(message.channel_id, message.author.id)
                    && open == Some(message.channel_id)
            }
            Dispatch::ChannelDelete(channel) => {
                self.typing.clear_channel(channel.id) && open == Some(channel.id)
            }
            Dispatch::Ready(_) | Dispatch::Resumed => {
                self.typing.clear();
                true
            }
            _ => false,
        };
        self.typing_dirty |= changed;
    }

    /// The display names typing in `channel`, longest-typing first.
    fn typing_names(&self, channel: Option<Snowflake>) -> Vec<String> {
        let Some(channel) = channel else {
            return Vec::new();
        };
        self.typing
            .users(self.clock.now(), channel)
            .into_iter()
            .filter(|id| self.store.suppression(*id).is_none())
            .map(|id| {
                self.store
                    .user(id)
                    .map_or_else(|| tr("Someone").to_owned(), |u| u.display_name().to_owned())
            })
            .collect()
    }

    /// Applies acknowledgements Discord accepted to the store without waiting
    /// for their echo, so the markers clear at once.
    fn apply_acked(&mut self) {
        for (channel, message) in self.history.take_acked() {
            if self.store.ack_local(channel, message).read {
                self.navigation_dirty = true;
            }
        }
        if self.navigation_dirty {
            self.navigation.reconcile(&self.store);
        }
    }
}

#[derive(Clone, Debug)]
enum SelectionIntent {
    Guild(Snowflake),
    Channel(Snowflake, Snowflake),
    PrivateChannel(Snowflake),
    /// A conversation known by ID alone (inbox jumps): a held DM or an
    /// openable channel of a held guild, whose guild is selected first.
    Open(Snowflake),
    /// A thread of the guild that the store may not hold yet (just created, or
    /// read from the archive): it is entered into the store, which still
    /// decides whether the account may open it.
    Thread(Snowflake, Box<Channel>),
    /// A keyboard step to the next or previous channel, unread one or server.
    /// Steps share the selection slot, so two before the worker runs collapse.
    Step(fastcord_discord::state::navigation::Step),
}

#[derive(Clone, Debug, Default)]
struct Request {
    revision: u64,
    selection: Option<SelectionIntent>,
    guilds: Window,
    channels: Window,
    private_channels: Window,
    /// Inbox rows wanted; the unread total is always computed (1).
    inbox: usize,
    /// The quick switcher's query while it is open (`None`: closed).
    quick: Option<String>,
    /// The server the management panel shows (`None`: closed).
    admin: Option<Snowflake>,
    /// The member the roles panel or permission inspection describes.
    admin_member: Option<Snowflake>,
    /// The announcement channel (server, channel) the follow picker is for.
    follow: Option<(Snowflake, Snowflake)>,
    /// The server whose emoji, stickers and sounds the assets panel manages.
    assets: Option<Snowflake>,
    /// Bumped when the local list layout (`organize`) changed.
    layout_rev: u64,
    timeline: crate::history::Request,
    emoji: emoji_picker::Request,
}

#[derive(Default)]
struct Latest {
    snapshot: Arc<NavigationSnapshot>,
    timeline: Arc<timeline::Snapshot>,
    emoji: Arc<emoji_picker::Snapshot>,
    status: Option<GatewayStatus>,
    private_notice: Option<String>,
    notified: bool,
    private_notice_pending: bool,
}

/// What one consumed notification carries to the UI.
pub struct Consumed {
    pub navigation: Arc<NavigationSnapshot>,
    pub timeline: Arc<timeline::Snapshot>,
    pub emoji: Arc<emoji_picker::Snapshot>,
    pub status: Option<GatewayStatus>,
    pub private_notice: Option<String>,
}

/// One step of the "Select messages" mode of a channel (issue #95).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectOp {
    Mode(bool),
    Toggle(Snowflake),
    /// The Delete button: asks for the Confirm.
    Ask,
    Cancel,
    Confirm,
}

/// One step of removing reactions from the open "who reacted" list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReactionOp {
    Ask(crate::timeline::ReactionTarget),
    Cancel,
    Confirm,
}

/// An explicit, ordered user action on the outbox or on one of the user's
/// messages. Unlike the coalesced [`Request`], none of these may be dropped or
/// merged, so they travel through a small bounded queue of their own.
pub enum Command {
    Moderate {
        guild: Snowflake,
        user: Snowflake,
        action: fastcord_discord::MemberAction,
        /// The optional audit-log reason; never printed or logged.
        reason: Option<String>,
        reply: tokio::sync::oneshot::Sender<Result<(), fastcord_discord::RestError>>,
    },
    Stage {
        guild: Snowflake,
        channel: Snowflake,
        action: fastcord_discord::StageAction,
        reply: tokio::sync::oneshot::Sender<Result<(), fastcord_discord::RestError>>,
    },
    /// One scheduled-event write.
    Event {
        guild: Snowflake,
        action: fastcord_discord::EventAction,
        reply: tokio::sync::oneshot::Sender<Result<(), fastcord_discord::RestError>>,
    },
    /// The complete event list of a guild that a list request returned.
    EventsLoaded {
        guild: Snowflake,
        events: Vec<fastcord_model::ScheduledEvent>,
    },
    Send {
        channel: Snowflake,
        content: String,
        files: Vec<UploadFile>,
        everyone: bool,
        reply: Option<Reply>,
    },
    Retry(OpId),
    Discard(OpId),
    /// Mark a conversation read up to its newest message (the user asked).
    MarkRead {
        guild: Option<Snowflake>,
        channel: Snowflake,
    },
    /// Mark several conversations read, each up to its newest message (the
    /// user asked); they share the one acknowledgement path.
    MarkReadMany(Vec<(Option<Snowflake>, Snowflake)>),
    /// Stop a send that is still uploading its files.
    CancelUpload(OpId),
    /// Save new text for one of the user's messages.
    Edit {
        channel: Snowflake,
        message: Snowflake,
        content: String,
        /// Attachments that stay when the user removed some.
        keep: Option<Vec<Snowflake>>,
    },
    /// Delete one of the user's messages, or anyone's with Manage Messages (the
    /// user confirmed it).
    Delete {
        channel: Snowflake,
        message: Snowflake,
    },
    Select {
        channel: Snowflake,
        op: SelectOp,
    },
    ReactionRemoval(ReactionOp),
    RetryChange {
        channel: Snowflake,
        message: Snowflake,
    },
    DismissChange {
        channel: Snowflake,
        message: Snowflake,
    },
    /// A reaction chip was clicked: add the user's reaction or take it back.
    ToggleReaction {
        channel: Snowflake,
        message: Snowflake,
        emoji: Emoji,
    },
    /// An emoji was picked for a message: add the user's reaction unless it
    /// already is theirs.
    AddReaction {
        channel: Snowflake,
        message: Snowflake,
        emoji: Emoji,
    },
    /// Show who reacted with an emoji (the user asked).
    ReactionUsers {
        channel: Snowflake,
        message: Snowflake,
        emoji: Emoji,
    },
    CloseReactionUsers,
    /// A poll answer was clicked: toggle it in the user's votes.
    PollVote {
        channel: Snowflake,
        message: Snowflake,
        answer: u32,
    },
    /// End the user's own poll.
    PollEnd {
        channel: Snowflake,
        message: Snowflake,
    },
    /// Hide or restore a message's link previews; `others` says the account
    /// may do so for messages that are not its own.
    SetEmbeds {
        channel: Snowflake,
        message: Snowflake,
        suppressed: bool,
        others: bool,
    },
    /// Show who voted for a poll answer (the user asked).
    PollVoters {
        channel: Snowflake,
        message: Snowflake,
        answer: u32,
    },
    /// An interaction the user started: a button, a select, a slash command or
    /// a modal answer (the worker validates it).
    Interact(Box<crate::interactions::Action>),
    /// The user is composing in this channel (the worker throttles and validates).
    Typing(Snowflake),
    OpenPrivate(Vec<Snowflake>),
    Stream(StreamIntent),
    /// Capture `source` and, once the capture runs, create a Go Live stream.
    StartShare {
        source: CaptureSource,
        request: StreamCreateRequest,
        /// Also send this audio (the shared application's, the system's, or
        /// a chosen PipeWire node).
        audio: Option<fastcord_platform::loopback::LoopbackScope>,
    },
    /// End the screen share and its stream.
    StopShare,
}

/// An explicit Go Live control action. Capture/send UI belongs to milestone 29.
#[derive(Clone, Debug)]
pub enum StreamIntent {
    Create(StreamCreateRequest),
    Watch(String),
    Unwatch(String),
    PauseResume { stream_key: String, paused: bool },
    Ping(String),
}

/// Copy-on-write edits of the shared controls: readers hold an `Arc`, so a
/// change clones the struct only while one is still held (#156).
pub(crate) trait EditControls {
    fn modify(&self, edit: impl FnOnce(&mut VoiceControls));
    fn modify_if(&self, edit: impl FnOnce(&mut VoiceControls) -> bool) -> bool;
}

impl EditControls for watch::Sender<Arc<VoiceControls>> {
    fn modify(&self, edit: impl FnOnce(&mut VoiceControls)) {
        self.send_modify(|controls| edit(Arc::make_mut(controls)));
    }
    fn modify_if(&self, edit: impl FnOnce(&mut VoiceControls) -> bool) -> bool {
        self.send_if_modified(|controls| edit(Arc::make_mut(controls)))
    }
}

/// Stores `value` and says whether it differed, for `watch::Sender::send_if_modified`.
fn replace_if_changed<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        return false;
    }
    *slot = value;
    true
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
            Self::Moderate {
                guild,
                user,
                action,
                ..
            } => f
                .debug_tuple("Moderate")
                .field(guild)
                .field(user)
                // A nickname or reason typed by the user never reaches a log.
                .field(&std::mem::discriminant(action))
                .finish(),
            Self::Stage {
                guild,
                channel,
                action,
                ..
            } => f
                .debug_tuple("Stage")
                .field(guild)
                .field(channel)
                .field(&std::mem::discriminant(action))
                .finish(),
            Self::Event { guild, action, .. } => f
                .debug_tuple("Event")
                .field(guild)
                .field(&std::mem::discriminant(action))
                .finish(),
            Self::EventsLoaded { guild, events } => f
                .debug_tuple("EventsLoaded")
                .field(guild)
                .field(&events.len())
                .finish(),
            Self::Send {
                channel, everyone, ..
            } => f
                .debug_struct("Send")
                .field("channel", channel)
                .field("everyone", everyone)
                .finish_non_exhaustive(),
            Self::Retry(id) => f.debug_tuple("Retry").field(id).finish(),
            Self::Discard(id) => f.debug_tuple("Discard").field(id).finish(),
            Self::CancelUpload(id) => f.debug_tuple("CancelUpload").field(id).finish(),
            Self::MarkReadMany(rows) => f.debug_tuple("MarkReadMany").field(&rows.len()).finish(),
            Self::MarkRead { channel, .. } => f
                .debug_struct("MarkRead")
                .field("channel", channel)
                .finish_non_exhaustive(),
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
            Self::Select { channel, op } => f
                .debug_struct("Select")
                .field("channel", channel)
                .field("op", op)
                .finish(),
            Self::ReactionRemoval(op) => f.debug_tuple("ReactionRemoval").field(op).finish(),
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
            Self::ToggleReaction {
                channel, message, ..
            } => f
                .debug_struct("ToggleReaction")
                .field("channel", channel)
                .field("message", message)
                .finish_non_exhaustive(),
            Self::AddReaction {
                channel, message, ..
            } => f
                .debug_struct("AddReaction")
                .field("channel", channel)
                .field("message", message)
                .finish_non_exhaustive(),
            Self::ReactionUsers {
                channel, message, ..
            } => f
                .debug_struct("ReactionUsers")
                .field("channel", channel)
                .field("message", message)
                .finish_non_exhaustive(),
            Self::CloseReactionUsers => f.write_str("CloseReactionUsers"),
            Self::PollVote {
                channel,
                message,
                answer,
            } => f
                .debug_struct("PollVote")
                .field("channel", channel)
                .field("message", message)
                .field("answer", answer)
                .finish(),
            Self::PollEnd { channel, message } => f
                .debug_struct("PollEnd")
                .field("channel", channel)
                .field("message", message)
                .finish(),
            Self::SetEmbeds {
                channel,
                message,
                suppressed,
                ..
            } => f
                .debug_struct("SetEmbeds")
                .field("channel", channel)
                .field("message", message)
                .field("suppressed", suppressed)
                .finish(),
            Self::PollVoters {
                channel,
                message,
                answer,
            } => f
                .debug_struct("PollVoters")
                .field("channel", channel)
                .field("message", message)
                .field("answer", answer)
                .finish(),
            Self::Interact(action) => f.debug_tuple("Interact").field(action).finish(),
            Self::Typing(channel) => f.debug_tuple("Typing").field(channel).finish(),
            Self::OpenPrivate(_) => f.write_str("OpenPrivate([selected recipients])"),
            Self::Stream(intent) => f.debug_tuple("Stream").field(intent).finish(),
            // The window title is private to the desktop and stays out of logs.
            Self::StartShare { request, .. } => f
                .debug_struct("StartShare")
                .field("request", request)
                .finish_non_exhaustive(),
            Self::StopShare => f.write_str("StopShare"),
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
    voice_controls: watch::Sender<Arc<VoiceControls>>,
    account_id: Arc<Mutex<Option<Snowflake>>>,
    /// Closes the live Gateway cleanly on Quit; set when the connection starts.
    closer: Arc<Mutex<Option<fastcord_discord::gateway::GatewayCloser>>>,
    latest: Arc<Mutex<Latest>>,
    commands: mpsc::Sender<Command>,
    command_receiver: Arc<Mutex<Option<mpsc::Receiver<Command>>>>,
    outbox_slots: Slots,
    /// The user's unconfirmed work, left by a worker that stopped on a limit.
    carried: Arc<Mutex<Option<Carried>>>,
    watched: Arc<parking_lot::Mutex<Vec<Arc<crate::playback::Session>>>>,
    share_view: watch::Sender<ShareView>,
    camera_tiles: Arc<CameraTiles>,
}

impl NavigationBridge {
    /// One bounded command with a one-slot answer; the reducer rechecks current
    /// permissions immediately before handing the request to the transport.
    pub async fn moderate(
        &self,
        guild: Snowflake,
        user: Snowflake,
        action: fastcord_discord::MemberAction,
        reason: Option<String>,
    ) -> Result<(), fastcord_discord::RestError> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.ask(
            Command::Moderate {
                guild,
                user,
                action,
                reason,
                reply,
            },
            answer,
        )
        .await
    }
    /// One explicit Stage action; the reducer rechecks permissions and state
    /// immediately before handing the request to the transport.
    pub async fn stage(
        &self,
        guild: Snowflake,
        channel: Snowflake,
        action: fastcord_discord::StageAction,
    ) -> Result<(), fastcord_discord::RestError> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.ask(
            Command::Stage {
                guild,
                channel,
                action,
                reply,
            },
            answer,
        )
        .await
    }
    /// One explicit scheduled-event write; the reducer rechecks permissions and
    /// the event's state immediately before handing the request to the transport.
    pub async fn scheduled_event(
        &self,
        guild: Snowflake,
        action: fastcord_discord::EventAction,
    ) -> Result<(), fastcord_discord::RestError> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.ask(
            Command::Event {
                guild,
                action,
                reply,
            },
            answer,
        )
        .await
    }
    /// Queues one write for the reducer and waits for its answer. A worker
    /// that is gone (the Gateway stopped, the account is closing) is
    /// `Disconnected`, never a rejected login (#173); only a full queue is
    /// back-pressure.
    async fn ask(
        &self,
        command: Command,
        answer: tokio::sync::oneshot::Receiver<Result<(), fastcord_discord::RestError>>,
    ) -> Result<(), fastcord_discord::RestError> {
        use fastcord_discord::RestError;
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => RestError::SchedulerCapacityExceeded,
                mpsc::error::TrySendError::Closed(_) => RestError::Disconnected,
            })?;
        answer.await.unwrap_or(Err(RestError::Disconnected))
    }

    /// Hands a guild's freshly listed events to the reducer. Dropped when the
    /// queue is full: the list can be loaded again.
    pub fn set_scheduled_events(
        &self,
        guild: Snowflake,
        events: Vec<fastcord_model::ScheduledEvent>,
    ) {
        let _ = self
            .commands
            .try_send(Command::EventsLoaded { guild, events });
    }
    pub fn new() -> Self {
        let (requests, _) = watch::channel(Request::default());
        let (voice_requests, _) = watch::channel(None);
        let (voice_audio, _) = watch::channel(VoiceAudioState::Idle);
        let (voice_controls, _) = watch::channel(Arc::new(VoiceControls::default()));
        let (commands, receiver) = mpsc::channel(COMMAND_QUEUE);
        Self {
            requests,
            voice_requests,
            voice_audio,
            voice_controls,
            account_id: Arc::new(Mutex::new(None)),
            closer: Arc::new(Mutex::new(None)),
            latest: Arc::default(),
            commands,
            command_receiver: Arc::new(Mutex::new(Some(receiver))),
            outbox_slots: Slots::default(),
            carried: Arc::default(),
            watched: Arc::default(),
            share_view: watch::channel(ShareView::default()).0,
            camera_tiles: Arc::default(),
        }
    }

    /// The tiles of the call view (the local preview and the remote cameras).
    pub fn camera_tiles(&self) -> Arc<CameraTiles> {
        Arc::clone(&self.camera_tiles)
    }

    /// The user turned the camera on or off. This is the only way it turns on.
    pub fn set_camera(&self, on: bool) {
        self.voice_controls.modify_if(|controls| {
            let camera = &mut controls.camera;
            if camera.wanted == on {
                return false;
            }
            camera.wanted = on;
            if on {
                camera.status = CameraStatus::Starting;
            } else if matches!(camera.status, CameraStatus::Failed(_)) {
                camera.status = CameraStatus::Off;
            }
            true
        });
    }

    /// Hands the saved camera choices to the call before the camera is turned
    /// on: the device, the background effect and the self-view mirror. A
    /// running camera is not restarted by this.
    pub fn prepare_camera(&self, device: Option<String>, effect: Effect, mirror: bool) {
        self.voice_controls.modify_if(|controls| {
            let camera = &mut controls.camera;
            if camera.wanted {
                return false;
            }
            let changed = camera.device != device;
            camera.device = device;
            camera.effect = effect;
            camera.mirror = mirror;
            if changed {
                camera.generation = camera.generation.wrapping_add(1);
            }
            true
        });
    }

    /// Changes the background effect of the camera, on or off, without
    /// restarting it.
    pub fn set_camera_effect(&self, effect: Effect) {
        self.voice_controls.modify_if(|controls| {
            let changed = controls.camera.effect != effect;
            controls.camera.effect = effect;
            changed
        });
    }

    /// Mirrors (or not) the local self-view; the sent picture is never mirrored.
    pub fn set_camera_mirror(&self, mirror: bool) {
        self.voice_controls.modify_if(|controls| {
            let changed = controls.camera.mirror != mirror;
            controls.camera.mirror = mirror;
            changed
        });
    }

    /// Chooses the camera by its stable ID (`None`: the first one). A running
    /// camera is released and the new one opened.
    pub fn select_camera(&self, device: Option<String>) {
        self.voice_controls.modify(|controls| {
            controls.camera.device = device;
            controls.camera.generation = controls.camera.generation.wrapping_add(1);
        });
    }

    /// Current audio state for the voice controls.
    pub fn voice_audio_state(&self) -> VoiceAudioState {
        *self.voice_audio.borrow()
    }
    pub fn voice_controls(&self) -> Arc<VoiceControls> {
        Arc::clone(&self.voice_controls.borrow())
    }
    /// Reads the controls in place, with no deep clone (#156). Keep `read`
    /// short and never call back into the bridge from it.
    pub fn voice_controls_with<R>(&self, read: impl FnOnce(&VoiceControls) -> R) -> R {
        read(&self.voice_controls.borrow())
    }
    /// Closes the Gateway with code 1000 and waits up to `wait` for its task
    /// to finish, so a quit never leaves a session open server-side. Does
    /// nothing before the connection starts or after it ended.
    pub async fn close_gateway(&self, wait: Duration) {
        let closer = self
            .closer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(closer) = closer {
            closer.close(wait).await;
        }
    }

    pub fn set_account_id(&self, account_id: Snowflake) {
        *self.account_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(account_id);
    }

    pub fn set_self_muted(&self, self_muted: bool) {
        self.voice_controls
            .modify(|controls| controls.self_muted = self_muted);
    }

    pub fn set_voice_input(&self, settings: &crate::shortcuts::Settings) {
        self.voice_controls.modify(|controls| {
            controls.input_mode = settings.mode;
            controls.ptt_release_ms = settings.release_ms;
            controls.sensitivity = settings.sensitivity;
        });
    }

    pub fn set_vad_forbidden(&self, forbidden: bool) {
        self.voice_controls
            .modify(|controls| controls.vad_forbidden = forbidden);
    }

    pub fn set_ptt(&self, down: bool) {
        self.voice_controls
            .modify_if(|controls| std::mem::replace(&mut controls.ptt_down, down) != down);
    }

    /// Releases push-to-talk immediately, without its release delay.
    pub fn cancel_ptt(&self) {
        self.voice_controls.modify(|controls| {
            controls.ptt_down = false;
            controls.ptt_cancel = controls.ptt_cancel.wrapping_add(1);
        });
    }

    pub fn set_push_to_mute(&self, down: bool) {
        self.voice_controls
            .modify_if(|controls| std::mem::replace(&mut controls.push_to_mute_down, down) != down);
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.voice_controls
            .modify(|controls| controls.deafened = deafened);
    }

    pub fn set_volume_percent(&self, user_id: Snowflake, percent: u16) -> bool {
        let percent = percent.min(200);
        self.voice_controls.modify(|controls| {
            if let Some(index) = controls.volumes.iter().position(|v| v.user_id == user_id) {
                controls.volumes.remove(index);
            } else if controls.volumes.len() == 256 {
                controls.volumes.remove(0);
            }
            controls.volumes.push(UserVolume { user_id, percent });
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
        self.voice_controls.modify(|controls| {
            let mut merged: Vec<_> = volumes
                .into_iter()
                .filter(|saved| {
                    !controls
                        .volumes
                        .iter()
                        .any(|live| live.user_id == saved.user_id)
                })
                .collect();
            merged.append(&mut controls.volumes);
            let excess = merged.len().saturating_sub(256);
            merged.drain(..excess);
            controls.volumes = merged;
        });
    }

    pub fn set_audio_devices(&self, preferences: &crate::audio_devices::Preferences) {
        self.voice_controls.modify(|controls| {
            controls.input_device = preferences.input.choice.clone();
            controls.output_device = preferences.output.choice.clone();
            controls.noise_suppression = preferences.noise_suppression;
            controls.echo_cancellation_off = !preferences.echo_cancellation;
            controls.input_gain = preferences.input_gain;
            controls.camera.device = preferences.camera.id.clone();
        });
    }

    pub fn select_echo_cancellation(&self, enabled: bool) {
        self.voice_controls
            .modify(|controls| controls.echo_cancellation_off = !enabled);
    }

    pub fn select_input_gain(&self, gain: fastcord_audio::InputGain) {
        self.voice_controls
            .modify_if(|controls| replace_if_changed(&mut controls.input_gain, gain));
    }

    /// The audio dialog opened or closed; the running call measures the microphone level only
    /// while it is open.
    pub fn set_diagnostics_open(&self, open: bool) {
        self.voice_controls
            .modify_if(|controls| replace_if_changed(&mut controls.diagnostics_open, open));
    }

    pub fn select_audio_device(
        &self,
        direction: fastcord_audio::Direction,
        choice: fastcord_audio::DeviceChoice,
    ) {
        self.voice_controls.modify(|controls| match direction {
            fastcord_audio::Direction::Input => {
                controls.input_device = choice;
                controls.input_generation = controls.input_generation.wrapping_add(1);
            }
            fastcord_audio::Direction::Output => {
                controls.output_device = choice;
                controls.output_generation = controls.output_generation.wrapping_add(1);
            }
        });
    }

    pub fn select_noise_suppression(&self, mode: fastcord_audio::NoiseSuppression) {
        self.voice_controls.modify(|controls| {
            controls.noise_suppression = mode;
            controls.noise_generation = controls.noise_generation.wrapping_add(1);
        });
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

    /// Watches the named Go Live stream through opcode 20. Up to
    /// [`MAX_WATCHED_STREAMS`](crate::call_layout::MAX_WATCHED_STREAMS) streams
    /// are watched at once, each on a connection of its own; more is refused
    /// with the reason, never by dropping one that is already watched.
    pub fn watch_stream(&self, stream_key: impl Into<String>) -> Result<(), &'static str> {
        let stream_key = stream_key.into();
        if StreamKey::from_wire(&stream_key).is_none() {
            return Err(tr("That is not a stream this client can watch."));
        }
        let mut watched = self.watched.lock();
        if watched.iter().any(|session| session.key == stream_key) {
            return Err(tr("You are already watching this stream."));
        }
        if watched.len() >= crate::call_layout::MAX_WATCHED_STREAMS {
            return Err(tr(
                "You are watching as many streams as fastcord allows at once. Stop watching one first.",
            ));
        }
        let session = crate::playback::Session::new(stream_key.clone());
        watched.push(session.clone());
        if self.stream_intent(StreamIntent::Watch(stream_key)) {
            Ok(())
        } else {
            session.close();
            watched.pop();
            Err(tr("Could not queue Watch; retry."))
        }
    }

    /// Stops watching (or ends an owned stream) through opcode 19.
    pub fn unwatch_stream(&self, stream_key: impl Into<String>) -> bool {
        let stream_key = stream_key.into();
        if StreamKey::from_wire(&stream_key).is_none()
            || !self.stream_intent(StreamIntent::Unwatch(stream_key.clone()))
        {
            return false;
        }
        let mut watched = self.watched.lock();
        if let Some(index) = watched.iter().position(|session| session.key == stream_key) {
            watched.remove(index).close();
        }
        true
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

    /// Every watched stream, oldest first.
    pub fn watched(&self) -> Vec<Arc<crate::playback::Session>> {
        self.watched.lock().clone()
    }

    pub fn watched_stream(&self, wire_key: &str) -> Option<Arc<crate::playback::Session>> {
        self.watched
            .lock()
            .iter()
            .find(|session| session.key == wire_key)
            .cloned()
    }

    pub fn stop_playback(&self) {
        for session in self.watched.lock().drain(..) {
            session.close();
        }
    }

    /// What the screen shows about the screen share.
    pub fn share_view(&self) -> ShareView {
        self.share_view.borrow().clone()
    }

    /// The voice channel the account is in or joining: where a share goes.
    pub fn voice_target(&self) -> Option<(Option<Snowflake>, Snowflake)> {
        let request = (*self.voice_requests.borrow())?;
        request
            .channel_id
            .map(|channel| (request.guild_id, channel))
    }

    /// Starts sharing `source` into the voice channel `request` names. The
    /// capture starts first; the stream is created once the OS accepted it.
    pub fn start_share(
        &self,
        source: CaptureSource,
        request: StreamCreateRequest,
        audio: Option<fastcord_platform::loopback::LoopbackScope>,
    ) -> bool {
        let text = trf("Starting capture of “{}”…", &[&source.title]);
        let queued = self
            .commands
            .try_send(Command::StartShare {
                source,
                request,
                audio,
            })
            .is_ok();
        self.share_view.send_replace(if queued {
            ShareView::active(text)
        } else {
            ShareView::failed(tr("Could not start sharing; try again.").to_owned())
        });
        queued
    }

    /// Ends the screen share (and deletes its stream).
    pub fn stop_share(&self) -> bool {
        self.commands.try_send(Command::StopShare).is_ok()
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
    #[cfg(test)]
    pub fn send_message(
        &self,
        channel: Snowflake,
        content: String,
        everyone: bool,
        reply: Option<Reply>,
    ) -> bool {
        self.send_message_with_files(channel, content, Vec::new(), everyone, reply)
    }

    /// [`send_message`](Self::send_message) with files to upload first.
    pub fn send_message_with_files(
        &self,
        channel: Snowflake,
        content: String,
        files: Vec<UploadFile>,
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
                files,
                everyone,
                reply,
            })
            .is_ok();
        if !queued {
            self.outbox_slots.release();
        }
        queued
    }

    /// Makes this (new) bridge continue `old`'s account work: the outbox's
    /// capacity and whatever its worker left unconfirmed (#149).
    pub fn inherit(&mut self, old: &NavigationBridge) {
        self.outbox_slots = old.outbox_slots.clone();
        self.carried = Arc::clone(&old.carried);
    }

    /// The worker's history: empty, or continuing a stopped worker's work.
    fn new_history(&self) -> History {
        let mut history = History::new(self.outbox_slots.clone());
        if let Some(carried) = self
            .carried
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            history.restore(carried);
        }
        history
    }

    pub fn retry_send(&self, id: OpId) -> bool {
        self.commands.try_send(Command::Retry(id)).is_ok()
    }

    /// The user is typing in `channel`; the worker sends at most one typing
    /// request per interval and only where sending is allowed.
    pub fn typing(&self, channel: Snowflake) -> bool {
        self.commands.try_send(Command::Typing(channel)).is_ok()
    }

    pub fn mark_read(&self, guild: Option<Snowflake>, channel: Snowflake) -> bool {
        self.commands
            .try_send(Command::MarkRead { guild, channel })
            .is_ok()
    }

    pub fn mark_read_many(&self, rows: Vec<(Option<Snowflake>, Snowflake)>) -> bool {
        self.commands.try_send(Command::MarkReadMany(rows)).is_ok()
    }

    /// How many inbox rows the snapshot should carry (0 keeps only the total).
    pub fn inbox_rows(&self, rows: usize) {
        self.requests.send_if_modified(|request| {
            if request.inbox == rows {
                return false;
            }
            request.inbox = rows;
            true
        });
    }

    /// The quick switcher's query while it is open, `None` once it closes.
    pub fn quick_query(&self, query: Option<&str>) {
        self.requests.send_if_modified(|request| {
            if request.quick.as_deref() == query {
                return false;
            }
            request.quick = query.map(str::to_owned);
            true
        });
    }

    /// The server the management panel is open for, `None` once it closes.
    pub fn admin_guild(&self, guild: Option<Snowflake>) {
        self.requests.send_if_modified(|request| {
            if request.admin == guild {
                return false;
            }
            request.admin = guild;
            true
        });
    }

    /// The announcement channel the follow picker is open for, `None` once it
    /// closes; the reducer then lists where it may be followed.
    pub fn follow_source(&self, source: Option<(Snowflake, Snowflake)>) {
        self.requests.send_if_modified(|request| {
            if request.follow == source {
                return false;
            }
            request.follow = source;
            true
        });
    }

    /// The server the assets panel manages, `None` once it closes.
    pub fn assets_guild(&self, guild: Option<Snowflake>) {
        self.requests.send_if_modified(|request| {
            if request.assets == guild {
                return false;
            }
            request.assets = guild;
            true
        });
    }

    /// The member the management panel describes, `None` when it needs none.
    pub fn admin_member(&self, user: Option<Snowflake>) {
        self.requests.send_if_modified(|request| {
            if request.admin_member == user {
                return false;
            }
            request.admin_member = user;
            true
        });
    }

    /// Steps to the next or previous channel, unread conversation or server.
    pub fn step(&self, step: fastcord_discord::state::navigation::Step) {
        self.clear_private_notice();
        self.requests.send_modify(|request| {
            request.revision = request.revision.wrapping_add(1);
            request.selection = Some(SelectionIntent::Step(step));
        });
    }

    /// Opens a conversation by ID alone.
    pub fn open_channel(&self, channel: Snowflake) {
        self.clear_private_notice();
        self.requests.send_modify(|request| {
            request.revision = request.revision.wrapping_add(1);
            request.selection = Some(SelectionIntent::Open(channel));
        });
    }

    /// Tells the worker whether what the timeline shows counts as read: the
    /// window is active and drawn.
    pub fn set_viewing(&self, viewing: bool) {
        self.requests.send_if_modified(|request| {
            if request.timeline.viewing == viewing {
                return false;
            }
            request.timeline.viewing = viewing;
            true
        });
    }

    pub fn cancel_upload(&self, id: OpId) -> bool {
        self.commands.try_send(Command::CancelUpload(id)).is_ok()
    }

    pub fn discard_send(&self, id: OpId) -> bool {
        self.commands.try_send(Command::Discard(id)).is_ok()
            || self.with_carried(|carried| carried.discard_send(id))
    }

    /// The worker stopped on a limit and its unconfirmed work waits for
    /// Reconnect in the bridge (#149).
    pub fn stopped(&self) -> bool {
        self.commands.is_closed()
    }

    /// Runs `f` on the work a stopped worker left; false when there is none.
    fn with_carried(&self, f: impl FnOnce(&mut Carried) -> bool) -> bool {
        self.stopped()
            && self
                .carried
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
                .is_some_and(f)
    }

    /// Queues new text for one of the user's messages. Returns whether it was
    /// accepted; on `false` nothing was queued and the caller keeps the text.
    pub fn edit_message(
        &self,
        channel: Snowflake,
        message: Snowflake,
        content: String,
        keep: Option<Vec<Snowflake>>,
    ) -> bool {
        self.commands
            .try_send(Command::Edit {
                channel,
                message,
                content,
                keep,
            })
            .is_ok()
    }

    /// Queues the confirmed deletion of one of the user's messages.
    pub fn delete_message(&self, channel: Snowflake, message: Snowflake) -> bool {
        self.commands
            .try_send(Command::Delete { channel, message })
            .is_ok()
    }

    /// Queues one step of the "Select messages" mode.
    pub fn select(&self, channel: Snowflake, op: SelectOp) -> bool {
        self.commands
            .try_send(Command::Select { channel, op })
            .is_ok()
    }

    /// Queues one step of removing reactions from the open list.
    pub fn reaction_removal(&self, op: ReactionOp) -> bool {
        self.commands.try_send(Command::ReactionRemoval(op)).is_ok()
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
            || self.with_carried(|carried| carried.dismiss_change(channel, message))
    }

    /// Queues a click on a reaction chip. Returns whether it was accepted.
    pub fn toggle_reaction(&self, channel: Snowflake, message: Snowflake, emoji: Emoji) -> bool {
        self.commands
            .try_send(Command::ToggleReaction {
                channel,
                message,
                emoji,
            })
            .is_ok()
    }

    /// Queues the user's pick of an emoji to react to a message with.
    pub fn add_reaction(&self, channel: Snowflake, message: Snowflake, emoji: Emoji) -> bool {
        self.commands
            .try_send(Command::AddReaction {
                channel,
                message,
                emoji,
            })
            .is_ok()
    }

    /// Queues the user's request to see who reacted with an emoji.
    pub fn reaction_users(&self, channel: Snowflake, message: Snowflake, emoji: Emoji) -> bool {
        self.commands
            .try_send(Command::ReactionUsers {
                channel,
                message,
                emoji,
            })
            .is_ok()
    }

    pub fn close_reaction_users(&self) -> bool {
        self.commands.try_send(Command::CloseReactionUsers).is_ok()
    }

    /// Queues a click on a poll answer.
    pub fn poll_vote(&self, channel: Snowflake, message: Snowflake, answer: u32) -> bool {
        self.commands
            .try_send(Command::PollVote {
                channel,
                message,
                answer,
            })
            .is_ok()
    }

    /// Queues the user's request to end their own poll.
    pub fn poll_end(&self, channel: Snowflake, message: Snowflake) -> bool {
        self.commands
            .try_send(Command::PollEnd { channel, message })
            .is_ok()
    }

    /// Queues the user's request to hide or restore a message's link previews.
    pub fn set_embeds(
        &self,
        channel: Snowflake,
        message: Snowflake,
        suppressed: bool,
        others: bool,
    ) -> bool {
        self.commands
            .try_send(Command::SetEmbeds {
                channel,
                message,
                suppressed,
                others,
            })
            .is_ok()
    }

    /// Queues an interaction the user started.
    pub fn interact(&self, action: crate::interactions::Action) -> bool {
        self.commands
            .try_send(Command::Interact(Box::new(action)))
            .is_ok()
    }

    /// Queues the user's request to see who voted for an answer.
    pub fn poll_voters(&self, channel: Snowflake, message: Snowflake, answer: u32) -> bool {
        self.commands
            .try_send(Command::PollVoters {
                channel,
                message,
                answer,
            })
            .is_ok()
    }

    pub(crate) fn publish(
        &self,
        snapshot: NavigationSnapshot,
        timeline: timeline::Snapshot,
        emoji: emoji_picker::Snapshot,
        status: Option<GatewayStatus>,
    ) -> bool {
        let mut latest = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        let status_changed = status
            .as_ref()
            .is_some_and(|status| latest.status.as_ref() != Some(status));
        let snapshot_changed = *latest.snapshot != snapshot;
        let timeline_changed = *latest.timeline != timeline;
        let emoji_changed = *latest.emoji != emoji;
        if !status_changed && !snapshot_changed && !timeline_changed && !emoji_changed {
            return false;
        }
        if snapshot_changed {
            latest.snapshot = Arc::new(snapshot);
        }
        if timeline_changed {
            latest.timeline = Arc::new(timeline);
        }
        if emoji_changed {
            latest.emoji = Arc::new(emoji);
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
            emoji: Arc::clone(&latest.emoji),
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

    pub fn open_thread(&self, guild: Snowflake, thread: Channel) {
        self.clear_private_notice();
        self.requests.send_modify(|request| {
            request.revision = request.revision.wrapping_add(1);
            request.selection = Some(SelectionIntent::Thread(guild, Box::new(thread)));
        });
    }

    /// The local list layout changed (folders, folded categories): rebuild.
    pub fn layout_changed(&self) {
        self.requests.send_modify(|request| {
            request.layout_rev = request.layout_rev.wrapping_add(1);
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

    pub fn emoji_request(&self, emoji: &emoji_picker::Request) {
        self.requests.send_if_modified(|request| {
            if request.emoji == *emoji {
                return false;
            }
            request.emoji = emoji.clone();
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
    Share,
}

struct Worker {
    start: Option<(RestClient, Arc<UserToken>)>,
    rest: RestClient,
    gateway: Option<Gateway>,
    tracker: Tracker,
    bridge: NavigationBridge,
    requests: watch::Receiver<Request>,
    voice_requests: watch::Receiver<Option<VoiceStateRequest>>,
    voice_audio: watch::Receiver<VoiceAudioState>,
    voice_controls: watch::Receiver<Arc<VoiceControls>>,
    commands: mpsc::Receiver<Command>,
    voice_flags: (bool, bool, bool),
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
    share: Option<stream_share::ShareSession>,
    /// The share's view changed since the loop last signaled the UI.
    share_dirty: bool,
    typing_send: TypingSend,
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
                self.bridge.set_private_notice(trf(
                    "Could not refresh direct conversations: {}",
                    &[&tr(&error.to_string())],
                ));
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
        // READY that already lists the DMs makes the REST list redundant.
        let ready = match &event {
            GatewayEvent::Dispatch {
                event: Dispatch::Ready(ready),
                ..
            } => Some(!ready.private_channels.is_empty()),
            _ => None,
        };
        let status = self.tracker.apply(event);
        self.resolve_voice_bitrate();
        match ready {
            Some(true) => {
                let generation = self.begin_private_list_fetch();
                self.private_list_loaded_generation = Some(generation);
            }
            Some(false) => start_private_list(self),
            None => {}
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
                let notice = match error {
                    PrivateChannelError::Rest(
                        error @ (fastcord_discord::RestError::WriteNotVerified(_)
                        | fastcord_discord::RestError::WritesPaused(_)),
                    ) => trf(
                        "Could not open direct conversation: {}",
                        &[&tr(&error.to_string())],
                    ),
                    other => trf(
                        "Could not open direct conversation: {}",
                        &[&format!("{other:?}")],
                    ),
                };
                self.bridge.set_private_notice(notice);
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
            changed = async {
                match self.share.as_mut() {
                    Some(share) => share.progress.changed().await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                changed.map(|()| WorkerInput::Share)
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
    /// A limit stop must not abort the user's posts mid-flight (their outcome
    /// would be unknowable): wait, without a Gateway, for the answers.
    async fn answer_in_flight(&mut self) {
        // Commands still queued were accepted from the user (a send holds an
        // outbox slot): run them like any other, then wait for their answers.
        // Closing first makes later senders fail visibly instead of queueing
        // into a receiver that is about to be dropped.
        self.commands.close();
        while let Ok(command) = self.commands.try_recv() {
            if !matches!(
                command,
                Command::OpenPrivate(_)
                    | Command::Stream(_)
                    | Command::StartShare { .. }
                    | Command::StopShare
            ) {
                self.command(command);
            }
        }
        let wait = async {
            while self.tracker.history.unanswered() {
                let completed = self.tracker.history.next_completed().await;
                self.tracker.history.complete(&self.rest, completed);
            }
        };
        // A request that outlives this is settled as uncertain by `carry`.
        let _ = tokio::time::timeout(IN_FLIGHT_WAIT, wait).await;
    }

    /// Leaves the settled unconfirmed work for the next worker of the account.
    fn deposit_carried(&mut self) {
        let carried = self.tracker.history.carry();
        *self
            .bridge
            .carried
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(carried);
    }

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

    /// How long until the next typing entry lapses (a little past it, so it
    /// has lapsed when the timer fires). Only awaited while one exists.
    fn typing_wait(&self) -> Duration {
        self.tracker
            .typing
            .next_expiry(self.tracker.history.open_channel())
            .map_or(Duration::ZERO, |at| {
                at.saturating_sub(self.tracker.clock.now()) + Duration::from_millis(5)
            })
    }

    /// Applies one explicit outbox or message action.
    fn command(&mut self, command: Command) {
        let history = &mut self.tracker.history;
        match command {
            Command::Moderate {
                guild,
                user,
                action,
                reason,
                reply,
            } => {
                if !self
                    .tracker
                    .store
                    .member_action_allowed(guild, user, &action)
                {
                    let _ = reply.send(Err(fastcord_discord::RestError::PermissionDenied));
                } else {
                    let rest = self.rest.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(
                            rest.member_action(guild, user, &action, reason.as_deref())
                                .await,
                        );
                    });
                }
            }
            Command::Stage {
                guild,
                channel,
                action,
                reply,
            } => {
                if !self.tracker.store.stage_allows(guild, channel, &action) {
                    let _ = reply.send(Err(fastcord_discord::RestError::PermissionDenied));
                } else {
                    let rest = self.rest.clone();
                    let unix = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |elapsed| elapsed.as_secs());
                    tokio::spawn(async move {
                        let now = fastcord_discord::iso8601(unix);
                        let _ = reply.send(rest.stage_action(guild, channel, &action, &now).await);
                    });
                }
            }
            Command::Event {
                guild,
                action,
                reply,
            } => {
                if !self.tracker.store.event_allows(guild, &action) {
                    let _ = reply.send(Err(fastcord_discord::RestError::PermissionDenied));
                } else {
                    let rest = self.rest.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(rest.scheduled_event_action(guild, &action).await);
                    });
                }
            }
            Command::EventsLoaded { guild, events } => {
                if !self
                    .tracker
                    .store
                    .replace_scheduled_events(guild, events)
                    .is_empty()
                {
                    self.tracker.navigation_dirty = true;
                    self.tracker.navigation.reconcile(&self.tracker.store);
                }
            }
            Command::Send {
                channel,
                content,
                files,
                everyone,
                reply,
            } => {
                history.send_with_files(&self.rest, channel, &content, files, everyone, reply);
            }
            Command::CancelUpload(id) => history.cancel_upload(id),
            Command::MarkRead { guild, channel } => {
                if let Some(last) = self.tracker.store.last_message(guild, channel) {
                    history.mark_read(&self.rest, channel, last);
                }
            }
            Command::MarkReadMany(rows) => {
                for (guild, channel) in rows {
                    if let Some(last) = self.tracker.store.last_message(guild, channel) {
                        history.mark_read(&self.rest, channel, last);
                    }
                }
            }
            Command::Retry(id) => history.retry_send(&self.rest, id),
            Command::Discard(id) => history.discard_send(id),
            Command::Edit {
                channel,
                message,
                content,
                keep,
            } => history.edit_message_keeping(&self.rest, channel, message, &content, keep),
            Command::Delete { channel, message } => {
                history.delete_message(&self.rest, channel, message);
            }
            Command::Select { channel, op } => match op {
                SelectOp::Mode(on) => history.select_messages(channel, on),
                SelectOp::Toggle(message) => history.toggle_selected(channel, message),
                SelectOp::Ask => history.ask_bulk_delete(channel),
                SelectOp::Cancel => history.cancel_bulk_delete(),
                SelectOp::Confirm => history.confirm_bulk_delete(&self.rest, channel),
            },
            Command::ReactionRemoval(op) => match op {
                ReactionOp::Ask(target) => history.ask_reaction_removal(target),
                ReactionOp::Cancel => history.cancel_reaction_removal(),
                ReactionOp::Confirm => history.confirm_reaction_removal(&self.rest),
            },
            Command::RetryChange { channel, message } => {
                history.retry_change(&self.rest, channel, message);
            }
            Command::DismissChange { channel, message } => {
                history.dismiss_change(channel, message);
            }
            Command::ToggleReaction {
                channel,
                message,
                emoji,
            } => history.toggle_reaction(&self.rest, channel, message, emoji),
            Command::AddReaction {
                channel,
                message,
                emoji,
            } => history.add_reaction(&self.rest, channel, message, emoji),
            Command::ReactionUsers {
                channel,
                message,
                emoji,
            } => history.reaction_users(&self.rest, channel, message, emoji),
            Command::CloseReactionUsers => history.close_reaction_users(),
            Command::PollVote {
                channel,
                message,
                answer,
            } => history.vote_poll(&self.rest, channel, message, answer),
            Command::PollEnd { channel, message } => {
                history.end_poll(&self.rest, channel, message);
            }
            Command::SetEmbeds {
                channel,
                message,
                suppressed,
                others,
            } => history.set_embeds_suppressed(&self.rest, channel, message, suppressed, others),
            Command::PollVoters {
                channel,
                message,
                answer,
            } => history.poll_voters(&self.rest, channel, message, answer),
            Command::Interact(action) => history.interact(&self.rest, *action),
            Command::Typing(channel) => {
                let now = self.tracker.clock.now();
                if history.can_send_in(channel) && self.typing_send.permit(now, channel) {
                    let rest = self.rest.clone();
                    // Best effort: a refusal or a limit just means no indicator.
                    tokio::spawn(async move {
                        let _ = rest.trigger_typing(channel).await;
                    });
                }
            }
            Command::OpenPrivate(_) => {}
            Command::Stream(_) | Command::StartShare { .. } | Command::StopShare => {
                unreachable!("stream and share commands are handled above")
            }
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

    /// The bitrate of the guild channel the account's voice state is in (the requested one
    /// until Discord reports it), so a move or a `CHANNEL_UPDATE` retargets the microphone.
    fn resolve_voice_bitrate(&self) {
        let bitrate = (*self.voice_requests.borrow())
            .and_then(|request| {
                let guild = self.tracker.store.guild(request.guild_id?)?;
                let me = self.tracker.store.current_user().map(|user| user.id);
                let channel = me
                    .and_then(|me| guild.voice_state(me))
                    .and_then(|state| state.channel_id)
                    .or(request.channel_id)?;
                Some(guild.channel(channel)?.bitrate)
            })
            .unwrap_or(0);
        self.bridge
            .voice_controls
            .modify_if(|controls| replace_if_changed(&mut controls.channel_bitrate, bitrate));
    }

    /// Looks up who is in the call (and who streams) in the state store. The
    /// snapshot is only rewritten when the answer changes, so this settles.
    fn resolve_voice_names(&self) {
        let guild = self
            .voice_requests
            .borrow()
            .and_then(|request| request.guild_id);
        let store = &self.tracker.store;
        let mut names = Vec::new();
        let mut avatars = Vec::new();
        {
            let controls = self.voice_controls.borrow();
            let users = controls
                .participants
                .iter()
                .map(|participant| participant.user_id)
                .chain(controls.streams.iter().map(|offer| offer.owner))
                .chain(controls.stage.seats.iter().map(|(user, _)| *user));
            for id in users {
                if names.len() == MAX_VOICE_NAMES || names.iter().any(|(seen, _)| *seen == id) {
                    continue;
                }
                let nick = guild
                    .and_then(|guild| store.guild(guild))
                    .and_then(|entry| entry.member(id))
                    .and_then(|member| member.nick.as_deref());
                let name = nick.or_else(|| store.user(id).map(|user| user.display_name()));
                if let Some(name) = name {
                    names.push((id, name.chars().take(MAX_NAME_CHARS).collect::<String>()));
                }
                if let Some(hash) = store.user(id).and_then(|user| user.avatar.clone()) {
                    avatars.push((id, hash));
                }
            }
        }
        self.bridge.voice_controls.modify_if(|controls| {
            let changed = controls.names != names || controls.avatars != avatars;
            if changed {
                controls.names = names;
                controls.avatars = avatars;
            }
            changed
        });
    }

    /// A local mute or deafen change is sent to Discord with the current voice state.
    fn voice_controls_changed(&mut self, gateway: &Gateway) {
        self.resolve_voice_names();
        let flags = {
            let controls = self.voice_controls.borrow_and_update();
            (
                controls.self_muted,
                controls.deafened,
                controls.camera.is_live(),
            )
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
            gateway.voice_state().request(
                VoiceStateRequest::join(request.guild_id, channel, flags.0, flags.1)
                    .with_video(flags.2),
            );
        }
    }
    fn request_voice_state(&mut self, gateway: &Gateway) -> bool {
        let Some(request) = *self.voice_requests.borrow_and_update() else {
            return false;
        };
        for session in self.bridge.watched() {
            self.stream_command(gateway, StreamIntent::Unwatch(session.key.clone()));
        }
        self.end_share(Some(gateway), ShareView::idle(), true);
        self.stop_all_streams();
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
        self.voice_flags = (request.self_mute, request.self_deaf, request.self_video);
        // A Stage is joined as audience until the server's own state says otherwise.
        self.refresh_stage();
        gateway.voice_state().request(request);
        true
    }

    fn stage_controls(&self) -> StageControls {
        let store = &self.tracker.store;
        let request = *self.voice_requests.borrow();
        let (Some(guild_id), Some(channel_id), Some(me)) = (
            request.and_then(|r| r.guild_id),
            request.and_then(|r| r.channel_id),
            store.current_user().map(|user| user.id),
        ) else {
            return StageControls::default();
        };
        let Some(guild) = store.guild(guild_id) else {
            return StageControls::default();
        };
        if guild
            .channel(channel_id)
            .is_none_or(|c| c.kind != fastcord_model::ChannelKind::GuildStageVoice)
        {
            return StageControls::default();
        }
        let own = guild
            .voice_state(me)
            .filter(|state| state.channel_id == Some(channel_id));
        let mut here: Vec<_> = guild
            .voice_states()
            .filter(|state| state.channel_id == Some(channel_id))
            .collect();
        here.sort_by_key(|s| {
            (
                s.suppress,
                s.request_to_speak_timestamp.is_none(),
                s.request_to_speak_timestamp.clone(),
                s.user_id,
            )
        });
        StageControls {
            active: true,
            suppressed: own.is_none_or(|state| state.suppress),
            requested: own.is_some_and(|state| state.request_to_speak_timestamp.is_some()),
            permissions: store.stage_permissions(guild_id, channel_id),
            topic: guild
                .stage_instance(channel_id)
                .map(|stage| stage.topic.clone()),
            guild: Some(guild_id),
            channel: Some(channel_id),
            seats: here
                .into_iter()
                .take(MAX_VOICE_NAMES)
                .map(|state| (state.user_id, StageSeat::of(state)))
                .collect(),
        }
    }

    fn refresh_stage(&self) {
        let stage = self.stage_controls();
        let request = *self.voice_requests.borrow();
        let activities: Vec<String> = request
            .and_then(|r| r.guild_id.zip(r.channel_id))
            .and_then(|(guild, channel)| {
                let guild = self.tracker.store.guild(guild)?;
                Some(guild.activities(channel).map(str::to_owned).collect())
            })
            .unwrap_or_default();
        self.bridge.voice_controls.modify_if(|controls| {
            let changed = controls.stage != stage || controls.activities != activities;
            if changed {
                controls.stage = stage;
                controls.activities = activities;
            }
            changed
        });
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
                    self.bridge.voice_controls.modify(|controls| {
                        controls.server_muted = update.state.mute;
                        controls.server_deafened = update.state.deaf;
                        // Moves between audience and speakers take effect at once;
                        // everything else about the Stage follows the store.
                        if controls.stage.active
                            && update.state.channel_id == controls.stage.channel
                        {
                            controls.stage.suppressed = update.state.suppress;
                            controls.stage.requested =
                                update.state.request_to_speak_timestamp.is_some();
                        }
                    });
                }
                if let Some(correlator) = self.voice_correlator.as_mut() {
                    let correlation = correlator.voice_state(&update.state);
                    return Some(correlation);
                }
            }
            Dispatch::VoiceChannelSound(play) => {
                let in_call = self
                    .voice_requests
                    .borrow()
                    .is_some_and(|request| request.channel_id == Some(play.channel_id));
                if in_call && self.tracker.store.suppression(play.user_id).is_none() {
                    self.bridge.voice_controls.modify(|controls| {
                        controls.sound_seq = controls.sound_seq.wrapping_add(1);
                        if controls.sound_cues.len() >= SOUND_CUES {
                            controls.sound_cues.remove(0);
                        }
                        controls.sound_cues.push(SoundCue {
                            seq: controls.sound_seq,
                            user: play.user_id,
                            sound: play.sound_id,
                            volume_percent: play.volume_percent,
                        });
                    });
                }
            }
            Dispatch::SoundboardChanged(guild) => {
                self.bridge.voice_controls.modify(|controls| {
                    controls.sound_seq = controls.sound_seq.wrapping_add(1);
                    controls.sound_catalog_change = Some((*guild, controls.sound_seq));
                });
            }
            Dispatch::StickersChanged(guild) => {
                self.bridge.voice_controls.modify(|controls| {
                    controls.sound_seq = controls.sound_seq.wrapping_add(1);
                    controls.sticker_change = Some((*guild, controls.sound_seq));
                });
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
                self.share_stream_deleted(&key, deleted.reason, deleted.unavailable);
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
                if let Some(share) = self.share.as_ref().filter(|share| share.key == key)
                    && let Some(worker) = share.worker.as_ref()
                {
                    // The account's own stream: it sends video and never decodes.
                    let task = tokio::spawn(stream_share::run(
                        self.rest.network().clone(),
                        credentials,
                        worker.control(),
                        Arc::clone(&share.sender),
                        share
                            .audio
                            .as_ref()
                            .map(|audio| Arc::clone(&audio.slot))
                            .unwrap_or_default(),
                    ));
                    self.stream_tasks.insert(key, task);
                    return;
                }
                let presentation = self
                    .bridge
                    .watched_stream(&key.to_wire())
                    // An owned stream has no receive pane yet (capture/send is
                    // M29); keep its independent RTC lifecycle without decoding.
                    .unwrap_or_else(|| crate::playback::Session::new(key.to_wire()));
                let status_session = presentation.clone();
                let status: stream_video::StatusSink = Arc::new(move |status| {
                    use stream_video::StreamStatus;
                    status_session.status(match status {
                        StreamStatus::Connecting => crate::playback::Status::Connecting,
                        StreamStatus::Decoding { backend } => {
                            crate::playback::Status::Playing(backend)
                        }
                        StreamStatus::Suspended => crate::playback::Status::Waiting,
                        StreamStatus::Failed(error) => crate::playback::Status::Failed(error),
                        StreamStatus::Ended => crate::playback::Status::Ended,
                    });
                });
                self.stream_tasks.insert(
                    key,
                    tokio::spawn(stream_audio::run(
                        (self.rest.network().clone(), credentials),
                        presentation.visible.clone(),
                        presentation.visibility(),
                        Box::new(presentation.sink()),
                        status,
                        self.voice_controls.clone(),
                        presentation.clone(),
                    )),
                );
            }
            StreamCorrelation::Reallocating { key, .. } => {
                self.stop_stream_key(&key.to_wire());
                if let Some(session) = self.bridge.watched_stream(&key.to_wire()) {
                    session.status(crate::playback::Status::Connecting);
                }
            }
            StreamCorrelation::Ended { key, .. } => {
                self.stop_stream_key(&key.to_wire());
                if let Some(session) = self.bridge.watched_stream(&key.to_wire()) {
                    session.status(crate::playback::Status::Ended);
                }
            }
            StreamCorrelation::Invalid { key: Some(key), .. } => {
                self.stop_stream_key(&key.to_wire());
                if let Some(session) = self.bridge.watched_stream(&key.to_wire()) {
                    session.status(crate::playback::Status::Failed(
                        tr("The stream's RTC identity or endpoint is invalid.").into(),
                    ));
                }
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
        // Pause capture before aborting its RTC task. Otherwise the next
        // picture sees the old closed sink and ends the whole share during
        // server reallocation or credential replacement.
        if let Some(share) = self.share.as_ref().filter(|share| share.key == key) {
            if let Some(worker) = &share.worker {
                worker.control().detach();
            }
            if let Some(audio) = &share.audio {
                audio.slot.lock().take();
            }
            share
                .sender
                .send_modify(|progress| progress.connection = stream_share::Connection::Connecting);
        }
        if let Some(task) = self.stream_tasks.remove(&key) {
            task.abort();
        }
    }

    /// Ends the screen share and every stream task without a stream delete: the
    /// parent voice or Gateway session they belonged to is gone.
    fn stop_all_streams(&mut self) {
        self.end_share(None, ShareView::idle(), false);
        for (_, task) in self.stream_tasks.drain() {
            task.abort();
        }
        self.bridge.stop_playback();
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
            // A device that never finishes closing must not freeze the
            // account: wait a bounded moment for the abort, then go on (#151).
            task.abort();
            let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
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
                    self.rest.network().clone(),
                    credentials,
                    rx,
                    self.bridge.voice_audio.clone(),
                    self.bridge.voice_controls.subscribe(),
                    self.bridge.voice_controls.clone(),
                    self.bridge.camera_tiles(),
                )));
            }
            Correlation::Ended | Correlation::Reallocating => {
                self.stop_all_streams();
                self.stop_voice().await;
            }
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
                Some(SelectionIntent::Channel(guild, channel)) => {
                    open_guild_channel(
                        &mut self.tracker.navigation,
                        &self.tracker.store,
                        guild,
                        channel,
                    );
                }
                Some(SelectionIntent::PrivateChannel(channel)) => {
                    self.tracker
                        .navigation
                        .select_private_channel(&self.tracker.store, channel);
                }
                Some(SelectionIntent::Step(step)) => {
                    self.tracker.navigation.step(&self.tracker.store, step);
                }
                Some(SelectionIntent::Open(channel)) => {
                    self.tracker
                        .navigation
                        .open_channel(&self.tracker.store, channel);
                }
                Some(SelectionIntent::Thread(guild, thread))
                    if self.tracker.navigation.selected_guild() == Some(guild)
                        && thread.kind.is_thread()
                        && thread.guild_id == Some(guild) =>
                {
                    let id = thread.id;
                    self.tracker.store.apply(GatewayEvent::Dispatch {
                        sequence: 0,
                        event: Dispatch::ChannelCreate(thread),
                    });
                    self.tracker
                        .navigation
                        .select_channel(&self.tracker.store, id);
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
        if let Some(me) = self.tracker.store.current_user().map(|user| user.id) {
            self.tracker
                .navigation
                .set_layout(crate::organize::current(me.0));
        }
        let mut navigation = self.tracker.navigation.snapshot_with_private(
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
        navigation.inbox = self
            .tracker
            .navigation
            .inbox(&self.tracker.store, request.inbox.max(1));
        navigation.quick = request.quick.as_deref().map_or_else(Vec::new, |query| {
            self.tracker.navigation.quick_switch(
                &self.tracker.store,
                query,
                fastcord_discord::state::navigation::MAX_QUICK_RESULTS,
            )
        });
        navigation.automod = self.tracker.automod.clone();
        navigation.admin = request.admin.and_then(|guild| {
            fastcord_discord::state::navigation::guild_admin(&self.tracker.store, guild)
        });
        navigation.assets = request.assets.and_then(|guild| {
            fastcord_discord::state::navigation::guild_assets(&self.tracker.store, guild)
        });
        navigation.follow = request.follow.and_then(|(guild, source)| {
            fastcord_discord::state::navigation::follow_targets(&self.tracker.store, guild, source)
        });
        navigation.admin_member = request.admin_member.and_then(|user| {
            fastcord_discord::state::navigation::admin_member(
                &self.tracker.store,
                navigation.admin.as_ref()?,
                user,
            )
        });
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
        let mut timeline = self.tracker.history.snapshot();
        let store = &self.tracker.store;
        for row in &mut timeline.rows {
            row.mentions_me = store.mentions_account(&row.message);
        }
        timeline::hide_suppressed(&mut timeline, &navigation.suppressed);
        timeline.typing = self.tracker.typing_names(timeline.channel_id);
        timeline.names = mention_names(&self.tracker.store, navigation.selected_guild, &timeline);
        timeline.candidates =
            mention_candidates(&self.tracker.store, navigation.selected_guild, &timeline);
        let emoji = emoji_picker::snapshot(&request.emoji, &self.tracker.store, &navigation);
        let parent = *self.bridge.voice_requests.borrow();
        let offers = parent
            .and_then(|parent| parent.guild_id.zip(parent.channel_id))
            .and_then(|(guild, channel)| {
                self.tracker.store.guild(guild).map(|entry| {
                    entry
                        .voice_states()
                        .filter(|voice| {
                            voice.self_stream
                                && voice.channel_id == Some(channel)
                                && Some(voice.user_id) != self.stream_intents.user_id
                        })
                        .take(32)
                        .map(|voice| StreamOffer {
                            key: StreamKey::Guild {
                                guild_id: guild,
                                channel_id: channel,
                                owner_id: voice.user_id,
                            }
                            .to_wire(),
                            owner: voice.user_id,
                        })
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();
        self.refresh_stage();
        self.bridge.voice_controls.modify_if(|controls| {
            if controls.streams.as_ref() == offers.as_slice() {
                false
            } else {
                controls.streams = offers.into();
                true
            }
        });
        self.bridge.publish(navigation, timeline, emoji, status)
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
        // Capture threads are joined when dropped: off the runtime thread,
        // as `end_share` does.
        if let Some(share) = self.share.take() {
            std::thread::spawn(move || drop(share));
        }
    }
}

/// Starts only when iced polls this account-owned stream. Dropping the panel
/// aborts it and releases the socket, store, presentation slot, and controls.
/// How long a Gateway stop waits for posts already with Discord.
const IN_FLIGHT_WAIT: Duration = Duration::from_secs(30);

pub fn status_stream(
    rest: RestClient,
    token: Arc<UserToken>,
    bridge: NavigationBridge,
) -> impl Stream<Item = ()> {
    let requests = bridge.requests.subscribe();
    let voice_requests = bridge.voice_requests.subscribe();
    let voice_audio = bridge.voice_audio.subscribe();
    let voice_controls = bridge.voice_controls.subscribe();
    let rest_client = rest.clone();
    // There is one worker per bridge; a second one would simply get no commands.
    let commands = bridge.take_commands().unwrap_or_else(|| mpsc::channel(1).1);
    let history = bridge.new_history();
    let worker = Worker {
        start: Some((rest, token)),
        rest: rest_client,
        gateway: None,
        tracker: Tracker {
            history,
            ..Tracker::default()
        },
        bridge,
        requests,
        voice_requests,
        voice_audio,
        voice_controls,
        commands,
        voice_flags: (false, false, false),
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
        share: None,
        share_dirty: false,
        typing_send: TypingSend::default(),
    };
    stream::unfold(worker, |mut worker| async move {
        let mut gateway = match worker.gateway.take() {
            Some(gateway) => gateway,
            None => {
                let (rest, token) = worker.start.take()?;
                let gateway = Gateway::start(rest, token);
                *worker
                    .bridge
                    .closer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(gateway.closer());
                gateway
            }
        };
        loop {
            let mut voice_changed = false;
            let typing_wait = worker.typing_wait();
            let typing_armed = worker
                .tracker
                .typing
                .next_expiry(worker.tracker.history.open_channel())
                .is_some();
            let (status, requested) = tokio::select! {
                event = gateway.next_event() => {
                    let event = event?;
                    if let GatewayEvent::Dispatch {
                        event: Dispatch::Ready(_),
                        ..
                    } = &event
                    {
                        crate::login::remember_installation(&worker.rest);
                    }
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
                            worker.tracker.apply_acked();
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
                        WorkerInput::Command(Command::StartShare {
                            source,
                            request,
                            audio,
                        }) => {
                            worker.start_share(source, request, audio);
                            (None, false)
                        }
                        WorkerInput::Command(Command::StopShare) => {
                            worker.end_share(Some(&gateway), ShareView::idle(), true);
                            (None, false)
                        }
                        WorkerInput::Share => {
                            worker.share_progress(&gateway);
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
                // One-shot: armed only while a typing indicator is showing.
                _ = tokio::time::sleep(typing_wait), if typing_armed =>
                {
                    let now = worker.tracker.clock.now();
                    let open = worker.tracker.history.open_channel();
                    worker.tracker.typing_dirty |= worker.tracker.typing.prune(now, open);
                    (None, false)
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
            voice_changed |= std::mem::take(&mut worker.share_dirty);
            if !requested
                && !worker.tracker.navigation_dirty
                && !worker.tracker.history.dirty()
                && !worker.tracker.typing_dirty
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
            worker.tracker.typing_dirty = false;
            let request = worker.bridge.take_request();
            let terminal = matches!(
                status,
                Some(GatewayStatus::Stopped(_)) | Some(GatewayStatus::AuthenticationRequired)
            );
            if terminal {
                worker.answer_in_flight().await;
            }
            let snapshot_notification = worker.refresh(&gateway, request, status);
            let notice_notification = worker.bridge.take_private_notice_pending();
            let notified = notice_notification || snapshot_notification;
            if terminal {
                worker.start = None;
                worker.stop_voice().await;
                worker.stop_all_streams();
                // After the snapshot above, which still shows the rows.
                worker.deposit_carried();
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
    /// The DM or group-DM channel of the private call joined (exclusive with
    /// `voice_guild`: there is one parent voice connection).
    pub voice_call: Option<Snowflake>,
    /// The draft and composer state; local to the UI until the user sends.
    pub composer: Composer,
    /// The user's message (channel, message) whose deletion waits for confirmation.
    pub confirm_delete: Option<(Snowflake, Snowflake)>,
    /// The (channel, message) whose secondary actions the timeline shows.
    pub more: Option<(Snowflake, Snowflake)>,
    pub private: Box<PrivateConversationUi>,
    pub emoji: emoji::State,
    pub avatars: crate::avatars::State,
    pub emoji_picker: emoji_picker::Picker,
    pub emoji_snapshot: Arc<emoji_picker::Snapshot>,
    /// Spoilers the viewer uncovered (message, spoiler), at most
    /// [`rich::MAX_REVEALED`].
    pub revealed: HashSet<(Snowflake, u16)>,
    /// The sidebar lists direct messages rather than a server's channels.
    pub directs: bool,
    /// How the call's videos are laid out and which one is pinned or full.
    pub call_view: crate::call_layout::CallView,
    /// The source picker of the screen share.
    pub share_picker: SharePicker,
    /// The picker's "Share audio" choice (on by default).
    pub share_audio: bool,
    /// Where the audio may come from when the user chooses (Linux), listed
    /// when the picker opens, and the choice.
    pub share_audio_sources: Vec<fastcord_platform::loopback::AudioSource>,
    pub share_audio_target: Option<fastcord_platform::loopback::AudioTarget>,
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
    /// The inline audio attachment being played; dropping it stops the player.
    pub media: Option<crate::media_ui::MediaSession>,
}

impl Drop for GatewayPanel {
    fn drop(&mut self) {
        self.controls.stop_playback();
    }
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
            row.message.id == key.message && row.message.attachment(key.attachment).is_some()
        })
}

/// Names of the roles and channels the shown rows mention, looked up in the
/// selected guild. Only what the rows reference is copied.
fn mention_names(
    store: &Store,
    guild: Option<Snowflake>,
    timeline: &timeline::Snapshot,
) -> rich::Names {
    let Some(entry) = guild.and_then(|guild| store.guild(guild)) else {
        return rich::Names::default();
    };
    let (roles, channels) = rich::wanted(timeline.rows.iter().map(timeline::content));
    rich::Names {
        roles: roles
            .into_iter()
            .filter_map(|id| {
                let role = entry.roles().iter().find(|role| role.id == id)?;
                Some((id, role.name.clone()))
            })
            .collect(),
        channels: channels
            .into_iter()
            .filter_map(|id| Some((id, entry.channel(id)?.name.clone()?)))
            .collect(),
    }
}

/// Most people, roles and channels one channel offers for completion.
const MAX_CANDIDATES: usize = 128;

/// What `@` and `#` can complete to: the people shown in the timeline, then
/// the selected server's roles and text channels. Bounded; nothing is held
/// beyond the snapshot.
fn mention_candidates(
    store: &Store,
    guild: Option<Snowflake>,
    timeline: &timeline::Snapshot,
) -> Vec<Candidate> {
    if timeline.channel_id.is_none() || !timeline.can_send {
        return Vec::new();
    }
    let mut out: Vec<Candidate> = Vec::new();
    let mut person = |user: &fastcord_model::User| {
        if out.len() < MAX_CANDIDATES && !out.iter().any(|c| c.id == user.id) {
            out.push(Candidate {
                kind: CandidateKind::User,
                id: user.id,
                label: user.display_name().to_owned(),
            });
        }
    };
    for row in timeline.rows.iter().rev() {
        person(&row.message.author);
        row.message.mentions.iter().for_each(&mut person);
    }
    if let Some(entry) = guild.and_then(|guild| store.guild(guild)) {
        let everyone = entry.id();
        out.extend(
            entry
                .roles()
                .iter()
                .filter(|role| role.id != everyone)
                .take(MAX_CANDIDATES)
                .map(|role| Candidate {
                    kind: CandidateKind::Role,
                    id: role.id,
                    label: role.name.clone(),
                }),
        );
        out.extend(
            entry
                .channels()
                .filter(|channel| {
                    matches!(
                        channel.kind,
                        fastcord_model::ChannelKind::GuildText
                            | fastcord_model::ChannelKind::GuildAnnouncement
                    )
                })
                .filter_map(|channel| {
                    Some(Candidate {
                        kind: CandidateKind::Channel,
                        id: channel.id,
                        label: channel.name.clone()?,
                    })
                })
                .take(MAX_CANDIDATES),
        );
    }
    out
}

fn occurrence_in(
    timeline: &timeline::Snapshot,
    picker: &emoji_picker::Picker,
    page: &emoji_picker::Snapshot,
    occurrence: &emoji::Occurrence,
) -> bool {
    occurrence_in_parsed(timeline, picker, page, occurrence, &mut HashMap::new())
}

/// `occurrence_in` with the rows' emoji already found kept in `parsed`, so a
/// pass over many occurrences parses each message once, not once per emoji
/// (#163).
fn occurrence_in_parsed(
    timeline: &timeline::Snapshot,
    picker: &emoji_picker::Picker,
    page: &emoji_picker::Snapshot,
    occurrence: &emoji::Occurrence,
    parsed: &mut HashMap<(Snowflake, u64), Vec<emoji::Asset>>,
) -> bool {
    match occurrence.origin {
        emoji::Origin::Timeline {
            channel,
            message,
            revision,
            run,
        } => {
            timeline.channel_id == Some(channel)
                && timeline
                    .rows
                    .iter()
                    .find(|row| row.message.id == message && row.revision == revision)
                    .is_some_and(|row| {
                        parsed
                            .entry((message, revision))
                            .or_insert_with(|| rich::emoji_assets(timeline::content(row)))
                            .get(usize::from(run))
                            == Some(&occurrence.asset)
                    })
        }
        emoji::Origin::Reaction {
            channel,
            message,
            revision,
            chip,
        } => {
            timeline.channel_id == Some(channel)
                && timeline
                    .rows
                    .iter()
                    .find(|row| row.message.id == message && row.revision == revision)
                    .is_some_and(|row| {
                        timeline::chips(row)
                            .nth(usize::from(chip))
                            .and_then(|chip| emoji::reaction_asset(chip.emoji))
                            == Some(occurrence.asset)
                    })
        }
        emoji::Origin::Picker { revision } => {
            picker.request.open
                && revision == picker.request.revision
                && revision == page.revision
                && page
                    .cells
                    .iter()
                    .any(|cell| cell.choice.asset() == occurrence.asset)
        }
    }
}

impl GatewayPanel {
    /// Uncovers a hidden spoiler (or hides it again), within a fixed bound.
    pub fn toggle_spoiler(&mut self, message: Snowflake, spoiler: u16) {
        if !self.revealed.remove(&(message, spoiler)) {
            if self.revealed.len() >= rich::MAX_REVEALED {
                self.revealed.clear();
            }
            self.revealed.insert((message, spoiler));
        }
    }

    pub fn new(id: u64, worker: Handle, controls: NavigationBridge) -> Self {
        Self {
            id,
            status: GatewayStatus::Connecting { attempt: 0 },
            navigation: Arc::default(),
            timeline: Arc::default(),
            controls,
            private: Box::default(),
            emoji: emoji::State::default(),
            avatars: crate::avatars::State::default(),
            emoji_picker: emoji_picker::Picker::default(),
            emoji_snapshot: Arc::default(),
            revealed: HashSet::new(),
            directs: false,
            call_view: crate::call_layout::CallView::default(),
            share_picker: SharePicker::default(),
            share_audio: true,
            share_audio_sources: Vec::new(),
            share_audio_target: None,
            voice_guild: None,
            voice_call: None,
            composer: Composer::default(),
            confirm_delete: None,
            more: None,
            _worker: worker.abort_on_drop(),
            attachments: None,
            attachment_views: HashMap::new(),
            attachment_workers: HashMap::new(),
            attachment_action_workers: HashMap::new(),
            attachment_ready_order: VecDeque::new(),
            attachment_viewer: None,
            attachment_notice: None,
            media: None,
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
        self.emoji_snapshot = consumed.emoji;
        if self.emoji_picker.request.channel != self.timeline.channel_id {
            self.emoji_picker.close();
        }
        let timeline = &self.timeline;
        let picker = &self.emoji_picker;
        let page = &self.emoji_snapshot;
        let mut parsed = HashMap::new();
        self.emoji.retain(|occurrence| {
            occurrence_in_parsed(timeline, picker, page, occurrence, &mut parsed)
        });
        let open_channel = self.timeline.channel_id;
        let timeline = &self.timeline;
        let still_loaded =
            |key: &crate::attachments::AttachmentKey| is_attachment_in(timeline, key);
        self.attachment_views.retain(|key, _| still_loaded(key));
        self.attachment_workers.retain(|key, _| still_loaded(key));
        // A Download the user started finishes (or fails visibly) wherever they
        // scroll or go; it ends with the account or a purge. Open ends with its
        // row (#167).
        self.attachment_action_workers
            .retain(|(key, action), _| *action == AttachmentAction::Download || still_loaded(key));
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
        // Audio stops with its row: a hidden message would leave no way to pause it.
        if self
            .media
            .as_ref()
            .is_some_and(|media| !still_loaded(&media.key))
        {
            self.media = None;
        }
        self.private.notice = consumed.private_notice;
        let tiles = crate::call_layout::tiles(
            &self.controls.watched(),
            &self.controls.camera_tiles().snapshot(),
            self.controls.voice_controls_with(|c| c.camera.wanted),
        );
        self.call_view.prune(&crate::call_layout::ids(&tiles));
        self.composer.select(self.timeline.channel_id);
        if self
            .confirm_delete
            .is_some_and(|(channel, _)| self.timeline.channel_id != Some(channel))
        {
            self.confirm_delete = None;
        }
        if self
            .more
            .is_some_and(|(channel, _)| self.timeline.channel_id != Some(channel))
        {
            self.more = None;
        }
        consumed.status
    }

    pub fn emoji_visibility(&mut self, occurrence: emoji::Occurrence, visible: bool) {
        if !visible
            || occurrence_in(
                &self.timeline,
                &self.emoji_picker,
                &self.emoji_snapshot,
                &occurrence,
            )
        {
            self.emoji.visibility(occurrence, visible);
        }
    }

    /// Whether the attachment's message is still among the rows being built.
    pub fn is_attachment_loaded(&self, key: &crate::attachments::AttachmentKey) -> bool {
        is_attachment_in(&self.timeline, key)
    }

    /// Anyone may pin in a DM; a server needs Manage Messages in the channel.
    fn can_pin(&self) -> bool {
        let open = self.timeline.channel_id;
        open.is_some()
            && (self
                .navigation
                .private_selection
                .as_ref()
                .map(|p| p.channel_id)
                == open
                || self
                    .navigation
                    .selection
                    .as_ref()
                    .is_some_and(|s| Some(s.channel_id) == open && s.permissions.manage_messages))
    }

    /// Other people's link previews can be hidden where the account has Manage
    /// Messages in a server channel (never in a direct conversation).
    fn can_suppress_embeds(&self) -> bool {
        let open = self.timeline.channel_id;
        open.is_some()
            && self
                .navigation
                .selection
                .as_ref()
                .is_some_and(|s| Some(s.channel_id) == open && s.permissions.manage_messages)
    }

    /// A public thread can be started from a message of a server text channel
    /// where the account may create them (not inside a thread).
    fn can_thread(&self) -> bool {
        let open = self.timeline.channel_id;
        open.is_some()
            && self.navigation.selection.as_ref().is_some_and(|s| {
                Some(s.channel_id) == open
                    && matches!(
                        s.kind,
                        fastcord_discord::state::navigation::ChannelKind::Text
                            | fastcord_discord::state::navigation::ChannelKind::Announcement
                    )
                    && s.permissions.create_threads
            })
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
            can_pin: self.can_pin(),
            publish: self
                .timeline
                .channel_id
                .map_or(timeline::Publish::No, |open| {
                    crate::announce::right(self.navigation.selection.as_ref(), open)
                }),
            publishing: None,
            can_thread: self.can_thread(),
            can_suppress_embeds: self.can_suppress_embeds(),
            can_moderate: self.can_suppress_embeds(),
            more: self
                .more
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
        if !matches!(event, Event::More(_)) {
            self.more = None;
        }
        let queued = match event {
            Event::More(message) => {
                self.more = message
                    .zip(open)
                    .map(|(message, channel)| (channel, message));
                return false;
            }
            Event::SetEmbeds {
                channel_id,
                message_id,
                suppressed,
            } if open == Some(channel_id) => {
                self.confirm_delete = None;
                let others = self.can_suppress_embeds();
                self.controls
                    .set_embeds(channel_id, message_id, suppressed, others)
            }
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
            Event::SelectMode { channel_id, on } if open == Some(channel_id) => {
                self.confirm_delete = None;
                self.controls.select(channel_id, SelectOp::Mode(on))
            }
            Event::ToggleSelect {
                channel_id,
                message_id,
            } if open == Some(channel_id) => self
                .controls
                .select(channel_id, SelectOp::Toggle(message_id)),
            Event::AskBulkDelete { channel_id } if open == Some(channel_id) => {
                self.controls.select(channel_id, SelectOp::Ask)
            }
            Event::CancelBulkDelete => {
                open.is_none_or(|channel| self.controls.select(channel, SelectOp::Cancel))
            }
            Event::ConfirmBulkDelete { channel_id } if open == Some(channel_id) => {
                self.controls.select(channel_id, SelectOp::Confirm)
            }
            Event::AskReactionRemoval(target) => {
                self.controls.reaction_removal(ReactionOp::Ask(target))
            }
            Event::CancelReactionRemoval => self.controls.reaction_removal(ReactionOp::Cancel),
            Event::ConfirmReactionRemoval => self.controls.reaction_removal(ReactionOp::Confirm),
            Event::RetryChange {
                channel_id,
                message_id,
            } => self.controls.retry_change(channel_id, message_id),
            Event::DismissChange {
                channel_id,
                message_id,
            } => self.controls.dismiss_change(channel_id, message_id),
            Event::ToggleReaction {
                channel_id,
                message_id,
                emoji,
            } if open == Some(channel_id) => {
                self.controls.toggle_reaction(channel_id, message_id, emoji)
            }
            Event::InspectReaction {
                channel_id,
                message_id,
                emoji,
            } if open == Some(channel_id) => {
                self.controls.reaction_users(channel_id, message_id, emoji)
            }
            Event::CloseReactionUsers => self.controls.close_reaction_users(),
            Event::PollVote {
                channel_id,
                message_id,
                answer_id,
            } if open == Some(channel_id) => {
                self.controls.poll_vote(channel_id, message_id, answer_id)
            }
            Event::PollEnd {
                channel_id,
                message_id,
            } if open == Some(channel_id) => self.controls.poll_end(channel_id, message_id),
            Event::Component {
                channel_id,
                message_id,
                custom_id,
                values,
            } if open == Some(channel_id) => {
                self.controls
                    .interact(crate::interactions::Action::Component {
                        channel: channel_id,
                        message: message_id,
                        custom_id,
                        values,
                    })
            }
            Event::DismissEphemeral {
                channel_id,
                message_id,
            } if open == Some(channel_id) => {
                self.controls
                    .interact(crate::interactions::Action::Dismiss {
                        channel: channel_id,
                        message: message_id,
                    })
            }
            Event::PollVoters {
                channel_id,
                message_id,
                answer_id,
            } if open == Some(channel_id) => {
                self.controls.poll_voters(channel_id, message_id, answer_id)
            }
            Event::React {
                channel_id,
                message_id,
            } if open == Some(channel_id) && self.timeline.can_react => {
                self.confirm_delete = None;
                self.emoji_picker.open_for_reaction(channel_id, message_id);
                true
            }
            _ => true,
        };
        if !queued {
            if self.controls.stopped() {
                self.composer.needs_reconnect();
            } else {
                self.composer.refused();
            }
        }
        self.prune_stopped();
        false
    }

    /// While the Gateway is stopped no worker refreshes the snapshot: drop the
    /// unsent rows and failed changes the user discarded from what is shown.
    pub fn prune_stopped(&mut self) {
        let carried = self
            .controls
            .carried
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(carried) = carried.as_ref().filter(|_| self.controls.stopped()) else {
            return;
        };
        let stale = self
            .timeline
            .outbox
            .iter()
            .any(|item| !carried.holds_send(item.id))
            || self.timeline.rows.iter().any(|row| {
                row.change
                    .as_ref()
                    .is_some_and(|_| !carried.holds_change(row.message.channel_id, row.message.id))
            });
        if !stale {
            return;
        }
        let timeline = Arc::make_mut(&mut self.timeline);
        timeline.outbox.retain(|item| carried.holds_send(item.id));
        for row in &mut timeline.rows {
            if row.change.is_some() && !carried.holds_change(row.message.channel_id, row.message.id)
            {
                row.change = None;
            }
        }
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;

    #[tokio::test]
    async fn a_gone_worker_is_disconnected_not_a_rejected_login() {
        use fastcord_discord::{MemberAction, RestError};
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        // The worker took the command and ended without answering.
        let (result, _) = tokio::join!(
            bridge.moderate(Snowflake(1), Snowflake(2), MemberAction::Kick, None),
            async {
                let command = commands.recv().await;
                drop(command);
            }
        );
        assert_eq!(result, Err(RestError::Disconnected));
        // The worker is gone: its queue is closed.
        drop(commands);
        let result = bridge
            .moderate(Snowflake(1), Snowflake(2), MemberAction::Kick, None)
            .await;
        assert_eq!(result, Err(RestError::Disconnected));
    }

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
    fn switcher_queries_and_steps_are_requests_that_never_touch_voice() {
        use fastcord_discord::state::navigation::{Step, StepScope};
        let bridge = NavigationBridge::new();
        let requests = bridge.requests.subscribe();
        let voice = bridge.voice_requests.subscribe();
        bridge.quick_query(Some("gen"));
        assert_eq!(requests.borrow().quick.as_deref(), Some("gen"));
        assert_eq!(requests.borrow().revision, 0, "a query is not a selection");
        bridge.quick_query(None);
        assert!(requests.borrow().quick.is_none());
        // A voice or Stage channel picked in the switcher opens its chat.
        bridge.open_channel(Snowflake(77));
        assert!(matches!(
            requests.borrow().selection,
            Some(SelectionIntent::Open(Snowflake(77)))
        ));
        let step = Step {
            scope: StepScope::Unread,
            forward: false,
        };
        bridge.step(step);
        assert!(matches!(
            requests.borrow().selection,
            Some(SelectionIntent::Step(actual)) if actual == step
        ));
        assert!(
            voice.borrow().is_none(),
            "navigation sends no voice request"
        );
    }

    #[test]
    fn choosing_a_chat_and_requesting_voice_never_change_each_other() {
        let bridge = NavigationBridge::new();
        let requests = bridge.requests.subscribe();
        let voice = bridge.voice_requests.subscribe();
        bridge.select_channel(Snowflake(1), Snowflake(2));
        assert!(
            voice.borrow().is_none(),
            "opening a chat sends no voice request"
        );
        assert!(bridge.request_voice_state(VoiceStateRequest::join(
            Some(Snowflake(1)),
            Snowflake(3),
            false,
            false
        )));
        assert!(matches!(
            requests.borrow().selection,
            Some(SelectionIntent::Channel(Snowflake(1), Snowflake(2)))
        ));
        let revision = requests.borrow().revision;
        assert!(bridge.request_voice_state(VoiceStateRequest::leave(Some(Snowflake(1)))));
        assert_eq!(
            requests.borrow().revision,
            revision,
            "leaving keeps the chat"
        );
    }

    #[test]
    fn explicit_stream_intents_reach_the_bounded_worker_queue_in_order() {
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let key = "guild:41:127:1";
        assert!(bridge.create_stream(StreamCreateRequest::guild(Snowflake(41), Snowflake(127),)));
        assert!(bridge.watch_stream(key).is_ok());
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
    fn watch_slots_are_capped_and_full_commands_do_not_claim_an_unwatch() {
        use std::sync::atomic::Ordering;
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        for _ in 0..COMMAND_QUEUE {
            assert!(bridge.ping_stream("guild:41:127:1"));
        }
        assert!(bridge.watch_stream("guild:41:127:99").is_err());
        assert!(bridge.watched().is_empty());
        while commands.try_recv().is_ok() {}
        assert!(bridge.watch_stream("guild:41:127:99").is_ok());
        let session = bridge.watched().pop().unwrap();
        session.set_visible(true);
        // Watching it twice is refused, a second stream is a second slot.
        assert!(bridge.watch_stream("guild:41:127:99").is_err());
        assert!(bridge.watch_stream("guild:41:127:98").is_ok());
        assert_eq!(bridge.watched().len(), 2);
        while commands.try_recv().is_ok() {}
        for _ in 0..COMMAND_QUEUE {
            assert!(bridge.ping_stream("guild:41:127:1"));
        }
        assert!(!bridge.unwatch_stream(session.key.clone()));
        assert!(session.active.load(Ordering::Acquire));
        assert_eq!(bridge.watched().len(), 2);
        let _ = commands.try_recv().unwrap();
        assert!(bridge.unwatch_stream(session.key.clone()));
        assert_eq!(bridge.watched().len(), 1);
        assert!(!session.active.load(Ordering::Acquire));
        assert!(!session.visible.load(Ordering::Acquire));
        assert!(session.frame().is_none());
    }

    #[test]
    fn more_streams_than_the_cap_are_refused_with_a_reason_and_keep_the_old_ones() {
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        for owner in 0..crate::call_layout::MAX_WATCHED_STREAMS {
            assert!(
                bridge
                    .watch_stream(format!("guild:41:127:{}", 100 + owner))
                    .is_ok()
            );
            let _ = commands.try_recv().unwrap();
        }
        let refusal = bridge.watch_stream("guild:41:127:999").unwrap_err();
        assert!(refusal.contains("Stop watching one first"));
        assert!(
            commands.try_recv().is_err(),
            "a refused watch sends nothing"
        );
        let kept = bridge.watched();
        assert_eq!(kept.len(), crate::call_layout::MAX_WATCHED_STREAMS);
        assert!(
            kept.iter()
                .all(|session| session.active.load(std::sync::atomic::Ordering::Acquire))
        );
        bridge.stop_playback();
        assert!(bridge.watched().is_empty());
        assert!(
            kept.iter()
                .all(|session| !session.active.load(std::sync::atomic::Ordering::Acquire))
        );
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
        assert!(bridge.publish(
            snapshot.clone(),
            timeline(),
            emoji_picker::Snapshot::default(),
            None
        ));
        for total in 2..10_000 {
            snapshot.guilds.total = total;
            assert!(!bridge.publish(
                snapshot.clone(),
                timeline(),
                emoji_picker::Snapshot::default(),
                None
            ));
        }
        assert_eq!(bridge.consume().navigation.guilds.total, 9_999);
        assert!(!bridge.publish(
            snapshot.clone(),
            timeline(),
            emoji_picker::Snapshot::default(),
            None
        ));
        snapshot.guilds.total = 10_000;
        assert!(bridge.publish(
            snapshot,
            timeline(),
            emoji_picker::Snapshot::default(),
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
        assert!(bridge.publish(
            navigation.clone(),
            snapshot.clone(),
            emoji_picker::Snapshot::default(),
            None
        ));
        snapshot.loading = false;
        snapshot.has_older = true;
        assert!(!bridge.publish(
            navigation.clone(),
            snapshot,
            emoji_picker::Snapshot::default(),
            None
        ));
        let consumed = bridge.consume();
        assert!(!consumed.timeline.loading);
        assert!(consumed.timeline.has_older);
        // Nothing changed: no notification, even though the slot was read.
        assert!(!bridge.publish(
            navigation,
            consumed.timeline.as_ref().clone(),
            emoji_picker::Snapshot::default(),
            None
        ));
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
    fn a_continuing_bridge_shares_the_outbox_capacity_and_carried_work() {
        let old = NavigationBridge::new();
        assert!(old.outbox_slots.reserve());
        let mut new = NavigationBridge::new();
        new.inherit(&old);
        assert_eq!(
            new.outbox_slots.used(),
            1,
            "the carried send keeps its slot"
        );
        *old.carried.lock().unwrap() = Some(History::default().carry());
        assert!(new.carried.lock().unwrap().is_some());
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
        assert!(bridge.edit_message(c, m, "secret words".to_owned(), None));
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
        assert!(!bridge.edit_message(c, m, "more".to_owned(), None));
        assert!(!bridge.delete_message(c, m));
    }

    #[test]
    fn reaction_commands_queue_in_order_and_a_full_queue_refuses_them() {
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let (c, m) = (Snowflake(500), Snowflake(7));
        let thumbs = Emoji {
            id: None,
            name: Some("\u{1f44d}".to_owned()),
            animated: false,
        };
        assert!(bridge.toggle_reaction(c, m, thumbs.clone()));
        assert!(bridge.add_reaction(c, m, thumbs.clone()));
        assert!(bridge.reaction_users(c, m, thumbs.clone()));
        assert!(bridge.close_reaction_users());
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::ToggleReaction { channel, message, emoji })
                if (channel, message, &emoji) == (c, m, &thumbs)
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::AddReaction { emoji, .. }) if emoji == thumbs
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::ReactionUsers { emoji, .. }) if emoji == thumbs
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::CloseReactionUsers)
        ));
        let shown = format!(
            "{:?} {:?}",
            Command::ToggleReaction {
                channel: c,
                message: m,
                emoji: thumbs.clone()
            },
            Command::CloseReactionUsers
        );
        assert!(shown.contains("ToggleReaction") && shown.contains("CloseReactionUsers"));
        // A full queue refuses rather than buffering without bound.
        for _ in 0..COMMAND_QUEUE {
            assert!(bridge.toggle_reaction(c, m, thumbs.clone()));
        }
        assert!(!bridge.toggle_reaction(c, m, thumbs.clone()));
        assert!(!bridge.add_reaction(c, m, thumbs.clone()));
        assert!(!bridge.reaction_users(c, m, thumbs));
        assert!(!bridge.close_reaction_users());
    }

    #[test]
    fn poll_actions_are_queued_for_the_open_channel_only_and_text_free() {
        use timeline::Event;
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let mut panel = panel(&bridge);
        let (channel_id, message_id) = (Snowflake(500), Snowflake(7));
        panel.message_action(Event::PollVote {
            channel_id: Snowflake(501),
            message_id,
            answer_id: 1,
        });
        panel.message_action(Event::PollEnd {
            channel_id: Snowflake(501),
            message_id,
        });
        panel.message_action(Event::PollVoters {
            channel_id: Snowflake(501),
            message_id,
            answer_id: 1,
        });
        assert!(commands.try_recv().is_err(), "another channel asks nothing");
        panel.message_action(Event::PollVote {
            channel_id,
            message_id,
            answer_id: 2,
        });
        panel.message_action(Event::PollVoters {
            channel_id,
            message_id,
            answer_id: 2,
        });
        panel.message_action(Event::PollEnd {
            channel_id,
            message_id,
        });
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::PollVote { channel, message, answer })
                if (channel, message, answer) == (channel_id, message_id, 2)
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::PollVoters { answer: 2, .. })
        ));
        assert!(matches!(commands.try_recv(), Ok(Command::PollEnd { .. })));
        for _ in 0..COMMAND_QUEUE {
            assert!(bridge.poll_vote(channel_id, message_id, 1));
        }
        assert!(!bridge.poll_vote(channel_id, message_id, 1));
        assert!(!bridge.poll_end(channel_id, message_id));
        assert!(!bridge.poll_voters(channel_id, message_id, 1));
    }

    #[test]
    fn interaction_actions_are_queued_for_the_open_channel_only() {
        use crate::interactions::Action;
        use timeline::Event;
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let mut panel = panel(&bridge);
        let (channel_id, message_id) = (Snowflake(500), Snowflake(7));
        let press = |channel_id| Event::Component {
            channel_id,
            message_id,
            custom_id: "yes".to_owned(),
            values: vec!["a".to_owned()],
        };
        panel.message_action(press(Snowflake(501)));
        panel.message_action(Event::DismissEphemeral {
            channel_id: Snowflake(501),
            message_id,
        });
        assert!(commands.try_recv().is_err(), "another channel asks nothing");
        panel.message_action(press(channel_id));
        panel.message_action(Event::DismissEphemeral {
            channel_id,
            message_id,
        });
        let Ok(Command::Interact(first)) = commands.try_recv() else {
            panic!("nothing queued");
        };
        assert_eq!(
            *first,
            Action::Component {
                channel: channel_id,
                message: message_id,
                custom_id: "yes".to_owned(),
                values: vec!["a".to_owned()],
            }
        );
        let Ok(Command::Interact(second)) = commands.try_recv() else {
            panic!("no dismissal");
        };
        assert_eq!(
            *second,
            Action::Dismiss {
                channel: channel_id,
                message: message_id
            }
        );
        // Typed answers never reach a log through Debug.
        assert!(!format!("{first:?}").contains("yes"));
        for _ in 0..COMMAND_QUEUE {
            assert!(bridge.interact(Action::CloseModal));
        }
        assert!(!bridge.interact(Action::CloseModal));
    }

    #[test]
    fn reaction_actions_act_on_the_open_channel_and_react_needs_the_permission() {
        use timeline::Event;
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let mut panel = panel(&bridge);
        let thumbs = Emoji {
            id: None,
            name: Some("\u{1f44d}".to_owned()),
            animated: false,
        };
        let (channel_id, message_id) = (Snowflake(500), Snowflake(7));
        // The picker opens for a message only where the worker said the user may react.
        panel.message_action(Event::React {
            channel_id,
            message_id,
        });
        assert!(!panel.emoji_picker.request.open);
        panel.timeline = Arc::new(timeline::Snapshot {
            can_react: true,
            can_tts: true,
            ..(*panel.timeline).clone()
        });
        panel.message_action(Event::React {
            channel_id: Snowflake(501),
            message_id,
        });
        assert!(!panel.emoji_picker.request.open, "another channel");
        panel.confirm_delete = Some((channel_id, message_id));
        panel.message_action(Event::React {
            channel_id,
            message_id,
        });
        assert!(panel.emoji_picker.request.open);
        assert_eq!(panel.emoji_picker.reaction(), Some(message_id));
        assert_eq!(panel.confirm_delete, None);
        assert!(commands.try_recv().is_err(), "choosing is not reacting");

        // Chip clicks and requests for the open channel are queued; others are not.
        panel.message_action(Event::ToggleReaction {
            channel_id,
            message_id,
            emoji: thumbs.clone(),
        });
        panel.message_action(Event::InspectReaction {
            channel_id,
            message_id,
            emoji: thumbs.clone(),
        });
        panel.message_action(Event::CloseReactionUsers);
        panel.message_action(Event::ToggleReaction {
            channel_id: Snowflake(501),
            message_id,
            emoji: thumbs.clone(),
        });
        panel.message_action(Event::InspectReaction {
            channel_id: Snowflake(501),
            message_id,
            emoji: thumbs,
        });
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::ToggleReaction { .. })
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::ReactionUsers { .. })
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::CloseReactionUsers)
        ));
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn chip_emoji_are_wanted_only_while_their_row_and_chip_still_show_them() {
        let bridge = NavigationBridge::new();
        let mut panel = panel(&bridge);
        let wave = Emoji {
            id: Some(Snowflake(9)),
            name: Some("wave".to_owned()),
            animated: true,
        };
        let mut message = (*panel.timeline.rows[0].message).clone();
        message.reactions = vec![fastcord_model::Reaction {
            count: 2,
            me: false,
            emoji: wave.clone(),
        }];
        panel.timeline = Arc::new(timeline::Snapshot {
            rows: vec![timeline::Row::new(Arc::new(message), 3)],
            ..(*panel.timeline).clone()
        });
        let asset = emoji::Asset::Custom {
            id: Snowflake(9),
            animated: true,
        };
        let occurrence = |channel: u64, revision: u64, chip: u8, asset| emoji::Occurrence {
            origin: emoji::Origin::Reaction {
                channel: Snowflake(channel),
                message: Snowflake(7),
                revision,
                chip,
            },
            asset,
        };
        let wanted = |panel: &GatewayPanel, occurrence| {
            occurrence_in(
                &panel.timeline,
                &panel.emoji_picker,
                &panel.emoji_snapshot,
                &occurrence,
            )
        };
        assert!(wanted(&panel, occurrence(500, 3, 0, asset)));
        assert!(!wanted(&panel, occurrence(501, 3, 0, asset)), "channel");
        assert!(!wanted(&panel, occurrence(500, 2, 0, asset)), "revision");
        assert!(
            !wanted(&panel, occurrence(500, 3, 1, asset)),
            "no such chip"
        );
        let other = emoji::Asset::Custom {
            id: Snowflake(10),
            animated: true,
        };
        assert!(!wanted(&panel, occurrence(500, 3, 0, other)), "other emoji");
        // An unanswered request adds a chip after Discord's, and it is wanted too.
        let mut pending = (*panel.timeline).clone();
        pending.rows[0].reactions = vec![crate::reactions::Pending {
            emoji: Emoji {
                id: Some(Snowflake(10)),
                name: Some("new".to_owned()),
                animated: true,
            },
            action: crate::reactions::Action::Add,
        }];
        panel.timeline = Arc::new(pending);
        assert!(wanted(&panel, occurrence(500, 3, 1, other)));
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
    fn drafts_follow_the_open_conversation_through_the_worker_snapshots() {
        use iced::widget::text_editor::{Action, Edit};
        let bridge = NavigationBridge::new();
        let mut panel = panel(&bridge);
        panel.composer.perform(Action::Edit(Edit::Paste(Arc::new(
            "half a thought".to_owned(),
        ))));
        let open = |channel: u64| {
            bridge.publish(
                NavigationSnapshot::default(),
                timeline::Snapshot {
                    channel_id: Some(Snowflake(channel)),
                    can_send: true,
                    ..timeline::Snapshot::default()
                },
                emoji_picker::Snapshot::default(),
                None,
            );
        };
        // Navigation from anywhere (sidebar, search, pins, inbox, switcher)
        // ends in the open conversation changing; the draft follows it.
        open(600);
        panel.consume();
        assert!(panel.composer.submission(true, 0).is_none(), "600 is empty");
        assert!(panel.composer.has_draft(Snowflake(500)));
        open(500);
        panel.consume();
        assert_eq!(
            panel.composer.submission(true, 0).unwrap().content,
            "half a thought"
        );
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
            emoji_picker::Snapshot::default(),
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

        bridge.publish(
            NavigationSnapshot::default(),
            with_attachment(500, 7),
            emoji_picker::Snapshot::default(),
            None,
        );
        panel.consume();
        for message in [7, 8] {
            let key = key(500, message);
            panel.attachment_views.insert(key, AttachmentView::Loading);
            panel.attachment_workers.insert(key, handle());
            panel
                .attachment_action_workers
                .insert((key, AttachmentAction::Open), handle());
        }
        // A Download the user started outlives its row and the channel (#167).
        panel
            .attachment_action_workers
            .insert((key(500, 8), AttachmentAction::Download), handle());
        panel.attachment_viewer = Some(crate::attachment_ui::AttachmentViewer::for_test(
            key(500, 8),
            attachment,
        ));
        panel.media = Some(crate::media_ui::MediaSession::for_test(key(500, 8)));
        // Message 8 is no longer among the built rows: its previews and workers
        // are cancelled; the modal view stays while the channel is open.
        bridge.publish(
            NavigationSnapshot::default(),
            with_attachment(500, 7),
            emoji_picker::Snapshot::default(),
            None,
        );
        panel.consume();
        assert_eq!(panel.attachment_views.len(), 1);
        assert!(panel.attachment_views.contains_key(&key(500, 7)));
        assert_eq!(panel.attachment_workers.len(), 1);
        assert_eq!(panel.attachment_action_workers.len(), 2);
        assert!(
            panel
                .attachment_action_workers
                .contains_key(&(key(500, 8), AttachmentAction::Download))
        );
        assert!(panel.is_attachment_loaded(&key(500, 7)));
        assert!(!panel.is_attachment_loaded(&key(500, 8)));
        assert!(panel.attachment_viewer.is_some());
        // Audio stops with its row: with the row gone there would be no way to pause it.
        assert!(panel.media.is_none());
        panel.media = Some(crate::media_ui::MediaSession::for_test(key(500, 7)));
        bridge.publish(
            NavigationSnapshot::default(),
            with_attachment(500, 7),
            emoji_picker::Snapshot::default(),
            None,
        );
        panel.consume();
        assert!(panel.media.is_some(), "kept while its row is built");
        // Leaving the channel cancels everything, the viewer included.
        bridge.publish(
            NavigationSnapshot::default(),
            with_attachment(501, 7),
            emoji_picker::Snapshot::default(),
            None,
        );
        panel.consume();
        assert!(panel.attachment_views.is_empty());
        assert!(panel.attachment_workers.is_empty());
        assert_eq!(
            panel.attachment_action_workers.keys().collect::<Vec<_>>(),
            [&(key(500, 8), AttachmentAction::Download)],
            "only the download is still running"
        );
        assert!(panel.attachment_viewer.is_none());
        assert!(panel.media.is_none());
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
                can_pin: false,
                can_thread: false,
                confirming: None,
                ..Default::default()
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

    #[test]
    fn loading_volume_settings_keeps_live_changes_and_both_bounds() {
        let bridge = NavigationBridge::new();
        bridge.set_volume_percent(Snowflake(1), 6);
        bridge.set_volume_settings(
            (1..=300)
                .map(|id| UserVolume {
                    user_id: Snowflake(id),
                    percent: 999,
                })
                .collect(),
        );
        let controls = bridge.voice_controls();
        assert_eq!(controls.volume_percent(Snowflake(1)), 6);
        assert_eq!(controls.volume_percent(Snowflake(256)), 200);
        assert_eq!(controls.volumes.len(), 256);
        bridge.set_volume_percent(Snowflake(2), 67);
        bridge.set_volume_percent(Snowflake(301), 62);
        let controls = bridge.voice_controls();
        assert_eq!(controls.volume_percent(Snowflake(2)), 67);
        assert_eq!(controls.volumes.len(), 256);
    }

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
            emojis: Vec::new(),
            members: Vec::new(),
            voice_states: None,
            member_count: 0,
            stage_instances: Vec::new(),
            scheduled_events: Vec::new(),
            settings: Default::default(),
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
            owner_id: None,
            icon: None,
            last_message_id: None,
            is_message_request: None,
            is_spam: None,
            thread_metadata: None,
            member: None,
            available_tags: Vec::new(),
            applied_tags: Vec::new(),
            flags: 0,
            message_count: None,
            default_sort_order: None,
            topic: None,
            nsfw: false,
            rate_limit_per_user: 0,
            bitrate: 0,
            user_limit: 0,
            rtc_region: None,
            video_quality_mode: None,
        };
        GatewayEvent::Dispatch {
            sequence: 1,
            event: Dispatch::Ready(Box::new(Ready {
                session_id: SessionId::new("fixture-session".to_owned()),
                user: user(),
                read: Default::default(),
                relationships: Vec::new(),
                ignored: Vec::new(),
                presences: Vec::new(),
                users: vec![user()],
                guilds: (0..guilds as u64).map(guild).collect(),
                unavailable_guilds: (0..unavailable as u64).map(Snowflake).collect(),
                private_channels: (0..dms as u64).map(dm).collect(),
            })),
        }
    }
    #[test]
    fn a_channel_of_another_server_opens_that_server_first() {
        let mut tracker = Tracker::default();
        let GatewayEvent::Dispatch {
            event: Dispatch::Ready(mut ready),
            ..
        } = ready_event(2, 0, 1)
        else {
            unreachable!()
        };
        for guild in &mut ready.guilds {
            guild.owner_id = Some(Snowflake(1));
        }
        ready.guilds[1].channels.push(
            serde_json::from_value(serde_json::json!({
                "id": "50", "type": 0, "guild_id": "1", "name": "general"
            }))
            .unwrap(),
        );
        tracker.apply(GatewayEvent::Dispatch {
            sequence: 1,
            event: Dispatch::Ready(ready),
        });
        let (store, navigation) = (&tracker.store, &mut tracker.navigation);
        assert!(navigation.select_private_channel(store, Snowflake(0)));
        open_guild_channel(navigation, store, Snowflake(1), Snowflake(50));
        assert_eq!(navigation.selected_guild(), Some(Snowflake(1)));
        assert_eq!(navigation.selected_channel(), Some(Snowflake(50)));
        // A channel that is not in the named server is still refused.
        open_guild_channel(navigation, store, Snowflake(0), Snowflake(50));
        assert_ne!(navigation.selected_channel(), Some(Snowflake(50)));
    }

    use fastcord_discord::gateway::{ClientProperties, HostOs};
    fn worker() -> Worker {
        let bridge = NavigationBridge::new();
        let properties = ClientProperties::bundled(HostOs::Linux, "en-US");
        let rest = RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-fixture-credential".to_owned()),
            properties,
        )
        .unwrap();
        Worker {
            start: None,
            rest: rest.clone(),
            gateway: None,
            tracker: Tracker::default(),
            requests: bridge.requests.subscribe(),
            voice_requests: bridge.voice_requests.subscribe(),
            voice_audio: bridge.voice_audio.subscribe(),
            voice_controls: bridge.voice_controls.subscribe(),
            voice_flags: (false, false, false),
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
            share: None,
            share_dirty: false,
            typing_send: TypingSend::default(),
            bridge,
        }
    }

    /// The real stop path: a send still queued in the command channel when the
    /// Gateway stops is not lost, keeps its slot as a visible failed row, and
    /// the Reconnect worker starts with it; later senders are refused.
    #[tokio::test]
    async fn a_limit_stop_carries_queued_sends_into_the_reconnect_worker() {
        let mut worker = worker();
        assert!(
            worker
                .bridge
                .send_message(Snowflake(300), "queued".to_owned(), false, None)
        );
        assert_eq!(worker.bridge.outbox_slots.used(), 1);
        worker.answer_in_flight().await;
        assert!(
            !worker
                .bridge
                .send_message(Snowflake(300), "late".to_owned(), false, None),
            "the stopped worker accepts nothing"
        );
        assert_eq!(worker.bridge.outbox_slots.used(), 1, "no slot leaks");
        worker.deposit_carried();

        let mut next = NavigationBridge::new();
        next.inherit(&worker.bridge);
        let history = next.new_history();
        let rows = history.outbox_items();
        assert_eq!(rows.len(), 1, "the queued send survived: {rows:?}");
        assert_eq!(&*rows[0].content, "queued");
        assert!(!matches!(
            rows[0].state,
            crate::outbox::OutboxState::Sending
        ));
        assert_eq!(next.outbox_slots.used(), 1);
    }

    /// A stopped worker with one queued send turned into a carried failed row.
    async fn stopped_with_one_send() -> (
        Worker,
        Vec<crate::outbox::OutboxItem>,
        fastcord_discord::Nonce,
    ) {
        let mut worker = worker();
        // As in production: the history releases the bridge's slots.
        worker.tracker.history = History::new(worker.bridge.outbox_slots.clone());
        assert!(
            worker
                .bridge
                .send_message(Snowflake(500), "unsent".to_owned(), false, None)
        );
        worker.answer_in_flight().await;
        let items = worker.tracker.history.outbox_items();
        let nonce = worker.tracker.history.nonce_of(items[0].id);
        worker.deposit_carried();
        (worker, items, nonce.expect("the send has a nonce"))
    }

    #[tokio::test]
    async fn discard_while_stopped_removes_the_row_and_frees_the_slot() {
        let (worker, items, _) = stopped_with_one_send().await;
        let id = items[0].id;
        let bridge = worker.bridge.clone();
        let (_, handle) = iced::Task::<()>::none().abortable();
        let mut panel = GatewayPanel::new(1, handle, bridge.clone());
        let mut shown = (*panel.timeline).clone();
        shown.outbox = items;
        panel.timeline = Arc::new(shown);
        assert_eq!(bridge.outbox_slots.used(), 1);
        assert!(bridge.stopped());
        assert!(bridge.discard_send(id));
        assert_eq!(bridge.outbox_slots.used(), 0);
        panel.prune_stopped();
        assert!(panel.timeline.outbox.is_empty(), "the row is gone");
        assert!(!bridge.discard_send(id), "only once");

        let mut next = NavigationBridge::new();
        next.inherit(&bridge);
        assert!(next.new_history().outbox_items().is_empty());
        assert_eq!(next.outbox_slots.used(), 0);
    }

    #[tokio::test]
    async fn retry_while_stopped_hints_and_keeps_the_row_then_reuses_the_nonce() {
        let (worker, items, nonce) = stopped_with_one_send().await;
        let id = items[0].id;
        let bridge = worker.bridge.clone();
        let mut composer = crate::composer::Composer::default();
        let snapshot = timeline::Snapshot::default();
        crate::composer::update(
            &mut composer,
            crate::composer::Event::Retry(id),
            &snapshot,
            &bridge,
        );
        assert!(
            composer
                .notice()
                .is_some_and(|text| text.contains("Reconnect first")),
            "{:?}",
            composer.notice()
        );
        assert_eq!(bridge.outbox_slots.used(), 1, "the row is kept");

        let mut next = NavigationBridge::new();
        next.inherit(&bridge);
        let mut history = next.new_history();
        assert_eq!(history.outbox_items().len(), 1);
        history.retry_send(&worker.rest, id);
        assert_eq!(
            history.nonce_of(id),
            Some(nonce),
            "same nonce after Reconnect"
        );
    }

    #[tokio::test]
    async fn repeated_stream_create_does_not_abort_either_send_or_watch_task() {
        let mut worker = worker();
        worker.stream_intents.ready(Snowflake(1));
        worker.stream_correlator = Some(StreamCorrelator::new(Snowflake(1), "fixture-session"));
        worker
            .stream_intents
            .record_create(&StreamCreateRequest::guild(Snowflake(41), Snowflake(127)));
        let own = StreamKey::from_wire("guild:41:127:1").unwrap();
        let other = StreamKey::from_wire("guild:41:127:2").unwrap();
        worker.stream_intents.watch(other.clone());
        for (key, rtc) in [(&own, 500), (&other, 600)] {
            let created = fastcord_model::StreamCreate {
                stream_key: key.to_wire(),
                rtc_server_id: Snowflake(rtc),
                rtc_channel_id: Snowflake(rtc + 1),
            };
            worker.stream_correlator.as_mut().unwrap().create(&created);
            worker
                .stream_tasks
                .insert(key.clone(), tokio::spawn(std::future::pending::<()>()));
            assert!(matches!(
                worker.stream_dispatch(&GatewayEvent::Dispatch {
                    sequence: 2,
                    event: Dispatch::StreamCreate(Box::new(created)),
                }),
                Some(StreamCorrelation::Waiting)
            ));
        }
        tokio::task::yield_now().await;
        assert_eq!(worker.stream_tasks.len(), 2);
        assert!(worker.stream_tasks.values().all(|task| !task.is_finished()));
        worker.stop_stream_key(&other.to_wire());
        assert!(!worker.stream_tasks[&own].is_finished());
        worker.stop_stream_key(&own.to_wire());
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
            owner_id: None,
            icon: None,
            last_message_id: None,
            is_message_request: None,
            is_spam: None,
            thread_metadata: None,
            member: None,
            available_tags: Vec::new(),
            applied_tags: Vec::new(),
            flags: 0,
            message_count: None,
            default_sort_order: None,
            topic: None,
            nsfw: false,
            rate_limit_per_user: 0,
            bitrate: 0,
            user_limit: 0,
            rtc_region: None,
            video_quality_mode: None,
        }
    }

    #[test]
    fn the_joined_channel_bitrate_follows_updates_and_moves_only() {
        let channel = |id: u64, bitrate: u32| {
            serde_json::from_str::<Channel>(&format!(
                r#"{{"id":"{id}","type":2,"guild_id":"0","bitrate":{bitrate}}}"#
            ))
            .unwrap()
        };
        let dispatch = |event| GatewayEvent::Dispatch { sequence: 2, event };
        let mut worker = worker();
        let mut ready = ready_event(1, 0, 0);
        if let GatewayEvent::Dispatch {
            event: Dispatch::Ready(ready),
            ..
        } = &mut ready
        {
            ready.guilds[0].channels = vec![channel(10, 96_000), channel(11, 32_000)];
        }
        worker.apply_gateway_event(ready, |_| {});
        let bitrate = |worker: &Worker| worker.bridge.voice_controls.borrow().send_bitrate();
        assert_eq!(bitrate(&worker), fastcord_audio::DEFAULT_BITRATE, "no call");

        worker
            .bridge
            .voice_requests
            .send_replace(Some(VoiceStateRequest::join(
                Some(Snowflake(0)),
                Snowflake(10),
                false,
                false,
            )));
        worker.apply_gateway_event(dispatch(Dispatch::Resumed), |_| {});
        assert_eq!(bitrate(&worker), 96_000);

        // Another channel's update leaves the call alone; the joined one's is applied.
        let mut seen = worker.bridge.voice_controls.subscribe();
        seen.mark_unchanged();
        worker.apply_gateway_event(
            dispatch(Dispatch::ChannelUpdate(Box::new(channel(11, 64_000)))),
            |_| {},
        );
        assert!(!seen.has_changed().unwrap());
        worker.apply_gateway_event(
            dispatch(Dispatch::ChannelUpdate(Box::new(channel(10, 1_000_000)))),
            |_| {},
        );
        assert_eq!(bitrate(&worker), 384_000, "network input is clamped");

        // A move reported by Discord wins over the requested channel.
        let moved = r#"{"guild_id":"0","channel_id":"11","user_id":"1","session_id":"s"}"#;
        let moved = fastcord_discord::gateway::VoiceStateUpdate {
            state: serde_json::from_str(moved).unwrap(),
            member: None,
            users: Vec::new(),
        };
        worker.apply_gateway_event(
            dispatch(Dispatch::VoiceStateUpdate(Box::new(moved))),
            |_| {},
        );
        assert_eq!(bitrate(&worker), 64_000);

        // A DM call keeps the default.
        worker
            .bridge
            .voice_requests
            .send_replace(Some(VoiceStateRequest::join(
                None,
                Snowflake(99),
                false,
                false,
            )));
        worker.apply_gateway_event(dispatch(Dispatch::Resumed), |_| {});
        assert_eq!(bitrate(&worker), fastcord_audio::DEFAULT_BITRATE);
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

    #[test]
    fn ready_with_private_channels_skips_the_rest_list_and_without_them_fetches() {
        let started = std::cell::Cell::new(0);
        let mut worker = worker();
        worker.apply_gateway_event(ready_event(0, 0, 2), |_| started.set(started.get() + 1));
        assert_eq!(started.get(), 0, "READY already carried the DMs");
        assert!(worker.private_list_task.is_none());
        assert_eq!(
            worker.private_list_loaded_generation,
            Some(worker.private_list_generation)
        );
        worker.apply_gateway_event(ready_event(0, 0, 0), |_| started.set(started.get() + 1));
        assert_eq!(started.get(), 1, "READY without DMs still fetches the list");
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
    fn typing_shows_until_a_new_session_and_unknown_users_are_someone() {
        let mut tracker = Tracker::default();
        let channel = Snowflake(7);
        let started = Dispatch::TypingStart(fastcord_discord::gateway::TypingStart {
            channel_id: channel,
            user_id: Snowflake(9),
        });
        // Another channel's typing is recorded but rebuilds nothing (#153).
        tracker.apply_typing(&started);
        assert!(!tracker.typing_dirty);
        assert!(
            tracker.typing.next_expiry(None).is_none(),
            "and arms no timer"
        );
        assert_eq!(tracker.typing_names(Some(channel)), ["Someone"]);
        tracker.history.set_open_channel(Some(channel));
        tracker.apply_typing(&started);
        assert!(tracker.typing_dirty);
        assert_eq!(tracker.typing_names(Some(channel)), ["Someone"]);
        assert!(tracker.typing_names(Some(Snowflake(8))).is_empty());
        tracker.apply_typing(&Dispatch::Resumed);
        assert!(tracker.typing_names(Some(channel)).is_empty());
        assert!(
            tracker.typing.next_expiry(Some(channel)).is_none(),
            "no timer when idle"
        );
    }

    #[test]
    fn typing_of_an_ignored_user_is_not_shown_and_returns_when_ignoring_stops() {
        let mut tracker = Tracker::default();
        let channel = Snowflake(7);
        tracker.apply_typing(&Dispatch::TypingStart(
            fastcord_discord::gateway::TypingStart {
                channel_id: channel,
                user_id: Snowflake(9),
            },
        ));
        let ignore = |ignored| GatewayEvent::Dispatch {
            sequence: 1,
            event: Dispatch::RelationshipUpdate(fastcord_discord::gateway::RelationshipUpdate {
                id: Snowflake(9),
                kind: None,
                ignored: Some(ignored),
            }),
        };
        let _ = tracker.apply(ignore(true));
        assert!(tracker.typing_names(Some(channel)).is_empty());
        let _ = tracker.apply(ignore(false));
        assert_eq!(tracker.typing_names(Some(channel)), ["Someone"]);
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
    /// The transmit gate: every restriction wins over push-to-talk and voice activity.
    #[test]
    fn transmit_gate_truth_table() {
        use crate::shortcuts::InputMode::{PushToTalk, VoiceActivity};
        for mode in [VoiceActivity, PushToTalk] {
            for (self_muted, deafened, server_muted, push_to_mute, vad_forbidden, ptt) in (0..64u8)
                .map(|bits| {
                    (
                        bits & 1 != 0,
                        bits & 2 != 0,
                        bits & 4 != 0,
                        bits & 8 != 0,
                        bits & 16 != 0,
                        bits & 32 != 0,
                    )
                })
            {
                for suppressed in [false, true] {
                    let controls = VoiceControls {
                        input_mode: mode,
                        self_muted,
                        deafened,
                        server_muted,
                        push_to_mute_down: push_to_mute,
                        vad_forbidden,
                        stage: StageControls {
                            suppressed,
                            ..StageControls::default()
                        },
                        ..VoiceControls::default()
                    };
                    let restricted =
                        self_muted || deafened || server_muted || push_to_mute || suppressed;
                    let needs_key = mode == PushToTalk || vad_forbidden;
                    assert_eq!(
                        controls.capture_enabled_with(ptt),
                        !restricted && (!needs_key || ptt),
                        "{mode:?} mute={self_muted} deaf={deafened} server={server_muted} ptm={push_to_mute} novad={vad_forbidden} ptt={ptt} audience={suppressed}"
                    );
                }
            }
        }
    }

    #[test]
    fn sensitivity_applies_only_to_voice_activity() {
        let mut controls = VoiceControls {
            sensitivity: Some(45),
            ..VoiceControls::default()
        };
        assert_eq!(controls.effective_sensitivity(), Some(45));
        controls.vad_forbidden = true;
        assert_eq!(controls.effective_sensitivity(), None);
    }

    #[test]
    fn cancelling_push_to_talk_releases_the_key_and_counts() {
        let bridge = NavigationBridge::new();
        bridge.set_ptt(true);
        assert!(bridge.voice_controls().ptt_down);
        bridge.cancel_ptt();
        let controls = bridge.voice_controls();
        assert!(!controls.ptt_down);
        assert_eq!(controls.ptt_cancel, 1);
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

    #[test]
    fn voice_names_fall_back_to_the_short_id_form() {
        let controls = VoiceControls {
            names: vec![(Snowflake(7), "Alice".to_owned())],
            ..VoiceControls::default()
        };
        assert_eq!(controls.display_name(Snowflake(7)), "Alice");
        assert_eq!(controls.display_name(Snowflake(12_345)), "User 2345");
    }

    #[test]
    fn noise_preferences_and_reselection_preserve_other_voice_controls() {
        let bridge = NavigationBridge::new();
        assert_eq!(
            bridge.voice_controls().noise_suppression,
            fastcord_audio::NoiseSuppression::Light
        );
        let preferences = crate::audio_devices::Preferences {
            noise_suppression: fastcord_audio::NoiseSuppression::HighQuality,
            ..crate::audio_devices::Preferences::default()
        };
        bridge.set_audio_devices(&preferences);
        bridge.set_self_muted(true);
        bridge.voice_controls.modify(|controls| {
            controls.noise_status = Some(fastcord_audio::NoiseSuppressionStatus {
                error: Some(fastcord_audio::DenoiseError::ModelLoad),
                ..fastcord_audio::NoiseSuppressionStatus::default()
            });
        });
        let before = bridge.voice_controls();
        assert_eq!(before.noise_suppression, preferences.noise_suppression);
        bridge.select_noise_suppression(preferences.noise_suppression);
        let after = bridge.voice_controls();
        assert_eq!(after.noise_suppression, before.noise_suppression);
        assert_eq!(after.noise_generation, before.noise_generation + 1);
        assert_eq!(after.noise_status, before.noise_status);
        assert_eq!(after.input_device, before.input_device);
        assert_eq!(after.output_device, before.output_device);
        assert!(after.self_muted);
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
    fn the_own_stage_seat_follows_the_servers_voice_state_at_once() {
        let mut worker = worker();
        *worker.bridge.account_id.lock().unwrap() = Some(Snowflake(1));
        worker.bridge.voice_controls.modify(|controls| {
            controls.stage = StageControls {
                active: true,
                suppressed: true,
                channel: Some(Snowflake(70)),
                ..StageControls::default()
            };
        });
        let update =
            |user: u64, channel: u64, suppress: bool, hand: Option<&str>| GatewayEvent::Dispatch {
                sequence: 1,
                event: Dispatch::VoiceStateUpdate(Box::new(
                    fastcord_discord::gateway::VoiceStateUpdate {
                        state: fastcord_model::VoiceState {
                            guild_id: Some(Snowflake(7)),
                            channel_id: Some(Snowflake(channel)),
                            user_id: Snowflake(user),
                            session_id: String::new(),
                            deaf: false,
                            mute: false,
                            self_deaf: false,
                            self_mute: false,
                            self_stream: false,
                            self_video: false,
                            suppress,
                            request_to_speak_timestamp: hand.map(str::to_owned),
                        },
                        member: None,
                        users: Vec::new(),
                    },
                )),
            };
        let seat = |worker: &Worker| {
            let controls = worker.bridge.voice_controls();
            (controls.stage.suppressed, controls.stage.requested)
        };
        // Someone else's state, or another channel's, changes nothing.
        worker.voice_dispatch(&update(2, 70, false, None));
        worker.voice_dispatch(&update(1, 71, false, None));
        assert_eq!(seat(&worker), (true, false));
        worker.voice_dispatch(&update(
            1,
            70,
            true,
            Some("2026-10-21T10:00:00.000000+00:00"),
        ));
        assert_eq!(seat(&worker), (true, true));
        worker.voice_dispatch(&update(1, 70, false, None));
        assert_eq!(seat(&worker), (false, false));
        assert!(worker.bridge.voice_controls().capture_enabled_with(true));
        // Moved back to the audience: the microphone closes at once.
        worker.voice_dispatch(&update(1, 70, true, None));
        assert!(!worker.bridge.voice_controls().capture_enabled_with(true));
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
                    bridge.voice_controls.modify(|controls| {
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
