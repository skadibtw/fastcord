#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(all(feature = "fake-discord", not(debug_assertions)))]
compile_error!("fake-discord is a debug-only test mode");

mod account_panel;
mod account_ui;
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
mod byte_lru;
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
mod disk_index;
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
mod handlers;
mod history;
mod i18n;
mod icons;
mod inbox;
mod interactions;
mod invite;
mod localtime;
mod login;
mod login_screen;
mod media_ui;
mod member_menu;
mod message_link;
mod mic_panel;
mod moderation;
mod navigation;
mod network_ui;
mod new_dm;
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
mod server_settings;
mod settings;
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
mod ui;
mod ui_sound;
mod ui_stats;
mod unverified;
mod variable_list;
mod views;
mod virtual_list;
mod voice_log;
mod voice_record;
mod voice_settings;

use crate::attachments::AttachmentKey;
use std::sync::Arc;

use crate::ui::{button, primary_button};
use fastcord_discord::state::navigation::AdminKind;
use fastcord_discord::{CallSignal, IndexScope, RemoteAuthEvent, RestError, UserToken};
use fastcord_model::{Snowflake, VoiceStateRequest};
use fastcord_platform::{NativeCredentialStore, StoreError};
use gateway::{AttachmentAction, GatewayPanel, GatewayStatus};
use iced::widget::{checkbox, container, scrollable, text};
use iced::{Color, Element, Length, Task};
use login::{LoginError, LoginOutcome, Persistence, Session, StoreReady};
#[cfg(test)]
use qr::QrStage;
use qr::{Applied, QrAttempt};
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

/// Opens an http(s) link in the default browser on its own thread: starting a
/// handler can take a while and must not stall the UI (#140). A refused or
/// failed link opens nothing; the caller has nothing to show for it.
fn open_link(url: &str) {
    let url = url.to_owned();
    std::thread::spawn(move || {
        let _ = fastcord_platform::open_url(&url);
    });
}

fn main() -> iced::Result {
    if std::env::args_os().any(|arg| arg == "--packet-helper") {
        let args: Vec<_> = std::env::args().collect();
        if args.len() == 4
            && args[1] == "--packet-helper"
            && let Ok(parent) = args[3].parse()
        {
            let _ = fastcord_platform::network::packet_helper_entry(&args[2], parent);
        }
        return Ok(());
    }
    #[cfg(debug_assertions)]
    if std::env::args_os().any(|arg| arg == "--network-check-login-packet") {
        network_ui::check_login();
        return Ok(());
    }
    #[cfg(debug_assertions)]
    if std::env::args_os().any(|arg| arg == "--network-check") {
        network_ui::check(None);
        return Ok(());
    }
    #[cfg(debug_assertions)]
    if std::env::args_os().any(|arg| arg == "--network-preview") {
        return network_ui::preview();
    }
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
    // Until the settings file is read, which may choose another language.
    i18n::set(i18n::Lang::system());
    // One session per process and per sign-in: a second launch hands its link
    // to the first and exits, never opening a second Gateway on the same token.
    #[cfg(feature = "fake-discord")]
    fake::init();
    // Fake mode takes no part in the hand-off: it neither receives a real
    // instance's links nor is found by one.
    let arg = std::env::args().nth(1).filter(|_| !fake_mode());
    let (forwarded, requests) = tokio::sync::mpsc::channel(8);
    notify::set_clicks(forwarded.clone());
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
    /// What the timeline rows alone ask for, kept until the snapshot changes (#156).
    timeline_wants: TimelineWants,
    network: fastcord_net::NetworkContext,
    network_settings: network_ui::State,
    phase: Phase,
    token: Arc<UserToken>,
    acknowledged: bool,
    memory_only: bool,
    login_ui: login_screen::State,
    notice: Option<String>,
    store: Option<NativeCredentialStore>,
    saved_accounts: Vec<Snowflake>,
    account_index: accounts::Index,
    qr_attempts: u64,
    password_login: String,
    password: Arc<UserToken>,
    captcha_solved: bool,
    gateway_sessions: u64,
    attachment_purge_failed: bool,
    audio_preferences: audio_devices::Preferences,
    audio_settings: audio_devices::Settings,
    /// The settings page shown while the settings screen is open.
    settings_section: settings::Section,
    mic_menu: bool,
    /// Installed speech voices, listed when the chat settings open.
    speech_voices: Vec<fastcord_platform::speech::Voice>,
    audio_saving: Option<audio_devices::Preferences>,
    volume_writer: Option<voice_settings::Writer>,
    volume_notice: Option<String>,
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
    /// Microphone gain, level meter and checks in the audio settings.
    mic_panel: mic_panel::State,
    /// Panels, dialogs and menus of the signed-in account.
    ui: account_ui::AccountUi,
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

impl Phase {
    /// The signed-in account's session and panel. Borrows only the phase, so
    /// a handler can hold them while it changes `App::ui` (#169).
    fn account(&self) -> Option<(&Arc<Session>, &GatewayPanel)> {
        match self {
            Self::Account {
                session, gateway, ..
            } => Some((session, gateway)),
            _ => None,
        }
    }

    fn account_mut(&mut self) -> Option<(&Arc<Session>, &mut GatewayPanel)> {
        match self {
            Self::Account {
                session, gateway, ..
            } => Some((session, gateway)),
            _ => None,
        }
    }

    fn session(&self) -> Option<&Arc<Session>> {
        self.account().map(|(session, _)| session)
    }

    fn gateway(&self) -> Option<&GatewayPanel> {
        self.account().map(|(_, gateway)| gateway)
    }

    fn gateway_mut(&mut self) -> Option<&mut GatewayPanel> {
        self.account_mut().map(|(_, gateway)| gateway)
    }
}

// Every message containing a token uses the redacted wrapper; Debug cannot
// expose text-input edits, worker results, or an authenticated session.
/// The share picker's screens and windows, and the audio nodes to choose from.
type ShareSourceList = (
    Vec<fastcord_platform::capture::CaptureSource>,
    Vec<fastcord_platform::loopback::AudioSource>,
);

/// A popover menu of the main window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Menu {
    /// Under the server's name.
    Server(Snowflake),
    /// The `⋯` of the conversation header.
    Conversation,
    /// The "+" of the message box.
    Composer,
}

/// A tab of the expressions window behind the message box's smiley.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expressions {
    Emoji,
    Gif,
    Sticker,
}

#[derive(Debug, Clone)]
enum Message {
    #[cfg(feature = "fake-discord")]
    FakeStep(fake::Step),
    #[cfg(feature = "fake-discord")]
    FakeShot(std::path::PathBuf, iced::window::Screenshot),
    TokenEdited(Arc<UserToken>),
    InputTooLong,
    Acknowledge(bool),
    LoginUi(login_screen::Event),
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
    VolumeSettingsLoaded,
    VoiceVolumesPurged {
        failed: bool,
    },
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
    VolumeSettingsStatus(u64, voice_settings::Status),
    OpenAudioSettings,
    CloseAudioSettings,
    Settings(settings::Event),
    /// Opens a page of a server's (or channel's) settings screen.
    ServerSettings(Snowflake, server_settings::Section),
    CloseServerSettings,
    /// A menu row was chosen: close the open menus, then do it.
    MenuPick(Box<Message>),
    /// The quick device menu above the microphone.
    MicMenu(bool),
    OpenMenu(Option<Menu>),
    NewDm(new_dm::Event),
    /// Message requests in the direct messages pane.
    ShowRequests,
    /// Switch the expressions window to a tab.
    Expressions(Expressions),
    /// Settings, Advanced: show or hide the unverified-writes list.
    ToggleUnverifiedList,
    /// Turns one group of unverified writes on or off for this session.
    UnverifiedWrite(fastcord_discord::WriteGroup, bool),
    /// Turns every group of unverified writes on or off for this session.
    UnverifiedWriteAll(bool),
    ChatPrefs(chat_prefs::Prefs),
    SpeechPrefs(speech::Prefs),
    SpeechVoices(Vec<fastcord_platform::speech::Voice>),
    SpeechTest,
    StopSpeech,
    RefreshAudioDevices,
    AudioDevicesListed(audio_devices::Inventory),
    Initialized(
        audio_devices::Preferences,
        Box<network_ui::Loaded>,
        Result<StoreReady, StoreError>,
    ),
    Network(network_ui::Action),
    NetworkChanged,
    NetworkProgress,
    SelectAudioDevice(fastcord_audio::Direction, audio_devices::DeviceOption),
    NoiseSuppressionChanged(fastcord_audio::NoiseSuppression),
    EchoCancellationChanged(bool),
    NotificationsToggled(bool),
    NotificationPreviewsToggled(bool),
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
    AvatarLoaded(
        u64,
        avatars::AvatarKey,
        Result<attachments::DecodedImage, attachments::AttachmentError>,
    ),
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
                timeline_wants: TimelineWants::default(),
                network: network_ui::new_context(),
                network_settings: network_ui::State::default(),
                token: Arc::new(UserToken::new(String::new())),
                acknowledged: false,
                memory_only: false,
                login_ui: login_screen::State::default(),
                notice: None,
                store: None,
                saved_accounts: Vec::new(),
                account_index: accounts::Index::default(),
                qr_attempts: 0,
                password_login: String::new(),
                password: Arc::new(UserToken::new(String::new())),
                captcha_solved: false,
                gateway_sessions: 0,
                attachment_purge_failed: false,
                audio_preferences: audio_devices::Preferences::default(),
                audio_settings: audio_devices::Settings::default(),
                settings_section: settings::Section::Voice,
                mic_menu: false,
                speech_voices: Vec::new(),
                audio_saving: None,
                volume_writer: None,
                volume_notice: None,
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
                autostart: (cfg!(any(windows, target_os = "linux", target_os = "macos"))
                    && !fake_mode())
                .then(fastcord_platform::autostart::is_enabled),
                pending_link: desktop::take_startup(),
                window_fullscreen: false,
                mic_panel: mic_panel::State::default(),
                ui: account_ui::AccountUi::default(),
            },
            Task::perform(
                async {
                    paste::purge_stale();
                    voice_record::purge_stale();
                    let preferences = audio_devices::load().await;
                    (
                        preferences,
                        network_ui::load().await,
                        login::initialize_store().await,
                    )
                },
                |(preferences, network, store)| {
                    Message::Initialized(preferences, Box::new(network), store)
                },
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
        ui_stats::update();
        let resynchronize = !matches!(
            message,
            Message::EmojiLoaded(..)
                | Message::AvatarLoaded(..)
                | Message::VideoReady(_)
                | Message::MediaFrame(_)
        );
        if !matches!(self.phase, Phase::Account { .. }) {
            self.ui.friends.purge();
            self.ui.member_menu.reset();
            self.ui.status.purge();
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
        if let Some(gateway) = self.phase.gateway() {
            gateway.controls.quick_query(self.ui.switcher.wanted());
            self.ui.guild_admin.sync(&gateway.navigation);
            self.ui.automod.observe(&gateway.navigation.automod);
            gateway.controls.admin_guild(
                self.ui
                    .guild_admin
                    .wanted()
                    .or(self.ui.announce.wanted_guild())
                    .or(self.ui.moderation.wanted())
                    .or(self.ui.apps.wanted_guild())
                    .or(self.ui.community.wanted())
                    .or(self.ui.automod.wanted())
                    // The settings screen's list needs the server's permissions.
                    .or(self.ui.assets.guild)
                    .or(match self.ui.menu {
                        Some(Menu::Server(guild)) => Some(guild),
                        _ => None,
                    }),
            );
            gateway
                .controls
                .follow_source(self.ui.announce.wanted_follow());
            gateway.controls.assets_guild(self.ui.assets.wanted());
            gateway
                .controls
                .admin_member(self.ui.guild_admin.wanted_member());
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
        let Some((session, gateway)) = self.phase.account_mut() else {
            return Task::none();
        };
        gateway
            .controls
            .emoji_request(&gateway.emoji_picker.request);
        let wanted = wanted_avatars(
            gateway,
            own_user(gateway, session),
            &self.ui.invite,
            &self.ui.discover,
            &mut self.timeline_wants,
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
        if let Some(gateway) = self.phase.gateway() {
            // Focus does not matter: a stream keeps playing in a window that is
            // visible but not focused, like the official client. Only a
            // minimized (zero-size) window or a covering view suspends video.
            let unobstructed = self.window_drawable
                && !self.audio_settings.open
                && gateway.attachment_viewer.is_none()
                && !gateway.emoji_picker.request.open;
            // Only the videos the layout draws are decoded; the rest are
            // suspended and, for cameras, no longer requested from the server.
            let (camera_wanted, speaking) = gateway.controls.voice_controls_with(|c| {
                let speaking: Vec<Snowflake> = c
                    .participants
                    .iter()
                    .filter(|participant| participant.speaking)
                    .map(|participant| participant.user_id)
                    .collect();
                (c.camera.wanted, speaking)
            });
            let tiles = call_layout::tiles(
                &gateway.controls.watched(),
                &gateway.controls.camera_tiles().snapshot(),
                camera_wanted,
            );
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
        let Some(gateway) = self.phase.gateway() else {
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
            iced::Subscription::run(network_ui::changes).map(|()| Message::NetworkChanged),
            iced::Subscription::run_with(
                network_ui::Updates(self.network.controller().clone()),
                network_ui::updates,
            )
            .map(|()| Message::NetworkProgress),
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
            #[cfg(feature = "fake-discord")]
            iced::Subscription::run(fake::script).map(Message::FakeStep),
        ])
    }

    /// Navigation chords (see [`switcher`]): modified keys and Escape while
    /// signed in, plus the plain arrows only while the switcher is open, so
    /// typing (an IME included) never produces a message.
    fn navigation_keys(&self) -> iced::Subscription<Message> {
        if !matches!(self.phase, Phase::Account { .. }) {
            // Before sign-in only the settings screen listens: Esc closes it.
            if !self.audio_settings.open {
                return iced::Subscription::none();
            }
            return iced::event::listen_with(|event, status, _| match event {
                iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
                    ..
                }) => Some(Message::Shortcut(switcher::Shortcut::Escape {
                    captured: matches!(status, iced::event::Status::Captured),
                })),
                _ => None,
            });
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
        if self.ui.switcher.is_open() {
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
        let Some(gateway) = self.phase.gateway_mut() else {
            return Task::none();
        };
        self.ui.threads.close();
        self.ui.friends.close();
        if self.ui.inbox.is_open() {
            self.ui.inbox.clear();
            gateway.controls.inbox_rows(self.ui.inbox.rows_wanted());
        }
        gateway.directs = guild.is_none();
        jump_to(&mut self.ui.search, gateway, guild, channel, message);
        self.ui.nav_pending = true;
        Task::none()
    }

    /// The tray icon's menu and tooltip follow the call and the unread mentions.
    fn sync_tray(&self) {
        let (in_call, muted, mentions) = match &self.phase {
            Phase::Account { gateway, .. } => {
                let in_call = self.call_active();
                (
                    in_call,
                    in_call && gateway.controls.voice_controls_with(|c| c.self_muted),
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
        if let Some(gateway) = self.phase.gateway() {
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
                    if let Some(gateway) = self.phase.gateway() {
                        match kind {
                            Action::PushToTalk => gateway.controls.set_ptt(down),
                            Action::PushToMute => gateway.controls.set_push_to_mute(down),
                            _ => {}
                        }
                    }
                }
                Do::Cancel(kind) => {
                    if let Some(gateway) = self.phase.gateway() {
                        match kind {
                            Action::PushToTalk => gateway.controls.cancel_ptt(),
                            Action::PushToMute => gateway.controls.set_push_to_mute(false),
                            _ => {}
                        }
                    }
                }
                Do::Toggle(kind) => {
                    if let Some(gateway) = self.phase.gateway()
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
        if let Some(gateway) = self.phase.gateway() {
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
        if let Some(gateway) = self.phase.gateway()
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
        if self.ui.voice.ticking() {
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
        if self.ui.soundboard.busy() {
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

    /// The animation wake-up exists only while a visible, active emoji animates,
    /// and fires once, when the earliest frame is due (at most 50 times a
    /// second): every message redraws the whole window, so a fixed 50 Hz timer
    /// redrew it even between frames.
    // Keep specialized dispatch outside the large account-message frame.
    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Network(action) => self.network_event(action),
            #[cfg(feature = "fake-discord")]
            Message::FakeStep(step) => self.fake_step(step),
            message => self.handle_message(message),
        }
    }

    fn network_event(&mut self, action: network_ui::Action) -> Task<Message> {
        match action {
            network_ui::Action::PacketStart => {
                if matches!(self.phase, Phase::Login) && !self.network_settings.saving {
                    self.network_settings.saving = true;
                    self.network_settings.notice =
                        Some("Waiting for Windows administrator approval…");
                    return Task::perform(network_ui::start_packet(), |r| {
                        Message::Network(network_ui::Action::PacketStarted(r))
                    });
                }
            }
            network_ui::Action::PacketStarted(result) => {
                self.network_settings.saving = false;
                if !matches!(self.phase, Phase::Login) {
                    self.network_settings.notice =
                        Some("Sign out before starting packet recovery.");
                    return Task::none();
                }
                match result {
                    Ok(route) => {
                        self.network_settings.packet_active = true;
                        self.network_settings.settings.mode = fastcord_net::settings::Mode::Auto;
                        self.network.controller().network_changed();
                        self.network = self
                            .network
                            .with_mode(fastcord_net::settings::Mode::Auto)
                            .with_prefix_factory(route.0);
                        self.network_settings.saving = true;
                        self.network_settings.notice =
                            Some("Packet helper started. Checking the public Discord API…");
                        return Task::perform(
                            network_ui::packet_check(self.network.clone()),
                            |r| Message::Network(network_ui::Action::PacketChecked(r)),
                        );
                    }
                    Err(e) => self.network_settings.notice = Some(e),
                }
            }
            network_ui::Action::PacketStop => {
                if matches!(self.phase, Phase::Login) && !self.network_settings.saving {
                    self.network_settings.packet_active = false;
                    self.network = self.network.without_prefix_factory();
                    self.network.controller().network_changed();
                    self.network_settings.notice = Some("Packet recovery stopped.");
                }
            }
            network_ui::Action::PacketCheck => {
                if matches!(self.phase, Phase::Login) && !self.network_settings.saving {
                    self.network_settings.saving = true;
                    self.network_settings.notice = Some("Checking the public Discord API…");
                    return Task::perform(network_ui::packet_check(self.network.clone()), |r| {
                        Message::Network(network_ui::Action::PacketChecked(r))
                    });
                }
            }
            network_ui::Action::PacketChecked(result) => {
                self.network_settings.saving = false;
                self.network_settings.notice = Some(result);
            }
            // The network page is a section of the settings screen.
            network_ui::Action::Open => {
                return self.handle(Message::Settings(settings::Event::Open(
                    settings::Section::Network,
                )));
            }
            network_ui::Action::Details(open) => self.network_settings.details = open,
            network_ui::Action::Retry => {
                let context = self.network.clone();
                return Task::perform(
                    async move {
                        context.wait_for_recovery().await;
                    },
                    |()| Message::Network(network_ui::Action::Retried),
                );
            }
            network_ui::Action::Retried => {
                self.network_settings.notice =
                    Some("Pending check finished. Ordinary reconnect limits still apply.")
            }
            network_ui::Action::Mode(mode) => {
                if matches!(self.phase, Phase::Login) && !self.network_settings.saving {
                    self.network_settings.saving = true;
                    self.network.controller().network_changed();
                    self.network_settings.settings.mode = mode;
                    self.network = self.network.with_mode(mode);
                    if mode != fastcord_net::settings::Mode::Auto {
                        self.network_settings.packet_active = false;
                        self.network = self.network.without_prefix_factory();
                    }
                    return Task::perform(
                        network_ui::save(self.network_settings.settings.clone()),
                        |result| Message::Network(network_ui::Action::Saved(result)),
                    );
                }
            }
            network_ui::Action::ProxyKind(kind) => {
                if matches!(self.phase, Phase::Login) {
                    self.network_settings.proxy_kind = Some(kind);
                }
            }
            network_ui::Action::ProxyHost(value) => {
                if matches!(self.phase, Phase::Login) && value.len() <= 253 {
                    self.network_settings.proxy_host = value;
                }
            }
            network_ui::Action::ProxyPort(value) => {
                if matches!(self.phase, Phase::Login) && value.len() <= 5 {
                    self.network_settings.proxy_port = value;
                }
            }
            network_ui::Action::ProxyUsername(value) => {
                if matches!(self.phase, Phase::Login) && value.len() <= 255 {
                    self.network_settings.proxy_username = value;
                }
            }
            network_ui::Action::ProxyPassword(value) => {
                if matches!(self.phase, Phase::Login) && value.expose_secret().len() <= 255 {
                    self.network_settings.proxy_password = Some(value);
                }
            }
            network_ui::Action::ApplyProxy => {
                if matches!(self.phase, Phase::Login) && !self.network_settings.saving {
                    let config = match self.network_settings.proxy_config() {
                        Ok(config) => config,
                        Err(error) => {
                            self.network_settings.notice = Some(error);
                            return Task::none();
                        }
                    };
                    let password = self
                        .network_settings
                        .proxy_password
                        .clone()
                        .unwrap_or_else(|| Arc::new(UserToken::new(String::new())));
                    if self
                        .network
                        .with_proxy(
                            config.clone(),
                            zeroize::Zeroizing::new(password.expose_secret().to_owned()),
                        )
                        .is_err()
                    {
                        self.network_settings.notice = Some("The proxy credentials are invalid.");
                        return Task::none();
                    }
                    let mut settings = self.network_settings.settings.clone();
                    settings.mode = fastcord_net::settings::Mode::Proxy;
                    settings.proxy = Some(config);
                    self.network_settings.saving = true;
                    return Task::perform(network_ui::apply_proxy(settings, password), |result| {
                        Message::Network(network_ui::Action::ProxyApplied(result))
                    });
                }
            }
            network_ui::Action::ProxyApplied(result) => {
                self.network_settings.saving = false;
                match result {
                    Ok(loaded) => {
                        self.network_settings.packet_active = false;
                        self.network = self.network.without_prefix_factory();
                        self.network.controller().network_changed();
                        self.network = network_ui::configure(&self.network, &loaded)
                            .unwrap_or_else(|_| {
                                self.network.with_mode(fastcord_net::settings::Mode::Proxy)
                            });
                        self.network_settings.apply(&loaded);
                        self.network_settings.notice = Some(
                            "Proxy applied. Routing failures will be shown; there is no direct fallback.",
                        );
                    }
                    Err(error) => self.network_settings.notice = Some(error),
                }
            }
            network_ui::Action::Saved(result) => {
                self.network_settings.saving = false;
                self.network_settings.notice =
                    Some(result.err().unwrap_or("Network settings saved."))
            }
        }
        Task::none()
    }

    #[cfg(feature = "fake-discord")]
    fn fake_step(&mut self, step: fake::Step) -> Task<Message> {
        match step {
            fake::Step::Open(text) => {
                self.pending_link = desktop::parse(&text);
                Task::none()
            }
            fake::Step::Size(width, height) => iced::window::latest()
                .and_then(move |id| iced::window::resize(id, iced::Size::new(width, height))),
            fake::Step::Snap(path) => {
                // A watcher may copy the screen itself: overlays drawn by the
                // compositor are missing from the window's own pixels.
                eprintln!("script: snap {}", path.display());
                iced::window::latest()
                    .and_then(iced::window::screenshot)
                    .map(move |shot| Message::FakeShot(path.clone(), shot))
            }
            fake::Step::Toggle(panel) => self.handle(match panel {
                "search" => Message::Search(search::Event::Toggle),
                "pins" => Message::Pins(pins::Event::Toggle),
                "inbox" => Message::Inbox(inbox::Event::Toggle),
                "friends" => Message::Friends(friends::Event::Open),
                "emoji" => Message::EmojiPicker(emoji_picker::Event::Toggle),
                "audio" => Message::OpenAudioSettings,
                "app" => Message::Settings(settings::Event::Open(settings::Section::App)),
                "network" => {
                    if self.network_settings.open {
                        Message::Settings(settings::Event::Close)
                    } else {
                        Message::Network(network_ui::Action::Open)
                    }
                }
                "chat" => Message::Settings(settings::Event::Open(settings::Section::Chat)),
                "login-qr" => {
                    Message::LoginUi(login_screen::Event::Method(login_screen::Method::Qr))
                }
                "login-password" => {
                    Message::LoginUi(login_screen::Event::Method(login_screen::Method::Password))
                }
                "login-token" => {
                    Message::LoginUi(login_screen::Event::Method(login_screen::Method::Token))
                }
                "login-accounts" => Message::LoginUi(login_screen::Event::Accounts),
                "server-menu" => Message::OpenMenu(Some(Menu::Server(Snowflake(5001)))),
                "conversation-menu" => Message::OpenMenu(Some(Menu::Conversation)),
                "account-menu" => Message::Status(status::Event::ToggleMenu),
                "mic-menu" => Message::MicMenu(true),
                "new-dm" => Message::NewDm(new_dm::Event::Open),
                "requests" => Message::ShowRequests,
                "settings" => Message::Settings(settings::Event::Open(settings::Section::Voice)),
                "close-menus" => Message::OpenMenu(None),
                "escape" => Message::Shortcut(switcher::Shortcut::Escape { captured: false }),
                "gif" => Message::Expressions(Expressions::Gif),
                "sticker" => Message::Expressions(Expressions::Sticker),
                "profile" => Message::Settings(settings::Event::Open(settings::Section::Profile)),
                "accounts" => Message::Settings(settings::Event::Open(settings::Section::Accounts)),
                "server-settings" => {
                    Message::GuildAdmin(guild_admin::Event::Settings(Snowflake(5001)))
                }
                "roles" => Message::GuildAdmin(guild_admin::Event::Roles(Snowflake(5001))),
                "group" => Message::Group(group_dm::Event::Toggle),
                "create-channel" => {
                    Message::GuildAdmin(guild_admin::Event::CreateChannel(Snowflake(5001), None))
                }
                "network-details" => {
                    Message::Network(network_ui::Action::Details(!self.network_settings.details))
                }
                "friend-menu" => match &self.phase {
                    Phase::Account { gateway, .. } => Message::Friends(friends::Event::Menu(
                        gateway.navigation.relationships.first().map(|r| r.user.id),
                    )),
                    _ => Message::OpenMenu(None),
                },
                "long-draft" => Message::Composer(composer::Event::Complete {
                    replaced: 0,
                    insertion: "A longer draft
with a second line
and a third
and a fourth"
                        .into(),
                }),
                _ => Message::Account(account_panel::Event::Toggle),
            }),
            fake::Step::Join(guild, channel) => {
                self.handle(Message::JoinVoice(Snowflake(guild), Snowflake(channel)))
            }
            fake::Step::Quit => self.handle(Message::Quit),
        }
    }

    fn handle_message(&mut self, message: Message) -> Task<Message> {
        if matches!(
            message,
            Message::JoinVoice(..) | Message::StartCall(_) | Message::AcceptCall(_)
        ) && !self.network.media_supported()
        {
            let opened = self.network_event(network_ui::Action::Open);
            self.network_settings.notice = Some(
                "This proxy configuration carries TCP only. Voice, camera and screen sharing require a UDP-capable relay; no direct media connection was opened.",
            );
            return opened;
        }
        match message {
            #[cfg(feature = "fake-discord")]
            message @ (Message::FakeStep(..) | Message::FakeShot(..)) => {
                self.desktop_message(message)
            }
            message @ (Message::NetworkProgress
            | Message::NetworkChanged
            | Message::Network(..)
            | Message::Initialized(..)
            | Message::VoiceInput(..)
            | Message::Tray(..)
            | Message::Forwarded(..)
            | Message::CloseRequested
            | Message::Quit
            | Message::Exit
            | Message::ReconnectGateway
            | Message::CloseToTrayToggled(..)
            | Message::AutostartToggled(..)
            | Message::ToggleCompact
            | Message::ShortcutKey(..)
            | Message::GlobalShortcut(..)
            | Message::SetWindowFullscreen(..)
            | Message::WindowActive(..)
            | Message::WindowSize(..)) => self.desktop_message(message),
            message @ (Message::Settings(..)
            | Message::ServerSettings(..)
            | Message::CloseServerSettings
            | Message::MicMenu(..)
            | Message::OpenAudioSettings
            | Message::RefreshAudioDevices
            | Message::CloseAudioSettings
            | Message::MicPanel(..)
            | Message::AudioDevicesListed(..)
            | Message::SelectAudioDevice(..)
            | Message::NoiseSuppressionChanged(..)
            | Message::EchoCancellationChanged(..)
            | Message::SpeechVoices(..)
            | Message::SpeechPrefs(..)
            | Message::SpeechTest
            | Message::StopSpeech
            | Message::ToggleUnverifiedList
            | Message::UnverifiedWrite(..)
            | Message::UnverifiedWriteAll(..)
            | Message::ChatPrefs(..)
            | Message::InterfaceSoundsToggled(..)
            | Message::InterfaceVolumeChanged(..)
            | Message::NotificationsToggled(..)
            | Message::NotificationPreviewsToggled(..)
            | Message::AudioPreferencesSaved(..)) => self.settings_message(message),
            message @ (Message::StartQr
            | Message::CancelQr
            | Message::Qr(..)
            | Message::PasswordLoginEdited(..)
            | Message::PasswordEdited(..)
            | Message::PasswordSubmit
            | Message::PasswordStep(..)
            | Message::CaptchaDone(..)
            | Message::MfaSubmit
            | Message::CancelMfa
            | Message::MfaResult(..)
            | Message::TokenEdited(..)
            | Message::InputTooLong
            | Message::LoginUi(..)
            | Message::Acknowledge(..)
            | Message::MemoryOnly(..)
            | Message::Login
            | Message::RetrySaved
            | Message::StoreReady(..)
            | Message::Restore(..)
            | Message::LoginFinished(..)
            | Message::ContinueInMemory
            | Message::CancelLogin
            | Message::Logout
            | Message::SwitchAccount(..)
            | Message::Account(..)
            | Message::AccountRemoved(..)
            | Message::AccountIndexSaved
            | Message::VolumeSettingsLoaded
            | Message::VoiceVolumesPurged { .. }
            | Message::Forget(..)
            | Message::RetryDelete
            | Message::Deleted(..)) => self.login_message(message),
            message @ (Message::ToggleVoiceDetails
            | Message::JoinVoice(..)
            | Message::StartCall(..)
            | Message::AcceptCall(..)
            | Message::DeclineCall(..)
            | Message::CallSignalled(..)
            | Message::LeaveVoice
            | Message::ActivateParticipant(..)
            | Message::WatchStream(..)
            | Message::SetStreamVolume(..)
            | Message::MuteStream(..)
            | Message::UnwatchStream(..)
            | Message::OpenCall
            | Message::ShowStream(..)
            | Message::SetCallMode(..)
            | Message::PinTile(..)
            | Message::FullTile(..)
            | Message::SetHidePeople(..)
            | Message::OpenSharePicker
            | Message::ShareSources(..)
            | Message::ToggleShareAudio(..)
            | Message::ChooseShareAudio(..)
            | Message::CloseSharePicker
            | Message::StartShare(..)
            | Message::StopShare
            | Message::VideoReady(..)
            | Message::ToggleCamera
            | Message::ListCameras
            | Message::CamerasListed(..)
            | Message::SelectCamera(..)
            | Message::SelectCameraEffect(..)
            | Message::SetMirrorPreview(..)
            | Message::SetSelfMute(..)
            | Message::SetDeafen(..)
            | Message::SetParticipantVolume(..)
            | Message::PresetParticipantVolume(..)
            | Message::CommitParticipantVolume(..)
            | Message::VolumeSettingsStatus(..)) => self.call_message(message),
            message @ (Message::AttachmentPurged(..)
            | Message::AttachmentLoaded(..)
            | Message::AttachmentActionFinished(..)
            | Message::MediaReady(..)
            | Message::MediaFrame(..)
            | Message::MediaTick
            | Message::CloseViewer
            | Message::MarkRead(..)
            | Message::FirstUnread(..)
            | Message::EmojiLoaded(..)
            | Message::AvatarLoaded(..)
            | Message::EmojiVisibility(..)
            | Message::EmojiPicker(..)
            | Message::Timeline(..)
            | Message::FilesPicked(..)
            | Message::ImagePasted(..)
            | Message::Composer(..)
            | Message::Expressions(..)
            | Message::FileDropped(..)) => self.conversation_message(message),
            message @ (Message::MenuPick(..)
            | Message::OpenMenu(..)
            | Message::NewDm(..)
            | Message::ShowRequests
            | Message::Gateway(..)
            | Message::Navigation(..)
            | Message::ShowDirects(..)
            | Message::VoiceRecordingToggled(..)
            | Message::Status(..)
            | Message::Friends(..)
            | Message::SelectGuild(..)
            | Message::SelectChannel(..)
            | Message::SelectPrivateChannel(..)
            | Message::PrivateRecipients(..)
            | Message::OpenPrivate
            | Message::GuildViewport(..)
            | Message::ChannelViewport(..)
            | Message::PrivateViewport(..)) => self.navigation_message(message),
            Message::Search(event) => self.search_event(event),
            Message::Pins(event) => self.pins_event(event),
            Message::Inbox(event) => self.inbox_event(event),
            Message::Organize(event) => self.organize_event(event),
            Message::NotifySettings(event) => self.notify_settings_event(event),
            Message::Switcher(event) => self.switcher_event(event),
            Message::Shortcut(shortcut) => self.navigation_shortcut(shortcut),
            Message::Group(event) => self.group_event(event),
            Message::Invite(event) => self.invite_event(event),
            Message::Announce(event) => self.announce_event(event),
            Message::Apps(event) => self.apps_event(event),
            Message::Community(event) => self.community_event(event),
            Message::Assets(event) => self.assets_event(event),
            Message::GuildAdmin(event) => {
                if matches!(event, guild_admin::Event::MemberRoles(..)) {
                    self.ui.member_menu.close();
                }
                self.guild_admin_event(event)
            }
            Message::Moderation(event) => self.moderation_event(event),
            Message::AutoMod(event) => self.automod_event(event),
            Message::Discover(event) => self.discover_event(event),
            Message::Onboarding(event) => self.onboarding_event(event),
            Message::Events(event) => self.events_event(event),
            Message::Identity(event) => self.identity_event(event),
            Message::Safety(event) => self.safety_event(event),
            Message::Report(event) => self.report_event(event),
            Message::Gif(event) => self.gif_event(event),
            Message::Sticker(event) => self.sticker_event(event),
            Message::Soundboard(event) => self.soundboard_event(event),
            Message::Stage(event) => self.stage_event(event),
            Message::Forward(event) => self.forward_event(event),
            Message::Poll(event) => self.poll_event(event),
            Message::Slash(event) => self.slash_event(event),
            Message::Voice(event) => self.voice_event(event),
            Message::Threads(event) => self.threads_event(event),
            Message::Forum(event) => self.forum_event(event),
            Message::MemberMenu(event) => self.member_menu_event(event),
        }
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
        let open = self.ui.soundboard.is_open();
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

    /// The server or channel settings page that is open: the first open
    /// panel that is one.
    fn server_page(&self) -> Option<(Snowflake, server_settings::Section)> {
        use server_settings::Section;
        self.ui
            .guild_admin
            .page()
            .or_else(|| self.ui.assets.guild.map(|g| (g, Section::Expressions)))
            .or_else(|| self.ui.community.page().map(|g| (g, Section::Community)))
            .or_else(|| self.ui.automod.page().map(|g| (g, Section::AutoMod)))
            .or_else(|| self.ui.apps.page())
            .or_else(|| self.ui.moderation.page())
    }

    /// Closes every panel that is a settings page.
    fn close_server_pages(&mut self) -> Task<Message> {
        let mut closing = Vec::new();
        if self.ui.guild_admin.page().is_some() {
            closing.push(Message::GuildAdmin(guild_admin::Event::Close));
        }
        if self.ui.assets.guild.is_some() {
            closing.push(Message::Assets(assets::Event::Close));
        }
        if self.ui.community.page().is_some() {
            closing.push(Message::Community(community::Event::Close));
        }
        if self.ui.automod.page().is_some() {
            closing.push(Message::AutoMod(automod::Event::Close));
        }
        if self.ui.apps.page().is_some() {
            closing.push(Message::Apps(apps::Event::Close));
        }
        if self.ui.moderation.page().is_some() {
            closing.push(Message::Moderation(moderation::Event::Close));
        }
        Task::batch(
            closing
                .into_iter()
                .map(|m| self.handle(m))
                .collect::<Vec<_>>(),
        )
    }

    /// The open tab of the expressions window, if it is open.
    fn expressions_tab(&self, gateway: &GatewayPanel) -> Option<Expressions> {
        if gateway.emoji_picker.request.open {
            Some(Expressions::Emoji)
        } else if self.ui.gif.open {
            Some(Expressions::Gif)
        } else if gateway
            .timeline
            .channel_id
            .is_some_and(|channel| self.ui.sticker.is_open_for(channel))
        {
            Some(Expressions::Sticker)
        } else {
            None
        }
    }

    /// Wide enough for every header tool.
    fn wide(&self) -> bool {
        self.window_size.width >= 1000.0
    }

    /// Closes every popover menu.
    /// The emoji, GIF and sticker window belongs to the conversation it was
    /// opened for; going elsewhere closes it.
    fn close_expressions(&mut self) {
        self.ui.gif.close();
        self.ui.sticker.close();
        if let Some(gateway) = self.phase.gateway_mut() {
            gateway.emoji_picker.close();
        }
    }

    /// What closes the dialog on top, if one is open (the inner `None`: it
    /// waits for a request and cannot be closed now).
    fn dialog_close(&self) -> Option<Option<Message>> {
        let (session, gateway) = self.phase.account()?;
        let navigation = &gateway.navigation;
        if self.ui.onboarding.is_open() {
            Some(Some(Message::Onboarding(onboarding::Event::Close)))
        } else if self.ui.events.is_open_for(navigation.selected_guild) {
            Some(Some(Message::Events(scheduled_events::Event::Close)))
        } else if self.ui.guild_admin.is_dialog() {
            Some(Some(Message::GuildAdmin(guild_admin::Event::Close)))
        } else if announce::view(&self.ui.announce, navigation).is_some() {
            Some(Some(Message::Announce(announce::Event::Close)))
        } else if self.server_page().is_none() && apps::view(&self.ui.apps, navigation).is_some() {
            Some(Some(Message::Apps(apps::Event::Close)))
        } else if self.ui.invite.is_dialog() {
            Some(self.ui.invite.dismiss().map(Message::Invite))
        } else if discover::view(&self.ui.discover, navigation, &gateway.avatars.views).is_some() {
            Some(Some(Message::Discover(discover::Event::Toggle)))
        } else if group_dm::view(&self.ui.group, navigation, session.user.id).is_some() {
            Some(Some(Message::Group(group_dm::Event::Close)))
        } else if report::view(&self.ui.report, navigation).is_some() {
            Some(Some(Message::Report(report::Event::Close)))
        } else if self.ui.forward.is_open() {
            Some(Some(Message::Forward(forward::Event::Close)))
        } else {
            None
        }
    }

    fn close_menus(&mut self) {
        self.ui.status.open = false;
        self.mic_menu = false;
        self.ui.menu = None;
    }

    fn can_login(&self) -> bool {
        if self.network_settings.saving {
            return false;
        }
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
        if self.network_settings.saving {
            return false;
        }
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
                self.network.clone(),
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
        // Join before account removal can purge its file rows. Shutdown wakes
        // the writer immediately rather than waiting out the coalescing delay.
        if let Some(writer) = self.volume_writer.take() {
            let status = writer.finish();
            if let Some(notice) = status.notice() {
                self.notice = Some(notice.to_owned());
            }
        }
        self.volume_notice = None;
        if let Some(gateway) = self.phase.gateway_mut() {
            // Drafts live in memory only; logout forgets them at once.
            gateway.composer.clear();
        }
        self.ui.reset();
        let (session, gateway) = self.phase.account()?;
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
        let Some(session) = self.phase.session() else {
            return Task::none();
        };
        let rejected = account_panel::State::is_rejected(&event);
        let (task, action) = self.ui.account_panel.update(event, &session.client);
        if rejected && let Some(gateway) = self.phase.gateway() {
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
                open_link(account_panel::SETTINGS_URL);
                task
            }
            None => task,
        }
    }

    fn start_restore(&mut self, account: Snowflake) -> Task<Message> {
        if self.network_settings.saving {
            return Task::none();
        }
        #[cfg(feature = "fake-discord")]
        if fake::active() {
            let _ = account;
            self.phase = Phase::Authenticating {
                saved_account: Some(account),
            };
            return Task::perform(
                login::token_login(
                    self.network.clone(),
                    Arc::new(UserToken::new(fake::TOKEN.to_owned())),
                    None,
                    false,
                ),
                Message::LoginFinished,
            );
        }
        let Some(store) = self.store.clone() else {
            self.notice = Some(LoginError::Store(StoreError::Unavailable).to_string());
            return Task::none();
        };
        self.clear_token();
        self.notice = None;
        self.phase = Phase::Authenticating {
            saved_account: Some(account),
        };
        Task::perform(
            login::restore(self.network.clone(), store, account),
            Message::LoginFinished,
        )
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
        if self.network_settings.saving {
            return Task::none();
        }
        if fake_mode() {
            self.notice = Some("QR login is not available in fake mode.".to_owned());
            return Task::none();
        }
        self.clear_token();
        self.qr_attempts += 1;
        let id = self.qr_attempts;
        let (task, worker) = Task::run(qr::events(self.network.clone()), move |event| {
            Message::Qr(id, event)
        })
        .abortable();
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
        let Some(gateway) = self.phase.gateway() else {
            return;
        };
        let snap = gateway
            .controls
            .voice_controls_with(|controls| ui_sound::Snap {
                state: gateway.controls.voice_audio_state(),
                self_muted: controls.self_muted,
                deafened: controls.deafened,
                push_to_mute: controls.push_to_mute_down,
                ptt: (controls.effective_mode() == shortcuts::InputMode::PushToTalk)
                    .then_some(controls.ptt_down),
                sharing: gateway.controls.share_view().active,
                users: controls.participants.iter().map(|p| p.user_id).collect(),
            });
        ui_sound::observe(snap);
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
        self.enter_account_from(session, persistence, None)
    }

    /// `previous` is the bridge of the same account's stopped worker: its
    /// unconfirmed sends and edits continue in the new one (#149).
    fn enter_account_from(
        &mut self,
        session: Arc<Session>,
        persistence: Persistence,
        previous: Option<gateway::NavigationBridge>,
    ) -> Task<Message> {
        self.gateway_sessions += 1;
        let id = self.gateway_sessions;
        if fake_mode() {
            unverified::enable_all(&session.client);
        }
        let mut controls = gateway::NavigationBridge::new();
        if let Some(previous) = &previous {
            controls.inherit(previous);
        }
        controls.set_account_id(session.user.id);
        let account = session.user.id;
        let volumes_loaded = {
            let controls = controls.clone();
            Task::perform(
                async move { tokio::task::spawn_blocking(move || voice_settings::load(account)).await },
                move |volumes| {
                    // Merged under anything changed meanwhile.
                    controls.set_volume_settings(volumes.unwrap_or_default());
                    Message::VolumeSettingsLoaded
                },
            )
        };
        let (writer, statuses) = voice_settings::Writer::start(account);
        self.volume_writer = Some(writer);
        self.volume_notice = None;
        let volume_events = Task::run(statuses, move |status| {
            Message::VolumeSettingsStatus(id, status)
        });
        controls.set_audio_devices(&self.audio_preferences);
        controls.set_voice_input(&self.audio_preferences.voice_input);
        self.shortcuts.rebuild(&self.audio_preferences.voice_input);
        let stream =
            gateway::status_stream(session.client.clone(), session.token(), controls.clone());
        let (task, worker) = Task::run(stream, move |()| Message::Navigation(id)).abortable();
        let attachment_cache = crate::attachments::AttachmentCache::default_root(
            &self.network,
            session.user.id,
            &session.client.identity().properties().browser_user_agent,
        )
        .ok();
        self.ui.reset();
        self.phase = Phase::Account {
            session,
            persistence,
            gateway: Box::new(
                GatewayPanel::new(id, worker, controls).with_attachment_cache(attachment_cache),
            ),
        };
        self.sync_viewing();
        Task::batch([task, volume_events, volumes_loaded])
    }

    fn purge_voice_volumes(account: Snowflake) -> Task<Message> {
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || voice_settings::purge(account).is_err())
                    .await
                    .unwrap_or(true)
            },
            |failed| Message::VoiceVolumesPurged { failed },
        )
    }

    /// What the timeline shows counts as read only while the window is active
    /// and drawn.
    fn sync_viewing(&self) {
        if let Some(gateway) = self.phase.gateway() {
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

/// The emoji, GIF and sticker window: the emoji grid sets its size.
const EXPRESSIONS_SIZE: iced::Size = iced::Size::new(440.0, 470.0);

/// Most embed pictures asked for at once.
const MAX_WANTED_PICTURES: usize = 16;

/// The part of `wanted_avatars` that depends only on the timeline snapshot:
/// invite codes, authors with stickers (newest first) and embed pictures.
/// Rebuilt when the `Arc` changes, not on every keystroke (#156).
#[derive(Default)]
struct TimelineWants {
    snapshot: Option<Arc<timeline::Snapshot>>,
    codes: Vec<String>,
    authors: Vec<avatars::AvatarKey>,
    pictures: Vec<avatars::AvatarKey>,
    /// Times the timeline rows were scanned (tests).
    #[cfg(test)]
    scans: usize,
}

impl TimelineWants {
    fn of(&mut self, snapshot: &Arc<timeline::Snapshot>) -> &Self {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|kept| Arc::ptr_eq(kept, snapshot))
        {
            return self;
        }
        let rows = &snapshot.rows;
        self.codes = invite::visible_codes(rows);
        self.authors = rows
            .iter()
            .rev()
            .flat_map(|row| {
                let stickers = row.message.sticker_items.iter().filter_map(|sticker| {
                    avatars::AvatarKey::sticker(sticker.id, sticker.format_type)
                });
                std::iter::once(avatars::AvatarKey::user(
                    row.message.author.id,
                    row.message.author.avatar.as_deref(),
                ))
                .chain(stickers)
            })
            .collect();
        // Embed pictures of the newest rows, a few at a time so they never
        // crowd out the avatars.
        self.pictures = rows
            .iter()
            .rev()
            .flat_map(|row| {
                let forwarded = row.message.forwarded().map_or(&[][..], |s| &s.embeds[..]);
                rich::embed_keys(&row.message.embeds).chain(rich::embed_keys(forwarded))
            })
            .take(MAX_WANTED_PICTURES)
            .collect();
        self.snapshot = Some(Arc::clone(snapshot));
        #[cfg(test)]
        {
            self.scans += 1;
        }
        self
    }
}

/// Avatars and icons the shown rows ask for: the user's own, the rail window,
/// then the timeline authors and stickers newest first. Bounded.
fn wanted_avatars(
    gateway: &gateway::GatewayPanel,
    user: &fastcord_model::User,
    invites: &invite::State,
    discover: &discover::State,
    rows_wanted: &mut TimelineWants,
) -> Vec<avatars::AvatarKey> {
    let TimelineWants {
        codes,
        authors,
        pictures,
        ..
    } = rows_wanted.of(&gateway.timeline);
    let mut wanted = vec![avatars::AvatarKey::user(user.id, user.avatar.as_deref())];
    let guilds = gateway
        .navigation
        .guilds
        .rows
        .iter()
        .filter_map(|guild| avatars::AvatarKey::guild(guild.id, guild.icon.as_deref()));
    let invite_icons = invites.cache.icon_keys(codes);
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
    let callers: Vec<_> = gateway.controls.voice_controls_with(|controls| {
        controls
            .participants
            .iter()
            .map(|p| controls.avatar_key(p.user_id))
            .collect()
    });
    let room = avatars::MAX_WANTED - pictures.len();
    let mut seen: std::collections::HashSet<avatars::AvatarKey> = wanted.iter().cloned().collect();
    for key in guilds
        .chain(invite_icons)
        .chain(discover.icon_keys())
        .chain(occupants)
        .chain(callers)
        .chain(authors.iter().cloned())
        .chain(pictures.iter().cloned())
    {
        if wanted.len() == room && !matches!(key, avatars::AvatarKey::Embed { .. }) {
            continue;
        }
        if wanted.len() == avatars::MAX_WANTED {
            break;
        }
        if seen.insert(key.clone()) {
            wanted.push(key);
        }
    }
    wanted
}

#[cfg(test)]
mod tests;
