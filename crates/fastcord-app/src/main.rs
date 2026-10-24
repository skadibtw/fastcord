#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(all(feature = "fake-discord", not(debug_assertions)))]
compile_error!("fake-discord is a debug-only test mode");

mod account_panel;
mod accounts;
mod announce;
mod apps;
mod assets;
mod attachment_ui;
mod attachments;
mod audio_devices;
mod automod;
mod av_player;
mod avatars;
mod call_layout;
mod call_ui;
mod captcha;
mod changes;
mod chat_prefs;
mod community;
mod component_view;
mod composer;
mod desktop;
mod discover;
mod drafts;
mod emoji;
mod emoji_picker;
#[cfg(feature = "fake-discord")]
mod fake;
mod forum;
mod forward;
mod friends;
mod gateway;
mod gif;
mod group_dm;
mod guild_admin;
mod history;
mod icons;
mod inbox;
mod interactions;
mod invite;
mod localtime;
mod login;
mod media_ui;
mod member_menu;
mod message_link;
mod mic_panel;
mod moderation;
mod navigation;
mod notify;
mod notify_settings;
mod onboarding;
mod organize;
mod outbox;
mod password;
mod paste;
mod pins;
mod playback;
mod poll_create;
mod polls;
mod profile_edit;
mod qr;
mod reactions;
mod render;
mod report;
mod rich;
mod safety;
mod scheduled_events;
mod search;
mod shortcuts;
mod slash;
mod soundboard;
mod speech;
mod stage;
mod status;
mod sticker;
mod style;
mod switcher;
mod threads;
mod thumbs;
mod timeline;
mod ui_sound;
mod unverified;
mod variable_list;
mod virtual_list;
mod voice_log;
mod voice_record;
mod voice_settings;

use crate::attachments::AttachmentKey;
use std::sync::Arc;

use fastcord_discord::{CallSignal, IndexScope, RemoteAuthEvent, RestError, UserToken};
use fastcord_model::{Snowflake, VoiceStateRequest};
use fastcord_platform::{NativeCredentialStore, StoreError};
use gateway::{AttachmentAction, GatewayPanel, GatewayStatus};
use iced::widget::{
    button, center, checkbox, column, container, qr_code, row, scrollable, text, text_input,
};
use iced::{Color, Element, Length, Task};
use login::{LoginError, LoginOutcome, Persistence, Session, StoreReady};
use qr::{Applied, QrAttempt, QrStage};
use zeroize::Zeroize;

const MAX_PRIVATE_RECIPIENT_INPUT_BYTES: usize = 256;
const MAX_PRIVATE_RECIPIENTS: usize = fastcord_discord::MAX_PRIVATE_RECIPIENTS;
/// How long Quit waits for the Gateway to close before exiting anyway.
const QUIT_CLOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Lands an inbox jump in the open conversation `channel`.
fn inbox_jump(controls: &gateway::NavigationBridge, channel: Snowflake, target: inbox::Target) {
    match target {
        inbox::Target::Message(message) => {
            controls.timeline_intent(history::Intent::JumpTo { channel, message });
        }
        inbox::Target::Unread(Some(acked)) => {
            controls.timeline_intent(history::Intent::FirstUnread { channel, acked });
        }
        inbox::Target::Unread(None) => {}
    }
}

fn search_bar_is_empty(gateway: &GatewayPanel) -> bool {
    App::search_target(gateway).is_none()
}

/// Opens a conversation, and with `message` jumps to it: in the open channel
/// at once, otherwise once the channel (which must be one the account can
/// see) has opened.
fn jump_to(
    search: &mut search::State,
    gateway: &GatewayPanel,
    guild: Option<Snowflake>,
    channel: Snowflake,
    message: Option<Snowflake>,
) {
    if let Some(message) = message {
        if gateway.timeline.channel_id == Some(channel) {
            gateway
                .controls
                .timeline_intent(history::Intent::JumpTo { channel, message });
            return;
        }
        search.set_pending_jump(channel, message);
    }
    match guild {
        Some(guild) => gateway.controls.select_channel(guild, channel),
        None => gateway.controls.select_private_channel(channel),
    }
}

/// One ring or stop-ringing request for a private call.
fn call_signal(session: &Session, channel: Snowflake, signal: CallSignal) -> Task<Message> {
    let client = session.client.clone();
    Task::perform(
        async move { client.call_signal(channel, signal).await },
        Message::CallSignalled,
    )
}

/// True only in the test-only fake mode, which exists only in a build with the
/// `fake-discord` feature.
fn fake_mode() -> bool {
    #[cfg(feature = "fake-discord")]
    return fake::active();
    #[cfg(not(feature = "fake-discord"))]
    false
}

fn window_title() -> &'static str {
    if fake_mode() {
        "fastcord [FAKE]"
    } else {
        "fastcord"
    }
}

fn main() -> iced::Result {
    // An offline UI check: runs before account restoration and single-instance
    // hand-off, with no session, credential access or Discord workers.
    if std::env::args_os().any(|arg| arg == "--alerts-preview") {
        return navigation::alerts_preview();
    }
    #[cfg(debug_assertions)]
    if std::env::args_os().any(|arg| arg == "--icon-gallery") {
        return icons::gallery();
    }
    // Before any thread exists: `time` refuses the local offset afterwards on Unix.
    localtime::init();
    // One session per process and per sign-in: a second launch hands its link
    // to the first and exits, never opening a second Gateway on the same token.
    #[cfg(feature = "fake-discord")]
    fake::init();
    // Fake mode takes no part in the hand-off: it neither receives a real
    // instance's links nor is found by one.
    let arg = std::env::args().nth(1).filter(|_| !fake_mode());
    let (forwarded, requests) = tokio::sync::mpsc::channel(8);
    let _instance = if fake_mode() {
        None
    } else {
        Some(
            match fastcord_platform::instance::acquire(arg.as_deref(), move |request| {
                let _ = forwarded.try_send(request);
            }) {
                fastcord_platform::instance::Start::Primary(guard) => guard,
                fastcord_platform::instance::Start::Secondary => return Ok(()),
            },
        )
    };
    desktop::set_forwarded(requests);
    desktop::set_startup(arg);
    render::application(App::boot, App::update, App::view)
        .title(window_title())
        .theme(App::theme)
        .scale_factor(App::scale_factor)
        .subscription(App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(1080.0, 640.0),
            icon: icons::window_icon(),
            // Close requests go through `Message::CloseRequested`: hide to the
            // tray or leave any call and quit.
            exit_on_close_request: false,
            ..iced::window::Settings::default()
        })
        .run()
}

struct App {
    phase: Phase,
    token: Arc<UserToken>,
    acknowledged: bool,
    memory_only: bool,
    notice: Option<String>,
    store: Option<NativeCredentialStore>,
    saved_accounts: Vec<Snowflake>,
    account_index: accounts::Index,
    account_panel: account_panel::State,
    qr_attempts: u64,
    password_login: String,
    password: Arc<UserToken>,
    captcha_solved: bool,
    gateway_sessions: u64,
    attachment_purge_failed: bool,
    audio_preferences: audio_devices::Preferences,
    audio_settings: audio_devices::Settings,
    chat_settings_open: bool,
    /// The unverified-writes list in the chat settings is expanded.
    unverified_open: bool,
    /// Installed speech voices, listed when the chat settings open.
    speech_voices: Vec<fastcord_platform::speech::Voice>,
    audio_saving: Option<audio_devices::Preferences>,
    /// The camera picker; probed only when the user asks.
    camera_list: gateway::CameraList,
    shortcuts: shortcuts::Runtime,
    window_active: bool,
    window_drawable: bool,
    /// The tray icon is up (or why it is not). Closing hides to it only when it is.
    tray: Result<(), String>,
    /// Closing the window hid it to the tray; the process and any call go on.
    window_hidden: bool,
    /// Compact call mode: the small always-on-top window with the call only.
    compact_call: bool,
    /// The window size, restored when compact call mode ends.
    window_size: iced::Size,
    /// The Windows start-at-sign-in setting as read from the registry; `None`
    /// where it does not exist.
    autostart: Option<bool>,
    /// A link from the command line or a second launch, opened once signed in.
    pending_link: Option<desktop::DeepLink>,
    /// The window was put in full screen by the call view's button.
    window_fullscreen: bool,
    /// The per-person volume sliders are open.
    voice_details: bool,
    search: search::State,
    pins: pins::State,
    inbox: inbox::State,
    notify_settings: notify_settings::State,
    group: group_dm::State,
    invite: invite::State,
    guild_admin: guild_admin::State,
    announce: announce::State,
    moderation: moderation::State,
    apps: apps::State,
    community: community::State,
    assets: assets::State,
    automod: automod::State,
    discover: discover::State,
    onboarding: onboarding::State,
    events: scheduled_events::State,
    identity: profile_edit::State,
    safety: safety::State,
    report: report::State,
    gif: gif::State,
    sticker: sticker::State,
    soundboard: soundboard::State,
    stage: stage::State,
    forward: forward::State,
    poll: poll_create::State,
    slash: slash::State,
    voice: voice_record::State,
    /// Microphone gain, level meter and checks in the audio settings.
    mic_panel: mic_panel::State,
    friends: friends::State,
    member_menu: member_menu::State,
    status: status::State,
    threads: threads::State,
    /// The quick switcher and the shortcuts help.
    switcher: switcher::State,
    /// A keyboard or switcher navigation is on its way: when the open
    /// conversation changes, focus returns to the composer.
    nav_pending: bool,
    forum: forum::State,
}

#[derive(Debug)]
enum Phase {
    Starting,
    Login,
    Authenticating {
        saved_account: Option<Snowflake>,
    },
    MemoryConsent {
        session: Arc<Session>,
        error: StoreError,
    },
    Account {
        session: Arc<Session>,
        persistence: Persistence,
        gateway: Box<GatewayPanel>,
    },
    Deleting {
        account: Snowflake,
    },
    DeleteFailed {
        account: Snowflake,
        error: StoreError,
    },
    Qr(QrAttempt),
    Mfa(fastcord_discord::MfaChallenge),
}

// Every message containing a token uses the redacted wrapper; Debug cannot
// expose text-input edits, worker results, or an authenticated session.
/// The share picker's screens and windows, and the audio nodes to choose from.
type ShareSourceList = (
    Vec<fastcord_platform::capture::CaptureSource>,
    Vec<fastcord_platform::loopback::AudioSource>,
);

#[derive(Debug, Clone)]
enum Message {
    TokenEdited(Arc<UserToken>),
    InputTooLong,
    Acknowledge(bool),
    MemoryOnly(bool),
    StartQr,
    CancelQr,
    Qr(u64, RemoteAuthEvent),
    PasswordLoginEdited(String),
    PasswordEdited(Arc<UserToken>),
    PasswordSubmit,
    PasswordStep(Result<password::Step, fastcord_discord::PasswordAuthError>),
    CaptchaDone(Result<fastcord_discord::CaptchaSolution, captcha::CaptchaError>),
    MfaSubmit,
    CancelMfa,
    MfaResult(Result<Arc<UserToken>, fastcord_discord::PasswordAuthError>),
    Login,
    RetrySaved,
    StoreReady(Result<StoreReady, StoreError>),
    Restore(Snowflake),
    LoginFinished(Result<LoginOutcome, LoginError>),
    ContinueInMemory,
    CancelLogin,
    Logout,
    Account(account_panel::Event),
    /// Log out of this account locally (keeping its saved login) and sign in the other.
    SwitchAccount(Snowflake),
    AccountRemoved(Snowflake, Result<(), StoreError>),
    AccountIndexSaved,
    Forget(Snowflake),
    RetryDelete,
    Deleted(Result<(), StoreError>),
    Gateway(u64, GatewayStatus),
    Navigation(u64),
    SelectGuild(Snowflake),
    SelectChannel(Snowflake, Snowflake),
    JoinVoice(Snowflake, Snowflake),
    LeaveVoice,
    /// Call a DM or group DM: join its voice and ring the others.
    StartCall(Snowflake),
    /// Answer a ringing call, or join one in progress.
    AcceptCall(Snowflake),
    /// Stop this account's ringing for a call without joining.
    DeclineCall(Snowflake),
    CallSignalled(Result<(), fastcord_discord::RestError>),
    WatchStream(String),
    ActivateParticipant(Snowflake),
    MemberMenu(member_menu::Event),
    UnwatchStream(String),
    SetStreamVolume(String, u16),
    MuteStream(String, bool),
    /// Open or close the call view (the large layout of the call's videos).
    ShowStream(bool),
    /// The voice panel's heading: the large call view, and the call's chat for a server call.
    OpenCall,
    SetCallMode(call_layout::Mode),
    /// Pin a video (it becomes the large one) or unpin it.
    PinTile(call_layout::TileId),
    /// One video fills the call view, or the full view ends.
    FullTile(Option<call_layout::TileId>),
    SetHidePeople(bool),
    /// The whole window goes full screen (or back).
    SetWindowFullscreen(bool),
    VideoReady(u64),
    /// The user clicked the camera button.
    ToggleCamera,
    ListCameras,
    CamerasListed(Result<Vec<fastcord_platform::capture::camera::CameraDevice>, String>),
    SelectCamera(gateway::CameraChoice),
    /// A background effect for the camera (issue #85), in a call or not.
    SelectCameraEffect(fastcord_video::effect::Effect),
    SetMirrorPreview(bool),
    OpenSharePicker,
    ShareSources(Result<ShareSourceList, String>),
    CloseSharePicker,
    ToggleShareAudio(bool),
    /// The output or input the share's audio comes from (Linux).
    ChooseShareAudio(fastcord_platform::loopback::AudioTarget),
    StartShare(fastcord_platform::capture::SourceId),
    StopShare,
    GuildViewport(virtual_list::Window),
    ChannelViewport(virtual_list::Window),
    SetSelfMute(bool),
    SetDeafen(bool),
    InterfaceSoundsToggled(bool),
    InterfaceVolumeChanged(u8),
    SetParticipantVolume(Snowflake, u16),
    CommitParticipantVolume(Snowflake),
    PresetParticipantVolume(Snowflake, u16),
    OpenAudioSettings,
    CloseAudioSettings,
    OpenChatSettings,
    CloseChatSettings,
    /// Settings, Advanced: show or hide the unverified-writes list.
    ToggleUnverifiedList,
    /// Turns one group of unverified writes on or off for this session.
    UnverifiedWrite(fastcord_discord::WriteGroup, bool),
    ChatPrefs(chat_prefs::Prefs),
    SpeechPrefs(speech::Prefs),
    SpeechVoices(Vec<fastcord_platform::speech::Voice>),
    SpeechTest,
    StopSpeech,
    RefreshAudioDevices,
    AudioDevicesListed(audio_devices::Inventory),
    Initialized(audio_devices::Preferences, Result<StoreReady, StoreError>),
    SelectAudioDevice(fastcord_audio::Direction, audio_devices::DeviceOption),
    NoiseSuppressionChanged(fastcord_audio::NoiseSuppression),
    EchoCancellationChanged(bool),
    NotificationsToggled(bool),
    AudioPreferencesSaved(Result<(), &'static str>),
    Timeline(timeline::Event),
    SelectPrivateChannel(Snowflake),
    PrivateRecipients(String),
    OpenPrivate,
    PrivateViewport(virtual_list::Window),
    AttachmentLoaded(
        u64,
        AttachmentKey,
        attachment_ui::Target,
        Result<attachment_ui::LoadedPreview, &'static str>,
    ),
    AttachmentPurged(bool),
    MediaReady(
        u64,
        AttachmentKey,
        Result<media_ui::PlayerHandle, &'static str>,
    ),
    MediaTick,
    /// A new picture of the playing attachment's video is ready to draw.
    MediaFrame(u64),
    AttachmentActionFinished(
        u64,
        AttachmentKey,
        AttachmentAction,
        attachment_ui::ActionOutcome,
    ),
    CloseViewer,
    Composer(composer::Event),
    Search(search::Event),
    Pins(pins::Event),
    Inbox(inbox::Event),
    Organize(organize::Event),
    NotifySettings(notify_settings::Event),
    Group(group_dm::Event),
    Invite(invite::Event),
    GuildAdmin(guild_admin::Event),
    Announce(announce::Event),
    Moderation(moderation::Event),
    Apps(apps::Event),
    Community(community::Event),
    Assets(assets::Event),
    AutoMod(automod::Event),
    Discover(discover::Event),
    Onboarding(onboarding::Event),
    Events(scheduled_events::Event),
    Identity(profile_edit::Event),
    Safety(safety::Event),
    Report(report::Event),
    Gif(gif::Event),
    Sticker(sticker::Event),
    Soundboard(soundboard::Event),
    Stage(stage::Event),
    Forward(forward::Event),
    Poll(poll_create::Event),
    Slash(slash::Event),
    Voice(voice_record::Event),
    MicPanel(mic_panel::Event),
    VoiceRecordingToggled(bool),
    Friends(friends::Event),
    Status(status::Event),
    Threads(threads::Event),
    Switcher(switcher::Event),
    Shortcut(switcher::Shortcut),
    Forum(forum::Event),
    EmojiPicker(emoji_picker::Event),
    EmojiVisibility(emoji::Occurrence, bool),
    EmojiLoaded(
        u64,
        emoji::Asset,
        u64,
        Result<attachments::DecodedAnimation, &'static str>,
    ),
    EmojiTick(std::time::Instant),
    AvatarLoaded(u64, avatars::AvatarKey, Option<attachments::DecodedImage>),
    WindowActive(bool),
    VoiceInput(shortcuts::Event),
    ShortcutKey(shortcuts::KeyInput),
    GlobalShortcut(shortcuts::GlobalEvent),
    /// The tray icon came up, failed, or was clicked.
    Tray(desktop::TrayEvent),
    /// A second launch handed its argument over (or only asked to be raised).
    Forwarded(fastcord_platform::instance::Forwarded),
    /// The window's close button: hide to the tray or quit, per the setting.
    CloseRequested,
    /// Leave any call, close the Gateway cleanly and exit.
    Quit,
    Exit,
    /// The Reconnect button after fastcord's own reconnect limits stopped the
    /// Gateway: a fresh connection with fresh counters.
    ReconnectGateway,
    CloseToTrayToggled(bool),
    AutostartToggled(bool),
    /// Enter or leave compact call mode (small, always on top).
    ToggleCompact,
    WindowSize(iced::Size),
    /// Direct messages (true) or a server's channels in the sidebar.
    ShowDirects(bool),
    /// Show or hide the per-person volume sliders.
    ToggleVoiceDetails,
    /// Mark a conversation read (guild, channel); the user asked.
    MarkRead(Option<Snowflake>, Snowflake),
    /// Show the first message after the read cursor (channel, cursor).
    FirstUnread(Snowflake, Snowflake),
    /// Files chosen in the system dialog to attach to the message.
    FilesPicked(Vec<std::path::PathBuf>),
    /// A file dropped on the window (one event per file).
    FileDropped(std::path::PathBuf),
    /// The clipboard image, saved as a PNG file, or why there is none.
    ImagePasted(Result<std::path::PathBuf, &'static str>),
}
impl App {
    fn boot() -> (Self, Task<Message>) {
        (
            Self {
                phase: Phase::Starting,
                token: Arc::new(UserToken::new(String::new())),
                acknowledged: false,
                memory_only: false,
                notice: None,
                store: None,
                saved_accounts: Vec::new(),
                account_index: accounts::Index::default(),
                account_panel: account_panel::State::default(),
                qr_attempts: 0,
                password_login: String::new(),
                password: Arc::new(UserToken::new(String::new())),
                captcha_solved: false,
                gateway_sessions: 0,
                attachment_purge_failed: false,
                audio_preferences: audio_devices::Preferences::default(),
                audio_settings: audio_devices::Settings::default(),
                chat_settings_open: false,
                unverified_open: false,
                speech_voices: Vec::new(),
                audio_saving: None,
                camera_list: gateway::CameraList::default(),
                shortcuts: shortcuts::Runtime::default(),
                window_active: true,
                window_drawable: true,
                tray: Err(if fake_mode() {
                    "The tray icon is off in fake mode.".to_owned()
                } else {
                    "The tray icon is starting.".to_owned()
                }),
                window_hidden: false,
                compact_call: false,
                window_size: iced::Size::new(1080.0, 640.0),
                autostart: (cfg!(windows) && !fake_mode())
                    .then(fastcord_platform::autostart::is_enabled),
                pending_link: desktop::take_startup(),
                window_fullscreen: false,
                voice_details: false,
                search: search::State::default(),
                pins: pins::State::default(),
                inbox: inbox::State::default(),
                notify_settings: notify_settings::State::default(),
                group: group_dm::State::default(),
                invite: invite::State::default(),
                guild_admin: guild_admin::State::default(),
                announce: announce::State::default(),
                moderation: moderation::State::default(),
                apps: apps::State::default(),
                community: community::State::default(),
                assets: assets::State::default(),
                automod: automod::State::default(),
                discover: discover::State::default(),
                onboarding: onboarding::State::default(),
                events: scheduled_events::State::default(),
                identity: profile_edit::State::default(),
                safety: safety::State::default(),
                report: report::State::default(),
                gif: gif::State::default(),
                sticker: sticker::State::default(),
                soundboard: soundboard::State::default(),
                stage: stage::State::default(),
                forward: forward::State::default(),
                poll: poll_create::State::default(),
                slash: slash::State::default(),
                voice: voice_record::State::default(),
                mic_panel: mic_panel::State::default(),
                friends: friends::State::default(),
                member_menu: member_menu::State::default(),
                status: status::State::default(),
                threads: threads::State::default(),
                switcher: switcher::State::default(),
                nav_pending: false,
                forum: forum::State::default(),
            },
            Task::perform(
                async {
                    paste::purge_stale();
                    voice_record::purge_stale();
                    let preferences = audio_devices::load().await;
                    (preferences, login::initialize_store().await)
                },
                |(preferences, store)| Message::Initialized(preferences, store),
            ),
        )
    }

    fn theme(&self) -> iced::Theme {
        style::theme()
    }

    /// The one place the text-size preference is applied: the whole interface scales.
    fn scale_factor(&self) -> f32 {
        self.audio_preferences.chat.scale_factor()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let resynchronize = !matches!(
            message,
            Message::EmojiTick(_)
                | Message::EmojiLoaded(..)
                | Message::AvatarLoaded(..)
                | Message::VideoReady(_)
                | Message::MediaFrame(_)
        );
        if !matches!(self.phase, Phase::Account { .. }) {
            self.friends.purge();
            self.member_menu.reset();
            self.status.purge();
            // Narration ends with the session (logout, account switch).
            speech::stop();
        }
        let mut task = self.handle(message);
        if self.pending_link.is_some()
            && matches!(self.phase, Phase::Account { .. })
            && let Some(link) = self.pending_link.take()
        {
            task = Task::batch([task, self.open_link(link)]);
        }
        if self.compact_call && !self.call_active() {
            task = Task::batch([task, self.end_compact()]);
        }
        if resynchronize {
            self.sync_tray();
        }
        if let Phase::Account { gateway, .. } = &self.phase {
            gateway.controls.quick_query(self.switcher.wanted());
            self.guild_admin.sync(&gateway.navigation);
            self.automod.observe(&gateway.navigation.automod);
            gateway.controls.admin_guild(
                self.guild_admin
                    .wanted()
                    .or(self.announce.wanted_guild())
                    .or(self.moderation.wanted())
                    .or(self.apps.wanted_guild())
                    .or(self.community.wanted())
                    .or(self.automod.wanted()),
            );
            gateway
                .controls
                .follow_source(self.announce.wanted_follow());
            gateway.controls.assets_guild(self.assets.wanted());
            gateway
                .controls
                .admin_member(self.guild_admin.wanted_member());
        }
        // Window full screen belongs to the call view: without it there is no
        // button to leave it, so it ends with the view.
        if self.window_fullscreen
            && !matches!(&self.phase, Phase::Account { gateway, .. } if gateway.call_view.open)
        {
            self.window_fullscreen = false;
            task = Task::batch([
                task,
                iced::window::latest()
                    .and_then(|id| iced::window::set_mode(id, iced::window::Mode::Windowed)),
            ]);
        }
        if resynchronize {
            self.synchronize_video();
            Task::batch([task, self.synchronize_emoji()])
        } else {
            task
        }
    }

    fn synchronize_emoji(&mut self) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &mut self.phase
        else {
            return Task::none();
        };
        gateway
            .controls
            .emoji_request(&gateway.emoji_picker.request);
        let wanted = wanted_avatars(
            gateway,
            own_user(gateway, session),
            &self.invite,
            &self.discover,
        );
        let avatars = gateway
            .avatars
            .synchronize(wanted, gateway.id, gateway.attachments.as_ref());
        let emoji = gateway.emoji.synchronize(
            self.window_active
                && self.window_drawable
                && !self.audio_settings.open
                && !gateway.call_view.open
                && gateway.attachment_viewer.is_none(),
            gateway.emoji_picker.request.open,
            gateway.id,
            gateway.attachments.as_ref(),
        );
        Task::batch([avatars, emoji])
    }

    fn synchronize_video(&self) {
        if let Phase::Account { gateway, .. } = &self.phase {
            // Focus does not matter: a stream keeps playing in a window that is
            // visible but not focused, like the official client. Only a
            // minimized (zero-size) window or a covering view suspends video.
            let unobstructed = self.window_drawable
                && !self.audio_settings.open
                && gateway.attachment_viewer.is_none()
                && !gateway.emoji_picker.request.open;
            // Only the videos the layout draws are decoded; the rest are
            // suspended and, for cameras, no longer requested from the server.
            let voice_controls = gateway.controls.voice_controls();
            let tiles = call_layout::tiles(
                &gateway.controls.watched(),
                &gateway.controls.camera_tiles().snapshot(),
                voice_controls.camera.wanted,
            );
            let speaking: Vec<Snowflake> = voice_controls
                .participants
                .iter()
                .filter(|participant| participant.speaking)
                .map(|participant| participant.user_id)
                .collect();
            let plan = gateway.call_view.plan(&call_layout::ids(&tiles), &speaking);
            for tile in &tiles {
                if matches!(tile.id, call_layout::TileId::Stream(_)) {
                    tile.session
                        .set_visible(unobstructed && plan.contains(&tile.id));
                }
            }
            gateway
                .controls
                .camera_tiles()
                .apply(gateway::TileVisibility {
                    on_screen: unobstructed,
                    preview_excluded: !plan.contains(&call_layout::TileId::Preview),
                    excluded: tiles
                        .iter()
                        .filter_map(|tile| tile.id.camera_user())
                        .filter(|user| !plan.contains(&call_layout::TileId::Camera(*user)))
                        .collect(),
                    small: plan.small_cameras(),
                });
        }
    }

    fn video_subscription(&self) -> iced::Subscription<Message> {
        let Phase::Account { gateway, .. } = &self.phase else {
            return iced::Subscription::none();
        };
        let mut subscriptions = Vec::new();
        for session in gateway.controls.watched() {
            subscriptions.push(session.subscription().map(Message::VideoReady));
        }
        let tiles = gateway.controls.camera_tiles().snapshot();
        for session in tiles
            .preview
            .iter()
            .chain(tiles.remote.iter().map(|(_, session)| session))
        {
            subscriptions.push(session.subscription().map(Message::VideoReady));
        }
        iced::Subscription::batch(subscriptions)
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        let activity = iced::event::listen_with(|event, _, _| match event {
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowSize(size))
            }
            iced::Event::Window(iced::window::Event::FileDropped(path)) => {
                Some(Message::FileDropped(path))
            }
            iced::Event::Window(iced::window::Event::CloseRequested) => {
                Some(Message::CloseRequested)
            }
            iced::Event::Window(iced::window::Event::Focused) => Some(Message::WindowActive(true)),
            iced::Event::Window(iced::window::Event::Unfocused | iced::window::Event::Closed) => {
                Some(Message::WindowActive(false))
            }
            _ => None,
        });
        iced::Subscription::batch([
            activity,
            self.animation_subscription(),
            self.video_subscription(),
            self.media_subscription(),
            self.soundboard_subscription(),
            self.voice_subscription(),
            self.mic_subscription(),
            self.media_frame_subscription(),
            self.shortcut_subscription(),
            self.navigation_keys(),
            if fake_mode() {
                iced::Subscription::none()
            } else {
                iced::Subscription::run(desktop::tray_stream).map(Message::Tray)
            },
            iced::Subscription::run(desktop::forwarded_stream).map(Message::Forwarded),
        ])
    }

    /// Navigation chords (see [`switcher`]): modified keys and Escape while
    /// signed in, plus the plain arrows only while the switcher is open, so
    /// typing (an IME included) never produces a message.
    fn navigation_keys(&self) -> iced::Subscription<Message> {
        if !matches!(self.phase, Phase::Account { .. }) {
            return iced::Subscription::none();
        }
        let mut parts = vec![iced::event::listen_with(|event, status, _| match event {
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                ..
            }) => switcher::shortcut(
                &key,
                &physical_key,
                modifiers,
                matches!(status, iced::event::Status::Captured),
            )
            .filter(|shortcut| !matches!(shortcut, switcher::Shortcut::Move(_)))
            .map(Message::Shortcut),
            _ => None,
        })];
        if self.switcher.is_open() {
            parts.push(iced::event::listen_with(|event, _, _| match event {
                iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key,
                    physical_key,
                    modifiers,
                    ..
                }) => switcher::shortcut(&key, &physical_key, modifiers, false)
                    .filter(|shortcut| matches!(shortcut, switcher::Shortcut::Move(_)))
                    .map(Message::Shortcut),
                _ => None,
            }));
        }
        iced::Subscription::batch(parts)
    }

    /// Whether a call is connecting or running, the only time shortcuts are heard.
    fn call_active(&self) -> bool {
        matches!(&self.phase, Phase::Account { gateway, .. } if gateway.controls.voice_audio_state() != gateway::VoiceAudioState::Idle)
    }

    /// What a click on the tray icon or its menu does.
    fn tray_command(&mut self, command: fastcord_platform::tray::Command) -> Task<Message> {
        use fastcord_platform::tray::Command;
        match command {
            Command::Open => self.show_window(),
            Command::ToggleMute => {
                self.apply_shortcuts(vec![shortcuts::Do::Toggle(shortcuts::Action::ToggleMute)])
            }
            Command::Disconnect if self.call_active() => self.handle(Message::LeaveVoice),
            Command::Disconnect => Task::none(),
            Command::Quit => self.handle(Message::Quit),
        }
    }

    /// Brings the window forward, showing it first if closing hid it.
    fn show_window(&mut self) -> Task<Message> {
        let was_hidden = std::mem::take(&mut self.window_hidden);
        self.window_drawable = true;
        self.sync_viewing();
        iced::window::latest().and_then(move |id| {
            if was_hidden {
                iced::window::set_mode(id, iced::window::Mode::Windowed)
            } else {
                Task::none()
            }
            .chain(iced::window::minimize(id, false))
            .chain(iced::window::gain_focus(id))
        })
    }

    /// Leaves compact call mode: normal stacking and the remembered size.
    fn end_compact(&mut self) -> Task<Message> {
        self.compact_call = false;
        let size = self.window_size;
        iced::window::latest().and_then(move |id| {
            iced::window::set_level(id, iced::window::Level::Normal)
                .chain(iced::window::resize(id, size))
        })
    }

    /// Opens a link from the command line, a second launch or the switcher; called
    /// only once signed in. An invite is only shown, joining stays a click.
    fn open_link(&mut self, link: desktop::DeepLink) -> Task<Message> {
        let (guild, channel, message) = match link {
            desktop::DeepLink::Invite(code) => return self.invite_event(invite::Event::Open(code)),
            desktop::DeepLink::Channel {
                guild,
                channel,
                message,
            } => (guild, channel, message),
        };
        let Phase::Account { gateway, .. } = &mut self.phase else {
            return Task::none();
        };
        self.threads.close();
        self.friends.close();
        if self.inbox.is_open() {
            self.inbox.clear();
            gateway.controls.inbox_rows(self.inbox.rows_wanted());
        }
        gateway.directs = guild.is_none();
        jump_to(&mut self.search, gateway, guild, channel, message);
        self.nav_pending = true;
        Task::none()
    }

    /// The tray icon's menu and tooltip follow the call and the unread mentions.
    fn sync_tray(&self) {
        let (in_call, muted, mentions) = match &self.phase {
            Phase::Account { gateway, .. } => {
                let in_call = self.call_active();
                (
                    in_call,
                    in_call && gateway.controls.voice_controls().self_muted,
                    gateway.navigation.inbox.mentions,
                )
            }
            _ => (false, false, 0),
        };
        fastcord_platform::tray::set_state(fastcord_platform::tray::State {
            in_call,
            muted,
            mentions,
        });
    }

    /// Key listeners exist only while a call has bindings (or a binding is being
    /// recorded): window keys for the window scope, one OS listener for the global one.
    fn shortcut_subscription(&self) -> iced::Subscription<Message> {
        let input = &self.audio_preferences.voice_input;
        let wanted = shortcuts::wanted(input, self.call_active());
        let mut parts = Vec::new();
        if (wanted && input.scope == shortcuts::Scope::Window) || self.shortcuts.wants_keys() {
            parts.push(
                iced::event::listen_with(|event, _, _| match event {
                    iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                        physical_key,
                        modifiers,
                        ..
                    }) => shortcuts::key_input(physical_key, modifiers, true),
                    iced::Event::Keyboard(iced::keyboard::Event::KeyReleased {
                        physical_key,
                        modifiers,
                        ..
                    }) => shortcuts::key_input(physical_key, modifiers, false),
                    _ => None,
                })
                .map(Message::ShortcutKey),
            );
        }
        if wanted && input.scope == shortcuts::Scope::Global {
            parts.push(
                iced::Subscription::run_with(
                    shortcuts::ListenerSpec(input.list()),
                    shortcuts::listener,
                )
                .map(Message::GlobalShortcut),
            );
        }
        iced::Subscription::batch(parts)
    }

    /// Releases every held hold-key at once (no release delay): bindings changed, the
    /// scope changed, or the listener went away.
    fn release_held_shortcuts(&mut self) {
        if let Phase::Account { gateway, .. } = &self.phase {
            gateway.controls.cancel_ptt();
            gateway.controls.set_push_to_mute(false);
        }
    }

    fn apply_shortcuts(&mut self, actions: Vec<shortcuts::Do>) -> Task<Message> {
        use shortcuts::{Action, Do};
        let mut follow = Vec::new();
        for action in actions {
            match action {
                Do::Hold(kind, down) => {
                    if let Phase::Account { gateway, .. } = &self.phase {
                        match kind {
                            Action::PushToTalk => gateway.controls.set_ptt(down),
                            Action::PushToMute => gateway.controls.set_push_to_mute(down),
                            _ => {}
                        }
                    }
                }
                Do::Cancel(kind) => {
                    if let Phase::Account { gateway, .. } = &self.phase {
                        match kind {
                            Action::PushToTalk => gateway.controls.cancel_ptt(),
                            Action::PushToMute => gateway.controls.set_push_to_mute(false),
                            _ => {}
                        }
                    }
                }
                Do::Toggle(kind) => {
                    if let Phase::Account { gateway, .. } = &self.phase
                        && gateway.controls.voice_audio_state() == gateway::VoiceAudioState::Running
                    {
                        let controls = gateway.controls.voice_controls();
                        follow.push(match kind {
                            Action::ToggleMute => Message::SetSelfMute(!controls.self_muted),
                            _ => Message::SetDeafen(!controls.deafened),
                        });
                    }
                }
                Do::Recorded(kind, binding) => {
                    match self.audio_preferences.voice_input.assign(kind, binding) {
                        Ok(()) => {
                            self.shortcuts.notice = None;
                            follow.push(Message::VoiceInput(shortcuts::Event::Saved));
                        }
                        Err(other) => {
                            self.shortcuts.notice = Some(format!(
                                "{} is already used by {}. Choose another key.",
                                binding.label(),
                                other.label()
                            ));
                        }
                    }
                }
                Do::RecordCancelled => self.shortcuts.notice = None,
            }
        }
        Task::batch(follow.into_iter().map(|message| self.handle(message)))
    }

    fn voice_input_event(&mut self, event: shortcuts::Event) -> Task<Message> {
        use shortcuts::Event;
        let input = &mut self.audio_preferences.voice_input;
        match event {
            Event::Mode(mode) => input.mode = mode,
            Event::ReleaseMs(ms) => input.release_ms = ms.min(shortcuts::MAX_RELEASE_MS),
            Event::Automatic(true) => input.sensitivity = None,
            Event::Automatic(false) => input.sensitivity = Some(shortcuts::DEFAULT_SENSITIVITY),
            Event::Sensitivity(level) => input.sensitivity = Some(level),
            Event::Scope(scope) => input.scope = scope,
            Event::Clear(action) => input.clear(action),
            Event::Record(action) => {
                self.shortcuts.recording = Some(action);
                self.shortcuts.notice = None;
                self.release_held_shortcuts();
                return Task::none();
            }
            Event::CancelRecord => {
                let _ = self.shortcuts.cancel_record();
                return Task::none();
            }
            Event::Saved => {}
        }
        if matches!(event, Event::Scope(_) | Event::Clear(_) | Event::Saved) {
            self.release_held_shortcuts();
        }
        self.shortcuts.rebuild(&self.audio_preferences.voice_input);
        if let Phase::Account { gateway, .. } = &self.phase {
            gateway
                .controls
                .set_voice_input(&self.audio_preferences.voice_input);
        }
        if self.audio_saving.is_none() {
            return self.save_audio_preferences();
        }
        Task::none()
    }

    /// Frame notifications exist only while an attachment with video is on screen.
    fn media_frame_subscription(&self) -> iced::Subscription<Message> {
        if let Phase::Account { gateway, .. } = &self.phase
            && let Some(session) = gateway
                .media
                .as_ref()
                .and_then(media_ui::MediaSession::video_session)
        {
            session.subscription().map(Message::MediaFrame)
        } else {
            iced::Subscription::none()
        }
    }

    /// The timer exists only while a voice message is being recorded or previewed.
    fn voice_subscription(&self) -> iced::Subscription<Message> {
        if self.voice.ticking() {
            iced::time::every(std::time::Duration::from_millis(100))
                .map(|_| Message::Voice(voice_record::Event::Tick))
        } else {
            iced::Subscription::none()
        }
    }

    /// The 10 Hz level timer exists only while the audio dialog is open.
    fn mic_subscription(&self) -> iced::Subscription<Message> {
        if self.audio_settings.open {
            iced::time::every(std::time::Duration::from_millis(100))
                .map(|_| Message::MicPanel(mic_panel::Event::Tick))
        } else {
            iced::Subscription::none()
        }
    }

    /// The 250 ms timer exists only while a soundboard sound is loading or playing.
    fn soundboard_subscription(&self) -> iced::Subscription<Message> {
        if self.soundboard.busy() {
            iced::time::every(std::time::Duration::from_millis(250))
                .map(|_| Message::Soundboard(soundboard::Event::Tick))
        } else {
            iced::Subscription::none()
        }
    }

    /// The position timer exists only while an attachment is audibly playing.
    fn media_subscription(&self) -> iced::Subscription<Message> {
        if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.media.as_ref().is_some_and(media_ui::MediaSession::playing))
        {
            iced::time::every(std::time::Duration::from_millis(250)).map(|_| Message::MediaTick)
        } else {
            iced::Subscription::none()
        }
    }

    /// The 20 ms animation timer exists only while a visible, active emoji animates.
    fn animation_subscription(&self) -> iced::Subscription<Message> {
        if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.emoji.animating()) {
            iced::time::every(std::time::Duration::from_millis(20)).map(Message::EmojiTick)
        } else {
            iced::Subscription::none()
        }
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Initialized(preferences, store) => {
                notify::set_enabled(!preferences.notifications_off);
                self.soundboard.set_prefs(preferences.soundboard.clone());
                self.audio_preferences = preferences;
                chat_prefs::apply(self.audio_preferences.chat);
                organize::publish(&self.audio_preferences.organize);
                speech::apply(&self.audio_preferences.speech);
                self.configure_ui_sound();
                self.shortcuts.rebuild(&self.audio_preferences.voice_input);
                // Fake mode signs in at once with its fixed token, memory-only.
                #[cfg(feature = "fake-discord")]
                if fake::active() {
                    self.phase = Phase::Authenticating {
                        saved_account: None,
                    };
                    return Task::perform(
                        login::token_login(
                            Arc::new(UserToken::new(fake::TOKEN.to_owned())),
                            None,
                            false,
                        ),
                        Message::LoginFinished,
                    );
                }
                return self.handle(Message::StoreReady(store));
            }
            Message::VoiceInput(event) => return self.voice_input_event(event),
            Message::Tray(event) => match event {
                desktop::TrayEvent::Started => self.tray = Ok(()),
                desktop::TrayEvent::Failed(error) => self.tray = Err(error),
                desktop::TrayEvent::Command(command) => return self.tray_command(command),
            },
            Message::Forwarded(request) => {
                if let fastcord_platform::instance::Forwarded::Open(text) = request
                    && let Some(link) = desktop::parse(&text)
                {
                    self.pending_link = Some(link);
                }
                return self.show_window();
            }
            Message::CloseRequested => {
                match desktop::close_action(self.audio_preferences.close_to_tray, self.tray.is_ok())
                {
                    desktop::CloseAction::Hide => {
                        self.window_hidden = true;
                        self.window_drawable = false;
                        self.sync_viewing();
                        return iced::window::latest()
                            .and_then(|id| iced::window::set_mode(id, iced::window::Mode::Hidden));
                    }
                    desktop::CloseAction::Quit => return self.handle(Message::Quit),
                }
            }
            Message::Quit => {
                // Leaving the call first lets the server drop the voice state at
                // once. The Gateway then closes with 1000 (waiting at most 2 s
                // for its task) so the session ends instead of lingering until a
                // heartbeat times out; the process exit releases the rest.
                let in_call = self.call_active();
                let controls = match &self.phase {
                    Phase::Account { gateway, .. } => Some(gateway.controls.clone()),
                    _ => None,
                };
                if !in_call && controls.is_none() {
                    return iced::exit();
                }
                let leave = if in_call {
                    self.handle(Message::LeaveVoice)
                } else {
                    Task::none()
                };
                return Task::batch([
                    leave,
                    Task::perform(
                        async move {
                            if in_call {
                                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                            }
                            if let Some(controls) = controls {
                                controls.close_gateway(QUIT_CLOSE_WAIT).await;
                            }
                        },
                        |()| Message::Exit,
                    ),
                ]);
            }
            Message::Exit => return iced::exit(),
            Message::ReconnectGateway => {
                let stopped = match &self.phase {
                    Phase::Account {
                        session,
                        persistence,
                        gateway,
                    } => matches!(
                        &gateway.status,
                        GatewayStatus::Stopped(reason) if reason.allows_manual_reconnect()
                    )
                    .then(|| (Arc::clone(session), *persistence)),
                    _ => None,
                };
                if let Some((session, persistence)) = stopped {
                    // A new Gateway task starts with empty reconnect counters.
                    return self.enter_account(session, persistence);
                }
            }
            Message::CloseToTrayToggled(on) => {
                self.audio_preferences.close_to_tray = on;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::AutostartToggled(on) => match fastcord_platform::autostart::set(on) {
                Ok(()) => self.autostart = Some(on),
                Err(error) => self.notice = Some(error.to_string()),
            },
            Message::ToggleCompact => {
                if self.compact_call {
                    return self.end_compact();
                }
                if self.call_active() {
                    self.compact_call = true;
                    let leave_fullscreen = std::mem::take(&mut self.window_fullscreen);
                    return iced::window::latest().and_then(move |id| {
                        if leave_fullscreen {
                            iced::window::set_mode(id, iced::window::Mode::Windowed)
                        } else {
                            Task::none()
                        }
                        .chain(iced::window::set_level(
                            id,
                            iced::window::Level::AlwaysOnTop,
                        ))
                        .chain(iced::window::resize(id, desktop::COMPACT_SIZE))
                    });
                }
            }
            Message::ShortcutKey(input) => {
                if input.down && input.code == "Escape" && self.shortcuts.recording.is_some() {
                    let cancelled = self.shortcuts.cancel_record().into_iter().collect();
                    return self.apply_shortcuts(cancelled);
                }
                let scope = self.audio_preferences.voice_input.scope;
                let actions = self.shortcuts.window_key(scope, &input);
                return self.apply_shortcuts(actions);
            }
            Message::GlobalShortcut(event) => {
                use shortcuts::GlobalEvent;
                match event {
                    GlobalEvent::Started => {
                        self.shortcuts.global_running = true;
                        self.shortcuts.global_error = None;
                    }
                    GlobalEvent::Failed(error) => {
                        self.shortcuts.global_running = false;
                        self.shortcuts.global_error = Some(error);
                    }
                    GlobalEvent::Lost => self.release_held_shortcuts(),
                    GlobalEvent::Event(event) => {
                        if let Some(action) = self.shortcuts.global(event) {
                            return self.apply_shortcuts(vec![action]);
                        }
                    }
                }
            }
            Message::OpenAudioSettings | Message::RefreshAudioDevices => {
                self.audio_settings.open = true;
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.set_diagnostics_open(true);
                }
                if !self.audio_settings.probing {
                    self.audio_settings.probing = true;
                    return Task::perform(audio_devices::enumerate(), Message::AudioDevicesListed);
                }
            }
            Message::CloseAudioSettings => {
                self.audio_settings.open = false;
                self.mic_panel.close();
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.set_diagnostics_open(false);
                }
            }
            Message::MicPanel(event) => return self.mic_event(event),
            Message::AudioDevicesListed(result) => {
                self.audio_settings.probing = false;
                match result.input {
                    Ok(inputs) => {
                        self.audio_settings.inputs = inputs;
                        self.audio_settings.input_error = None;
                    }
                    Err(error) => self.audio_settings.input_error = Some(error),
                }
                match result.output {
                    Ok(outputs) => {
                        self.audio_settings.outputs = outputs;
                        self.audio_settings.output_error = None;
                    }
                    Err(error) => self.audio_settings.output_error = Some(error),
                }
            }
            Message::SelectAudioDevice(direction, option) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway
                        .controls
                        .select_audio_device(direction, option.choice.clone());
                }
                self.audio_preferences.select(direction, option);
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::NoiseSuppressionChanged(mode) => {
                self.audio_preferences.noise_suppression = mode;
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.select_noise_suppression(mode);
                }
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::EchoCancellationChanged(enabled) => {
                self.audio_preferences.echo_cancellation = enabled;
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.select_echo_cancellation(enabled);
                }
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::OpenChatSettings => {
                self.chat_settings_open = true;
                return Task::perform(speech::load_voices(), |voices| {
                    Message::SpeechVoices(voices.unwrap_or_default())
                });
            }
            Message::SpeechVoices(voices) => self.speech_voices = voices,
            Message::SpeechPrefs(prefs) => {
                speech::apply(&prefs);
                self.audio_preferences.speech = prefs;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::SpeechTest => speech::speak("This is how messages will sound.".to_owned()),
            Message::StopSpeech => speech::stop(),
            Message::CloseChatSettings => self.chat_settings_open = false,
            Message::ToggleUnverifiedList => self.unverified_open = !self.unverified_open,
            Message::UnverifiedWrite(group, on) => {
                if let Phase::Account { session, .. } = &self.phase {
                    session.client.set_write_group(group, on);
                }
            }
            Message::ChatPrefs(prefs) => {
                self.audio_preferences.chat = prefs;
                chat_prefs::apply(prefs);
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::InterfaceSoundsToggled(on) => {
                self.audio_preferences.interface_sounds = on;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::InterfaceVolumeChanged(volume) => {
                self.audio_preferences.interface_volume = volume;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::NotificationsToggled(on) => {
                self.audio_preferences.notifications_off = !on;
                notify::set_enabled(on);
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::AudioPreferencesSaved(result) => {
                let saved = self.audio_saving.take();
                if let Err(error) = result {
                    self.notice = Some(error.to_owned());
                }
                if saved.as_ref() != Some(&self.audio_preferences) {
                    return self.save_audio_preferences();
                }
            }
            Message::StartQr
                if self.acknowledged && matches!(self.phase, Phase::Login | Phase::Qr(_)) =>
            {
                self.notice = None;
                return self.start_qr();
            }
            Message::CancelQr if matches!(self.phase, Phase::Qr(_)) => {
                // Dropping the attempt aborts its worker: the socket closes and
                // the attempt's keys are dropped.
                self.phase = Phase::Login;
                self.notice = None;
            }
            Message::Qr(id, event) if matches!(&self.phase, Phase::Qr(attempt) if attempt.id == id) =>
            {
                let Phase::Qr(attempt) = &mut self.phase else {
                    return Task::none();
                };
                if let Applied::Authorized(token) = attempt.apply(event) {
                    // Same validation and storage path as pasted tokens.
                    self.phase = Phase::Authenticating {
                        saved_account: None,
                    };
                    self.notice = None;
                    return Task::perform(
                        login::token_login(token, self.store.clone(), !self.memory_only),
                        Message::LoginFinished,
                    );
                }
            }
            Message::PasswordLoginEdited(value) if matches!(self.phase, Phase::Login) => {
                self.password_login = value;
            }
            Message::PasswordEdited(value) if matches!(self.phase, Phase::Login) => {
                self.password = value;
            }
            Message::PasswordSubmit
                if matches!(self.phase, Phase::Login) && self.can_password_login() =>
            {
                self.notice = None;
                self.captcha_solved = false;
                self.phase = Phase::Authenticating {
                    saved_account: None,
                };
                return self.password_attempt(None);
            }
            Message::PasswordStep(result) if matches!(self.phase, Phase::Authenticating { .. }) => {
                match result {
                    Ok(password::Step::Token(token)) => {
                        self.clear_password();
                        return Task::perform(
                            login::token_login(token, self.store.clone(), !self.memory_only),
                            Message::LoginFinished,
                        );
                    }
                    Ok(password::Step::Mfa(challenge)) => {
                        self.clear_password();
                        self.clear_token();
                        self.phase = Phase::Mfa(challenge);
                    }
                    Ok(password::Step::Captcha(_)) if self.captcha_solved => {
                        // One challenge per login: a repeat means Discord did not accept ours.
                        self.clear_password();
                        self.phase = Phase::Login;
                        self.notice = Some("Discord asked for another CAPTCHA. Login stopped; try again later or use QR or token login.".to_owned());
                    }
                    Ok(password::Step::Captcha(challenge)) => {
                        self.notice =
                            Some("Complete the CAPTCHA in the window that opened.".to_owned());
                        return Task::perform(captcha::solve(challenge), Message::CaptchaDone);
                    }
                    Err(error) => {
                        self.clear_password();
                        self.phase = Phase::Login;
                        self.notice = Some(error.to_string());
                    }
                }
            }
            Message::CaptchaDone(result) if matches!(self.phase, Phase::Authenticating { .. }) => {
                match result {
                    Ok(solution) => {
                        self.captcha_solved = true;
                        self.notice = None;
                        return self.password_attempt(Some(solution));
                    }
                    Err(error) => {
                        self.clear_password();
                        self.phase = Phase::Login;
                        self.notice = Some(error.to_string());
                    }
                }
            }
            Message::MfaSubmit
                if matches!(self.phase, Phase::Mfa(_))
                    && !self.token.expose_secret().trim().is_empty() =>
            {
                let Phase::Mfa(challenge) = std::mem::replace(
                    &mut self.phase,
                    Phase::Authenticating {
                        saved_account: None,
                    },
                ) else {
                    return Task::none();
                };
                let code = self.take_input_token();
                return Task::perform(password::second_factor(challenge, code), Message::MfaResult);
            }
            Message::CancelMfa if matches!(self.phase, Phase::Mfa(_)) => {
                self.clear_token();
                self.phase = Phase::Login;
            }
            Message::MfaResult(result) if matches!(self.phase, Phase::Authenticating { .. }) => {
                match result {
                    Ok(token) => {
                        return Task::perform(
                            login::token_login(token, self.store.clone(), !self.memory_only),
                            Message::LoginFinished,
                        );
                    }
                    Err(error) => {
                        self.phase = Phase::Login;
                        self.notice = Some(error.to_string());
                    }
                }
            }
            Message::TokenEdited(token) if matches!(self.phase, Phase::Login | Phase::Mfa(_)) => {
                self.token = token;
            }
            Message::InputTooLong if matches!(self.phase, Phase::Login) => {
                self.notice = Some(LoginError::InvalidInput.to_string());
                self.clear_token();
            }
            Message::Acknowledge(value) if matches!(self.phase, Phase::Login) => {
                self.acknowledged = value;
            }
            Message::MemoryOnly(value) if matches!(self.phase, Phase::Login) => {
                self.memory_only = value;
            }
            Message::Login if matches!(self.phase, Phase::Login) && self.can_login() => {
                let token = self.take_input_token();
                self.notice = None;
                self.phase = Phase::Authenticating {
                    saved_account: None,
                };
                return Task::perform(
                    login::token_login(token, self.store.clone(), !self.memory_only),
                    Message::LoginFinished,
                );
            }
            Message::RetrySaved if matches!(self.phase, Phase::Login) => {
                self.phase = Phase::Starting;
                self.clear_token();
                self.notice = None;
                return Task::perform(login::initialize_store(), Message::StoreReady);
            }
            Message::StoreReady(result) if matches!(self.phase, Phase::Starting) => {
                self.phase = Phase::Login;
                match result {
                    Ok(ready) => {
                        self.store = Some(ready.store);
                        self.saved_accounts = ready.accounts;
                        self.account_index = ready.index;
                        if self.saved_accounts.len() == 1 {
                            return self.start_restore(self.saved_accounts[0]);
                        }
                    }
                    Err(error) => {
                        self.store = None;
                        self.notice = Some(LoginError::Store(error).to_string());
                    }
                }
            }
            Message::Restore(account) if matches!(self.phase, Phase::Login) => {
                return self.start_restore(account);
            }
            Message::LoginFinished(result)
                if matches!(self.phase, Phase::Authenticating { .. }) =>
            {
                match result {
                    Ok(LoginOutcome::Authenticated(session, persistence)) => {
                        if persistence == Persistence::Saved
                            && !self.saved_accounts.contains(&session.user.id)
                        {
                            self.saved_accounts.push(session.user.id);
                        }
                        let mut index_task = Task::none();
                        if persistence == Persistence::Saved {
                            self.account_index.upsert(&session.user);
                            index_task = self.save_account_index();
                        }
                        self.notice = None;
                        return Task::batch([index_task, self.enter_account(session, persistence)]);
                    }
                    Ok(LoginOutcome::NeedsMemoryConsent(session, error)) => {
                        self.phase = Phase::MemoryConsent { session, error };
                    }
                    Err(error) => {
                        self.phase = Phase::Login;
                        self.notice = Some(error.to_string());
                    }
                }
            }
            Message::ContinueInMemory => {
                if let Phase::MemoryConsent { session, .. } = &self.phase {
                    let session = Arc::clone(session);
                    return self.enter_account(session, Persistence::MemoryOnly);
                }
            }
            Message::CancelLogin if matches!(self.phase, Phase::MemoryConsent { .. }) => {
                self.reset_login();
            }
            Message::Gateway(id, status) if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == id) =>
            {
                if status == GatewayStatus::AuthenticationRequired
                    || matches!(
                        &status,
                        GatewayStatus::Stopped(
                            fastcord_discord::gateway::StopReason::ActionRequired(_)
                        )
                    )
                {
                    // Cancel panels and their in-flight tasks as on logout,
                    // including requests outside the Gateway subscription.
                    let cache = self.teardown_account().and_then(|(_, cache)| cache);
                    self.reset_login();
                    self.notice = Some(
                        "Discord no longer accepts this login, or requires an account check. All account connections and requests have stopped. Open the official Discord client to check Account Standing and complete any verification or password reset before signing in here again. A saved copy of the old token can be removed with Forget saved login.".to_owned(),
                    );
                    return cache.map_or(Task::none(), |cache| {
                        Task::perform(async move { cache.purge().await }, |result| {
                            Message::AttachmentPurged(result.is_ok())
                        })
                    });
                } else if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.status = status;
                }
            }
            Message::AttachmentPurged(succeeded) => {
                self.attachment_purge_failed = !succeeded;
                if !succeeded && matches!(self.phase, Phase::Login) {
                    self.notice =
                        Some("The local attachment cache could not be fully cleared.".to_owned());
                }
            }
            Message::AttachmentLoaded(generation, key, target, result) if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == generation) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    attachment_ui::loaded(gateway, key, target, result);
                }
            }
            Message::AttachmentActionFinished(generation, key, action, outcome) if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == generation) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.attachment_action_workers.remove(&(key, action));
                    attachment_ui::finished(gateway, outcome);
                }
            }
            Message::MediaReady(generation, key, result) if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == generation) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    media_ui::ready(gateway, key, result);
                }
            }
            Message::MediaFrame(id) => {
                if let Phase::Account { gateway, .. } = &self.phase
                    && let Some(session) = gateway
                        .media
                        .as_ref()
                        .and_then(media_ui::MediaSession::video_session)
                {
                    session.consume(id);
                }
            }
            Message::MediaTick => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    media_ui::tick(gateway);
                }
            }
            Message::CloseViewer => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.attachment_viewer = None;
                }
            }
            Message::Navigation(id) => {
                let before = match &self.phase {
                    Phase::Account { gateway, .. } => gateway.timeline.channel_id,
                    _ => None,
                };
                let status = if let Phase::Account { gateway, .. } = &mut self.phase
                    && gateway.id == id
                {
                    gateway.consume()
                } else {
                    None
                };
                if let Some(status) = status {
                    return self.handle(Message::Gateway(id, status));
                }
                if let Phase::Account { gateway, .. } = &self.phase
                    && let Some(message) =
                        self.search.take_pending_jump(gateway.timeline.channel_id)
                    && let Some(channel) = gateway.timeline.channel_id
                {
                    gateway
                        .controls
                        .timeline_intent(history::Intent::JumpTo { channel, message });
                }
                if self.search.take_jump_expired() || self.inbox.take_expired() {
                    self.notice = Some("That message is in a channel you cannot open.".to_owned());
                }
                if let Phase::Account { gateway, .. } = &self.phase
                    && let Some((channel, target)) =
                        self.inbox.take_pending(gateway.timeline.channel_id)
                {
                    inbox_jump(&gateway.controls, channel, target);
                }
                let mut invite_task = Task::none();
                if let Phase::Account {
                    gateway, session, ..
                } = &self.phase
                {
                    self.sticker.sync(gateway.timeline.channel_id);
                    self.invite.cache.sync_guilds(&gateway.navigation);
                    invite_task = self
                        .invite
                        .pump(&gateway.timeline.rows, &session.client)
                        .map(Message::Invite);
                    self.forward.sync(gateway.timeline.channel_id);
                    self.announce.sync(gateway.timeline.channel_id);
                    self.group.sync(
                        gateway
                            .navigation
                            .private_selection
                            .as_ref()
                            .map(|selection| selection.channel_id),
                    );
                    self.identity.sync(&gateway.navigation);
                    self.safety.sync(&gateway.navigation);
                    self.report
                        .sync(self.report.watched().and_then(|(channel, message)| {
                            timeline::is_retained(&gateway.timeline, channel, message)
                        }));
                    self.poll.sync(gateway.timeline.channel_id);
                    self.slash.sync(gateway.timeline.channel_id);
                    self.slash
                        .modal_shown(gateway.timeline.interaction.modal.as_ref());
                    self.voice.sync(gateway.timeline.channel_id);
                }
                // A navigation the user started (keys, switcher, sidebar) has
                // landed: the composer takes focus back, and the sidebar shows
                // servers or direct messages to match what opened.
                let mut refocus = Task::none();
                if self.nav_pending
                    && let Phase::Account { gateway, .. } = &mut self.phase
                    && gateway.timeline.channel_id.is_some()
                    && gateway.timeline.channel_id != before
                {
                    self.nav_pending = false;
                    gateway.directs = gateway.navigation.private_selection.is_some();
                    refocus = iced::widget::operation::focus(composer::EDITOR_ID);
                }
                self.observe_ui_sound();
                return Task::batch([
                    refocus,
                    self.soundboard_follow(),
                    self.assets_follow(),
                    invite_task,
                ]);
            }
            Message::ShowDirects(on) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.directs = on;
                }
            }
            Message::ToggleVoiceDetails => self.voice_details = !self.voice_details,
            Message::Search(event) => return self.search_event(event),
            Message::Pins(event) => return self.pins_event(event),
            Message::Inbox(event) => return self.inbox_event(event),
            Message::Organize(event) => return self.organize_event(event),
            Message::NotifySettings(event) => return self.notify_settings_event(event),
            Message::Switcher(event) => return self.switcher_event(event),
            Message::Shortcut(shortcut) => return self.navigation_shortcut(shortcut),
            Message::Group(event) => return self.group_event(event),
            Message::Invite(event) => return self.invite_event(event),
            Message::Announce(event) => return self.announce_event(event),
            Message::Apps(event) => return self.apps_event(event),
            Message::Community(event) => return self.community_event(event),
            Message::Assets(event) => return self.assets_event(event),
            Message::GuildAdmin(event) => {
                if matches!(event, guild_admin::Event::MemberRoles(..)) {
                    self.member_menu.close();
                }
                return self.guild_admin_event(event);
            }
            Message::Moderation(event) => return self.moderation_event(event),
            Message::AutoMod(event) => return self.automod_event(event),
            Message::Discover(event) => return self.discover_event(event),
            Message::Onboarding(event) => return self.onboarding_event(event),
            Message::Events(event) => return self.events_event(event),
            Message::Identity(event) => return self.identity_event(event),
            Message::Safety(event) => return self.safety_event(event),
            Message::Report(event) => return self.report_event(event),
            Message::Gif(event) => return self.gif_event(event),
            Message::Sticker(event) => return self.sticker_event(event),
            Message::Soundboard(event) => return self.soundboard_event(event),
            Message::Stage(event) => return self.stage_event(event),
            Message::Forward(event) => return self.forward_event(event),
            Message::Poll(event) => return self.poll_event(event),
            Message::Slash(event) => return self.slash_event(event),
            Message::Voice(event) => return self.voice_event(event),
            Message::VoiceRecordingToggled(on) => {
                self.audio_preferences.voice_recording = on;
                if !on {
                    self.voice.reset();
                }
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::Status(event) => {
                if let Phase::Account {
                    session, gateway, ..
                } = &self.phase
                {
                    let own = gateway.navigation.presences.get(&session.user.id);
                    return self
                        .status
                        .update(event, own, &session.client)
                        .map(Message::Status);
                }
            }
            Message::Threads(event) => return self.threads_event(event),
            Message::Forum(event) => return self.forum_event(event),
            Message::Friends(event) => {
                if matches!(
                    event,
                    friends::Event::Profile(_)
                        | friends::Event::OpenDm(_)
                        | friends::Event::Report(_)
                ) {
                    self.member_menu.close();
                }
                if let Phase::Account {
                    session, gateway, ..
                } = &mut self.phase
                {
                    match event {
                        friends::Event::OpenDm(user) => {
                            self.friends.close();
                            gateway.directs = true;
                            gateway.private.notice = Some(
                                if gateway.controls.open_private_channel(vec![user]) {
                                    "Opening the conversation…"
                                } else {
                                    "The conversation request could not be queued."
                                }
                                .to_owned(),
                            );
                        }
                        friends::Event::Report(user) => {
                            let mut subject = report::Subject::new(
                                fastcord_discord::ReportTarget::User(user.id),
                                "user",
                                user.display_name(),
                            );
                            subject.person = Some((user.id, user.display_name().to_owned()));
                            let _ = self
                                .friends
                                .update(friends::Event::CloseProfile, &session.client);
                            return self
                                .report
                                .begin(subject, &session.client)
                                .map(Message::Report);
                        }
                        event => {
                            if matches!(event, friends::Event::Open) {
                                gateway.directs = true;
                            }
                            return self
                                .friends
                                .update(event, &session.client)
                                .map(Message::Friends);
                        }
                    }
                }
            }
            Message::SelectGuild(guild) => {
                self.member_menu.close();
                self.threads.close();
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.directs = false;
                    gateway.controls.select_guild(guild);
                    return iced::widget::operation::scroll_to(
                        "channel-list",
                        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                    );
                }
            }
            Message::SelectChannel(guild, channel) => {
                self.member_menu.close();
                // Opens a channel's messages (a voice or Stage channel's chat
                // included); it never touches the voice connection.
                self.threads.close();
                self.nav_pending = true;
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.controls.select_channel(guild, channel);
                }
            }
            Message::JoinVoice(guild, channel) => {
                // Joins the call only; the open chat stays as it is.
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    let join = gateway.navigation.selected_guild == Some(guild)
                        && gateway.navigation.channels.rows.iter().any(|row| {
                            row.id == channel
                                && row.kind
                                    == fastcord_discord::state::navigation::ChannelKind::Voice
                                && row.enabled
                                && row.permissions.connect
                        });
                    let vad_forbidden = gateway
                        .navigation
                        .channels
                        .rows
                        .iter()
                        .any(|row| row.id == channel && !row.permissions.use_vad);
                    if join {
                        self.stage.reset();
                        gateway.controls.set_vad_forbidden(vad_forbidden);
                        gateway.voice_guild = Some(guild);
                        gateway.voice_call = None;
                        let controls = gateway.controls.voice_controls();
                        gateway
                            .controls
                            .request_voice_state(VoiceStateRequest::join(
                                Some(guild),
                                channel,
                                controls.self_muted,
                                controls.deafened,
                            ));
                    }
                }
            }
            Message::StartCall(channel) | Message::AcceptCall(channel) => {
                if let Phase::Account {
                    session, gateway, ..
                } = &mut self.phase
                {
                    let busy = gateway.voice_call == Some(channel)
                        && gateway.controls.voice_audio_state() != gateway::VoiceAudioState::Idle;
                    if !busy {
                        // One parent voice connection: this request replaces a
                        // guild voice join (the correlator moves away from it).
                        gateway.voice_guild = None;
                        gateway.controls.set_vad_forbidden(false);
                        gateway.voice_call = Some(channel);
                        let controls = gateway.controls.voice_controls();
                        gateway
                            .controls
                            .request_voice_state(VoiceStateRequest::join(
                                None,
                                channel,
                                controls.self_muted,
                                controls.deafened,
                            ));
                        if matches!(message, Message::StartCall(_)) {
                            return call_signal(session, channel, CallSignal::Ring);
                        }
                    }
                }
            }
            Message::DeclineCall(channel) => {
                if let Phase::Account { session, .. } = &self.phase {
                    return call_signal(session, channel, CallSignal::StopRinging);
                }
            }
            Message::CallSignalled(result) => {
                self.notice = result.err().map(|error| match error {
                    RestError::WriteNotVerified(_) => error.to_string(),
                    _ => "The call request failed; try again.".to_owned(),
                });
            }
            Message::LeaveVoice => {
                self.stage.reset();
                let mut cancel = None;
                if let Phase::Account {
                    session, gateway, ..
                } = &mut self.phase
                {
                    let call = gateway.voice_call.take();
                    if let Some(guild) = gateway.voice_guild.take().map(Some).or(call.map(|_| None))
                    {
                        gateway
                            .controls
                            .request_voice_state(VoiceStateRequest::leave(guild));
                    }
                    // Hanging up before anyone answered also stops the ringing.
                    cancel = call
                        .filter(|channel| {
                            gateway.navigation.calls.iter().any(|call| {
                                call.channel_id == *channel
                                    && !call.ringing.is_empty()
                                    && call
                                        .participants
                                        .iter()
                                        .all(|user| *user == session.user.id)
                            })
                        })
                        .map(|channel| call_signal(session, channel, CallSignal::StopRinging));
                }
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    for session in gateway.controls.watched() {
                        if !gateway.controls.unwatch_stream(session.key.clone()) {
                            self.notice = Some(
                                "Could not queue Stop watching; retry before leaving voice.".into(),
                            );
                        }
                    }
                    gateway.call_view = call_layout::CallView::default();
                }
                if let Some(task) = cancel {
                    return task;
                }
            }
            Message::MemberMenu(event) => return self.member_menu_event(event),
            Message::ActivateParticipant(user) => {
                self.member_menu.close();
                if let Phase::Account {
                    gateway, session, ..
                } = &mut self.phase
                {
                    if user == session.user.id {
                        return Task::none();
                    }
                    let voice = gateway.controls.voice_controls();
                    if let Some(offer) = voice.streams.iter().find(|offer| offer.owner == user) {
                        return self.handle(Message::WatchStream(offer.key.clone()));
                    }
                    if voice
                        .participants
                        .iter()
                        .any(|participant| participant.user_id == user)
                    {
                        gateway.call_view.open = true;
                    } else {
                        self.notice =
                            Some("Join this person's voice channel to watch their stream.".into());
                    }
                }
            }
            Message::WatchStream(key) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    let controls = gateway.controls.voice_controls();
                    if gateway.controls.voice_audio_state() != gateway::VoiceAudioState::Running
                        || !controls.streams.iter().any(|offer| offer.key == key)
                    {
                        self.notice = Some(
                            "Could not watch this stream. Join its voice channel and retry.".into(),
                        );
                    } else {
                        let previous = gateway
                            .controls
                            .watched_stream(&key)
                            .filter(|session| session.failed());
                        if previous.is_some() && !gateway.controls.unwatch_stream(key.clone()) {
                            self.notice = Some("Could not queue Retry watching; retry.".into());
                            return Task::none();
                        }
                        match gateway.controls.watch_stream(key.clone()) {
                            Ok(()) => {
                                if let Some(previous) = previous
                                    && let Some(session) = gateway.controls.watched_stream(&key)
                                {
                                    session.set_audio(previous.audio_settings());
                                }
                                gateway.call_view.open = true;
                                self.notice = None;
                            }
                            Err(reason) => self.notice = Some(reason.into()),
                        }
                    }
                }
            }
            Message::SetStreamVolume(key, percent) => {
                if let Phase::Account { gateway, .. } = &self.phase
                    && let Some(session) = gateway.controls.watched_stream(&key)
                {
                    session.set_audio(playback::AudioSettings {
                        percent,
                        ..session.audio_settings()
                    });
                }
            }
            Message::MuteStream(key, muted) => {
                if let Phase::Account { gateway, .. } = &self.phase
                    && let Some(session) = gateway.controls.watched_stream(&key)
                {
                    session.set_audio(playback::AudioSettings {
                        muted,
                        ..session.audio_settings()
                    });
                }
            }
            Message::UnwatchStream(key) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    if gateway.controls.unwatch_stream(key) {
                        self.notice = None;
                    } else {
                        self.notice = Some("Could not queue Stop watching; retry.".into());
                    }
                }
            }
            Message::OpenCall => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    if let Some((Some(guild), channel)) = gateway.controls.voice_target() {
                        self.threads.close();
                        gateway.controls.select_channel(guild, channel);
                    }
                    gateway.call_view.open = gateway.voice_guild.or(gateway.voice_call).is_some();
                }
            }
            Message::ShowStream(open) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    let any = !gateway.controls.watched().is_empty()
                        || !gateway.controls.camera_tiles().snapshot().remote.is_empty()
                        || gateway.controls.voice_controls().camera.wanted;
                    gateway.call_view.open = open && any;
                    if !open {
                        gateway.call_view.full = None;
                    }
                }
            }
            Message::SetCallMode(mode) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.call_view.mode = mode;
                    gateway.call_view.full = None;
                }
            }
            Message::PinTile(id) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.call_view.toggle_pin(id);
                    gateway.call_view.full = None;
                }
            }
            Message::FullTile(id) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.call_view.full = id;
                    gateway.call_view.open |= gateway.call_view.full.is_some();
                }
            }
            Message::SetHidePeople(hide) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.call_view.hide_people = hide;
                }
            }
            Message::SetWindowFullscreen(on) => {
                self.window_fullscreen = on;
                return iced::window::latest().and_then(move |id| {
                    iced::window::set_mode(
                        id,
                        if on {
                            iced::window::Mode::Fullscreen
                        } else {
                            iced::window::Mode::Windowed
                        },
                    )
                });
            }
            Message::OpenSharePicker => {
                if let Phase::Account { gateway, .. } = &mut self.phase
                    && !gateway.controls.share_view().active
                    && !gateway.share_picker.is_open()
                {
                    gateway.share_picker = gateway::SharePicker::Loading;
                    // Enumeration touches the window system: never on the UI thread.
                    return Task::perform(
                        async {
                            tokio::task::spawn_blocking(|| {
                                fastcord_platform::capture::availability()?;
                                let sources = fastcord_platform::capture::list_sources()?;
                                // Only where the user chooses; a missing daemon
                                // just leaves the list empty.
                                let audio = if gateway::AudioOffer::current()
                                    == gateway::AudioOffer::Choose
                                {
                                    fastcord_platform::loopback::list_targets().unwrap_or_default()
                                } else {
                                    Vec::new()
                                };
                                Ok::<_, fastcord_platform::capture::CaptureError>((sources, audio))
                            })
                            .await
                            .map_err(|_| "Listing screens and windows failed.".to_owned())
                            .and_then(|listed| listed.map_err(|error| error.to_string()))
                        },
                        Message::ShareSources,
                    );
                }
            }
            Message::ShareSources(result) => {
                if let Phase::Account { gateway, .. } = &mut self.phase
                    && matches!(gateway.share_picker, gateway::SharePicker::Loading)
                {
                    gateway.share_picker = match result {
                        Ok((sources, audio)) => {
                            if !audio
                                .iter()
                                .any(|source| Some(source.target) == gateway.share_audio_target)
                            {
                                gateway.share_audio_target = None;
                            }
                            gateway.share_audio_sources = audio;
                            gateway::SharePicker::Listed(sources)
                        }
                        Err(error) => gateway::SharePicker::Failed(error),
                    };
                }
            }
            Message::ToggleShareAudio(on) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.share_audio = on;
                }
            }
            Message::ChooseShareAudio(target) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.share_audio_target = Some(target);
                }
            }
            Message::CloseSharePicker => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.share_picker = gateway::SharePicker::Closed;
                }
            }
            Message::StartShare(id) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    let source = match &gateway.share_picker {
                        gateway::SharePicker::Listed(sources) => {
                            sources.iter().find(|source| source.id == id).cloned()
                        }
                        _ => None,
                    };
                    gateway.share_picker = gateway::SharePicker::Closed;
                    let target = gateway.controls.voice_target().filter(|_| {
                        gateway.controls.voice_audio_state() == gateway::VoiceAudioState::Running
                    });
                    if let (Some(source), Some((guild, channel))) = (source, target) {
                        let request = match guild {
                            Some(guild) => {
                                fastcord_model::StreamCreateRequest::guild(guild, channel)
                            }
                            None => fastcord_model::StreamCreateRequest::call(channel),
                        };
                        let offer = gateway::AudioOffer::current();
                        let audio = gateway
                            .share_audio
                            .then(|| offer.scope(source.id, gateway.share_audio_target))
                            .flatten();
                        self.notice = (gateway.share_audio
                            && offer == gateway::AudioOffer::Choose
                            && audio.is_none())
                        .then(|| {
                            "No audio source was chosen, so only the picture is shared.".into()
                        });
                        gateway.controls.start_share(source, request, audio);
                    } else {
                        self.notice =
                            Some("Join a voice channel before sharing your screen.".into());
                    }
                }
            }
            Message::StopShare => {
                if let Phase::Account { gateway, .. } = &self.phase
                    && !gateway.controls.stop_share()
                {
                    self.notice = Some("Could not stop sharing; try again.".into());
                }
            }
            Message::VideoReady(id) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    for session in gateway.controls.watched() {
                        session.consume(id);
                    }
                    let tiles = gateway.controls.camera_tiles().snapshot();
                    for session in tiles
                        .preview
                        .iter()
                        .chain(tiles.remote.iter().map(|(_, session)| session))
                    {
                        session.consume(id);
                    }
                }
            }
            Message::ToggleCamera => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    // The only way the camera turns on: an explicit click, in a
                    // running call.
                    let on = !gateway.controls.voice_controls().camera.wanted;
                    if !on
                        || gateway.controls.voice_audio_state() == gateway::VoiceAudioState::Running
                    {
                        if on {
                            // The saved device, effect and mirror apply from
                            // the first picture.
                            let saved = &self.audio_preferences.camera;
                            gateway.controls.prepare_camera(
                                saved.id.clone(),
                                saved.effect,
                                saved.mirror,
                            );
                        }
                        gateway.controls.set_camera(on);
                    }
                }
            }
            Message::ListCameras => {
                if !self.camera_list.probing {
                    self.camera_list.probing = true;
                    return Task::perform(gateway::list_cameras(), Message::CamerasListed);
                }
            }
            Message::CamerasListed(result) => {
                self.camera_list.probing = false;
                self.camera_list.probed = true;
                match result {
                    Ok(devices) => {
                        self.camera_list.devices = devices;
                        self.camera_list.error = None;
                    }
                    Err(error) => self.camera_list.error = Some(error),
                }
            }
            Message::SelectCamera(choice) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.select_camera(choice.id.clone());
                }
                self.audio_preferences.camera = audio_devices::CameraSelection {
                    id: choice.id,
                    label: choice.label,
                    ..self.audio_preferences.camera.clone()
                };
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::SelectCameraEffect(effect) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.set_camera_effect(effect);
                }
                self.audio_preferences.camera.effect = effect;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::SetMirrorPreview(mirror) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.set_camera_mirror(mirror);
                }
                self.audio_preferences.camera.mirror = mirror;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::SetSelfMute(muted) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.set_self_muted(muted);
                }
            }
            Message::SetDeafen(deafened) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.set_deafened(deafened);
                }
            }
            // The slider updates the live gain while dragging and persists once on release.
            Message::SetParticipantVolume(user, percent) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.set_volume_percent(user, percent);
                }
            }
            Message::CommitParticipantVolume(user) => {
                if let Phase::Account {
                    session, gateway, ..
                } = &self.phase
                {
                    let percent = gateway.controls.voice_controls().volume_percent(user);
                    if voice_settings::save(session.user.id, user, percent).is_err() {
                        self.notice = Some("Could not save the voice volume setting.".to_owned());
                    }
                }
            }
            Message::PresetParticipantVolume(user, percent) => {
                if let Phase::Account {
                    session, gateway, ..
                } = &self.phase
                    && gateway.controls.set_volume_percent(user, percent)
                    && voice_settings::save(session.user.id, user, percent).is_err()
                {
                    self.notice = Some("Could not save the voice volume setting.".to_owned());
                }
            }
            Message::SelectPrivateChannel(channel) => {
                self.threads.close();
                self.nav_pending = true;
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    self.friends.close();
                    gateway.directs = true;
                    gateway.controls.select_private_channel(channel);
                }
            }
            Message::PrivateRecipients(value) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    if value.len() <= MAX_PRIVATE_RECIPIENT_INPUT_BYTES {
                        gateway.private.recipients = value;
                        gateway.private.notice = None;
                    } else {
                        gateway.private.notice =
                            Some("Recipient selection is limited to 256 UTF-8 bytes.".to_owned());
                    }
                }
            }
            Message::OpenPrivate => {
                if let Phase::Account {
                    session, gateway, ..
                } = &mut self.phase
                {
                    match parse_private_recipients(&gateway.private.recipients, session.user.id) {
                        Ok(recipients) => {
                            gateway.private.notice =
                                Some(if gateway.controls.open_private_channel(recipients) {
                                    "Opening the selected private conversation…".to_owned()
                                } else {
                                    "The private conversation request could not be queued."
                                        .to_owned()
                                });
                        }
                        Err(error) => gateway.private.notice = Some(error.to_owned()),
                    }
                }
            }
            Message::GuildViewport(window) | Message::ChannelViewport(window) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway
                        .controls
                        .viewport(matches!(message, Message::GuildViewport(_)), window);
                }
            }
            Message::PrivateViewport(window) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.private_viewport(window);
                }
            }
            Message::WindowActive(active) => {
                self.window_active = active;
                self.sync_viewing();
                notify::set_active(active);
                if !active {
                    let scope = self.audio_preferences.voice_input.scope;
                    let released = self.shortcuts.window_lost_focus(scope);
                    return self.apply_shortcuts(released);
                }
            }
            Message::WindowSize(size) => {
                self.window_drawable = size.width > 0.0 && size.height > 0.0;
                if self.window_drawable && !self.compact_call {
                    self.window_size = size;
                }
                self.sync_viewing();
            }
            Message::MarkRead(guild, channel) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.mark_read(guild, channel);
                }
            }
            Message::FirstUnread(channel, acked) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway
                        .controls
                        .timeline_intent(history::Intent::FirstUnread { channel, acked });
                }
            }
            Message::EmojiTick(now) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.emoji.tick(now);
                }
            }
            Message::EmojiLoaded(generation, asset, ticket, result) if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == generation) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    return gateway.emoji.loaded(
                        generation,
                        asset,
                        ticket,
                        result,
                        gateway.attachments.as_ref(),
                    );
                }
            }
            Message::AvatarLoaded(generation, key, image) => {
                if let Phase::Account { gateway, .. } = &mut self.phase
                    && gateway.id == generation
                {
                    return gateway.avatars.loaded(
                        generation,
                        key,
                        image,
                        gateway.attachments.as_ref(),
                    );
                }
            }
            Message::EmojiVisibility(occurrence, visible) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.emoji_visibility(occurrence, visible);
                }
            }
            Message::EmojiPicker(event) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    let reset_scroll = matches!(
                        event,
                        emoji_picker::Event::Toggle
                            | emoji_picker::Event::Search(_)
                            | emoji_picker::Event::Category(_)
                            | emoji_picker::Event::Variants(_)
                    );
                    if matches!(event, emoji_picker::Event::Toggle)
                        && !gateway.timeline.can_send
                        && gateway.composer.editing().is_none()
                    {
                        return Task::none();
                    }
                    if let Some(choice) = gateway.emoji_picker.update(
                        event,
                        gateway.timeline.channel_id,
                        &gateway.emoji_snapshot,
                    ) {
                        let accepted =
                            match (gateway.emoji_picker.reaction(), gateway.timeline.channel_id) {
                                // Opened for a message: the pick reacts to it.
                                (Some(message), Some(channel)) => {
                                    let queued = gateway.controls.add_reaction(
                                        channel,
                                        message,
                                        choice.reaction_emoji(),
                                    );
                                    if !queued {
                                        gateway.composer.refused();
                                    }
                                    queued
                                }
                                // Otherwise it is text for the composer.
                                _ => gateway.composer.insert_emoji(&choice.source()),
                            };
                        if accepted {
                            gateway.emoji_picker.inserted(choice);
                        }
                    }
                    if reset_scroll && gateway.emoji_picker.request.open {
                        return Task::batch([
                            iced::widget::operation::scroll_to(
                                "emoji-list",
                                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                            ),
                            iced::widget::operation::focus("emoji-search"),
                        ]);
                    }
                    if !gateway.emoji_picker.request.open {
                        return iced::widget::operation::focus(composer::EDITOR_ID);
                    }
                }
            }
            Message::Timeline(event) => {
                if let Phase::Account {
                    session, gateway, ..
                } = &mut self.phase
                {
                    match event {
                        timeline::Event::CopyText {
                            channel_id,
                            message_id,
                        } => {
                            if let Some(source) =
                                timeline::copy_text(&gateway.timeline, channel_id, message_id)
                            {
                                return iced::clipboard::write(source.to_owned());
                            }
                        }
                        timeline::Event::ReadAloud {
                            channel_id,
                            message_id,
                        } => {
                            if let Some(message) =
                                timeline::message_of(&gateway.timeline, channel_id, message_id)
                            {
                                speech::speak(speech::announcement(message));
                            }
                        }
                        timeline::Event::StopReading => speech::stop(),
                        timeline::Event::Forward {
                            channel_id,
                            message_id,
                        } => {
                            let preview =
                                timeline::copy_text(&gateway.timeline, channel_id, message_id)
                                    .unwrap_or_default()
                                    .to_owned();
                            let source = fastcord_discord::ForwardSource {
                                channel: channel_id,
                                message: message_id,
                                guild: gateway
                                    .navigation
                                    .selected_guild
                                    .filter(|_| !gateway.directs),
                            };
                            return self.forward_event(forward::Event::Open { source, preview });
                        }
                        timeline::Event::Publish {
                            channel_id,
                            message_id,
                        } => {
                            return self.announce_event(announce::Event::AskPublish(
                                channel_id, message_id,
                            ));
                        }
                        timeline::Event::ConfirmPublish {
                            channel_id,
                            message_id,
                        } => {
                            return self.announce_event(announce::Event::ConfirmPublish(
                                channel_id, message_id,
                            ));
                        }
                        timeline::Event::CancelPublish => {
                            return self.announce_event(announce::Event::CancelPublish);
                        }
                        timeline::Event::CopyLink {
                            channel_id,
                            message_id,
                        } => {
                            let guild = gateway
                                .navigation
                                .selected_guild
                                .filter(|_| !gateway.directs);
                            return iced::clipboard::write(message_link::format(
                                guild, channel_id, message_id,
                            ));
                        }
                        timeline::Event::JoinInvite(code) => {
                            return self.invite_event(invite::Event::Join(code));
                        }
                        timeline::Event::OpenGuild(guild) => {
                            return self.handle(Message::SelectGuild(guild));
                        }
                        timeline::Event::CopyId { message_id } => {
                            return iced::clipboard::write(message_id.to_string());
                        }
                        timeline::Event::OpenLink(url) => {
                            if let Some(link) = message_link::parse(&url) {
                                // Only a channel the account can open is selected;
                                // otherwise the wait ends with a notice.
                                jump_to(
                                    &mut self.search,
                                    gateway,
                                    link.guild,
                                    link.channel,
                                    Some(link.message),
                                );
                            } else if rich::safe_url(&url) {
                                let _ = fastcord_platform::open_url(&url);
                            }
                        }
                        timeline::Event::Report {
                            channel_id,
                            message_id,
                        } => {
                            let Some(found) =
                                timeline::report_subject(&gateway.timeline, channel_id, message_id)
                            else {
                                return Task::none();
                            };
                            let guild = gateway
                                .navigation
                                .selected_guild
                                .filter(|_| !gateway.directs);
                            let mut subject = report::Subject::new(
                                fastcord_discord::ReportTarget::Message {
                                    channel: channel_id,
                                    message: message_id,
                                },
                                if found.voice {
                                    "voice message"
                                } else {
                                    "message"
                                },
                                found.author.display_name(),
                            );
                            subject.excerpt = found.excerpt;
                            subject.link =
                                Some(message_link::format(guild, channel_id, message_id));
                            subject.person =
                                Some((found.author.id, found.author.display_name().to_owned()));
                            return self
                                .report
                                .begin(subject, &session.client)
                                .map(Message::Report);
                        }
                        timeline::Event::OpenProfile(user) => {
                            return self
                                .friends
                                .update(friends::Event::Profile(user), &session.client)
                                .map(Message::Friends);
                        }
                        timeline::Event::StartThread {
                            channel_id,
                            message_id,
                        } => {
                            if let Some(selection) = gateway
                                .navigation
                                .selection
                                .as_ref()
                                .filter(|s| s.channel_id == channel_id)
                            {
                                let first =
                                    timeline::copy_text(&gateway.timeline, channel_id, message_id)
                                        .unwrap_or_default()
                                        .to_owned();
                                return self
                                    .threads
                                    .update(
                                        threads::Event::Start(
                                            selection.guild_id,
                                            channel_id,
                                            message_id,
                                            first,
                                        ),
                                        &session.client,
                                    )
                                    .map(Message::Threads);
                            }
                        }
                        // A thread this client has never held cannot be opened
                        // from here; the archive list brings it in.
                        timeline::Event::OpenThread(thread) => {
                            if let Some(guild) = gateway.navigation.selected_guild {
                                self.threads.close();
                                gateway.controls.select_channel(guild, thread);
                            }
                        }
                        timeline::Event::ToggleSpoiler {
                            message_id,
                            spoiler,
                        } => gateway.toggle_spoiler(message_id, spoiler),
                        timeline::Event::EmojiVisibility(occurrence, visible) => {
                            gateway.emoji_visibility(occurrence, visible);
                        }
                        timeline::Event::LoadAttachment {
                            channel_id,
                            message_id,
                            attachment_id,
                        } => {
                            return attachment_ui::load_preview(
                                gateway,
                                session,
                                channel_id,
                                message_id,
                                attachment_id,
                            );
                        }
                        timeline::Event::PlayAttachment {
                            channel_id,
                            message_id,
                            attachment_id,
                        } => {
                            return media_ui::play(
                                gateway,
                                session,
                                self.audio_preferences.output.choice.clone(),
                                channel_id,
                                message_id,
                                attachment_id,
                            );
                        }
                        timeline::Event::Media(control) => media_ui::control(gateway, control),
                        timeline::Event::ViewAttachment {
                            channel_id,
                            message_id,
                            attachment_id,
                        } => {
                            return attachment_ui::open_viewer(
                                gateway,
                                session,
                                channel_id,
                                message_id,
                                attachment_id,
                            );
                        }
                        timeline::Event::DownloadAttachment {
                            channel_id,
                            message_id,
                            attachment_id,
                        } => {
                            return attachment_ui::start_action(
                                gateway,
                                session,
                                channel_id,
                                message_id,
                                attachment_id,
                                AttachmentAction::Download,
                            );
                        }
                        timeline::Event::OpenAttachment {
                            channel_id,
                            message_id,
                            attachment_id,
                        } => {
                            return attachment_ui::start_action(
                                gateway,
                                session,
                                channel_id,
                                message_id,
                                attachment_id,
                                AttachmentAction::Open,
                            );
                        }
                        timeline::Event::Viewport {
                            channel_id,
                            viewport,
                        } => gateway.controls.timeline_viewport(channel_id, viewport),
                        timeline::Event::Measured {
                            channel_id,
                            measurement,
                        } => gateway.controls.timeline_measured(channel_id, measurement),
                        timeline::Event::JumpLatest { channel_id } => {
                            gateway
                                .controls
                                .timeline_intent(history::Intent::Latest(channel_id));
                        }
                        timeline::Event::JumpTo {
                            channel_id,
                            message_id,
                        } => gateway.controls.timeline_intent(history::Intent::JumpTo {
                            channel: channel_id,
                            message: message_id,
                        }),
                        timeline::Event::SetPinned {
                            channel_id,
                            message_id,
                            pinned,
                        } => {
                            return self.pins_event(pins::Event::SetPinned(
                                channel_id, message_id, pinned,
                            ));
                        }
                        timeline::Event::LoadOlder { channel_id } => {
                            gateway
                                .controls
                                .timeline_intent(history::Intent::Older(channel_id));
                        }
                        timeline::Event::Retry { channel_id } => {
                            gateway
                                .controls
                                .timeline_intent(history::Intent::Retry(channel_id));
                        }
                        action @ (timeline::Event::Edit { .. }
                        | timeline::Event::Reply { .. }
                        | timeline::Event::Delete { .. }
                        | timeline::Event::ConfirmDelete { .. }
                        | timeline::Event::CancelDelete
                        | timeline::Event::SelectMode { .. }
                        | timeline::Event::ToggleSelect { .. }
                        | timeline::Event::AskBulkDelete { .. }
                        | timeline::Event::CancelBulkDelete
                        | timeline::Event::ConfirmBulkDelete { .. }
                        | timeline::Event::AskReactionRemoval(_)
                        | timeline::Event::CancelReactionRemoval
                        | timeline::Event::ConfirmReactionRemoval
                        | timeline::Event::RetryChange { .. }
                        | timeline::Event::DismissChange { .. }
                        | timeline::Event::ToggleReaction { .. }
                        | timeline::Event::InspectReaction { .. }
                        | timeline::Event::CloseReactionUsers
                        | timeline::Event::PollVote { .. }
                        | timeline::Event::PollEnd { .. }
                        | timeline::Event::SetEmbeds { .. }
                        | timeline::Event::More(_)
                        | timeline::Event::PollVoters { .. }
                        | timeline::Event::Component { .. }
                        | timeline::Event::DismissEphemeral { .. }
                        | timeline::Event::React { .. }) => {
                            if gateway.message_action(action) {
                                return iced::widget::operation::focus(composer::EDITOR_ID);
                            }
                        }
                    }
                }
            }
            Message::FilesPicked(paths) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.composer.attach(paths);
                }
            }
            Message::ImagePasted(result) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    match result {
                        Ok(path) => gateway.composer.attach_pasted(&path),
                        Err(reason) => gateway.composer.paste_failed(reason),
                    }
                }
            }
            Message::Composer(composer::Event::PasteImage) => {
                if matches!(self.phase, Phase::Account { .. }) {
                    return Task::perform(
                        async {
                            tokio::task::spawn_blocking(paste::paste_image)
                                .await
                                .unwrap_or(Err("The pasted image could not be read."))
                        },
                        Message::ImagePasted,
                    );
                }
            }
            Message::FileDropped(path) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.composer.attach(vec![path]);
                }
            }
            Message::Composer(composer::Event::Attach) => {
                if matches!(self.phase, Phase::Account { .. }) {
                    return Task::perform(
                        async {
                            rfd::AsyncFileDialog::new()
                                .set_title("Attach files")
                                .pick_files()
                                .await
                                .map(|files| {
                                    files
                                        .into_iter()
                                        .map(|file| file.path().to_owned())
                                        .collect()
                                })
                                .unwrap_or_default()
                        },
                        Message::FilesPicked,
                    );
                }
            }
            Message::Composer(event) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    let refocus = matches!(event, composer::Event::Send | composer::Event::Edit(_));
                    // Typing is announced only by a focused window's edits.
                    let typing = self
                        .window_active
                        .then(|| gateway.composer.typing_channel(&event))
                        .flatten()
                        .filter(|channel| {
                            gateway.timeline.can_send
                                && gateway.timeline.channel_id == Some(*channel)
                        });
                    composer::update(
                        &mut gateway.composer,
                        event,
                        &gateway.timeline,
                        &gateway.controls,
                    );
                    if let Some(channel) = typing {
                        gateway.controls.typing(channel);
                    }
                    let slash = self.slash_typed();
                    if refocus {
                        return Task::batch([
                            iced::widget::operation::focus(composer::EDITOR_ID),
                            slash,
                        ]);
                    }
                    return slash;
                }
            }
            Message::Logout => {
                if let Some((account, cache)) = self.teardown_account() {
                    // Dropping the account phase aborts attachment workers before purge starts.
                    let delete = self.start_delete(account);
                    return Task::batch([delete, Self::purge_cache(cache)]);
                }
            }
            Message::SwitchAccount(target) => {
                let Phase::Account {
                    session,
                    persistence,
                    ..
                } = &self.phase
                else {
                    return Task::none();
                };
                // A memory-only login would be lost, and the target must be saved.
                if *persistence != Persistence::Saved
                    || session.user.id == target
                    || !self.saved_accounts.contains(&target)
                    || self.store.is_none()
                {
                    return Task::none();
                }
                // Everything of the current account is gone (as for logout, but
                // its saved login stays) before the other one starts signing in.
                let cache = self.teardown_account().and_then(|(_, cache)| cache);
                return Task::batch([self.start_restore(target), Self::purge_cache(cache)]);
            }
            Message::Account(event) => return self.account_event(event),
            Message::AccountRemoved(account, result) => {
                if result.is_ok() {
                    self.saved_accounts.retain(|id| *id != account);
                    self.account_index.remove(account);
                    let _ = voice_settings::purge(account);
                    return self.save_account_index();
                }
                self.account_panel
                    .fail("The native store could not remove that saved login.");
            }
            Message::AccountIndexSaved => {}
            Message::Forget(account) if matches!(self.phase, Phase::Login) => {
                return self.start_delete(account);
            }
            Message::RetryDelete => {
                if let Phase::DeleteFailed { account, .. } = self.phase {
                    return self.start_delete(account);
                }
            }
            Message::Deleted(result) => {
                if let Phase::Deleting { account } = self.phase {
                    match result {
                        Ok(()) => {
                            self.saved_accounts.retain(|id| *id != account);
                            self.account_index.remove(account);
                            let index_task = self.save_account_index();
                            let purge_failed = self.attachment_purge_failed;
                            self.attachment_purge_failed = false;
                            self.reset_login();
                            let voice_purge_failed = voice_settings::purge(account).is_err();
                            self.notice = Some(if purge_failed || voice_purge_failed {
                                "Logged out locally. The saved credential was removed, but some local attachment cache or saved voice volumes could not be cleared. Other Discord sessions were not revoked."
                            } else {
                                "Logged out locally. The saved credential was removed and account state was cleared. Other Discord sessions were not revoked."
                            }.to_owned());
                            return index_task;
                        }
                        Err(error) => {
                            self.phase = Phase::DeleteFailed { account, error };
                        }
                    }
                }
            }
            _ => {}
        }
        Task::none()
    }

    /// The soundboard button of the voice panel: shown for a server call whose channel the
    /// sidebar knows (its permissions come from that row). Previewing needs no permission;
    /// playing needs `USE_SOUNDBOARD` and `SPEAK`.
    fn soundboard_toggle(&self, gateway: &GatewayPanel) -> Option<(bool, Message)> {
        let (Some(guild), channel) = gateway.controls.voice_target()? else {
            return None;
        };
        let permissions = gateway
            .navigation
            .channels
            .rows
            .iter()
            .find(|row| row.id == channel)
            .map(|row| row.permissions)?;
        let open = self.soundboard.is_open();
        let event = if open {
            soundboard::Event::Close
        } else {
            soundboard::Event::Open(soundboard::Context {
                guild,
                channel,
                can_play: permissions.use_soundboard && permissions.speak,
            })
        };
        Some((open, Message::Soundboard(event)))
    }

    fn share_picker_view<'a>(
        picker: &'a gateway::SharePicker,
        share_audio: bool,
        audio_sources: &'a [fastcord_platform::loopback::AudioSource],
        audio_target: Option<fastcord_platform::loopback::AudioTarget>,
    ) -> Element<'a, Message> {
        use fastcord_platform::capture::SourceKind;
        let mut content = column![
            row![
                text("Share your screen").size(22).width(Length::Fill),
                button("Cancel").on_press(Message::CloseSharePicker),
            ]
            .spacing(12),
            text(if cfg!(windows) {
                "Choose a screen or a window. Windows draws a border around what is shared, and your voice stays connected."
            } else if cfg!(target_os = "linux") {
                "Choose a screen or a window; your desktop then asks which one in its own dialog. Your voice stays connected."
            } else {
                "Choose a screen or a window. Your voice stays connected."
            })
            .size(12),
        ]
        .spacing(12);
        match picker {
            gateway::SharePicker::Closed => {}
            gateway::SharePicker::Loading => {
                content = content.push(text("Looking for screens and windows…"));
            }
            gateway::SharePicker::Failed(error) => {
                content = content.push(text(error.clone()));
            }
            gateway::SharePicker::Listed(sources) => {
                let mut screens = column![text("Screens").size(16)].spacing(6);
                let mut windows = column![text("Windows").size(16)].spacing(6);
                let mut any_window = false;
                for source in sources {
                    let entry = button(text(source.title.clone()))
                        .on_press(Message::StartShare(source.id))
                        .width(Length::Fill);
                    match source.id.kind() {
                        SourceKind::Display => screens = screens.push(entry),
                        SourceKind::Window => {
                            any_window = true;
                            windows = windows.push(entry);
                        }
                    }
                }
                if !any_window {
                    windows = windows.push(text("No shareable windows are open.").size(12));
                }
                let offer = gateway::AudioOffer::current();
                if offer != gateway::AudioOffer::Unavailable {
                    let mut audio = column![
                        checkbox(share_audio)
                            .label("Share audio")
                            .on_toggle(Message::ToggleShareAudio)
                    ]
                    .spacing(4);
                    audio = audio.push(text(offer.note()).size(12));
                    if offer == gateway::AudioOffer::Choose && share_audio {
                        if audio_sources.is_empty() {
                            audio = audio
                                .push(text("No PipeWire outputs or inputs were found.").size(12));
                        }
                        for source in audio_sources {
                            audio = audio.push(iced::widget::radio(
                                source.title.as_str(),
                                source.target,
                                audio_target,
                                Message::ChooseShareAudio,
                            ));
                        }
                    }
                    let chosen_carries_call =
                        audio_target.is_some_and(|target| target.whole_output);
                    if offer.warns() || (share_audio && chosen_carries_call) {
                        audio = audio.push(
                            text("Warning: viewers may hear your call while this is on.")
                                .size(12)
                                .color(iced::Color::from_rgb(0.9, 0.55, 0.1)),
                        );
                    }
                    content = content.push(audio);
                } else {
                    content = content.push(text(offer.note()).size(12));
                }
                content = content
                    .push(scrollable(column![screens, windows].spacing(16)).height(Length::Fill));
            }
        }
        container(content)
            .padding(20)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(Color::from_rgb8(24, 25, 30).into()),
                ..Default::default()
            })
            .into()
    }

    fn can_login(&self) -> bool {
        self.acknowledged && !self.token.expose_secret().trim().is_empty()
    }

    fn take_input_token(&mut self) -> Arc<UserToken> {
        let token = std::mem::replace(&mut self.token, Arc::new(UserToken::new(String::new())));
        let trimmed = token.expose_secret().trim();
        if trimmed.len() == token.expose_secret().len() {
            token
        } else {
            Arc::new(UserToken::new(trimmed.to_owned()))
        }
    }

    fn can_password_login(&self) -> bool {
        self.acknowledged
            && !self.password_login.trim().is_empty()
            && !self.password.expose_secret().is_empty()
    }

    fn clear_password(&mut self) {
        self.password = Arc::new(UserToken::new(String::new()));
        self.password_login.clear();
        self.captcha_solved = false;
    }

    fn password_attempt(
        &self,
        captcha: Option<fastcord_discord::CaptchaSolution>,
    ) -> Task<Message> {
        Task::perform(
            password::attempt(
                self.password_login.clone(),
                Arc::clone(&self.password),
                captcha,
            ),
            Message::PasswordStep,
        )
    }

    fn clear_token(&mut self) {
        self.token = Arc::new(UserToken::new(String::new()));
    }

    fn reset_login(&mut self) {
        self.phase = Phase::Login;
        self.clear_token();
        self.acknowledged = false;
        self.memory_only = false;
        self.notice = None;
    }

    /// Stops and forgets everything of the active account (panels, drafts,
    /// workers), the common part of logout and switching. Returns the account
    /// and its attachment cache, which the caller purges.
    fn teardown_account(
        &mut self,
    ) -> Option<(Snowflake, Option<crate::attachments::AttachmentCache>)> {
        self.switcher = switcher::State::default();
        self.nav_pending = false;
        // The switch lives in the account's client, which is dropped with it.
        self.unverified_open = false;
        if let Phase::Account { gateway, .. } = &mut self.phase {
            // Drafts live in memory only; logout forgets them at once.
            gateway.composer.clear();
        }
        self.pins = pins::State::default();
        self.inbox.clear();
        self.notify_settings.clear();
        self.group.clear();
        self.invite.clear();
        self.guild_admin.clear();
        self.announce.clear();
        self.moderation.clear();
        self.apps.clear();
        self.community.clear();
        self.assets.clear();
        self.automod.clear();
        self.discover.clear();
        self.onboarding.clear();
        self.events.clear();
        self.identity.clear();
        self.safety.clear();
        self.account_panel.clear();
        self.report.clear();
        self.gif = gif::State::default();
        self.sticker = sticker::State::default();
        self.soundboard.reset();
        self.stage.reset();
        self.forward = forward::State::default();
        self.poll = poll_create::State::default();
        self.slash.reset();
        self.voice.reset();
        self.threads.drop_all();
        self.forum.drop_all();
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return None;
        };
        session.client.stop_authenticated_work();
        Some((session.user.id, gateway.attachments.clone()))
    }

    fn purge_cache(cache: Option<crate::attachments::AttachmentCache>) -> Task<Message> {
        cache.map_or(Task::none(), |cache| {
            Task::perform(async move { cache.purge().await }, |result| {
                Message::AttachmentPurged(result.is_ok())
            })
        })
    }

    fn save_account_index(&self) -> Task<Message> {
        let index = self.account_index.clone();
        Task::perform(
            async move {
                let _ = tokio::task::spawn_blocking(move || accounts::save(&index)).await;
            },
            |()| Message::AccountIndexSaved,
        )
    }

    fn account_event(&mut self, event: account_panel::Event) -> Task<Message> {
        let Phase::Account { session, .. } = &self.phase else {
            return Task::none();
        };
        let rejected = account_panel::State::is_rejected(&event);
        let (task, action) = self.account_panel.update(event, &session.client);
        if rejected && let Phase::Account { gateway, .. } = &self.phase {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        let task = task.map(Message::Account);
        match action {
            Some(account_panel::Action::Switch(id)) => {
                Task::batch([task, self.handle(Message::SwitchAccount(id))])
            }
            Some(account_panel::Action::Remove(id)) => {
                let store = self.store.clone();
                Task::batch([
                    task,
                    Task::perform(login::delete_saved(store, id), move |result| {
                        Message::AccountRemoved(id, result)
                    }),
                ])
            }
            Some(account_panel::Action::Logout) => {
                Task::batch([task, self.handle(Message::Logout)])
            }
            Some(account_panel::Action::Apps) => {
                Task::batch([task, self.apps_event(apps::Event::OpenAuthorized)])
            }
            Some(account_panel::Action::OpenWeb) => {
                let _ = fastcord_platform::open_url(account_panel::SETTINGS_URL);
                task
            }
            None => task,
        }
    }

    fn start_restore(&mut self, account: Snowflake) -> Task<Message> {
        let Some(store) = self.store.clone() else {
            self.notice = Some(LoginError::Store(StoreError::Unavailable).to_string());
            return Task::none();
        };
        self.clear_token();
        self.notice = None;
        self.phase = Phase::Authenticating {
            saved_account: Some(account),
        };
        Task::perform(login::restore(store, account), Message::LoginFinished)
    }

    fn start_delete(&mut self, account: Snowflake) -> Task<Message> {
        self.reset_login();
        self.phase = Phase::Deleting { account };
        Task::perform(
            login::delete_saved(self.store.clone(), account),
            Message::Deleted,
        )
    }
    fn start_qr(&mut self) -> Task<Message> {
        if fake_mode() {
            self.notice = Some("QR login is not available in fake mode.".to_owned());
            return Task::none();
        }
        self.clear_token();
        self.qr_attempts += 1;
        let id = self.qr_attempts;
        let (task, worker) =
            Task::run(qr::events(), move |event| Message::Qr(id, event)).abortable();
        self.phase = Phase::Qr(QrAttempt::new(id, worker));
        task
    }

    fn configure_ui_sound(&self) {
        let prefs = &self.audio_preferences;
        ui_sound::configure(
            prefs.interface_sounds,
            prefs.interface_volume,
            &prefs.output.choice,
        );
    }

    /// Plays the interface sound for what changed in the call since the last look.
    fn observe_ui_sound(&self) {
        let Phase::Account { gateway, .. } = &self.phase else {
            return;
        };
        let controls = gateway.controls.voice_controls();
        ui_sound::observe(ui_sound::Snap {
            state: gateway.controls.voice_audio_state(),
            self_muted: controls.self_muted,
            deafened: controls.deafened,
            push_to_mute: controls.push_to_mute_down,
            ptt: (controls.effective_mode() == shortcuts::InputMode::PushToTalk)
                .then_some(controls.ptt_down),
            sharing: gateway.controls.share_view().active,
            users: controls.participants.iter().map(|p| p.user_id).collect(),
        });
    }

    fn save_audio_preferences(&mut self) -> Task<Message> {
        self.configure_ui_sound();
        let preferences = self.audio_preferences.clone();
        self.audio_saving = Some(preferences.clone());
        Task::perform(
            audio_devices::save(preferences),
            Message::AudioPreferencesSaved,
        )
    }

    /// Shows the account and starts its Gateway connection. The connection
    /// belongs to the panel: leaving the account screen (logout, rejected
    /// token) drops it, which closes the Gateway session.
    fn enter_account(&mut self, session: Arc<Session>, persistence: Persistence) -> Task<Message> {
        self.gateway_sessions += 1;
        let id = self.gateway_sessions;
        if fake_mode() {
            unverified::enable_all(&session.client);
        }
        let controls = gateway::NavigationBridge::new();
        controls.set_account_id(session.user.id);
        controls.set_volume_settings(voice_settings::load(session.user.id));
        controls.set_audio_devices(&self.audio_preferences);
        controls.set_voice_input(&self.audio_preferences.voice_input);
        self.shortcuts.rebuild(&self.audio_preferences.voice_input);
        let stream = gateway::status_stream(
            session.client.clone(),
            session.token(),
            session.properties.clone(),
            controls.clone(),
        );
        let (task, worker) = Task::run(stream, move |()| Message::Navigation(id)).abortable();
        let attachment_cache =
            crate::attachments::AttachmentCache::default_root(session.user.id).ok();
        self.search = search::State::default();
        self.pins = pins::State::default();
        self.inbox.clear();
        self.notify_settings.clear();
        self.group.clear();
        self.invite.clear();
        self.guild_admin.clear();
        self.announce.clear();
        self.moderation.clear();
        self.apps.clear();
        self.community.clear();
        self.assets.clear();
        self.automod.clear();
        self.discover.clear();
        self.onboarding.clear();
        self.events.clear();
        self.identity.clear();
        self.safety.clear();
        self.account_panel.clear();
        self.report.clear();
        self.gif = gif::State::default();
        self.sticker = sticker::State::default();
        self.forward = forward::State::default();
        self.poll = poll_create::State::default();
        self.slash.reset();
        self.voice.reset();
        self.threads.drop_all();
        self.forum.drop_all();
        self.friends.reset();
        self.member_menu.reset();
        self.status.reset();
        self.phase = Phase::Account {
            session,
            persistence,
            gateway: Box::new(
                GatewayPanel::new(id, worker, controls).with_attachment_cache(attachment_cache),
            ),
        };
        self.sync_viewing();
        task
    }

    /// What the timeline shows counts as read only while the window is active
    /// and drawn.
    fn sync_viewing(&self) {
        if let Phase::Account { gateway, .. } = &self.phase {
            gateway
                .controls
                .set_viewing(self.window_active && self.window_drawable);
        }
    }

    /// The open conversation as the search needs it: (guild, channel).
    fn search_target(gateway: &GatewayPanel) -> Option<(Option<Snowflake>, Snowflake)> {
        let navigation = &gateway.navigation;
        if let Some(private) = &navigation.private_selection {
            return Some((None, private.channel_id));
        }
        let selection = navigation.selection.as_ref()?;
        selection
            .permissions
            .read_history
            .then_some((Some(selection.guild_id), selection.channel_id))
    }

    fn forum_event(&mut self, event: forum::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let (task, opened) =
            self.forum
                .update(event, &session.client, gateway.navigation.forum.as_ref());
        if let Some(thread) = opened
            && let Some(guild) = thread.guild_id
        {
            gateway.controls.open_thread(guild, thread);
        }
        task.map(Message::Forum)
    }

    fn threads_event(&mut self, event: threads::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        match event {
            threads::Event::OpenArchived(index) => {
                if let Some((guild, thread)) = self.threads.archived_thread(index) {
                    self.threads.close();
                    gateway.controls.open_thread(guild, thread);
                }
                Task::none()
            }
            threads::Event::SetState {
                thread,
                archived,
                locked,
            } => {
                // Checked against the current read model right before sending.
                let allowed = gateway.navigation.selection.as_ref().is_some_and(|s| {
                    s.channel_id == thread
                        && s.permissions.manage_threads
                        && s.thread.is_some_and(|t| {
                            archived.is_none_or(|a| a != t.archived)
                                && locked.is_none_or(|l| l != t.locked)
                        })
                });
                self.threads
                    .set_state((thread, archived, locked), allowed, &session.client)
                    .map(Message::Threads)
            }
            threads::Event::Created(Ok(thread)) => {
                self.threads.close();
                if let Some(guild) = thread.guild_id {
                    gateway.controls.open_thread(guild, *thread);
                }
                Task::none()
            }
            event => self
                .threads
                .update(event, &session.client)
                .map(Message::Threads),
        }
    }

    fn gif_event(&mut self, event: gif::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &mut self.phase
        else {
            return Task::none();
        };
        if let gif::Event::Pick(index) = event {
            if let Some(url) = self.gif.url(index).map(str::to_owned)
                && gateway.composer.insert_emoji(&url)
            {
                self.gif.close();
                return iced::widget::operation::focus(composer::EDITOR_ID);
            }
            return Task::none();
        }
        if matches!(event, gif::Event::Toggle) && !self.gif.open && !gateway.timeline.can_send {
            return Task::none();
        }
        self.gif
            .update(event, &session.client, gateway.attachments.as_ref())
            .map(Message::Gif)
    }

    fn assets_event(&mut self, event: assets::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rejected = assets::State::is_rejected(&event);
        let env = assets::Env {
            client: &session.client,
            navigation: &gateway.navigation,
            cache: gateway.attachments.as_ref(),
            output: self.audio_preferences.output.choice.clone(),
        };
        let task = self.assets.update(event, &env);
        if rejected {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Assets)
    }

    /// Lists stickers and sounds again when Discord reports a change.
    fn assets_follow(&mut self) -> Task<Message> {
        let Phase::Account { gateway, .. } = &self.phase else {
            return Task::none();
        };
        let controls = gateway.controls.voice_controls();
        let (stickers, sounds) = (controls.sticker_change, controls.sound_catalog_change);
        let tasks: Vec<_> = self
            .assets
            .new_changes(stickers, sounds)
            .into_iter()
            .map(|event| self.assets_event(event))
            .collect();
        Task::batch(tasks)
    }

    fn apps_event(&mut self, event: apps::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rejected = apps::State::is_rejected(&event);
        let (task, action) = self
            .apps
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        let task = task.map(Message::Apps);
        match action {
            Some(apps::Action::Open(url)) => {
                let _ = fastcord_platform::open_url(&url);
                task
            }
            Some(apps::Action::Copy(text)) => Task::batch([task, iced::clipboard::write(text)]),
            None => task,
        }
    }

    fn announce_event(&mut self, event: announce::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rejected = announce::State::is_rejected(&event);
        let task = self.announce.update(
            event,
            &gateway.navigation,
            &gateway.timeline,
            &session.client,
        );
        if rejected {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Announce)
    }

    fn forward_event(&mut self, event: forward::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let candidates = forward_candidates(gateway);
        self.forward
            .update(event, &candidates, &session.client)
            .map(Message::Forward)
    }

    /// The scope whose application commands the open conversation can use.
    fn slash_scope(gateway: &gateway::GatewayPanel, channel: Snowflake) -> IndexScope {
        match gateway.navigation.selected_guild {
            Some(guild) if gateway.navigation.selection.is_some() => IndexScope::Guild(guild),
            _ => IndexScope::Channel(channel),
        }
    }

    /// The draft changed: a leading slash loads the command list.
    fn slash_typed(&mut self) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        let env = slash::Env {
            channel,
            scope: Self::slash_scope(gateway, channel),
            can_send: gateway.timeline.can_send,
            client: &session.client,
            bridge: &gateway.controls,
            interaction: &gateway.timeline.interaction,
            now: std::time::Instant::now(),
        };
        self.slash
            .typed(gateway.composer.slash_query().is_some(), &env)
            .map(Message::Slash)
    }

    fn slash_event(&mut self, event: slash::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &mut self.phase
        else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        let env = slash::Env {
            channel,
            scope: Self::slash_scope(gateway, channel),
            can_send: gateway.timeline.can_send,
            client: &session.client,
            bridge: &gateway.controls,
            interaction: &gateway.timeline.interaction,
            now: std::time::Instant::now(),
        };
        let rejected = slash::State::is_rejected(&event);
        let (task, used) = self.slash.update(event, &env);
        if used {
            gateway.composer.clear_text();
        }
        if rejected {
            // Discord rejected the login: stop the account like any other 401.
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Slash)
    }

    fn poll_event(&mut self, event: poll_create::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        let context = poll_create::Context {
            channel,
            can_send: gateway.timeline.can_send,
        };
        self.poll
            .update(event, context, &session.client)
            .map(Message::Poll)
    }

    /// The voice-message panel. A recording the user sent goes through the ordinary outbox as a
    /// text-free message with one file, so its upload, progress, cancel, retry and ambiguity
    /// handling are those of any attachment.
    fn voice_event(&mut self, event: voice_record::Event) -> Task<Message> {
        let Phase::Account { gateway, .. } = &self.phase else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        let context = self.voice_context(gateway, channel);
        let (task, file) = self.voice.update(event, &context);
        if let Some(file) = file {
            if gateway.controls.send_message_with_files(
                channel,
                String::new(),
                vec![file],
                false,
                None,
            ) {
                self.voice.sent();
            } else {
                self.notice = Some("The message could not be queued. Try again.".to_owned());
            }
        }
        task.map(Message::Voice)
    }

    fn voice_context(
        &self,
        gateway: &gateway::GatewayPanel,
        channel: Snowflake,
    ) -> voice_record::Context {
        voice_record::Context {
            channel,
            can_send: gateway.timeline.can_send,
            enabled: self.audio_preferences.voice_recording,
            input: self.audio_preferences.input.choice.clone(),
            output: self.audio_preferences.output.choice.clone(),
            noise_suppression: self.audio_preferences.noise_suppression,
        }
    }

    /// Microphone gain and checks in the audio settings. A gain change applies to the call, a
    /// running check and the saved preference at once; the file is written when the slider is
    /// released.
    fn mic_event(&mut self, event: mic_panel::Event) -> Task<Message> {
        use mic_panel::Event;
        let gain = self.audio_preferences.input_gain;
        let changed = match event {
            Event::Gain(db) => Some((
                fastcord_audio::InputGain {
                    manual_db: db,
                    ..gain
                }
                .clamped(),
                false,
            )),
            Event::Automatic(on) => Some((
                fastcord_audio::InputGain {
                    automatic: on,
                    ..gain
                },
                true,
            )),
            Event::GainReleased => {
                return if self.audio_saving.is_none() {
                    self.save_audio_preferences()
                } else {
                    Task::none()
                };
            }
            Event::Tick => {
                let call = match &self.phase {
                    Phase::Account { gateway, .. } => {
                        gateway.controls.voice_controls().input_diagnostics
                    }
                    _ => None,
                };
                self.mic_panel.tick(call);
                return Task::none();
            }
            _ => None,
        };
        if let Some((gain, save)) = changed {
            self.audio_preferences.input_gain = gain;
            if let Phase::Account { gateway, .. } = &self.phase {
                gateway.controls.select_input_gain(gain);
            }
            self.mic_panel.set_gain(gain);
            return if save && self.audio_saving.is_none() {
                self.save_audio_preferences()
            } else {
                Task::none()
            };
        }
        let context = mic_panel::Context {
            input: self.audio_preferences.input.choice.clone(),
            output: self.audio_preferences.output.choice.clone(),
            noise_suppression: self.audio_preferences.noise_suppression,
            gain,
            in_call: self.call_active(),
        };
        self.mic_panel
            .update(event, &context)
            .map(Message::MicPanel)
    }

    /// The environment a soundboard update borrows: the account, the call and the output.
    fn soundboard_env(
        phase: &Phase,
        output: fastcord_audio::DeviceChoice,
    ) -> Option<soundboard::Env<'_>> {
        let Phase::Account {
            session, gateway, ..
        } = phase
        else {
            return None;
        };
        let controls = gateway.controls.voice_controls();
        let in_call = gateway.controls.voice_audio_state() != gateway::VoiceAudioState::Idle;
        Some(soundboard::Env {
            client: &session.client,
            cache: gateway.attachments.as_ref(),
            output,
            deafened: controls.deafened || controls.server_deafened,
            voice: gateway.controls.voice_target().filter(|_| in_call),
        })
    }

    /// Saves the soundboard choices when an update changed them.
    fn save_soundboard_prefs(&mut self) -> Task<Message> {
        match self.soundboard.take_dirty_prefs() {
            Some(prefs) => {
                self.audio_preferences.soundboard = prefs;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
                Task::none()
            }
            None => Task::none(),
        }
    }

    /// Follows the call after each gateway notification: sounds end with the call or a
    /// deafen, the panel follows its channel, and new plays and catalog changes arrive.
    fn soundboard_follow(&mut self) -> Task<Message> {
        let output = self.audio_preferences.output.choice.clone();
        let Some(env) = Self::soundboard_env(&self.phase, output) else {
            return Task::none();
        };
        let Phase::Account { gateway, .. } = &self.phase else {
            return Task::none();
        };
        let controls = gateway.controls.voice_controls();
        self.soundboard.sync_call(env.voice, env.deafened);
        let mut tasks = Vec::new();
        for cue in &controls.sound_cues {
            tasks.push(self.soundboard.update(soundboard::Event::Cue(*cue), &env));
        }
        if let Some(guild) = self
            .soundboard
            .new_catalog_change(controls.sound_catalog_change)
        {
            tasks.push(
                self.soundboard
                    .update(soundboard::Event::CatalogChanged(guild), &env),
            );
        }
        let saved = self.save_soundboard_prefs();
        Task::batch(
            tasks
                .into_iter()
                .map(|task| task.map(Message::Soundboard))
                .chain([saved]),
        )
    }

    fn stage_event(&mut self, event: stage::Event) -> Task<Message> {
        use stage::Event;
        let Phase::Account {
            gateway, session, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let stage = gateway.controls.voice_controls().stage;
        let action = match event {
            Event::Done(serial, result) => {
                self.stage.finish(serial, result);
                return Task::none();
            }
            Event::OpenTopic(editing) => {
                let draft = if editing {
                    stage.topic.clone().unwrap_or_default()
                } else {
                    String::new()
                };
                self.stage.topic = Some((editing, draft));
                return Task::none();
            }
            Event::Topic(value) => {
                if let Some((_, draft)) = &mut self.stage.topic {
                    *draft = value
                        .chars()
                        .take(fastcord_discord::MAX_TOPIC_CHARS)
                        .collect();
                }
                return Task::none();
            }
            Event::CloseTopic => {
                self.stage.topic = None;
                return Task::none();
            }
            Event::SubmitTopic => match self.stage.submission() {
                Some(action) => action,
                None => return Task::none(),
            },
            Event::Do(action) => action,
        };
        self.member_menu.close();
        let (Some(guild), Some(channel)) = (stage.guild, stage.channel) else {
            return Task::none();
        };
        if session.client.writes_restricted() {
            return Task::none();
        }
        let Some(serial) = self.stage.begin() else {
            return Task::none();
        };
        let bridge = gateway.controls.clone();
        Task::perform(
            async move { bridge.stage(guild, channel, action).await },
            move |result| Message::Stage(Event::Done(serial, result)),
        )
    }

    fn soundboard_event(&mut self, event: soundboard::Event) -> Task<Message> {
        let output = self.audio_preferences.output.choice.clone();
        let Some(env) = Self::soundboard_env(&self.phase, output) else {
            return Task::none();
        };
        let rejected = soundboard::State::is_rejected(&event);
        let task = self.soundboard.update(event, &env);
        if rejected && let Phase::Account { gateway, .. } = &self.phase {
            // Discord rejected the login: stop the account like any other 401.
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        let saved = self.save_soundboard_prefs();
        Task::batch([task.map(Message::Soundboard), saved])
    }

    fn sticker_event(&mut self, event: sticker::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        let context = sticker::Context {
            guild: gateway
                .navigation
                .selected_guild
                .filter(|_| !gateway.directs),
            channel,
            can_send: gateway.timeline.can_send,
        };
        self.sticker
            .update(
                event,
                context,
                &session.client,
                gateway.attachments.as_ref(),
            )
            .map(Message::Sticker)
    }

    fn group_event(&mut self, event: group_dm::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rejected = group_dm::State::is_rejected(&event);
        let task = self
            .group
            .update(event, &gateway.navigation, session.user.id, &session.client);
        if rejected {
            // Discord rejected the login: stop the account like any other 401.
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Group)
    }

    fn onboarding_event(&mut self, event: onboarding::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        match event {
            onboarding::Event::Rich(event) => return self.handle(Message::Timeline(event)),
            onboarding::Event::GoChannel(guild, channel) => {
                // The guide never opens a channel the account cannot see.
                if onboarding::visible_channel(&gateway.navigation, channel) {
                    gateway.controls.select_channel(guild, channel);
                }
                return Task::none();
            }
            _ => {}
        }
        let rejected = onboarding::State::is_rejected(&event);
        let task = self
            .onboarding
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Onboarding)
    }

    fn events_event(&mut self, event: scheduled_events::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        if let scheduled_events::Event::Join(guild, channel) = event {
            // Only an event that is still held and not over, in a channel the
            // account may connect to, joins; the join path checks the channel again.
            if scheduled_events::joinable(&gateway.navigation, guild, channel) {
                return self.handle(Message::JoinVoice(guild, channel));
            }
            return Task::none();
        }
        let rejected = scheduled_events::State::is_rejected(&event);
        let task = self.events.update(
            event,
            &gateway.navigation,
            &session.client,
            &gateway.controls,
        );
        if rejected {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Events)
    }

    fn discover_event(&mut self, event: discover::Event) -> Task<Message> {
        let Phase::Account { session, .. } = &self.phase else {
            return Task::none();
        };
        let rejected = discover::State::is_rejected(&event);
        let task = self.discover.update(event, &session.client);
        if rejected && let Phase::Account { gateway, .. } = &self.phase {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Discover)
    }

    fn invite_event(&mut self, event: invite::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rejected = invite::State::is_rejected(&event);
        let task = self.invite.update(
            event,
            &gateway.navigation,
            &gateway.timeline.rows,
            &session.client,
        );
        if rejected {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Invite)
    }

    fn community_event(&mut self, event: community::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rejected = community::State::is_rejected(&event);
        let task =
            self.community
                .update(event, &gateway.navigation, &session.client, session.user.id);
        if rejected {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Community)
    }

    fn guild_admin_event(&mut self, event: guild_admin::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        match event {
            guild_admin::Event::Apps(guild) => {
                return self.apps_event(apps::Event::OpenGuild(guild));
            }
            guild_admin::Event::Webhooks(guild, channel) => {
                return self.apps_event(apps::Event::OpenWebhooks(guild, channel));
            }
            _ => {}
        }
        if let guild_admin::Event::Assets(guild) = event {
            // The menu hands over to the emoji, sticker and sound panel.
            let _ = self
                .guild_admin
                .update(event, &gateway.navigation, &session.client);
            return self.assets_event(assets::Event::Open(guild));
        }
        if let guild_admin::Event::Followed(guild, channel) = event {
            // The channel panel hands over to the followed-announcements list.
            return self.announce_event(announce::Event::OpenFollowers(guild, channel));
        }
        if let guild_admin::Event::Community(guild) = event {
            // The settings hand over to the community setup panel.
            let _ = self.guild_admin.update(
                guild_admin::Event::Close,
                &gateway.navigation,
                &session.client,
            );
            return self.community_event(community::Event::Open(guild));
        }
        if let guild_admin::Event::Leave(guild) = event {
            // The menu hands over to the leave confirmation of the invite module.
            let _ = self
                .guild_admin
                .update(event, &gateway.navigation, &session.client);
            return self.invite_event(invite::Event::AskLeave(guild));
        }
        if let guild_admin::Event::AutoMod(guild) = event {
            // The menu hands over to the AutoMod panel.
            let _ = self
                .guild_admin
                .update(event, &gateway.navigation, &session.client);
            return self.automod_event(automod::Event::Open(guild));
        }
        if let guild_admin::Event::Bans(guild) | guild_admin::Event::Audit(guild) = event {
            // The menu hands over to the moderation panels.
            let audit = matches!(event, guild_admin::Event::Audit(_));
            let _ = self
                .guild_admin
                .update(event, &gateway.navigation, &session.client);
            return self.moderation_event(if audit {
                moderation::Event::Audit(guild)
            } else {
                moderation::Event::Bans(guild)
            });
        }
        let rejected = guild_admin::State::is_rejected(&event);
        let task = self
            .guild_admin
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::GuildAdmin)
    }

    fn moderation_event(&mut self, event: moderation::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rejected = moderation::State::is_rejected(&event);
        let task = self
            .moderation
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Moderation)
    }

    fn automod_event(&mut self, event: automod::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rejected = automod::State::is_rejected(&event);
        let task = self
            .automod
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            let id = gateway.id;
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::AutoMod)
    }

    fn identity_event(&mut self, event: profile_edit::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rejected = profile_edit::State::is_rejected(&event);
        let task = self.identity.update(
            event,
            &gateway.navigation,
            own_user(gateway, session),
            &session.client,
        );
        if rejected {
            let id = gateway.id;
            // Discord rejected the login: stop the account like any other 401.
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Identity)
    }

    fn member_menu_event(&mut self, event: member_menu::Event) -> Task<Message> {
        use member_menu::Event;
        let Phase::Account { gateway, .. } = &self.phase else {
            return Task::none();
        };
        match event {
            Event::Open(user, point) => {
                self.member_menu.target = Some(member_menu::Target {
                    user,
                    point,
                    guild: (!gateway.directs)
                        .then_some(gateway.navigation.selected_guild)
                        .flatten(),
                    confirm: None,
                    sub: None,
                    reason: String::new(),
                    nick: String::new(),
                    ban_window: 0,
                });
                self.member_menu.notice = None;
            }
            Event::Close => self.member_menu.close(),
            Event::Done(serial, result) => {
                if self.member_menu.pending == Some(serial) {
                    self.member_menu.pending = None;
                    self.member_menu.notice =
                        Some(result.map_or_else(moderation::failure, |()| "Done.".into()));
                    if let Some(target) = &mut self.member_menu.target {
                        target.confirm = None;
                        target.sub = None;
                        target.reason.clear();
                    }
                }
            }
            Event::Sub(sub) => {
                if let Some(target) = &mut self.member_menu.target {
                    target.sub = sub;
                    target.confirm = None;
                    if sub == Some(member_menu::Sub::Nick) {
                        // Start from the nickname the member shows now.
                        target.nick = gateway
                            .navigation
                            .members
                            .get(&target.user.id)
                            .and_then(|m| m.nick.clone())
                            .unwrap_or_default();
                    }
                }
            }
            Event::Reason(text) => {
                if let Some(target) = &mut self.member_menu.target {
                    target.reason = fastcord_discord::cut_reason(text);
                }
            }
            Event::Nick(text) => {
                if let Some(target) = &mut self.member_menu.target {
                    target.nick = text
                        .chars()
                        .take(fastcord_discord::MAX_NICK_CHARS)
                        .collect();
                }
            }
            Event::BanWindow(seconds) => {
                if let Some(target) = &mut self.member_menu.target {
                    target.ban_window = seconds;
                }
            }
            Event::Select(action) => {
                if self.member_menu.pending.is_some() {
                    return Task::none();
                }
                let Some(target) = &mut self.member_menu.target else {
                    return Task::none();
                };
                if action.needs_confirm() {
                    target.confirm = Some(action);
                    return Task::none();
                }
                return self.begin_member_action(action);
            }
            Event::Confirm => {
                if let Some(action) = self
                    .member_menu
                    .target
                    .as_ref()
                    .and_then(|target| target.confirm.clone())
                {
                    return self.begin_member_action(action);
                }
            }
        }
        Task::none()
    }

    fn begin_member_action(&mut self, action: fastcord_discord::MemberAction) -> Task<Message> {
        use fastcord_discord::MemberAction;
        let Phase::Account {
            gateway, session, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let Some(target) = &self.member_menu.target else {
            return Task::none();
        };
        let Some(guild) = target
            .guild
            .filter(|id| Some(*id) == gateway.navigation.selected_guild)
        else {
            return Task::none();
        };
        if self.member_menu.pending.is_some() || session.client.writes_restricted() {
            return Task::none();
        }
        let own = gateway
            .navigation
            .current_user
            .as_ref()
            .is_some_and(|me| me.id == target.user.id);
        let allowed = gateway
            .navigation
            .members
            .get(&target.user.id)
            .is_some_and(|member| member.moderation.allows(&action))
            && match &action {
                MemberAction::OwnNick(_) => own,
                MemberAction::Move(dest) => gateway
                    .navigation
                    .move_targets
                    .iter()
                    .any(|(id, _)| id == dest),
                _ => !own,
            };
        if !allowed {
            self.member_menu.notice =
                Some("This action is no longer available for this member.".into());
            return Task::none();
        }
        let reason = Some(target.reason.trim().to_owned()).filter(|r| !r.is_empty());
        let (bridge, user) = (gateway.controls.clone(), target.user.id);
        self.member_menu.serial = self.member_menu.serial.wrapping_add(1);
        let serial = self.member_menu.serial;
        self.member_menu.pending = Some(serial);
        self.member_menu.notice = None;
        Task::perform(
            async move { bridge.moderate(guild, user, action, reason).await },
            move |result| Message::MemberMenu(member_menu::Event::Done(serial, result)),
        )
    }

    fn report_event(&mut self, event: report::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        // Ignore and Block after a report are their own requests.
        let follow = match &event {
            report::Event::Follow(action, user) => Some((*action, *user)),
            _ => None,
        };
        if matches!(event, report::Event::OpenServer) {
            let Some(guild) = gateway
                .navigation
                .selected_guild
                .filter(|_| !gateway.directs)
            else {
                return Task::none();
            };
            let subject = report::Subject::new(
                fastcord_discord::ReportTarget::Server(guild),
                "server",
                &gateway.navigation.selected_guild_name,
            );
            return self
                .report
                .begin(subject, &session.client)
                .map(Message::Report);
        }
        // The review is judged against what the timeline holds right now.
        self.report
            .sync(self.report.watched().and_then(|(channel, message)| {
                timeline::is_retained(&gateway.timeline, channel, message)
            }));
        let rejected = report::State::is_rejected(&event);
        let mut task = self
            .report
            .update(event, &session.client)
            .map(Message::Report);
        if let Some((action, user)) = follow {
            task = Task::batch([
                task,
                self.friends
                    .update(friends::Event::Act(user, action), &session.client)
                    .map(Message::Friends),
            ]);
        }
        if rejected {
            let id = gateway.id;
            // Discord rejected the login: stop the account like any other 401.
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task
    }

    fn safety_event(&mut self, event: safety::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rejected = safety::State::is_rejected(&event);
        let task = self
            .safety
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            let id = gateway.id;
            // Discord rejected the login: stop the account like any other 401.
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Safety)
    }

    /// Events of the quick switcher and the shortcuts help.
    fn switcher_event(&mut self, event: switcher::Event) -> Task<Message> {
        // Enter on a pasted Discord link opens it instead of a search result.
        if matches!(event, switcher::Event::Submit)
            && let Some(link) = self.switcher.wanted().and_then(desktop::parse)
        {
            self.switcher.close();
            return self.open_link(link);
        }
        let Phase::Account { gateway, .. } = &mut self.phase else {
            return Task::none();
        };
        match self.switcher.update(event, &gateway.navigation.quick) {
            switcher::Action::None => Task::none(),
            switcher::Action::Open(entry) => {
                // Exactly a sidebar click: the conversation opens (a voice
                // channel's chat, never its call) and its draft comes back.
                self.threads.close();
                self.friends.close();
                if self.inbox.is_open() {
                    self.inbox.clear();
                    gateway.controls.inbox_rows(self.inbox.rows_wanted());
                }
                gateway.directs = entry.guild.is_none();
                gateway.controls.open_channel(entry.channel);
                self.nav_pending = true;
                iced::widget::operation::focus(composer::EDITOR_ID)
            }
        }
    }

    /// A navigation chord (see [`switcher`]).
    fn navigation_shortcut(&mut self, shortcut: switcher::Shortcut) -> Task<Message> {
        use switcher::Shortcut;
        let Phase::Account { gateway, .. } = &mut self.phase else {
            return Task::none();
        };
        let composer = || iced::widget::operation::focus(composer::EDITOR_ID);
        // While a voice binding is being recorded the chords belong to it.
        if self.shortcuts.recording.is_some() && !matches!(shortcut, Shortcut::Escape { .. }) {
            return Task::none();
        }
        match shortcut {
            Shortcut::Switcher => {
                self.switcher.toggle();
                if self.switcher.is_open() {
                    iced::widget::operation::focus(switcher::INPUT_ID)
                } else {
                    composer()
                }
            }
            Shortcut::Help => {
                self.switcher.toggle_help();
                composer()
            }
            Shortcut::Move(delta) => {
                if self.switcher.is_open() {
                    self.switcher
                        .update(switcher::Event::Move(delta), &gateway.navigation.quick);
                }
                Task::none()
            }
            Shortcut::Step(step) => {
                if self.switcher.is_open() || self.switcher.help_open() {
                    return Task::none();
                }
                self.threads.close();
                self.friends.close();
                if self.inbox.is_open() {
                    self.inbox.clear();
                    gateway.controls.inbox_rows(self.inbox.rows_wanted());
                }
                if step.scope == fastcord_discord::state::navigation::StepScope::Server {
                    gateway.directs = false;
                }
                gateway.controls.step(step);
                self.nav_pending = true;
                Task::none()
            }
            Shortcut::Escape { captured } => {
                if self.member_menu.target.is_some() {
                    self.member_menu.close();
                    return Task::none();
                }
                if self.switcher.close() {
                    return composer();
                }
                if gateway.attachment_viewer.take().is_some() {
                    return composer();
                }
                if gateway.emoji_picker.request.open {
                    gateway.emoji_picker.close();
                    return composer();
                }
                // A focused composer handled Escape itself.
                if !captured {
                    if gateway.composer.editing().is_some() {
                        gateway.composer.cancel_edit();
                        return composer();
                    }
                    if gateway.composer.replying().is_some() {
                        gateway.composer.cancel_reply();
                        return composer();
                    }
                }
                Task::none()
            }
        }
    }

    /// A click on a folder, category or the hide-muted choice: kept locally for
    /// this account and applied by the next navigation snapshot.
    fn organize_event(&mut self, event: organize::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        self.audio_preferences
            .organize
            .apply(session.user.id.0, event);
        organize::publish(&self.audio_preferences.organize);
        gateway.controls.layout_changed();
        if self.audio_saving.is_none() {
            return self.save_audio_preferences();
        }
        Task::none()
    }

    fn notify_settings_event(&mut self, event: notify_settings::Event) -> Task<Message> {
        if let notify_settings::Event::HideMuted(guild) = event {
            return self.organize_event(organize::Event::HideMuted(guild));
        }
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rejected = matches!(
            &event,
            notify_settings::Event::Done(Err(RestError::AuthenticationRequired))
        );
        let task =
            self.notify_settings
                .update(event, &gateway.navigation.notifications, &session.client);
        if rejected {
            return self.handle(Message::Gateway(
                gateway.id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::NotifySettings)
    }

    fn inbox_event(&mut self, event: inbox::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let rows = &gateway.navigation.inbox.rows;
        match event {
            inbox::Event::MarkRead(index) => {
                if let Some(row) = rows.get(index) {
                    gateway.controls.mark_read(row.guild, row.channel);
                }
                return Task::none();
            }
            inbox::Event::MarkAllRead => {
                let all = rows.iter().map(|row| (row.guild, row.channel)).collect();
                gateway.controls.mark_read_many(all);
                return Task::none();
            }
            _ => {}
        }
        let rejected = matches!(
            &event,
            inbox::Event::Loaded(_, Err(RestError::AuthenticationRequired))
                | inbox::Event::Muted(_, Err(RestError::AuthenticationRequired))
        );
        let (task, action) = self
            .inbox
            .update(event, &gateway.navigation.inbox, &session.client);
        gateway.controls.inbox_rows(self.inbox.rows_wanted());
        if let inbox::Action::Open(channel, target) = action {
            if gateway.timeline.channel_id == Some(channel) {
                inbox_jump(&gateway.controls, channel, target);
            } else {
                // Only a conversation the account can open is selected;
                // otherwise the wait ends with a notice.
                self.inbox.set_pending(channel, target);
                gateway.controls.open_channel(channel);
            }
        }
        if rejected {
            // Discord rejected the login: stop the account like any other 401.
            return self.handle(Message::Gateway(
                gateway.id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Inbox)
    }

    fn pins_event(&mut self, event: pins::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        if let pins::Event::Jump(index) = event {
            if self.pins.shows(channel)
                && let Some(pin) = self.pins.pin(index)
            {
                gateway.controls.timeline_intent(history::Intent::JumpTo {
                    channel,
                    message: pin.message,
                });
            }
            return Task::none();
        }
        self.pins
            .update(event, &session.client, channel)
            .map(Message::Pins)
    }

    fn search_event(&mut self, event: search::Event) -> Task<Message> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return Task::none();
        };
        if let search::Event::Jump(index) = event {
            if let Some(hit) = self.search.hit(index).cloned() {
                if gateway.timeline.channel_id == Some(hit.channel) {
                    gateway.controls.timeline_intent(history::Intent::JumpTo {
                        channel: hit.channel,
                        message: hit.message,
                    });
                } else {
                    self.search.set_pending_jump(hit.channel, hit.message);
                    match hit.guild {
                        Some(guild) => gateway.controls.select_channel(guild, hit.channel),
                        None => gateway.controls.select_private_channel(hit.channel),
                    }
                }
            }
            return Task::none();
        }
        let Some((guild, channel)) = Self::search_target(gateway) else {
            return Task::none();
        };
        let context = search::Context {
            client: &session.client,
            account: session.user.id,
            guild,
            channel,
        };
        self.search.update(event, &context).map(Message::Search)
    }

    fn view(&self) -> Element<'_, Message> {
        let body = self.view_body();
        #[cfg(feature = "fake-discord")]
        if let Some(banner) = fake::banner() {
            return column![banner, body].into();
        }
        body
    }

    fn view_body(&self) -> Element<'_, Message> {
        if self.compact_call
            && let Phase::Account { gateway, .. } = &self.phase
        {
            return desktop::compact_view(&gateway.controls.voice_controls());
        }
        if self.chat_settings_open {
            return center(scrollable(
                column![chat_prefs::view(
                    self.audio_preferences.chat,
                    &chat_prefs::SpeechView {
                        prefs: &self.audio_preferences.speech,
                        voices: &self.speech_voices,
                        error: speech::error(),
                        speaking: speech::speaking(),
                    },
                    unverified::view(
                        match &self.phase {
                            Phase::Account { session, .. } => Some(&session.client),
                            _ => None,
                        },
                        self.unverified_open,
                    ),
                )]
                .padding(12),
            ))
            .into();
        }
        if self.audio_settings.open {
            let (input_error, output_error, noise_status, audio_stats, vad_forbidden, transmit) =
                match &self.phase {
                    Phase::Account { gateway, .. } => {
                        let controls = gateway.controls.voice_controls();
                        (
                            controls.input_error,
                            controls.output_error,
                            controls.noise_status,
                            controls.audio_stats,
                            controls.vad_forbidden,
                            audio_devices::transmit_summary(
                                controls.audio_stats,
                                controls.transport_stats,
                                controls.capture_enabled(),
                            ),
                        )
                    }
                    _ => (None, None, None, None, false, String::new()),
                };
            // The dialog is taller than the default window: scroll instead of hiding Close.
            return center(scrollable(
                column![
                    audio_devices::view(
                        &self.audio_settings,
                        &self.audio_preferences,
                        input_error,
                        output_error,
                        noise_status,
                        audio_stats,
                        transmit,
                        column![
                            shortcuts::view(
                                &self.audio_preferences.voice_input,
                                &self.shortcuts,
                                vad_forbidden,
                                self.call_active(),
                            ),
                            desktop::view(
                                self.audio_preferences.close_to_tray,
                                &self.tray,
                                self.autostart,
                            ),
                            mic_panel::view(
                                &self.mic_panel,
                                self.audio_preferences.input_gain,
                                self.call_active(),
                                (self.audio_preferences.voice_input.mode
                                    == shortcuts::InputMode::VoiceActivity)
                                    .then_some(self.audio_preferences.voice_input.sensitivity)
                                    .flatten(),
                            ),
                        ]
                        .spacing(12)
                        .into(),
                    ),
                    text(self.notice.as_deref().unwrap_or("")),
                ]
                .spacing(12)
                .padding(12),
            ))
            .into();
        }
        if let Phase::Account {
            session,
            persistence,
            gateway,
        } = &self.phase
        {
            let voice_audio = gateway.controls.voice_audio_state();
            let voice_controls = gateway.controls.voice_controls();
            let search_bar: Element<'_, Message> = match Self::search_target(gateway) {
                Some((guild, _)) => {
                    search::view(&self.search, guild.is_some()).map(Message::Search)
                }
                None => text("").size(1).into(),
            };
            let watching = gateway.controls.watched();
            let video_tiles = call_layout::tiles(
                &watching,
                &gateway.controls.camera_tiles().snapshot(),
                voice_controls.camera.wanted,
            );
            let video_plan = gateway.call_view.plan(
                &call_layout::ids(&video_tiles),
                &call_ui::speaking(&voice_controls),
            );
            let in_voice = voice_audio != gateway::VoiceAudioState::Idle;
            let running = voice_audio == gateway::VoiceAudioState::Running;
            let bold = iced::Font {
                weight: iced::font::Weight::Bold,
                ..iced::Font::DEFAULT
            };

            // ---- the voice panel and its settings, above the account footer ----
            let share = gateway.controls.share_view();
            let voice_panel: Option<Element<'_, Message>> = in_voice.then(|| {
                let place = match (gateway.voice_guild, gateway.voice_call) {
                    (Some(_), _) => {
                        let channel = gateway.controls.voice_target().map(|(_, channel)| channel);
                        let name = gateway
                            .navigation
                            .channels
                            .rows
                            .iter()
                            .find(|row| Some(row.id) == channel && row.occupant.is_none())
                            .map(|row| row.name.as_str());
                        match (name, gateway.navigation.selected_guild_name.as_str()) {
                            (Some(name), "") => name.to_owned(),
                            (Some(name), guild) => format!("{name} / {guild}"),
                            (None, _) => "Voice channel".to_owned(),
                        }
                    }
                    (None, Some(call)) => gateway
                        .navigation
                        .private_channels
                        .rows
                        .iter()
                        .find(|row| row.id == call)
                        .map_or_else(|| "Private call".to_owned(), |row| row.label.clone()),
                    (None, None) => String::new(),
                };
                call_ui::bar(call_ui::Bar {
                    state: voice_audio,
                    place,
                    camera: gateway::can_send_camera().then_some((
                        voice_controls.camera.wanted,
                        running || voice_controls.camera.wanted,
                    )),
                    share: cfg!(windows).then(|| {
                        if share.active {
                            (true, true)
                        } else {
                            (
                                false,
                                running
                                    && (gateway.voice_guild.is_some()
                                        || gateway.voice_call.is_some())
                                    && !gateway.share_picker.is_open(),
                            )
                        }
                    }),
                    soundboard: self.soundboard_toggle(gateway),
                    settings_open: self.voice_details,
                    can_leave: gateway.voice_guild.or(gateway.voice_call).is_some(),
                    share_text: share.text.clone(),
                })
            });
            let voice_settings: Option<Element<'_, Message>> = (in_voice && self.voice_details)
                .then(|| {
                    let saved = &self.audio_preferences.camera;
                    let camera = &voice_controls.camera;
                    call_ui::settings(call_ui::Settings {
                        camera: gateway::can_send_camera().then(|| call_ui::CameraSettings {
                            choices: self.camera_list.choices(saved),
                            selected: self.camera_list.selected(saved),
                            effect: saved.effect,
                            mirror: saved.mirror,
                            probing: self.camera_list.probing,
                            status: match &self.camera_list.error {
                                Some(error) if camera.status == gateway::CameraStatus::Off => {
                                    error.clone()
                                }
                                _ => gateway::describe_camera(camera),
                            },
                        }),
                        voice: &voice_controls,
                        watching: watching
                            .iter()
                            .map(|session| {
                                let owner = call_layout::stream_owner(&session.key).map_or_else(
                                    || "stream".to_owned(),
                                    |owner| voice_controls.display_name(owner),
                                );
                                (
                                    session.clone(),
                                    format!("{owner}: {}", session.description()),
                                )
                            })
                            .collect(),
                        offers: voice_controls
                            .streams
                            .iter()
                            .filter(|offer| {
                                !watching.iter().any(|session| session.key == offer.key)
                            })
                            .map(|offer| {
                                (offer.key.clone(), voice_controls.display_name(offer.owner))
                            })
                            .collect(),
                        running,
                    })
                });
            let mut talking = call_ui::speaking(&voice_controls);
            if voice_controls.self_speaking {
                talking.push(session.user.id);
            }

            // ---- connection and notices above the conversation ----
            let mut notes: Vec<(String, bool)> = Vec::new();
            let mut say = |line: String, danger: bool| {
                if !line.is_empty() {
                    notes.push((line, danger));
                }
            };
            if !matches!(gateway.status, gateway::GatewayStatus::Ready(_)) {
                say(
                    gateway.status.describe(),
                    matches!(gateway.status, gateway::GatewayStatus::Stopped(_)),
                );
            }
            if let Some(error) = voice_controls.input_error {
                say(
                    format!("Microphone stopped: {error}. Open Audio devices to retry."),
                    true,
                );
            }
            if let Some(error) = voice_controls.output_error {
                say(
                    format!(
                        "Playback stopped: {error}. Explicit devices never fall back; open Audio devices to retry."
                    ),
                    true,
                );
            }
            say(self.notice.clone().unwrap_or_default(), false);
            say(gateway.attachment_notice.unwrap_or("").to_owned(), false);
            let has_search = !search_bar_is_empty(gateway);
            let mut banner_column = column![].spacing(2);
            if has_search {
                banner_column = banner_column.push(search_bar);
            }
            if let Some(channel) = gateway.timeline.channel_id
                && self.pins.shows(channel)
            {
                banner_column = banner_column.push(pins::view(&self.pins).map(Message::Pins));
            }
            if let Some(panel) = gif::view(&self.gif) {
                banner_column = banner_column.push(panel.map(Message::Gif));
            }
            if let Some(panel) = soundboard::view(&self.soundboard) {
                banner_column = banner_column.push(panel.map(Message::Soundboard));
            }
            if let Some(panel) = stage::banner(
                &self.stage,
                &voice_controls,
                session.client.writes_restricted(),
            ) {
                banner_column = banner_column.push(panel);
            }
            if let Some(channel) = gateway.timeline.channel_id
                && self.sticker.is_open_for(channel)
                && let Some(panel) = sticker::view(&self.sticker)
            {
                banner_column = banner_column.push(panel.map(Message::Sticker));
            }
            if let Some(channel) = gateway.timeline.channel_id
                && let Some(panel) = slash::view(
                    &self.slash,
                    gateway
                        .composer
                        .slash_query()
                        .filter(|_| gateway.timeline.can_send)
                        .as_deref(),
                    Self::slash_scope(gateway, channel),
                    &gateway.timeline.interaction,
                    std::time::Instant::now(),
                )
            {
                banner_column = banner_column.push(panel.map(Message::Slash));
            }
            if let Some(channel) = gateway.timeline.channel_id
                && self.poll.is_open_for(channel)
                && let Some(panel) = poll_create::view(&self.poll)
            {
                banner_column = banner_column.push(panel.map(Message::Poll));
            }
            if let Some(channel) = gateway.timeline.channel_id
                && let Some(panel) =
                    voice_record::view(&self.voice, &self.voice_context(gateway, channel))
            {
                banner_column = banner_column.push(panel.map(Message::Voice));
            }
            if let Some(panel) =
                onboarding::view(&self.onboarding, &gateway.navigation, &gateway.revealed)
            {
                banner_column = banner_column.push(panel.map(Message::Onboarding));
            }
            if let Some(panel) = scheduled_events::view(&self.events, &gateway.navigation) {
                banner_column = banner_column.push(panel.map(Message::Events));
            }
            if let Some(panel) = guild_admin::view(&self.guild_admin, &gateway.navigation) {
                banner_column = banner_column.push(panel.map(Message::GuildAdmin));
            }
            if let Some(panel) = community::view(&self.community, &gateway.navigation) {
                banner_column = banner_column.push(panel.map(Message::Community));
            }
            if let Some(panel) = announce::view(&self.announce, &gateway.navigation) {
                banner_column = banner_column.push(panel.map(Message::Announce));
            }
            if let Some(panel) = moderation::view(&self.moderation, &gateway.navigation) {
                banner_column = banner_column.push(panel.map(Message::Moderation));
            }
            if let Some(panel) = apps::view(&self.apps, &gateway.navigation) {
                banner_column = banner_column.push(panel.map(Message::Apps));
            }
            if let Some(panel) = assets::view(&self.assets, &gateway.navigation) {
                banner_column = banner_column.push(panel.map(Message::Assets));
            }
            if let Some(panel) = automod::view(&self.automod, &gateway.navigation) {
                banner_column = banner_column.push(panel.map(Message::AutoMod));
            }
            if let Some(panel) = invite::view(&self.invite, localtime::now()) {
                banner_column = banner_column.push(panel.map(Message::Invite));
            }
            if let Some(panel) =
                discover::view(&self.discover, &gateway.navigation, &gateway.avatars.views)
            {
                banner_column = banner_column.push(panel.map(Message::Discover));
            }
            if let Some(panel) = group_dm::view(&self.group, &gateway.navigation, session.user.id) {
                banner_column = banner_column.push(panel.map(Message::Group));
            }
            if let Some(panel) = profile_edit::view(
                &self.identity,
                &gateway.navigation,
                own_user(gateway, session),
            ) {
                banner_column = banner_column.push(panel.map(Message::Identity));
            }
            if let Some(panel) = safety::view(&self.safety, &gateway.navigation) {
                banner_column = banner_column.push(panel.map(Message::Safety));
            }
            if let Some(panel) = account_panel::view(
                &self.account_panel,
                &self.account_index,
                session.user.id,
                *persistence == Persistence::Saved,
            ) {
                banner_column = banner_column.push(panel.map(Message::Account));
            }
            if let Some(panel) = report::view(&self.report, &gateway.navigation) {
                banner_column = banner_column.push(panel.map(Message::Report));
            }
            if self.forward.is_open()
                && let Some(panel) = forward::view(&self.forward, &forward_candidates(gateway))
            {
                banner_column = banner_column.push(panel.map(Message::Forward));
            }
            if let Some(panel) = threads::view(&self.threads) {
                banner_column = banner_column.push(panel.map(Message::Threads));
            }
            for (line, danger) in notes {
                let line = text(line).size(12);
                banner_column = banner_column.push(if danger {
                    line.style(text::danger)
                } else {
                    line.style(text::secondary)
                });
            }
            if matches!(
                &gateway.status,
                gateway::GatewayStatus::Stopped(reason) if reason.allows_manual_reconnect()
            ) {
                banner_column = banner_column.push(
                    button(text("Reconnect").size(12))
                        .on_press(Message::ReconnectGateway)
                        .style(style::action_button(false)),
                );
            }
            for call in &gateway.navigation.incoming_calls {
                // Answering joins voice; a call already joined has no prompt.
                if gateway.voice_call == Some(call.channel_id) && in_voice {
                    continue;
                }
                banner_column = banner_column.push(
                    container(
                        row![
                            text(format!("Incoming call: {}", call.label))
                                .size(13)
                                .font(bold)
                                .width(Length::Fill),
                            button(text("Accept").size(12))
                                .on_press(Message::AcceptCall(call.channel_id))
                                .style(style::action_button(false)),
                            button(text("Decline").size(12))
                                .on_press(Message::DeclineCall(call.channel_id))
                                .style(style::action_button(true)),
                        ]
                        .spacing(8)
                        .align_y(iced::alignment::Vertical::Center),
                    )
                    .padding([6, 10])
                    .style(style::card),
                );
            }
            if session.client.writes_restricted() {
                banner_column = banner_column.push(text("Discord requires account verification. Sending is paused; open the official Discord client, complete its check, then reconnect fastcord.").size(13).style(text::danger));
            }
            let banner: Option<Element<'_, Message>> = Some(banner_column.into());

            // ---- the user's panel at the foot of the sidebar ----
            let me = own_user(gateway, session);
            let display = me.display_name().to_owned();
            let own = gateway.navigation.presences.get(&session.user.id);
            let own_status = self.status.status(own);
            let mut foot = column![];
            if self.status.open {
                foot = foot.push(
                    self.status
                        .menu(
                            own,
                            if *persistence == Persistence::Saved {
                                "Saved login"
                            } else {
                                "Memory-only"
                            },
                        )
                        .map(Message::Status),
                );
            }
            if let Some(settings) = voice_settings {
                foot = foot.push(settings);
            }
            if let Some(panel) = stage::own(
                &self.stage,
                &voice_controls,
                session.client.writes_restricted(),
            ) {
                foot = foot.push(panel);
            }
            if let Some(panel) = voice_panel {
                foot = foot.push(panel);
            }
            let requests = gateway.navigation.requests.total;
            let identity = row![
                avatars::view(
                    &gateway.avatars.views,
                    &avatars::AvatarKey::user(me.id, me.avatar.as_deref(),),
                    32.0,
                    || container(text(style::initial(&display)).size(14).font(bold))
                        .center(32)
                        .style(style::avatar(style::avatar_color(session.user.id.0)))
                        .into(),
                ),
                column![
                    text(display)
                        .size(13)
                        .font(bold)
                        .wrapping(text::Wrapping::None),
                    button(
                        row![
                            status::dot(own_status),
                            text(
                                self.status
                                    .custom(own)
                                    .or(own_status.map(status::label))
                                    .unwrap_or("Set status")
                                    .to_owned()
                            )
                            .size(11)
                            .wrapping(text::Wrapping::None),
                        ]
                        .spacing(5)
                        .align_y(iced::alignment::Vertical::Center)
                    )
                    .padding(0)
                    .on_press(Message::Status(status::Event::ToggleMenu))
                    .style(style::tool_button(false, false)),
                ]
                .width(Length::Fill),
            ]
            .spacing(8)
            .align_y(iced::alignment::Vertical::Center);
            let action = |icon, on_press, active, danger, tip: &str, badge| icons::Action {
                icon,
                on_press,
                active,
                danger,
                tip: tip.to_owned(),
                badge,
            };
            foot = foot.push(
                container(icons::footer(
                    identity.into(),
                    [
                        action(
                            icons::Icon::Mic {
                                muted: voice_controls.self_muted,
                            },
                            running.then_some(Message::SetSelfMute(!voice_controls.self_muted)),
                            !voice_controls.self_muted,
                            voice_controls.self_muted,
                            if !running {
                                "Microphone (join a voice channel to use)"
                            } else if voice_controls.self_muted {
                                "Microphone muted. Click to unmute"
                            } else {
                                "Microphone on. Click to mute"
                            },
                            0,
                        ),
                        action(
                            icons::Icon::Headphones {
                                deafened: voice_controls.deafened,
                            },
                            running.then_some(Message::SetDeafen(!voice_controls.deafened)),
                            !voice_controls.deafened,
                            voice_controls.deafened,
                            if !running {
                                "Deafen (join a voice channel to use)"
                            } else if voice_controls.deafened {
                                "Deafened. Click to hear again"
                            } else {
                                "Hearing. Click to deafen"
                            },
                            0,
                        ),
                        action(
                            icons::Icon::Profile,
                            Some(Message::Identity(profile_edit::Event::Toggle)),
                            self.identity.is_open(),
                            false,
                            "Edit your profile",
                            0,
                        ),
                        action(
                            icons::Icon::Safety,
                            Some(Message::Safety(safety::Event::Toggle)),
                            self.safety.is_open() || requests > 0,
                            false,
                            &match requests {
                                0 => "Safety and message requests".to_owned(),
                                n => format!("Safety and message requests ({n} pending)"),
                            },
                            requests.try_into().unwrap_or(u32::MAX),
                        ),
                        action(
                            icons::Icon::Lock,
                            Some(Message::Account(account_panel::Event::Toggle)),
                            self.account_panel.is_open(),
                            false,
                            "Accounts, security and log out",
                            0,
                        ),
                        action(
                            icons::Icon::Audio,
                            Some(Message::OpenAudioSettings),
                            true,
                            false,
                            "Audio settings",
                            0,
                        ),
                        action(
                            icons::Icon::Settings,
                            Some(Message::OpenChatSettings),
                            true,
                            false,
                            "Chat & accessibility settings",
                            0,
                        ),
                    ],
                ))
                .padding([8, 8])
                .style(style::footer),
            );

            let account = container(navigation::view(
                &gateway.navigation,
                &gateway.timeline,
                &gateway.composer,
                {
                    let mut interaction = gateway.interaction();
                    interaction.publishing = self.announce.publishing();
                    interaction
                },
                navigation::PrivateConversation {
                    recipients: &gateway.private.recipients,
                    notice: gateway.private.notice.as_deref(),
                },
                timeline::Attachments {
                    account: session.user.id,
                    views: &gateway.attachment_views,
                    media: gateway.media.as_ref(),
                    emojis: &gateway.emoji.views,
                    avatars: &gateway.avatars.views,
                    revealed: &gateway.revealed,
                    invites: &self.invite.cache,
                    members: if gateway.directs {
                        &NO_MEMBERS
                    } else {
                        &gateway.navigation.members
                    },
                },
                navigation::Chrome {
                    directs: gateway.directs,
                    footer: foot.into(),
                    banner,
                    speaking: talking,
                    voice_call: gateway.voice_call.filter(|_| in_voice),
                    forum: gateway.navigation.forum.as_ref().map(|forum| {
                        forum::view(
                            &self.forum,
                            forum,
                            self.threads.forum_archive(forum.channel),
                            session.user.id,
                            localtime::now(),
                        )
                    }),
                    inbox: self
                        .inbox
                        .is_open()
                        .then(|| {
                            inbox::view(&self.inbox, &gateway.navigation.inbox).map(Message::Inbox)
                        })
                        .or_else(|| {
                            self.notify_settings.is_open().then(|| {
                                notify_settings::view(
                                    &self.notify_settings,
                                    &gateway.navigation.notifications,
                                    gateway.directs,
                                )
                                .map(Message::NotifySettings)
                            })
                        }),
                    friends: self.friends.open.then(|| {
                        friends::view(
                            &self.friends,
                            &gateway.navigation.relationships,
                            &gateway.navigation.presences,
                        )
                        .map(Message::Friends)
                    }),
                },
            ))
            .height(Length::Fill);
            // A profile popup covers whatever is under it.
            let account: Element<'_, Message> = match friends::profile_view(
                &self.friends,
                &gateway.navigation.relationships,
                session.user.id,
                friends::MemberContext::of(
                    self.friends.profile_user(),
                    &gateway.navigation,
                    gateway.directs,
                ),
            ) {
                Some(popup) => iced::widget::stack![account, popup.map(Message::Friends)].into(),
                None => account.into(),
            };
            // The larger image view covers the account screen, which stays in
            // the widget tree underneath so the timeline keeps its scroll state.
            let screen: Element<'_, Message> = match &gateway.attachment_viewer {
                None if gateway.call_view.open && !gateway.emoji_picker.request.open => {
                    let pane = call_ui::panel(
                        &gateway.call_view,
                        &video_tiles,
                        &video_plan,
                        &voice_controls,
                        self.window_fullscreen,
                        self.notice.as_deref(),
                    );
                    iced::widget::stack![account, pane].into()
                }
                None if gateway.share_picker.is_open() && !gateway.emoji_picker.request.open => {
                    iced::widget::stack![
                        account,
                        Self::share_picker_view(
                            &gateway.share_picker,
                            gateway.share_audio,
                            &gateway.share_audio_sources,
                            gateway.share_audio_target,
                        )
                    ]
                    .into()
                }
                Some(viewer) => {
                    iced::widget::stack![account, attachment_ui::viewer_view(viewer)].into()
                }
                None if gateway.emoji_picker.request.open => iced::widget::stack![
                    account,
                    container(emoji_picker::view(
                        &gateway.emoji_picker,
                        &gateway.emoji_snapshot,
                        &gateway.emoji.views
                    ))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Right)
                    .align_y(iced::alignment::Vertical::Bottom)
                    .padding(20),
                ]
                .into(),
                None => account,
            };
            let screen = match member_menu::view(
                &self.member_menu,
                &gateway.navigation,
                voice_controls.clone(),
                session.client.writes_restricted(),
            ) {
                Some(menu) => iced::widget::stack![screen, menu].into(),
                None => screen,
            };
            let overlay: Option<Element<'_, Message>> = if self.switcher.is_open() {
                Some(
                    switcher::view(&self.switcher, &gateway.navigation.quick, |channel| {
                        gateway.composer.has_draft(channel)
                    })
                    .map(Message::Switcher),
                )
            } else if self.switcher.help_open() {
                Some(switcher::help_view().map(Message::Switcher))
            } else {
                None
            };
            return match overlay {
                Some(overlay) => iced::widget::stack![screen, overlay].into(),
                None => screen,
            };
        }
        let content: Element<'_, Message> = match &self.phase {
            Phase::Starting => text("Looking for a saved login in the native credential store…").into(),
            Phase::Login => self.login_view(),
            Phase::Qr(attempt) => self.qr_view(attempt),
            Phase::Mfa(_) => self.mfa_view(),
            Phase::Authenticating { saved_account } => {
                text(if saved_account.is_some() {
                    "Restoring saved login: validating the token with Discord…"
                } else {
                    "Validating your token with Discord…"
                }).into()
            }
            Phase::MemoryConsent { session, error } => column![
                text("Token validated; login is not saved").size(26),
                text(format!("{} (@{})", session.user.display_name(), session.user.username)),
                text(error.to_string()),
                text("Continue only if you accept a memory-only session. The token will not be written to a file; you will need to log in again next launch. Previously saved credentials remain until removed."),
                row![button("Continue without saving").on_press(Message::ContinueInMemory), button("Cancel login").on_press(Message::CancelLogin)].spacing(12),
            ].spacing(20).into(),
            Phase::Account { .. } => unreachable!("account view returned above"),
            Phase::Deleting { .. } => text("Account state cleared. Removing the saved credential…").into(),
            Phase::DeleteFailed { error, .. } => column![
                text("Account state cleared; logout is not complete").size(26),
                text(error.to_string()),
                text("The native store could not confirm removal of this account's saved credential. A previous saved login may restore next launch. Unlock the store and retry. Account state and in-memory secrets have already been cleared."),
                button("Retry credential removal").on_press(Message::RetryDelete),
            ].spacing(20).into(),
        };
        center(container(scrollable(content)).max_width(650).padding(28)).into()
    }

    fn login_view(&self) -> Element<'_, Message> {
        let mut content = column![
            row![
                icons::sized(icons::Icon::Brand, style::BLURPLE, 40.0),
                text("fastcord").size(36),
            ].spacing(12).align_y(iced::Alignment::Center),
            text("Warning: third-party Discord clients violate Discord’s Terms of Service and may get your account banned. Use an alternate account. Never share your token; it grants access to your account. Never share your password or token with anyone but Discord and this app."),
            checkbox(self.acknowledged).label("I understand the risk and am using an alternate account").on_toggle(Message::Acknowledge),
            checkbox(self.memory_only).label("Memory-only session (do not save this login)").on_toggle(Message::MemoryOnly),
            text("Otherwise, the login is saved securely using Windows Credential Manager, macOS Keychain, or Linux Secret Service. Native-store and network operations do not run on the UI thread."),
            text("Log in with QR code (recommended)").size(24),
            text("Open Discord on your phone, go to Settings, choose Scan QR Code, and scan the code fastcord shows. You never handle the token yourself."),
            button("Show QR code").on_press_maybe(self.acknowledged.then_some(Message::StartQr)),
            text("Email and password").size(24),
            text("Discord may ask you to complete a CAPTCHA and a two-factor code. fastcord only shows them to you; it never solves or bypasses them."),
            text_input("Email or phone", &self.password_login)
                .on_input(Message::PasswordLoginEdited)
                .padding(12),
            text_input("Password", self.password.expose_secret())
                .secure(true)
                .on_input(password_edited)
                .on_submit_maybe(self.can_password_login().then_some(Message::PasswordSubmit))
                .padding(12),
            button("Log in with password").on_press_maybe(self.can_password_login().then_some(Message::PasswordSubmit)),
            text("Token login (advanced)").size(24),
            text_input("Paste your alt-account token", self.token.expose_secret())
                .id("token-input")
                .secure(true)
                .on_input(token_edited)
                .on_submit_maybe(self.can_login().then_some(Message::Login))
                .padding(12),
            button("Log in with token").on_press_maybe(self.can_login().then_some(Message::Login)),
            button("Audio devices").on_press(Message::OpenAudioSettings),
        ].spacing(16).width(Length::Fill);
        if let Some(notice) = &self.notice {
            content = content.push(text(notice));
        }
        for account in &self.saved_accounts {
            content = content.push(
                row![
                    button(text(format!(
                        "Restore {}",
                        self.account_index
                            .get(*account)
                            .map_or_else(|| format!("account {account}"), accounts::Entry::label)
                    )))
                    .on_press(Message::Restore(*account)),
                    button("Forget saved login").on_press(Message::Forget(*account)),
                ]
                .spacing(12),
            );
        }
        content
            .push(button("Retry saved login").on_press(Message::RetrySaved))
            .into()
    }

    fn mfa_view(&self) -> Element<'_, Message> {
        let mut content = column![
            text("Two-factor authentication").size(24),
            text("Enter the code from your authenticator app."),
            text_input("Authentication code", self.token.expose_secret())
                .secure(true)
                .on_input(token_edited)
                .on_submit(Message::MfaSubmit)
                .padding(12),
            row![
                button("Continue").on_press(Message::MfaSubmit),
                button("Cancel").on_press(Message::CancelMfa)
            ]
            .spacing(12),
        ]
        .spacing(16)
        .width(Length::Fill);
        if let Some(notice) = &self.notice {
            content = content.push(text(notice));
        }
        content.into()
    }

    fn qr_view<'a>(&'a self, attempt: &'a QrAttempt) -> Element<'a, Message> {
        let mut content = column![text("Log in with QR code").size(32)]
            .spacing(20)
            .width(Length::Fill);
        content = match &attempt.stage {
            QrStage::Connecting => content.push(text(
                "Connecting to Discord’s QR login service and generating a one-time key…",
            )),
            QrStage::Showing { code, expires_in } => content
                .push(
                    container(
                        qr_code(code)
                            .cell_size(7)
                            // Scanners expect dark modules on a light field in any theme.
                            .style(|_| qr_code::Style {
                                cell: Color::BLACK,
                                background: Color::WHITE,
                            }),
                    )
                    .padding(8)
                    .style(|_| container::background(Color::WHITE)),
                )
                .push(text(
                    "On your phone: Discord, Settings, Scan QR Code. Then confirm the login on the phone.",
                ))
                .push(text(format!(
                    "This code expires in {}. Only scan a code shown by fastcord on this screen.",
                    qr::describe_lifetime(*expires_in)
                )))
                .push(
                    row![
                        button("Generate new code").on_press(Message::StartQr),
                        button("Cancel").on_press(Message::CancelQr),
                    ]
                    .spacing(12),
                ),
            QrStage::Scanned(user) => content
                .push(text(format!("Scanned by @{}", user.username)).size(26))
                .push(text(format!("Account ID: {}", user.id)))
                .push(text(
                    "Confirm the login on your phone. If this is not your alt account, cancel on the phone or here.",
                ))
                .push(button("Cancel").on_press(Message::CancelQr)),
            QrStage::Ended(error) => {
                let mut actions = row![].spacing(12);
                if error.can_regenerate() {
                    actions = actions.push(button("Generate new code").on_press(Message::StartQr));
                }
                content
                    .push(text(error.to_string()))
                    .push(actions.push(button("Back to login").on_press(Message::CancelQr)))
            }
        };
        if self.memory_only {
            content = content.push(text("Memory-only session: this login will not be saved."));
        }
        content.into()
    }
}

fn parse_private_recipients(
    input: &str,
    account: Snowflake,
) -> Result<Vec<Snowflake>, &'static str> {
    if input.len() > MAX_PRIVATE_RECIPIENT_INPUT_BYTES {
        return Err("Recipient selection exceeds 256 UTF-8 bytes.");
    }
    let mut recipients = Vec::new();
    for token in input.split(|character: char| character == ',' || character.is_whitespace()) {
        if token.is_empty() {
            continue;
        }
        let id = token
            .parse::<Snowflake>()
            .map_err(|_| "Enter recipient snowflake IDs separated by commas.")?;
        if id == account {
            return Err("Your own account ID is not a recipient.");
        }
        if recipients.contains(&id) {
            return Err("Each recipient ID must appear only once.");
        }
        if recipients.len() == MAX_PRIVATE_RECIPIENTS {
            return Err("A private conversation can include at most 9 other recipients.");
        }
        recipients.push(id);
    }
    if recipients.is_empty() {
        return Err("Select at least one recipient ID.");
    }
    Ok(recipients)
}

fn password_edited(mut value: String) -> Message {
    if value.len() > fastcord_discord::MAX_CREDENTIAL_BYTES {
        value.zeroize();
        Message::InputTooLong
    } else {
        Message::PasswordEdited(Arc::new(UserToken::new(value)))
    }
}

fn token_edited(mut value: String) -> Message {
    if value.len() > login::MAX_TOKEN_BYTES {
        value.zeroize();
        Message::InputTooLong
    } else {
        Message::TokenEdited(Arc::new(UserToken::new(value)))
    }
}

/// Places a forward may go to: the sendable channels of the open server and
/// every conversation in the navigation windows, as they are now.
fn forward_candidates(gateway: &gateway::GatewayPanel) -> Vec<forward::Candidate> {
    use fastcord_discord::state::navigation::ChannelKind as Kind;
    let channels = gateway
        .navigation
        .channels
        .rows
        .iter()
        .filter(|row| !gateway.directs && row.enabled)
        .filter(|row| row.permissions.view_channel && row.permissions.send_messages)
        .filter(|row| {
            matches!(row.kind, Kind::Text | Kind::Announcement | Kind::Thread)
                && row
                    .thread
                    .as_ref()
                    .is_none_or(|thread| !thread.archived && !thread.locked)
        })
        .map(|row| forward::Candidate::new(row.id, &format!("#{}", row.name)));
    let conversations = gateway
        .navigation
        .private_channels
        .rows
        .iter()
        .map(|row| forward::Candidate::new(row.id, &row.label));
    channels
        .chain(conversations)
        .take(forward::MAX_CANDIDATES)
        .collect()
}

static NO_MEMBERS: std::sync::LazyLock<
    std::collections::HashMap<
        fastcord_model::Snowflake,
        fastcord_discord::state::navigation::MemberView,
    >,
> = std::sync::LazyLock::new(Default::default);

/// The account as the Gateway last reported it (display name and avatar
/// change through `USER_UPDATE`); the login's copy until the first READY.
fn own_user<'a>(
    gateway: &'a gateway::GatewayPanel,
    session: &'a login::Session,
) -> &'a fastcord_model::User {
    gateway
        .navigation
        .current_user
        .as_ref()
        .filter(|user| user.id == session.user.id)
        .unwrap_or(&session.user)
}

/// Most embed pictures asked for at once.
const MAX_WANTED_PICTURES: usize = 16;

/// Avatars and icons the shown rows ask for: the user's own, the rail window,
/// then the timeline authors and stickers newest first. Bounded.
fn wanted_avatars(
    gateway: &gateway::GatewayPanel,
    user: &fastcord_model::User,
    invites: &invite::State,
    discover: &discover::State,
) -> Vec<avatars::AvatarKey> {
    let mut wanted = vec![avatars::AvatarKey::user(user.id, user.avatar.as_deref())];
    let guilds = gateway
        .navigation
        .guilds
        .rows
        .iter()
        .filter_map(|guild| avatars::AvatarKey::guild(guild.id, guild.icon.as_deref()));
    let invite_icons = invites
        .cache
        .icon_keys(&invite::visible_codes(&gateway.timeline.rows));
    let authors = gateway.timeline.rows.iter().rev().flat_map(|row| {
        let stickers = row
            .message
            .sticker_items
            .iter()
            .filter_map(|sticker| avatars::AvatarKey::sticker(sticker.id, sticker.format_type));
        std::iter::once(avatars::AvatarKey::user(
            row.message.author.id,
            row.message.author.avatar.as_deref(),
        ))
        .chain(stickers)
    });
    let occupants = gateway
        .navigation
        .channels
        .rows
        .iter()
        .filter_map(|row| match &row.occupant {
            Some(fastcord_discord::state::navigation::Occupant::Member {
                user_id, avatar, ..
            }) => Some(avatars::AvatarKey::user(*user_id, avatar.as_deref())),
            _ => None,
        });
    let controls = gateway.controls.voice_controls();
    let callers = controls
        .participants
        .iter()
        .map(|p| controls.avatar_key(p.user_id));
    // Embed pictures of the newest rows, a few at a time so they never crowd
    // out the avatars.
    let pictures = gateway
        .timeline
        .rows
        .iter()
        .rev()
        .flat_map(|row| {
            let forwarded = row.message.forwarded().map_or(&[][..], |s| &s.embeds[..]);
            rich::embed_keys(&row.message.embeds).chain(rich::embed_keys(forwarded))
        })
        .take(MAX_WANTED_PICTURES)
        .collect::<Vec<_>>();
    let room = avatars::MAX_WANTED - pictures.len();
    for key in guilds
        .chain(invite_icons)
        .chain(discover.icon_keys())
        .chain(occupants)
        .chain(callers)
        .chain(authors)
        .chain(pictures)
    {
        if wanted.len() == room && !matches!(key, avatars::AvatarKey::Embed { .. }) {
            continue;
        }
        if wanted.len() == avatars::MAX_WANTED {
            break;
        }
        if !wanted.contains(&key) {
            wanted.push(key);
        }
    }
    wanted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login_app() -> App {
        let (mut app, _) = App::boot();
        app.phase = Phase::Login;
        app
    }

    fn authenticate(app: &mut App, persistence: Persistence) -> Arc<Session> {
        let session = login::fixture_session();
        app.phase = Phase::Authenticating {
            saved_account: None,
        };
        let _ = app.update(Message::LoginFinished(Ok(LoginOutcome::Authenticated(
            Arc::clone(&session),
            persistence,
        ))));
        session
    }

    #[test]
    fn settings_are_lazy_independent_and_preserve_pending_latest_save() {
        let mut app = login_app();
        assert!(!app.audio_settings.probing);
        let _ = app.update(Message::OpenAudioSettings);
        assert!(app.audio_settings.probing);
        let _ = app.update(Message::AudioDevicesListed(audio_devices::Inventory {
            input: Err("Input unavailable"),
            output: Ok(Vec::new()),
        }));
        assert!(!app.audio_settings.probing);
        assert_eq!(app.audio_settings.input_error, Some("Input unavailable"));
        assert_eq!(app.audio_settings.output_error, None);
        let option = |id: &str| audio_devices::DeviceOption {
            choice: fastcord_audio::DeviceChoice::Device(
                fastcord_audio::DeviceKey::from_persisted(id).unwrap(),
            ),
            label: "Headset".to_owned(),
            unavailable: false,
        };
        let _ = app.update(Message::SelectAudioDevice(
            fastcord_audio::Direction::Input,
            option("mic"),
        ));
        let first = app.audio_saving.clone().unwrap();
        let _ = app.update(Message::SelectAudioDevice(
            fastcord_audio::Direction::Output,
            option("headset"),
        ));
        assert_eq!(app.audio_saving, Some(first));
        let _ = app.update(Message::AudioPreferencesSaved(Ok(())));
        assert_eq!(app.audio_saving.as_ref(), Some(&app.audio_preferences));
        let _ = app.update(Message::AudioPreferencesSaved(Ok(())));
        assert!(app.audio_saving.is_none());
        let _ = app.view();
        let _ = app.update(Message::CloseAudioSettings);
        assert!(!app.audio_settings.open);
    }

    #[test]
    fn noise_preferences_are_coalesced_restored_and_reselected_on_the_controls() {
        use fastcord_audio::NoiseSuppression::{HighQuality, Light, Off};
        let mut app = login_app();
        let _ = app.update(Message::NoiseSuppressionChanged(Off));
        assert_eq!(app.audio_preferences.noise_suppression, Off);
        assert_eq!(app.audio_saving.as_ref().unwrap().noise_suppression, Off);
        let _ = app.update(Message::NoiseSuppressionChanged(HighQuality));
        assert_eq!(app.audio_saving.as_ref().unwrap().noise_suppression, Off);
        let _ = app.update(Message::AudioPreferencesSaved(Ok(())));
        assert_eq!(
            app.audio_saving.as_ref().unwrap().noise_suppression,
            HighQuality
        );
        let _ = app.update(Message::AudioPreferencesSaved(Ok(())));
        assert!(app.audio_saving.is_none());

        authenticate(&mut app, Persistence::MemoryOnly);
        let Phase::Account { gateway, .. } = &app.phase else {
            panic!("not logged in");
        };
        let restored = gateway.controls.voice_controls();
        assert_eq!(restored.noise_suppression, HighQuality);
        let _ = app.update(Message::NoiseSuppressionChanged(Light));
        let _ = app.update(Message::NoiseSuppressionChanged(Light));
        let Phase::Account { gateway, .. } = &app.phase else {
            panic!("not logged in");
        };
        let changed = gateway.controls.voice_controls();
        assert_eq!(changed.noise_suppression, Light);
        assert_eq!(
            changed.noise_generation,
            restored.noise_generation.wrapping_add(2)
        );
        assert_eq!(app.audio_preferences.noise_suppression, Light);
    }

    #[test]
    fn the_audio_dialog_switches_the_level_meter_on_and_off_and_gain_reaches_the_call() {
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        let diagnostics = |app: &App| match &app.phase {
            Phase::Account { gateway, .. } => gateway.controls.voice_controls().diagnostics_open,
            _ => panic!("not logged in"),
        };
        assert!(!diagnostics(&app), "closed: nothing is measured");
        // The 10 Hz timer exists only while the dialog is open.
        let _ = app.update(Message::OpenAudioSettings);
        assert!(diagnostics(&app));
        let _ = app.update(Message::MicPanel(mic_panel::Event::Gain(9)));
        let _ = app.update(Message::MicPanel(mic_panel::Event::Automatic(true)));
        let wanted = fastcord_audio::InputGain {
            manual_db: 9,
            automatic: true,
        };
        assert_eq!(app.audio_preferences.input_gain, wanted);
        let Phase::Account { gateway, .. } = &app.phase else {
            panic!("not logged in");
        };
        assert_eq!(gateway.controls.voice_controls().input_gain, wanted);
        let _ = app.view();
        let _ = app.update(Message::CloseAudioSettings);
        assert!(!diagnostics(&app));
        assert!(!app.mic_panel.running());
    }

    #[test]
    fn restored_device_ids_enter_the_call_controls_without_changing_mute_or_volumes() {
        let mut app = login_app();
        app.audio_preferences.input.choice = fastcord_audio::DeviceChoice::Device(
            fastcord_audio::DeviceKey::from_persisted("saved-microphone").unwrap(),
        );
        app.audio_preferences.output.choice = fastcord_audio::DeviceChoice::Device(
            fastcord_audio::DeviceKey::from_persisted("saved-headset").unwrap(),
        );
        authenticate(&mut app, Persistence::MemoryOnly);
        let Phase::Account { gateway, .. } = &app.phase else {
            panic!("not logged in")
        };
        let restored = gateway.controls.voice_controls();
        assert_eq!(restored.input_device, app.audio_preferences.input.choice);
        assert_eq!(restored.output_device, app.audio_preferences.output.choice);
        gateway.controls.set_self_muted(true);
        gateway.controls.set_deafened(true);
        gateway.controls.set_volume_percent(Snowflake(99), 175);
        gateway.controls.select_audio_device(
            fastcord_audio::Direction::Output,
            fastcord_audio::DeviceChoice::SystemDefault,
        );
        let changed = gateway.controls.voice_controls();
        assert!(changed.self_muted && changed.deafened);
        assert_eq!(changed.volume_percent(Snowflake(99)), 175);
        assert_eq!(changed.input_device, restored.input_device);
        assert_eq!(
            changed.output_device,
            fastcord_audio::DeviceChoice::SystemDefault
        );
    }

    #[test]
    fn password_input_messages_and_pending_sessions_are_redacted() {
        let message = token_edited("dummy-offline-secret".to_owned());
        assert!(!format!("{message:?}").contains("dummy-offline-secret"));
        let result = Message::LoginFinished(Ok(LoginOutcome::NeedsMemoryConsent(
            login::fixture_session(),
            StoreError::Locked,
        )));
        assert!(!format!("{result:?}").contains("dummy-offline-secret"));
        assert!(matches!(
            token_edited("x".repeat(login::MAX_TOKEN_BYTES + 1)),
            Message::InputTooLong
        ));
    }

    #[test]
    fn submitting_moves_the_secret_and_trims_paste_whitespace() {
        let mut app = login_app();
        let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
        let _ = app.update(Message::TokenEdited(Arc::clone(&token)));
        let submitted = app.take_input_token();
        assert!(Arc::ptr_eq(&token, &submitted));
        assert!(app.token.expose_secret().is_empty());
        let _ = app.update(token_edited("  dummy-offline-secret \n".to_owned()));
        let submitted = app.take_input_token();
        assert!(submitted.expose_secret() == "dummy-offline-secret");
        assert!(app.token.expose_secret().is_empty());
    }

    #[test]
    fn token_login_requires_warning_acknowledgement_and_bounded_nonempty_input() {
        let mut app = login_app();
        let _ = app.update(token_edited("dummy-offline-secret".into()));
        assert!(!app.can_login());
        let _ = app.update(Message::Login);
        assert!(matches!(app.phase, Phase::Login));
        let _ = app.update(Message::Acknowledge(true));
        assert!(app.can_login());
        let _ = app.update(Message::InputTooLong);
        assert!(app.token.expose_secret().is_empty());
        assert!(!app.can_login());
    }

    #[test]
    fn invalid_token_stops_and_never_retains_an_authenticated_account() {
        let mut app = login_app();
        app.phase = Phase::Authenticating {
            saved_account: None,
        };
        let _ = app.update(Message::LoginFinished(Err(LoginError::Rest(
            fastcord_discord::RestError::AuthenticationRequired,
        ))));
        assert!(matches!(app.phase, Phase::Login));
        assert!(app.notice.as_deref().unwrap().contains("rejected"));
        assert!(app.saved_accounts.is_empty());
        assert!(app.token.expose_secret().is_empty());
    }

    #[test]
    fn locked_store_does_not_silently_establish_a_memory_session() {
        let mut app = login_app();
        app.phase = Phase::Authenticating {
            saved_account: None,
        };
        let session = login::fixture_session();
        let _ = app.update(Message::LoginFinished(Ok(
            LoginOutcome::NeedsMemoryConsent(session, StoreError::Locked),
        )));
        assert!(matches!(app.phase, Phase::MemoryConsent { .. }));
        assert!(app.saved_accounts.is_empty());
        let _ = app.update(Message::ContinueInMemory);
        assert!(matches!(
            app.phase,
            Phase::Account {
                persistence: Persistence::MemoryOnly,
                ..
            }
        ));
        assert!(app.saved_accounts.is_empty());
    }

    #[tokio::test]
    async fn logout_immediately_stops_workers_and_purges_account_and_input() {
        let mut app = login_app();
        let session = authenticate(&mut app, Persistence::Saved);
        let account = session.user.id;
        assert!(app.saved_accounts.contains(&account));
        let client = session.client.clone();
        drop(session);
        app.acknowledged = true;
        app.memory_only = true;
        let _ = app.update(Message::Logout);
        client.authentication_required().await;
        assert!(matches!(app.phase, Phase::Deleting { .. }));
        assert!(app.token.expose_secret().is_empty());
        assert!(!app.acknowledged && !app.memory_only);
        let _ = app.update(Message::Deleted(Err(StoreError::Locked)));
        assert!(matches!(app.phase, Phase::DeleteFailed { .. }));
        assert!(app.saved_accounts.contains(&account));
        let _ = app.update(Message::RetryDelete);
        let _ = app.update(Message::Deleted(Ok(())));
        assert!(matches!(app.phase, Phase::Login));
        assert!(app.saved_accounts.is_empty());
    }

    #[test]
    fn memory_only_logout_does_not_create_a_saved_login_for_next_launch() {
        let mut app = login_app();
        drop(authenticate(&mut app, Persistence::MemoryOnly));
        assert!(app.saved_accounts.is_empty());
        let _ = app.update(Message::Logout);
        assert!(matches!(app.phase, Phase::Deleting { .. }));
        let _ = app.update(Message::Deleted(Ok(())));
        assert!(matches!(app.phase, Phase::Login));
        assert!(app.saved_accounts.is_empty());
        assert!(app.token.expose_secret().is_empty());
    }

    #[test]
    fn cancelling_memory_consent_purges_the_validated_account() {
        let mut app = login_app();
        app.phase = Phase::MemoryConsent {
            session: login::fixture_session(),
            error: StoreError::Unavailable,
        };
        let _ = app.update(Message::CancelLogin);
        assert!(matches!(app.phase, Phase::Login));
        assert!(app.token.expose_secret().is_empty());
    }

    fn qr_link() -> fastcord_discord::QrLink {
        fastcord_discord::QrLink::new(
            "UZ0-kOVzXDZTFVV5_QlpURSO2BQHrtkKWHNpIGoDI0k",
            std::time::Duration::from_secs(300),
        )
    }

    fn qr_stage(app: &App) -> &QrStage {
        match &app.phase {
            Phase::Qr(attempt) => &attempt.stage,
            other => panic!("expected the QR screen, found {other:?}"),
        }
    }

    fn qr_id(app: &App) -> u64 {
        match &app.phase {
            Phase::Qr(attempt) => attempt.id,
            other => panic!("expected the QR screen, found {other:?}"),
        }
    }

    #[test]
    fn qr_login_starts_only_after_the_warning_is_acknowledged() {
        let mut app = login_app();
        let _ = app.update(Message::StartQr);
        assert!(matches!(app.phase, Phase::Login));
        let _ = app.update(Message::Acknowledge(true));
        let _ = app.update(token_edited("dummy-offline-secret".into()));
        let _ = app.update(Message::StartQr);
        assert!(matches!(qr_stage(&app), QrStage::Connecting));
        // A half-typed token must not linger while another method runs.
        assert!(app.token.expose_secret().is_empty());
    }

    #[test]
    fn qr_events_drive_the_screen_and_authorization_uses_the_token_path() {
        let mut app = login_app();
        app.acknowledged = true;
        let _ = app.update(Message::StartQr);
        let id = qr_id(&app);
        let _ = app.update(Message::Qr(id, RemoteAuthEvent::Qr(qr_link())));
        assert!(matches!(qr_stage(&app), QrStage::Showing { .. }));
        let user = fastcord_discord::RemoteUser {
            id: Snowflake(175_928_847_299_117_063),
            username: "alt_fixture".to_owned(),
            avatar: None,
        };
        let _ = app.update(Message::Qr(id, RemoteAuthEvent::PendingUser(user)));
        assert!(matches!(qr_stage(&app), QrStage::Scanned(_)));
        let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
        let _ = app.update(Message::Qr(id, RemoteAuthEvent::Authorized(token)));
        // Validation and storage happen in the same worker as token paste.
        assert!(matches!(
            app.phase,
            Phase::Authenticating {
                saved_account: None
            }
        ));
        assert!(app.token.expose_secret().is_empty());
    }

    #[test]
    fn regenerating_uses_a_new_attempt_and_ignores_the_old_one() {
        let mut app = login_app();
        app.acknowledged = true;
        let _ = app.update(Message::StartQr);
        let first = qr_id(&app);
        let _ = app.update(Message::Qr(first, RemoteAuthEvent::Qr(qr_link())));
        let _ = app.update(Message::StartQr);
        let second = qr_id(&app);
        assert_ne!(first, second);
        assert!(matches!(qr_stage(&app), QrStage::Connecting));
        // Stragglers from the replaced attempt, even an authorization, do nothing.
        let _ = app.update(Message::Qr(first, RemoteAuthEvent::Qr(qr_link())));
        let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
        let _ = app.update(Message::Qr(first, RemoteAuthEvent::Authorized(token)));
        assert!(matches!(qr_stage(&app), QrStage::Connecting));
    }

    #[test]
    fn cancel_returns_to_login_and_drops_the_attempt() {
        let mut app = login_app();
        app.acknowledged = true;
        let _ = app.update(Message::StartQr);
        let id = qr_id(&app);
        let _ = app.update(Message::Qr(id, RemoteAuthEvent::Qr(qr_link())));
        let _ = app.update(Message::CancelQr);
        assert!(matches!(app.phase, Phase::Login));
        assert!(app.acknowledged, "cancel keeps the acknowledgement");
        let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
        let _ = app.update(Message::Qr(id, RemoteAuthEvent::Authorized(token)));
        assert!(matches!(app.phase, Phase::Login));
    }

    #[test]
    fn timeout_phone_cancel_and_captcha_end_the_attempt_with_clear_text() {
        use fastcord_discord::RemoteAuthError;
        for (error, regenerate) in [
            (RemoteAuthError::Expired, true),
            (RemoteAuthError::CancelledOnPhone, true),
            (RemoteAuthError::Captcha, false),
        ] {
            let mut app = login_app();
            app.acknowledged = true;
            let _ = app.update(Message::StartQr);
            let id = qr_id(&app);
            let _ = app.update(Message::Qr(id, RemoteAuthEvent::Qr(qr_link())));
            let _ = app.update(Message::Qr(id, RemoteAuthEvent::Failed(error)));
            assert!(matches!(qr_stage(&app), QrStage::Ended(e) if *e == error));
            assert_eq!(error.can_regenerate(), regenerate);
            // Regenerating from an ended attempt starts a clean one.
            if regenerate {
                let _ = app.update(Message::StartQr);
                assert!(matches!(qr_stage(&app), QrStage::Connecting));
            }
        }
        assert!(RemoteAuthError::Captcha.to_string().contains("token login"));
        assert!(
            RemoteAuthError::Captcha
                .to_string()
                .contains("official Discord client")
        );
    }

    #[test]
    fn qr_messages_and_phases_never_print_codes_users_or_tokens() {
        let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
        let message = Message::Qr(1, RemoteAuthEvent::Authorized(token));
        assert!(!format!("{message:?}").contains("dummy-offline-secret"));
        let message = Message::Qr(1, RemoteAuthEvent::Qr(qr_link()));
        assert!(!format!("{message:?}").contains("UZ0-kOVz"));
        let mut app = login_app();
        app.acknowledged = true;
        let _ = app.update(Message::StartQr);
        let id = qr_id(&app);
        let _ = app.update(Message::Qr(id, RemoteAuthEvent::Qr(qr_link())));
        assert!(!format!("{:?}", app.phase).contains("UZ0-kOVz"));
    }

    fn gateway_status(app: &App) -> (u64, GatewayStatus) {
        match &app.phase {
            Phase::Account { gateway, .. } => (gateway.id, gateway.status.clone()),
            other => panic!("expected the account screen, found {other:?}"),
        }
    }

    #[test]
    fn the_account_screen_owns_a_gateway_that_starts_connecting() {
        let mut app = login_app();
        drop(authenticate(&mut app, Persistence::Saved));
        let (id, status) = gateway_status(&app);
        assert_eq!(status, GatewayStatus::Connecting { attempt: 0 });
        assert!(status.describe().starts_with("Connecting to Discord"));
        // A second session gets a new identity.
        app.phase = Phase::Login;
        drop(authenticate(&mut app, Persistence::MemoryOnly));
        assert_ne!(gateway_status(&app).0, id);
    }

    #[test]
    fn a_limit_stop_offers_reconnect_which_starts_a_fresh_gateway_and_other_stops_do_not() {
        use fastcord_discord::gateway::StopReason;
        let mut app = login_app();
        drop(authenticate(&mut app, Persistence::Saved));
        let (id, _) = gateway_status(&app);
        // A reason that reconnecting cannot fix offers nothing.
        let _ = app.update(Message::Gateway(
            id,
            GatewayStatus::Stopped(StopReason::EventTooLarge),
        ));
        let _ = app.update(Message::ReconnectGateway);
        assert_eq!(gateway_status(&app).0, id);
        // fastcord's own limits do: the screen stays, with a new Gateway.
        let _ = app.update(Message::Gateway(
            id,
            GatewayStatus::Stopped(StopReason::ReconnectFailures),
        ));
        assert!(gateway_status(&app).1.describe().contains("Ten connection"));
        drop(app.update(Message::ReconnectGateway));
        let (new_id, status) = gateway_status(&app);
        assert_ne!(new_id, id);
        assert_eq!(status, GatewayStatus::Connecting { attempt: 0 });
        // Nothing to reconnect once connecting again.
        let _ = app.update(Message::ReconnectGateway);
        assert_eq!(gateway_status(&app).0, new_id);
    }

    #[test]
    fn memory_only_continuation_also_starts_the_gateway() {
        let mut app = login_app();
        app.phase = Phase::MemoryConsent {
            session: login::fixture_session(),
            error: StoreError::Locked,
        };
        let _ = app.update(Message::ContinueInMemory);
        assert_eq!(
            gateway_status(&app).1,
            GatewayStatus::Connecting { attempt: 0 }
        );
    }

    #[test]
    fn gateway_status_updates_apply_but_stale_sessions_are_ignored() {
        let mut app = login_app();
        drop(authenticate(&mut app, Persistence::Saved));
        let (id, _) = gateway_status(&app);
        let counts = gateway::Counts {
            guilds: 2,
            unavailable: 0,
            direct_messages: 1,
        };
        let _ = app.update(Message::Gateway(id, GatewayStatus::Ready(counts)));
        assert_eq!(gateway_status(&app).1, GatewayStatus::Ready(counts));
        // A straggler from an earlier session must not change the screen.
        let _ = app.update(Message::Gateway(id + 1, GatewayStatus::Resuming));
        assert_eq!(gateway_status(&app).1, GatewayStatus::Ready(counts));
    }

    #[tokio::test]
    async fn a_rejected_token_stops_everything_and_returns_to_login() {
        let mut app = login_app();
        let session = authenticate(&mut app, Persistence::Saved);
        let client = session.client.clone();
        let account = session.user.id;
        drop(session);
        let (id, _) = gateway_status(&app);
        let _ = app.update(Message::Gateway(id, GatewayStatus::AuthenticationRequired));
        client.authentication_required().await;
        assert!(matches!(app.phase, Phase::Login));
        assert!(app.notice.as_deref().unwrap().contains("no longer accepts"));
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("official Discord client")
        );
        assert!(app.notice.as_deref().unwrap().contains("password reset"));
        assert!(app.token.expose_secret().is_empty());
        // The saved credential is not silently deleted; the user decides.
        assert!(app.saved_accounts.contains(&account));
    }

    #[tokio::test]
    async fn a_gateway_account_check_tears_down_panels_and_requests() {
        let mut app = login_app();
        let session = authenticate(&mut app, Persistence::Saved);
        let client = session.client.clone();
        let (id, _) = gateway_status(&app);
        let task = app.update(Message::NotifySettings(notify_settings::Event::Toggle));
        assert_eq!(task.units(), 0);
        assert!(app.notify_settings.is_open());
        let _ = app.update(Message::Gateway(
            id,
            GatewayStatus::Stopped(fastcord_discord::gateway::StopReason::ActionRequired(
                "REQUIRE_VERIFIED_EMAIL".to_owned(),
            )),
        ));
        client.authentication_required().await;
        assert!(matches!(app.phase, Phase::Login));
        assert!(!app.notify_settings.is_open());
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("official Discord client")
        );
    }

    #[tokio::test]
    async fn the_unverified_write_switch_is_session_only_and_resets_on_logout() {
        use fastcord_discord::WriteGroup;
        let mut app = login_app();
        let session = authenticate(&mut app, Persistence::Saved);
        let _ = app.update(Message::UnverifiedWrite(WriteGroup::Reports, true));
        let _ = app.update(Message::UnverifiedWrite(WriteGroup::Typing, false));
        assert!(session.client.write_group_enabled(WriteGroup::Reports));
        assert!(!session.client.write_group_enabled(WriteGroup::Typing));
        drop(session);
        let _ = app.update(Message::ToggleUnverifiedList);
        assert!(app.unverified_open);
        let _ = app.update(Message::Logout);
        assert!(!app.unverified_open);
        let _ = app.update(Message::Deleted(Ok(())));
        // The next sign-in has a new client with the defaults, not the old switch.
        let next = authenticate(&mut app, Persistence::MemoryOnly);
        assert!(!next.client.write_group_enabled(WriteGroup::Reports));
        assert!(next.client.write_group_enabled(WriteGroup::Typing));
    }

    #[tokio::test]
    async fn logout_drops_the_gateway_with_the_account_screen() {
        let mut app = login_app();
        drop(authenticate(&mut app, Persistence::Saved));
        let (id, _) = gateway_status(&app);
        let _ = app.update(Message::Logout);
        assert!(matches!(app.phase, Phase::Deleting { .. }));
        // Late status from the closed connection changes nothing.
        let _ = app.update(Message::Gateway(id, GatewayStatus::Resuming));
        assert!(matches!(app.phase, Phase::Deleting { .. }));
    }

    #[tokio::test]
    async fn switching_tears_down_the_account_before_the_other_one_signs_in() {
        // The Linux Secret Service client blocks on its own runtime, which may not
        // start inside this test's runtime.
        let Ok(store) = std::thread::spawn(NativeCredentialStore::new)
            .join()
            .unwrap()
        else {
            return; // no native store on this machine
        };
        let mut app = login_app();
        let session = authenticate(&mut app, Persistence::Saved);
        let account = session.user.id;
        let client = session.client.clone();
        drop(session);
        app.store = Some(store);
        let other = Snowflake(42);
        app.saved_accounts.push(other);
        let (id, _) = gateway_status(&app);
        // Switching to the active or an unsaved account does nothing.
        let _ = app.update(Message::SwitchAccount(account));
        let _ = app.update(Message::SwitchAccount(Snowflake(43)));
        assert!(matches!(app.phase, Phase::Account { .. }));
        let _ = app.update(Message::Account(account_panel::Event::Toggle));
        assert!(app.account_panel.is_open());

        let _ = app.update(Message::SwitchAccount(other));
        // The old gateway and voice are gone, panels cleared, and the token
        // of the old account has stopped working before the new login starts.
        client.authentication_required().await;
        assert!(
            matches!(app.phase, Phase::Authenticating { saved_account: Some(a) } if a == other)
        );
        assert!(!app.account_panel.is_open());
        let _ = app.update(Message::Gateway(id, GatewayStatus::Resuming));
        assert!(matches!(app.phase, Phase::Authenticating { .. }));
        // Unlike logout, the old account's saved login stays.
        assert!(app.saved_accounts.contains(&account));
    }

    #[test]
    fn a_memory_only_session_cannot_be_switched_away_from() {
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        app.saved_accounts.push(Snowflake(42));
        let _ = app.update(Message::SwitchAccount(Snowflake(42)));
        assert!(matches!(app.phase, Phase::Account { .. }));
    }

    #[test]
    fn chords_open_overlays_and_escape_unwinds_them_before_cancelling_a_reply() {
        use switcher::Shortcut;
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        let channel = Snowflake(500);
        let reply = outbox::Reply {
            message: Snowflake(7),
            author: "a".into(),
            content: "b".into(),
            mention_author: false,
        };
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        gateway.composer.select(Some(channel));
        assert!(gateway.composer.restore_send("", &[], Some(reply)));
        let _ = app.update(Message::Shortcut(Shortcut::Switcher));
        assert!(app.switcher.is_open());
        let _ = app.update(Message::Shortcut(Shortcut::Help));
        assert!(app.switcher.help_open() && !app.switcher.is_open());
        // Escape closes the overlay first and leaves the reply alone.
        let _ = app.update(Message::Shortcut(Shortcut::Escape { captured: false }));
        assert!(!app.switcher.help_open());
        let replying = |app: &App| matches!(&app.phase, Phase::Account { gateway, .. } if gateway.composer.replying().is_some());
        assert!(replying(&app));
        // A focused composer already handled its own Escape.
        let _ = app.update(Message::Shortcut(Shortcut::Escape { captured: true }));
        assert!(replying(&app));
        let _ = app.update(Message::Shortcut(Shortcut::Escape { captured: false }));
        assert!(!replying(&app));
        // Plain arrows do nothing while the switcher is closed.
        let _ = app.update(Message::Shortcut(Shortcut::Move(1)));
        assert!(!app.switcher.is_open());
    }

    #[test]
    fn picking_a_switcher_result_closes_it_and_awaits_the_navigation() {
        use fastcord_discord::state::navigation::{NavigationSnapshot, QuickEntry, QuickKind};
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        let entry = |id: u64, guild: Option<u64>, kind| QuickEntry {
            channel: Snowflake(id),
            guild: guild.map(Snowflake),
            guild_name: String::new(),
            name: "x".to_owned(),
            kind,
            unread: false,
            mentions: 0,
        };
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        gateway.navigation = Arc::new(NavigationSnapshot {
            quick: vec![
                entry(1, Some(9), QuickKind::Voice),
                entry(2, None, QuickKind::Direct),
            ],
            ..NavigationSnapshot::default()
        });
        for (index, directs) in [(0, false), (1, true)] {
            let _ = app.update(Message::Shortcut(switcher::Shortcut::Switcher));
            assert!(app.switcher.is_open());
            app.nav_pending = false;
            let _ = app.update(Message::Switcher(switcher::Event::Pick(index)));
            assert!(!app.switcher.is_open());
            assert!(app.nav_pending, "focus returns once the chat is open");
            let Phase::Account { gateway, .. } = &app.phase else {
                panic!("account");
            };
            assert_eq!(gateway.directs, directs);
            assert!(
                gateway.controls.voice_target().is_none(),
                "a voice row opens its chat, never the call"
            );
        }
    }

    #[test]
    fn gateway_messages_and_phases_never_print_secrets() {
        let mut app = login_app();
        drop(authenticate(&mut app, Persistence::Saved));
        let (id, _) = gateway_status(&app);
        let message = Message::Gateway(
            id,
            GatewayStatus::Stopped(fastcord_discord::gateway::StopReason::EventTooLarge),
        );
        assert!(!format!("{message:?}").contains("dummy-offline-secret"));
        assert!(!format!("{:?}", app.phase).contains("dummy-offline-secret"));
    }
    #[test]
    fn private_recipient_selection_is_manual_bounded_and_unique() {
        let account = Snowflake(1);
        assert_eq!(
            parse_private_recipients(" 42, 43 ", account).unwrap(),
            [Snowflake(42), Snowflake(43)]
        );
        assert!(parse_private_recipients("", account).is_err());
        assert!(parse_private_recipients("42,42", account).is_err());
        assert!(parse_private_recipients("1", account).is_err());
        assert!(parse_private_recipients("not-an-id", account).is_err());
        assert!(
            parse_private_recipients(
                &(1..=MAX_PRIVATE_RECIPIENTS + 1)
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                account
            )
            .is_err()
        );
        assert!(
            parse_private_recipients(&"x".repeat(MAX_PRIVATE_RECIPIENT_INPUT_BYTES + 1), account)
                .is_err()
        );
    }

    #[test]
    fn animation_subscription_exists_only_for_visible_active_animated_assets() {
        let mut app = login_app();
        assert_eq!(app.animation_subscription().units(), 0, "idle: no timer");
        authenticate(&mut app, Persistence::MemoryOnly);
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        let asset = emoji::Asset::Custom {
            id: Snowflake(12345),
            animated: true,
        };
        gateway.emoji.visibility(
            emoji::Occurrence {
                asset,
                origin: emoji::Origin::Timeline {
                    channel: Snowflake(500),
                    message: Snowflake(42),
                    revision: 1,
                    run: 0,
                },
            },
            true,
        );
        let _ = gateway.emoji.synchronize(true, false, gateway.id, None);
        assert_eq!(
            app.animation_subscription().units(),
            0,
            "visible but not yet decoded: nothing to animate"
        );
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        let frame = attachments::DecodedImage {
            width: 1,
            height: 1,
            rgba: bytes::Bytes::from_static(&[255, 0, 0, 255]),
        };
        gateway.emoji.views.insert(
            asset,
            emoji::View::from_decoded(attachments::DecodedAnimation {
                frames: vec![frame.clone(), frame],
                delays: vec![std::time::Duration::from_millis(20); 2],
            }),
        );
        assert_ne!(
            app.animation_subscription().units(),
            0,
            "a visible active animation has a timer"
        );
        let _ = app.update(Message::WindowActive(false));
        assert_eq!(
            app.animation_subscription().units(),
            0,
            "unfocused/minimized windows cancel animation"
        );
        let Phase::Account { gateway, .. } = &app.phase else {
            panic!("account");
        };
        assert!(gateway.emoji.views.is_empty());
    }

    #[test]
    fn a_picker_opened_for_a_message_reacts_to_it_and_the_composers_button_inserts_text() {
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        let channel = Snowflake(500);
        let thumbs = emoji_picker::Choice::Unicode(emoji::unicode("\u{1f44d}").unwrap());
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        // The account's own worker is not running in tests, so its command queue
        // is gone; a panel with a bridge whose receiver the test holds replaces it.
        let bridge = gateway::NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let (_, worker) = iced::Task::<()>::none().abortable();
        **gateway = GatewayPanel::new(gateway.id, worker, bridge);
        gateway.timeline = Arc::new(timeline::Snapshot {
            channel_id: Some(channel),
            can_send: true,
            can_react: true,
            can_tts: true,
            ..timeline::Snapshot::default()
        });
        gateway.composer.select(Some(channel));
        let page = |gateway: &GatewayPanel| {
            Arc::new(emoji_picker::Snapshot {
                revision: gateway.emoji_picker.request.revision,
                channel: Some(channel),
                offset: 0,
                total: 1,
                cells: vec![emoji_picker::Cell {
                    choice: thumbs.clone(),
                    guild_name: None,
                    variants: None,
                }],
            })
        };

        // React on a message, then pick: a reaction request, not editor text.
        let _ = app.update(Message::Timeline(timeline::Event::React {
            channel_id: channel,
            message_id: Snowflake(42),
        }));
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        assert!(gateway.emoji_picker.request.open);
        gateway.emoji_snapshot = page(gateway);
        let _ = app.update(Message::EmojiPicker(emoji_picker::Event::Select(
            thumbs.clone(),
        )));
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        assert!(!gateway.emoji_picker.request.open);
        assert!(gateway.composer.submission(true, 0).is_none(), "no text");
        assert!(matches!(
            commands.try_recv(),
            Ok(gateway::Command::AddReaction { channel: c, message, emoji })
                if (c, message) == (channel, Snowflake(42))
                    && emoji == thumbs.reaction_emoji()
        ));
        assert!(commands.try_recv().is_err());
        assert_eq!(gateway.emoji_picker.request.recent[0], thumbs);

        // The composer's button still inserts text.
        let _ = app.update(Message::EmojiPicker(emoji_picker::Event::Toggle));
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        assert!(gateway.emoji_picker.request.open);
        assert_eq!(gateway.emoji_picker.reaction(), None);
        gateway.emoji_snapshot = page(gateway);
        let _ = app.update(Message::EmojiPicker(emoji_picker::Event::Select(thumbs)));
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        assert!(gateway.composer.submission(true, 0).is_some(), "text");
        assert!(commands.try_recv().is_err(), "nothing was reacted to");
    }

    #[test]
    fn media_subscription_ticks_only_while_audio_is_playing() {
        use fastcord_audio::PlayerState;
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        assert_eq!(app.media_subscription().units(), 0);
        let key = AttachmentKey {
            account: Snowflake(1),
            channel: Snowflake(2),
            message: Snowflake(3),
            attachment: Snowflake(4),
        };
        let set = |app: &mut App, phase, state| {
            let Phase::Account { gateway, .. } = &mut app.phase else {
                panic!("account");
            };
            let mut media = media_ui::MediaSession::for_test(key);
            media.phase = phase;
            media.snapshot.state = state;
            gateway.media = Some(media);
        };
        set(&mut app, media_ui::Phase::Loading, PlayerState::Playing);
        assert_eq!(app.media_subscription().units(), 0);
        set(&mut app, media_ui::Phase::Active, PlayerState::Playing);
        assert_eq!(app.media_subscription().units(), 1);
        set(&mut app, media_ui::Phase::Active, PlayerState::Paused);
        assert_eq!(app.media_subscription().units(), 0);
        set(&mut app, media_ui::Phase::Failed("x"), PlayerState::Playing);
        assert_eq!(app.media_subscription().units(), 0);
    }

    #[test]
    fn video_subscription_tracks_pane_focus_resize_occlusion_and_unwatch() {
        use std::sync::atomic::Ordering;
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        let bridge = gateway::NavigationBridge::new();
        let _commands = bridge.take_commands().unwrap();
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        let (_, worker) = iced::Task::<()>::none().abortable();
        **gateway = GatewayPanel::new(gateway.id, worker, bridge.clone());
        assert!(bridge.watch_stream("guild:41:127:99").is_ok());
        let watched = bridge.watched().pop().unwrap();
        assert_eq!(app.video_subscription().units(), 0);
        let _ = app.update(Message::ShowStream(true));
        assert_eq!(app.video_subscription().units(), 1);
        // Losing focus keeps the stream playing, like the official client.
        let _ = app.update(Message::WindowActive(false));
        assert_eq!(app.video_subscription().units(), 1);
        assert!(watched.visible.load(Ordering::Acquire));
        let _ = app.update(Message::WindowActive(true));
        assert_eq!(app.video_subscription().units(), 1);
        let _ = app.update(Message::ShowStream(false));
        assert_eq!(app.video_subscription().units(), 0);
        let _ = app.update(Message::ShowStream(true));
        let _ = app.update(Message::OpenAudioSettings);
        assert_eq!(app.video_subscription().units(), 0);
        let _ = app.update(Message::CloseAudioSettings);
        assert_eq!(app.video_subscription().units(), 1);
        let _ = app.update(Message::WindowSize(iced::Size::ZERO));
        assert_eq!(app.video_subscription().units(), 0);
        let _ = app.update(Message::WindowSize(iced::Size::new(640.0, 480.0)));
        assert_eq!(app.video_subscription().units(), 1);
        let _ = app.update(Message::UnwatchStream("guild:41:127:99".into()));
        assert_eq!(app.video_subscription().units(), 0);
        assert!(bridge.watched().is_empty());
        assert!(!watched.active.load(Ordering::Acquire));
        assert!(watched.frame().is_none());
    }

    #[test]
    fn several_watched_streams_are_decoded_only_while_the_layout_draws_them() {
        use crate::call_layout::{Mode, TileId};
        use std::sync::atomic::Ordering;
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        let bridge = gateway::NavigationBridge::new();
        let _commands = bridge.take_commands().unwrap();
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        let (_, worker) = iced::Task::<()>::none().abortable();
        **gateway = GatewayPanel::new(gateway.id, worker, bridge.clone());
        let keys: Vec<String> = (1..=4).map(|n| format!("guild:41:127:{n}")).collect();
        for key in &keys {
            assert!(bridge.watch_stream(key.clone()).is_ok());
        }
        let visible = |bridge: &gateway::NavigationBridge| {
            bridge
                .watched()
                .iter()
                .filter(|session| session.visible.load(Ordering::Acquire))
                .count()
        };
        // Watching alone draws nothing: the call view is closed.
        let _ = app.update(Message::WindowActive(true));
        assert_eq!((visible(&bridge), app.video_subscription().units()), (0, 0));
        let _ = app.update(Message::ShowStream(true));
        assert_eq!((visible(&bridge), app.video_subscription().units()), (4, 4));
        // Focus: still all four (one large, three small); a full view draws one.
        let _ = app.update(Message::SetCallMode(Mode::Focus));
        assert_eq!(visible(&bridge), 4);
        let _ = app.update(Message::FullTile(Some(TileId::Stream(keys[2].clone()))));
        assert_eq!((visible(&bridge), app.video_subscription().units()), (1, 1));
        assert!(bridge.watched()[2].visible.load(Ordering::Acquire));
        let _ = app.update(Message::FullTile(None));
        assert_eq!(visible(&bridge), 4);
        // Minimizing and covering stop every decoder; one stream can leave alone.
        let _ = app.update(Message::WindowSize(iced::Size::ZERO));
        assert_eq!(visible(&bridge), 0);
        let _ = app.update(Message::WindowSize(iced::Size::new(640.0, 480.0)));
        let _ = app.update(Message::UnwatchStream(keys[0].clone()));
        assert_eq!((bridge.watched().len(), visible(&bridge)), (3, 3));
        // Closing the call view hides all; the sessions stay connected.
        let _ = app.update(Message::ShowStream(false));
        assert_eq!((bridge.watched().len(), visible(&bridge)), (3, 0));
        for key in &keys[1..] {
            let _ = app.update(Message::UnwatchStream(key.clone()));
        }
        assert!(bridge.watched().is_empty());
    }

    #[test]
    fn a_voice_channels_chat_and_its_call_are_selected_independently() {
        use fastcord_discord::state::navigation::{
            ChannelKind, ChannelRow, NavigationSnapshot, PermissionSummary, Window,
        };
        let (guild, voice, other) = (Snowflake(10), Snowflake(11), Snowflake(12));
        let row = |id, connect| ChannelRow {
            id,
            name: "Lounge".to_owned(),
            kind: ChannelKind::Voice,
            parent_id: None,
            enabled: true,
            selected: false,
            permissions: PermissionSummary {
                view_channel: true,
                connect,
                ..PermissionSummary::default()
            },
            unread: false,
            mentions: 0,
            thread: None,
            stage: false,
            collapsed: false,
            occupant: None,
        };
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("not logged in");
        };
        gateway.navigation = Arc::new(NavigationSnapshot {
            selected_guild: Some(guild),
            channels: Window {
                offset: 0,
                total: 2,
                rows: vec![row(voice, true), row(other, false)],
            },
            ..NavigationSnapshot::default()
        });
        let state = |app: &App| match &app.phase {
            Phase::Account { gateway, .. } => {
                (gateway.voice_guild, gateway.controls.voice_audio_state())
            }
            _ => panic!("not logged in"),
        };
        // Opening a chat, even of the connectable channel, never joins.
        let _ = app.update(Message::SelectChannel(guild, voice));
        let _ = app.update(Message::SelectChannel(guild, other));
        assert_eq!(state(&app), (None, gateway::VoiceAudioState::Idle));
        // Joining a call does not touch the chat selection (the intent queue
        // holds the last SelectChannel), and a channel without CONNECT is refused.
        let _ = app.update(Message::JoinVoice(guild, other));
        assert_eq!(state(&app), (None, gateway::VoiceAudioState::Idle));
        let _ = app.update(Message::JoinVoice(guild, voice));
        assert_eq!(
            state(&app),
            (Some(guild), gateway::VoiceAudioState::Connecting)
        );
        // Switching the chat while connected neither leaves nor moves the call.
        let _ = app.update(Message::SelectChannel(guild, other));
        assert_eq!(
            state(&app),
            (Some(guild), gateway::VoiceAudioState::Connecting)
        );
    }

    #[test]
    fn the_voice_panel_exists_only_in_a_call_and_sits_above_the_account_footer() {
        use crate::variable_list::harness::Ui;
        use fastcord_discord::state::navigation::{
            ChannelKind, ChannelRow, NavigationSnapshot, PermissionSummary, Window,
        };
        use std::collections::BTreeSet;
        let (guild, voice) = (Snowflake(10), Snowflake(11));
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("not logged in");
        };
        gateway.navigation = Arc::new(NavigationSnapshot {
            selected_guild: Some(guild),
            channels: Window {
                offset: 0,
                total: 1,
                rows: vec![ChannelRow {
                    id: voice,
                    name: "Lounge".to_owned(),
                    kind: ChannelKind::Voice,
                    parent_id: None,
                    enabled: true,
                    selected: false,
                    permissions: PermissionSummary {
                        view_channel: true,
                        connect: true,
                        ..PermissionSummary::default()
                    },
                    unread: false,
                    mentions: 0,
                    thread: None,
                    stage: false,
                    collapsed: false,
                    occupant: None,
                }],
            },
            ..NavigationSnapshot::default()
        });
        // Clicks over the bottom of the sidebar (rail is 72 px, sidebar 240 px).
        let bottom = |app: &App| -> BTreeSet<String> {
            let mut ui = Ui::new(1000.0, 700.0, app.view());
            let mut hit = BTreeSet::new();
            for y in (440..700).step_by(4) {
                for x in (72..312).step_by(4) {
                    for message in ui.click(x as f32, y as f32) {
                        let name = format!("{message:?}");
                        hit.insert(name.split(['(', ' ']).next().unwrap().to_owned());
                    }
                }
            }
            hit
        };
        let idle = bottom(&app);
        assert!(
            !idle.contains("OpenCall") && !idle.contains("LeaveVoice"),
            "{idle:?}"
        );
        // The footer's own actions are there all along.
        assert!(idle.contains("Account"), "{idle:?}");
        let _ = app.update(Message::JoinVoice(guild, voice));
        let joined = bottom(&app);
        for name in ["OpenCall", "LeaveVoice", "ToggleVoiceDetails", "Account"] {
            assert!(joined.contains(name), "{name}: {joined:?}");
        }
        // Mute and deafen stay in the account footer, not in the voice panel.
        assert!(joined.contains("OpenAudioSettings"), "{joined:?}");
    }

    #[test]
    fn closing_quits_by_default_and_hides_only_with_the_tray_chosen_and_up() {
        let mut app = login_app();
        assert!(!app.audio_preferences.close_to_tray, "default is quit");
        let _ = app.update(Message::CloseRequested);
        assert!(!app.window_hidden, "default: the window is not hidden");
        app.audio_preferences.close_to_tray = true;
        let _ = app.update(Message::CloseRequested);
        assert!(!app.window_hidden, "no tray icon, no hiding");
        let _ = app.update(Message::Tray(desktop::TrayEvent::Started));
        let _ = app.update(Message::CloseRequested);
        assert!(app.window_hidden && !app.window_drawable);
        // The tray's Open brings it back.
        let _ = app.update(Message::Tray(desktop::TrayEvent::Command(
            fastcord_platform::tray::Command::Open,
        )));
        assert!(!app.window_hidden && app.window_drawable);
    }

    #[test]
    fn a_link_from_a_second_launch_waits_for_sign_in_and_is_opened_once() {
        use fastcord_platform::instance::Forwarded;
        let mut app = login_app();
        let _ = app.update(Message::Forwarded(Forwarded::Raise));
        assert_eq!(app.pending_link, None);
        let _ = app.update(Message::Forwarded(Forwarded::Open("not a link".into())));
        assert_eq!(app.pending_link, None);
        let _ = app.update(Message::Forwarded(Forwarded::Open(
            "discord://-/channels/1/2/3".into(),
        )));
        assert!(app.pending_link.is_some());
        authenticate(&mut app, Persistence::MemoryOnly);
        assert_eq!(app.pending_link, None, "taken once signed in");
    }
}
