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
mod unverified;
mod variable_list;
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
use iced::widget::{checkbox, column, container, row, scrollable, text};
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
        if let Phase::Account { gateway, .. } = &self.phase {
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
            &self.ui.invite,
            &self.ui.discover,
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
        let Phase::Account { gateway, .. } = &mut self.phase else {
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
            Message::NetworkProgress => {}
            Message::NetworkChanged => {
                self.network.controller().network_changed();
            }
            Message::Network(action) => return self.network_event(action),
            Message::Initialized(preferences, network, store) => {
                self.network =
                    network_ui::configure(&self.network, &network).unwrap_or_else(|_| {
                        self.network.with_mode(fastcord_net::settings::Mode::Proxy)
                    });
                self.network_settings.apply(&network);
                if let Some(warning) = network.warning {
                    self.notice = Some(warning.to_owned());
                }
                notify::set_enabled(!preferences.notifications_off);
                notify::set_previews(!preferences.notification_previews_off);
                self.ui.soundboard.set_prefs(preferences.soundboard.clone());
                self.audio_preferences = preferences;
                chat_prefs::apply(self.audio_preferences.chat);
                organize::publish(&self.audio_preferences.organize);
                speech::apply(&self.audio_preferences.speech);
                self.configure_ui_sound();
                self.shortcuts.rebuild(&self.audio_preferences.voice_input);
                // Fake mode signs in at once with its fixed token, memory-only.
                // `--login-screen` shows the sign-in screen with two pretend
                // saved accounts; Continue signs in with the fake token.
                #[cfg(feature = "fake-discord")]
                if fake::active() && fake::login_screen() {
                    self.phase = Phase::Login;
                    for (id, name) in [(1001, "Fake Me"), (1002, "Juno")] {
                        self.saved_accounts.push(Snowflake(id));
                        self.account_index.upsert(&fastcord_model::User {
                            id: Snowflake(id),
                            username: name.to_lowercase(),
                            global_name: Some(name.to_owned()),
                            avatar: None,
                            bot: false,
                        });
                    }
                    return Task::none();
                }
                #[cfg(feature = "fake-discord")]
                if fake::active() {
                    self.phase = Phase::Authenticating {
                        saved_account: None,
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
                return self.handle(Message::StoreReady(store));
            }
            Message::VoiceInput(event) => return self.voice_input_event(event),
            #[cfg(feature = "fake-discord")]
            Message::FakeStep(step) => return self.fake_step(step),
            #[cfg(feature = "fake-discord")]
            Message::FakeShot(path, shot) => fake::save(&path, &shot),
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
                let volume_writer = self.volume_writer.take();
                // Leaving the call first lets the server drop the voice state at
                // once. The Gateway then closes with 1000 (waiting at most 2 s
                // for its task) so the session ends instead of lingering until a
                // heartbeat times out; the process exit releases the rest.
                let in_call = self.call_active();
                let controls = match &self.phase {
                    Phase::Account { gateway, .. } => Some(gateway.controls.clone()),
                    _ => None,
                };
                if !in_call && controls.is_none() && volume_writer.is_none() {
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
                            if let Some(writer) = volume_writer {
                                let _ = tokio::task::spawn_blocking(move || writer.finish()).await;
                            }
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
                    // Drafts are the user's, not the worker's: the composer
                    // moves to the new panel (#149).
                    let composer = match &mut self.phase {
                        Phase::Account { gateway, .. } => std::mem::take(&mut gateway.composer),
                        _ => composer::Composer::default(),
                    };
                    // The user asked: the new Gateway task gets fresh
                    // reconnect attempts (#139).
                    session.client.allow_manual_gateway_reconnect();
                    let task = self.enter_account(session, persistence);
                    if let Phase::Account { gateway, .. } = &mut self.phase {
                        gateway.composer = composer;
                    }
                    return task;
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
            Message::Settings(settings::Event::Open(section)) => {
                let opening = !self.audio_settings.open;
                let moved = std::mem::replace(&mut self.settings_section, section) != section;
                self.network_settings.open = section == settings::Section::Network;
                // A new page starts at its title.
                let mut tasks = vec![if moved || opening {
                    iced::widget::operation::scroll_to(
                        settings::PAGE,
                        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                    )
                } else {
                    Task::none()
                }];
                if opening {
                    tasks.push(self.handle(Message::OpenAudioSettings));
                }
                if let Phase::Account { .. } = &self.phase {
                    // The account pages are the existing panels, kept open
                    // while their page is shown.
                    if section == settings::Section::Profile && !self.ui.identity.is_open() {
                        tasks.push(self.handle(Message::Identity(profile_edit::Event::Toggle)));
                    }
                    if section == settings::Section::Accounts && !self.ui.account_panel.is_open() {
                        tasks.push(self.handle(Message::Account(account_panel::Event::Toggle)));
                    }
                    if section == settings::Section::Privacy {
                        if !self.ui.safety.is_open() {
                            tasks.push(self.handle(Message::Safety(safety::Event::Toggle)));
                        }
                        tasks.push(
                            self.handle(Message::Safety(safety::Event::Tab(safety::Tab::Privacy))),
                        );
                    }
                }
                return Task::batch(tasks);
            }
            Message::ServerSettings(guild, section) => {
                let Phase::Account { gateway, .. } = &self.phase else {
                    return Task::none();
                };
                let voice = section.channel().is_some_and(|channel| {
                    gateway.navigation.admin.as_ref().is_some_and(|admin| {
                        admin.channels.iter().any(|c| {
                            c.id == channel && matches!(c.kind, AdminKind::Voice | AdminKind::Stage)
                        })
                    })
                });
                self.close_menus();
                let close = self.close_server_pages();
                use server_settings::Section;
                let open = self.handle(Message::GuildAdmin(match section {
                    Section::Overview => guild_admin::Event::Settings(guild),
                    Section::Roles => guild_admin::Event::Roles(guild),
                    Section::Expressions => guild_admin::Event::Assets(guild),
                    Section::Community => guild_admin::Event::Community(guild),
                    Section::AutoMod => guild_admin::Event::AutoMod(guild),
                    Section::Integrations => guild_admin::Event::Apps(guild),
                    Section::Bans => guild_admin::Event::Bans(guild),
                    Section::Audit => guild_admin::Event::Audit(guild),
                    Section::Channel(channel) => {
                        guild_admin::Event::EditChannel(guild, channel, voice)
                    }
                    Section::Webhooks(channel) => guild_admin::Event::Webhooks(guild, channel),
                }));
                return Task::batch([
                    close,
                    open,
                    iced::widget::operation::scroll_to(
                        settings::PAGE,
                        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                    ),
                ]);
            }
            Message::CloseServerSettings => return self.close_server_pages(),
            Message::MenuPick(message) => {
                self.close_menus();
                return self.handle(*message);
            }
            Message::OpenMenu(menu) => {
                self.close_menus();
                self.ui.menu = menu;
            }
            Message::NewDm(new_dm::Event::Create) => {
                let recipients = self.ui.new_dm.recipients();
                let _ = self.handle(Message::PrivateRecipients(recipients));
                if let Phase::Account {
                    session, gateway, ..
                } = &self.phase
                    && parse_private_recipients(&gateway.private.recipients, session.user.id)
                        .is_ok()
                {
                    self.ui.new_dm.update(new_dm::Event::Close);
                }
                return self.handle(Message::OpenPrivate);
            }
            Message::NewDm(event) => {
                let opening = matches!(event, new_dm::Event::Open);
                self.ui.new_dm.update(event);
                if opening {
                    self.close_menus();
                    if let Phase::Account { gateway, .. } = &mut self.phase {
                        gateway.private.notice = None;
                    }
                    return iced::widget::operation::focus("new-dm-search");
                }
            }
            Message::ShowRequests => {
                self.ui.friends.close();
                self.ui.requests_open = true;
                let mut tasks = vec![self.handle(Message::ShowDirects(true))];
                if !self.ui.safety.is_open() {
                    tasks.push(self.handle(Message::Safety(safety::Event::Toggle)));
                }
                tasks.push(self.handle(Message::Safety(safety::Event::Tab(safety::Tab::Requests))));
                return Task::batch(tasks);
            }
            Message::MicMenu(open) => {
                self.mic_menu = open;
                if open && self.audio_settings.inputs.is_empty() && !self.audio_settings.probing {
                    self.audio_settings.probing = true;
                    return Task::perform(audio_devices::enumerate(), Message::AudioDevicesListed);
                }
            }
            Message::Settings(settings::Event::Close) => {
                return self.handle(Message::CloseAudioSettings);
            }
            Message::OpenAudioSettings if !self.audio_settings.open => {
                self.close_menus();
                self.audio_settings.open = true;
                let voices = Task::perform(speech::load_voices(), |voices| {
                    Message::SpeechVoices(voices.unwrap_or_default())
                });
                return Task::batch([voices, self.handle(Message::RefreshAudioDevices)]);
            }
            Message::OpenAudioSettings | Message::RefreshAudioDevices => {
                if let Phase::Account { gateway, .. } = &self.phase
                    && self.audio_settings.open
                {
                    gateway.controls.set_diagnostics_open(true);
                }
                if !self.audio_settings.probing {
                    self.audio_settings.probing = true;
                    return Task::perform(audio_devices::enumerate(), Message::AudioDevicesListed);
                }
            }
            Message::CloseAudioSettings => {
                self.audio_settings.open = false;
                self.network_settings.open = false;
                self.mic_panel.close();
                let mut closing = Vec::new();
                if self.ui.identity.is_open() {
                    closing.push(Message::Identity(profile_edit::Event::Toggle));
                }
                if self.ui.account_panel.is_open() {
                    closing.push(Message::Account(account_panel::Event::Toggle));
                }
                if self.ui.safety.is_open() {
                    closing.push(Message::Safety(safety::Event::Close));
                }
                if !closing.is_empty() {
                    let tasks: Vec<_> = closing.into_iter().map(|m| self.handle(m)).collect();
                    if let Phase::Account { gateway, .. } = &self.phase {
                        gateway.controls.set_diagnostics_open(false);
                    }
                    return Task::batch(tasks);
                }
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
            Message::ToggleUnverifiedList => self.ui.unverified_open = !self.ui.unverified_open,
            Message::UnverifiedWrite(group, on) => {
                if let Phase::Account { session, .. } = &self.phase {
                    session.client.set_write_group(group, on);
                }
            }
            Message::UnverifiedWriteAll(on) => {
                if let Phase::Account { session, .. } = &self.phase {
                    unverified::set_all(&session.client, on);
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
            Message::NotificationPreviewsToggled(on) => {
                self.audio_preferences.notification_previews_off = !on;
                notify::set_previews(on);
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
                        login::token_login(
                            self.network.clone(),
                            token,
                            self.store.clone(),
                            !self.memory_only,
                        ),
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
                            login::token_login(
                                self.network.clone(),
                                token,
                                self.store.clone(),
                                !self.memory_only,
                            ),
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
                return Task::perform(
                    password::second_factor(self.network.clone(), challenge, code),
                    Message::MfaResult,
                );
            }
            Message::CancelMfa if matches!(self.phase, Phase::Mfa(_)) => {
                self.clear_token();
                self.phase = Phase::Login;
            }
            Message::MfaResult(result) if matches!(self.phase, Phase::Authenticating { .. }) => {
                match result {
                    Ok(token) => {
                        return Task::perform(
                            login::token_login(
                                self.network.clone(),
                                token,
                                self.store.clone(),
                                !self.memory_only,
                            ),
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
            Message::LoginUi(event) => return self.login_ui.update(event),
            Message::Acknowledge(value) if matches!(self.phase, Phase::Login | Phase::Qr(_)) => {
                self.acknowledged = value;
            }
            Message::MemoryOnly(value) if matches!(self.phase, Phase::Login | Phase::Qr(_)) => {
                self.memory_only = value;
            }
            Message::Login if matches!(self.phase, Phase::Login) && self.can_login() => {
                let token = self.take_input_token();
                self.notice = None;
                self.phase = Phase::Authenticating {
                    saved_account: None,
                };
                return Task::perform(
                    login::token_login(
                        self.network.clone(),
                        token,
                        self.store.clone(),
                        !self.memory_only,
                    ),
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
                        self.notice = Some(error.notice(&self.network));
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
                // Every snapshot carries the last status, so applying it must not
                // end the update: the follow-up below runs unless it ended the account.
                let mut status_task = Task::none();
                if let Some(status) = status {
                    status_task = self.handle(Message::Gateway(id, status));
                    if !matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == id) {
                        return status_task;
                    }
                }
                if let Phase::Account { gateway, .. } = &self.phase
                    && let Some(message) = self
                        .ui
                        .search
                        .take_pending_jump(gateway.timeline.channel_id)
                    && let Some(channel) = gateway.timeline.channel_id
                {
                    gateway
                        .controls
                        .timeline_intent(history::Intent::JumpTo { channel, message });
                }
                if self.ui.search.take_jump_expired() || self.ui.inbox.take_expired() {
                    self.notice = Some("That message is in a channel you cannot open.".to_owned());
                }
                if let Phase::Account { gateway, .. } = &self.phase
                    && let Some((channel, target)) =
                        self.ui.inbox.take_pending(gateway.timeline.channel_id)
                {
                    inbox_jump(&gateway.controls, channel, target);
                }
                let mut invite_task = Task::none();
                if let Phase::Account {
                    gateway, session, ..
                } = &self.phase
                {
                    self.ui.sticker.sync(gateway.timeline.channel_id);
                    self.ui.invite.cache.sync_guilds(&gateway.navigation);
                    invite_task = self
                        .ui
                        .invite
                        .pump(&gateway.timeline.rows, &session.client)
                        .map(Message::Invite);
                    self.ui.forward.sync(gateway.timeline.channel_id);
                    self.ui.announce.sync(gateway.timeline.channel_id);
                    self.ui.group.sync(
                        gateway
                            .navigation
                            .private_selection
                            .as_ref()
                            .map(|selection| selection.channel_id),
                    );
                    self.ui.identity.sync(&gateway.navigation);
                    self.ui.safety.sync(&gateway.navigation);
                    self.ui
                        .report
                        .sync(self.ui.report.watched().and_then(|(channel, message)| {
                            timeline::is_retained(&gateway.timeline, channel, message)
                        }));
                    self.ui.poll.sync(gateway.timeline.channel_id);
                    self.ui.slash.sync(gateway.timeline.channel_id);
                    self.ui
                        .slash
                        .modal_shown(gateway.timeline.interaction.modal.as_ref());
                    self.ui.voice.sync(gateway.timeline.channel_id);
                }
                // A navigation the user started (keys, switcher, sidebar) has
                // landed: the composer takes focus back, and the sidebar shows
                // servers or direct messages to match what opened.
                let mut refocus = Task::none();
                if self.ui.nav_pending
                    && let Phase::Account { gateway, .. } = &mut self.phase
                    && gateway.timeline.channel_id.is_some()
                    && gateway.timeline.channel_id != before
                {
                    self.ui.nav_pending = false;
                    gateway.directs = gateway.navigation.private_selection.is_some();
                    refocus = iced::widget::operation::focus(composer::EDITOR_ID);
                }
                self.observe_ui_sound();
                return Task::batch([
                    status_task,
                    refocus,
                    self.soundboard_follow(),
                    self.assets_follow(),
                    invite_task,
                ]);
            }
            Message::ShowDirects(on) => {
                self.close_expressions();
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.directs = on;
                }
            }
            Message::ToggleVoiceDetails => {
                self.ui.voice_details = !self.ui.voice_details;
                if !self.ui.voice_details
                    && let Some(writer) = &self.volume_writer
                {
                    writer.flush();
                }
            }
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
                    self.ui.member_menu.close();
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
                    self.ui.voice.reset();
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
                        .ui
                        .status
                        .update(event, own, &session.client)
                        .map(Message::Status);
                }
            }
            Message::Threads(event) => return self.threads_event(event),
            Message::Forum(event) => return self.forum_event(event),
            Message::Friends(event) => {
                self.ui.requests_open = false;
                if matches!(event, friends::Event::Open) {
                    self.close_expressions();
                }
                if matches!(
                    event,
                    friends::Event::Profile(_)
                        | friends::Event::OpenDm(_)
                        | friends::Event::Report(_)
                ) {
                    self.ui.member_menu.close();
                }
                if let Phase::Account {
                    session, gateway, ..
                } = &mut self.phase
                {
                    match event {
                        friends::Event::OpenDm(user) => {
                            self.ui.friends.close();
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
                                .ui
                                .friends
                                .update(friends::Event::CloseProfile, &session.client);
                            return self
                                .ui
                                .report
                                .begin(subject, &session.client)
                                .map(Message::Report);
                        }
                        event => {
                            if matches!(event, friends::Event::Open) {
                                gateway.directs = true;
                            }
                            return self
                                .ui
                                .friends
                                .update_for(
                                    event,
                                    &gateway.navigation.relationships,
                                    &session.client,
                                )
                                .map(Message::Friends);
                        }
                    }
                }
            }
            Message::SelectGuild(guild) => {
                self.close_expressions();
                self.ui.requests_open = false;
                self.ui.member_menu.close();
                self.ui.threads.close();
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
                self.close_expressions();
                self.ui.member_menu.close();
                // Opens a channel's messages (a voice or Stage channel's chat
                // included); it never touches the voice connection.
                self.ui.threads.close();
                self.ui.nav_pending = true;
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
                        self.ui.stage.reset();
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
                if let Some(writer) = &self.volume_writer {
                    writer.flush();
                }
                self.ui.stage.reset();
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
                self.ui.member_menu.close();
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
                        self.ui.threads.close();
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
            // Both keyboard/drag changes and presets reach the same bounded writer.
            Message::SetParticipantVolume(user, percent)
            | Message::PresetParticipantVolume(user, percent) => {
                if let Phase::Account { gateway, .. } = &self.phase {
                    gateway.controls.set_volume_percent(user, percent);
                    if let Some(writer) = &self.volume_writer
                        && !writer.set(user, percent)
                    {
                        self.volume_notice =
                            voice_settings::Status::Failed.notice().map(str::to_owned);
                    }
                }
            }
            Message::CommitParticipantVolume(user) => {
                if let Some(writer) = &self.volume_writer
                    && let Phase::Account { gateway, .. } = &self.phase
                {
                    writer.set(user, gateway.controls.voice_controls().volume_percent(user));
                    writer.flush();
                }
            }
            Message::VolumeSettingsStatus(id, status) => {
                if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == id) {
                    self.volume_notice = status.notice().map(str::to_owned);
                }
            }
            Message::SelectPrivateChannel(channel) => {
                self.close_expressions();
                self.ui.requests_open = false;
                self.ui.threads.close();
                self.ui.nav_pending = true;
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    self.ui.friends.close();
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
                                    &mut self.ui.search,
                                    gateway,
                                    link.guild,
                                    link.channel,
                                    Some(link.message),
                                );
                            } else if let Some(url) = rich::link_target(&url) {
                                open_link(url.as_str());
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
                                .ui
                                .report
                                .begin(subject, &session.client)
                                .map(Message::Report);
                        }
                        timeline::Event::OpenProfile(user) => {
                            return self
                                .ui
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
                                    .ui
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
                                self.ui.threads.close();
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
            Message::Composer(composer::Event::Plus) => {
                return self.handle(Message::OpenMenu(Some(Menu::Composer)));
            }
            Message::Expressions(tab) => {
                let Phase::Account { gateway, .. } = &self.phase else {
                    return Task::none();
                };
                let open = self.expressions_tab(gateway);
                let mut steps = Vec::new();
                for (each, message) in [
                    (
                        Expressions::Emoji,
                        Message::EmojiPicker(emoji_picker::Event::Toggle),
                    ),
                    (Expressions::Gif, Message::Gif(gif::Event::Toggle)),
                    (
                        Expressions::Sticker,
                        Message::Sticker(sticker::Event::Toggle),
                    ),
                ] {
                    // Toggle off the open tab, toggle on the chosen one.
                    if (open == Some(each)) != (tab == each) {
                        steps.push(message);
                    }
                }
                let tasks: Vec<_> = steps.into_iter().map(|m| self.handle(m)).collect();
                return Task::batch(tasks);
            }
            Message::Composer(composer::Event::Toggle(composer::Picker::Emoji))
                if matches!(&self.phase, Phase::Account { gateway, .. }
                    if self.expressions_tab(gateway).is_some_and(|tab| tab != Expressions::Emoji)) =>
            {
                // The smiley closes the window whichever tab is shown.
                let Phase::Account { gateway, .. } = &self.phase else {
                    return Task::none();
                };
                let tab = self.expressions_tab(gateway);
                return self.handle(match tab {
                    Some(Expressions::Gif) => Message::Gif(gif::Event::Toggle),
                    _ => Message::Sticker(sticker::Event::Toggle),
                });
            }
            Message::Composer(composer::Event::Toggle(picker)) => {
                return self.handle(match picker {
                    composer::Picker::Emoji => Message::EmojiPicker(emoji_picker::Event::Toggle),
                    composer::Picker::Poll => Message::Poll(poll_create::Event::Toggle),
                });
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
                    // A clicked completion leaves the focus on its button; take it back.
                    let refocus = matches!(
                        event,
                        composer::Event::Send
                            | composer::Event::Edit(_)
                            | composer::Event::Complete { .. }
                    );
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
                self.ui
                    .account_panel
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

    fn share_picker_view<'a>(
        picker: &'a gateway::SharePicker,
        share_audio: bool,
        audio_sources: &'a [fastcord_platform::loopback::AudioSource],
        audio_target: Option<fastcord_platform::loopback::AudioTarget>,
    ) -> Element<'a, Message> {
        use fastcord_platform::capture::SourceKind;
        let mut content = column![
            row![
                text(i18n::tr("Share your screen")).size(22).width(Length::Fill),
                button(text(i18n::tr("Cancel"))).on_press(Message::CloseSharePicker),
            ]
            .spacing(12),
            text(i18n::tr(if cfg!(windows) {
                "Choose a screen or a window. Windows draws a border around what is shared, and your voice stays connected."
            } else if cfg!(target_os = "linux") {
                "Choose a screen or a window; your desktop then asks which one in its own dialog. Your voice stays connected."
            } else {
                "Choose a screen or a window. Your voice stays connected."
            }))
            .size(12),
        ]
        .spacing(12);
        match picker {
            gateway::SharePicker::Closed => {}
            gateway::SharePicker::Loading => {
                content = content.push(text(i18n::tr("Looking for screens and windows…")));
            }
            gateway::SharePicker::Failed(error) => {
                content = content.push(text(i18n::tr(error).to_owned()));
            }
            gateway::SharePicker::Listed(sources) => {
                let mut screens = column![text(i18n::tr("Screens")).size(16)].spacing(6);
                let mut windows = column![text(i18n::tr("Windows")).size(16)].spacing(6);
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
                    windows =
                        windows.push(text(i18n::tr("No shareable windows are open.")).size(12));
                }
                let offer = gateway::AudioOffer::current();
                if offer != gateway::AudioOffer::Unavailable {
                    let mut audio = column![
                        checkbox(share_audio)
                            .label(i18n::tr("Share audio"))
                            .on_toggle(Message::ToggleShareAudio)
                    ]
                    .spacing(4);
                    audio = audio.push(text(offer.note()).size(12));
                    if offer == gateway::AudioOffer::Choose && share_audio {
                        if audio_sources.is_empty() {
                            audio = audio.push(
                                text(i18n::tr("No PipeWire outputs or inputs were found."))
                                    .size(12),
                            );
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
                            text(i18n::tr(
                                "Warning: viewers may hear your call while this is on.",
                            ))
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

    /// The menu under the server's name: invite and manage first, then the
    /// server's pages and alerts, then report and leave. Management rows are
    /// greyed out until the server's permissions are known and when missing.
    fn server_menu<'a>(
        &'a self,
        gateway: &'a GatewayPanel,
        guild: Snowflake,
    ) -> Element<'a, Message> {
        let pick = |message| Some(Message::MenuPick(Box::new(message)));
        let admin = gateway
            .navigation
            .admin
            .as_ref()
            .filter(|admin| admin.id == guild);
        let can = |allowed: fn(&fastcord_discord::state::navigation::GuildAdmin) -> bool| {
            admin.is_some_and(allowed)
        };
        let invite_channel = gateway
            .navigation
            .selection
            .as_ref()
            .filter(|s| s.guild_id == guild)
            .filter(|s| {
                s.permissions.create_invite
                    || s.permissions.manage_guild
                    || s.permissions.manage_channel
            })
            .map(|s| s.channel_id);
        let admin_event = |event| Message::GuildAdmin(event);
        let mut rows = column![ui::item(
            Some(icons::Icon::Invite),
            i18n::tr("Invite people"),
            invite_channel
                .and_then(|channel| pick(Message::Invite(invite::Event::Create(guild, channel)))),
        ),]
        .spacing(1);
        // Only what this account may do is listed; every management page
        // is a section of the one settings screen.
        let settings = server_settings::Section::first(admin);
        if settings.is_some() || can(|a| a.can_manage_channels) {
            rows = rows.push(ui::separator());
        }
        if let Some(section) = settings {
            rows = rows.push(ui::item(
                Some(icons::Icon::Settings),
                i18n::tr("Server settings"),
                pick(Message::ServerSettings(guild, section)),
            ));
        }
        if can(|a| a.can_manage_channels) {
            rows = rows.push(ui::item(
                Some(icons::Icon::Plus),
                i18n::tr("Create channel"),
                pick(admin_event(guild_admin::Event::CreateChannel(guild, None))),
            ));
        }
        rows = rows
            .push(ui::separator())
            .push(ui::item(
                Some(icons::Icon::Calendar),
                i18n::tr("Events"),
                pick(Message::Events(scheduled_events::Event::Open(guild))),
            ))
            .push(ui::item(
                Some(icons::Icon::Info),
                i18n::tr("Server guide"),
                pick(Message::Onboarding(onboarding::Event::OpenGuide(guild))),
            ))
            .push(ui::item(
                Some(icons::Icon::Grid),
                i18n::tr("Channels and roles"),
                pick(Message::Onboarding(onboarding::Event::OpenQuestions(
                    guild, true,
                ))),
            ))
            .push(ui::item(
                Some(icons::Icon::Bell),
                i18n::tr("Notification settings"),
                pick(Message::NotifySettings(notify_settings::Event::Toggle)),
            ))
            .push(ui::separator())
            .push(ui::item(
                Some(icons::Icon::Report),
                i18n::tr("Report server"),
                pick(Message::Report(report::Event::OpenServer)),
            ))
            .push(ui::danger_item(
                Some(icons::Icon::Exit),
                i18n::tr("Leave server"),
                pick(admin_event(guild_admin::Event::Leave(guild))),
            ));
        ui::popover(
            ui::menu_card(rows, 232.0),
            ui::Anchor::TopLeft(navigation::RAIL_WIDTH + 4.0, 50.0),
            Message::OpenMenu(None),
        )
    }

    /// The `⋯` menu of the open conversation, under the header's right end.
    fn conversation_menu<'a>(&'a self, gateway: &'a GatewayPanel) -> Element<'a, Message> {
        let mut rows = column![].spacing(1);
        for (label, icon, message) in
            navigation::conversation_menu(&gateway.navigation, gateway.directs, self.wide())
        {
            rows = rows.push(ui::item(
                Some(icon),
                i18n::tr(label),
                Some(Message::MenuPick(Box::new(message))),
            ));
        }
        ui::popover(
            ui::menu_card(rows, 232.0),
            ui::Anchor::TopRight(12.0, 46.0),
            Message::OpenMenu(None),
        )
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

    /// The server or channel settings screen around the open page's panel.
    fn server_screen<'a>(
        &'a self,
        gateway: &'a GatewayPanel,
        guild: Snowflake,
        section: server_settings::Section,
    ) -> Element<'a, Message> {
        use server_settings::Section;
        let navigation = &gateway.navigation;
        let empty = || -> Element<'a, Message> { text("").into() };
        let content: Element<'a, Message> = match section {
            Section::Overview | Section::Roles | Section::Channel(_) => {
                guild_admin::view(&self.ui.guild_admin, navigation)
                    .map_or_else(empty, |panel| panel.map(Message::GuildAdmin))
            }
            Section::Expressions => assets::view(&self.ui.assets, navigation)
                .map_or_else(empty, |panel| panel.map(Message::Assets)),
            Section::Community => community::view(&self.ui.community, navigation)
                .map_or_else(empty, |panel| panel.map(Message::Community)),
            Section::AutoMod => automod::view(&self.ui.automod, navigation)
                .map_or_else(empty, |panel| panel.map(Message::AutoMod)),
            Section::Integrations | Section::Webhooks(_) => apps::view(&self.ui.apps, navigation)
                .map_or_else(empty, |panel| panel.map(Message::Apps)),
            Section::Bans | Section::Audit => moderation::view(&self.ui.moderation, navigation)
                .map_or_else(empty, |panel| panel.map(Message::Moderation)),
        };
        let admin = navigation.admin.as_ref().filter(|admin| admin.id == guild);
        let guild_name = admin.map(|admin| admin.name.as_str()).or_else(|| {
            navigation
                .guilds
                .rows
                .iter()
                .find(|row| row.id == guild)
                .map(|row| row.name.as_str())
        });
        let channel = section.channel().and_then(|id| {
            admin?.channels.iter().find(|c| c.id == id).map(|c| {
                (
                    c.name.as_str(),
                    matches!(c.kind, AdminKind::Text | AdminKind::Announcement),
                )
            })
        });
        server_settings::view(
            server_settings::Page {
                guild,
                guild_name: guild_name.unwrap_or(""),
                section,
                channel,
                admin,
            },
            content,
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

    /// Emoji, GIF and stickers in one window above the message box. A
    /// reaction or an edit offers emoji only.
    fn expressions_view<'a>(&'a self, gateway: &'a GatewayPanel) -> Element<'a, Message> {
        let tab = self.expressions_tab(gateway).unwrap_or(Expressions::Emoji);
        let body: Element<'a, Message> = match tab {
            Expressions::Emoji => emoji_picker::view(
                &gateway.emoji_picker,
                &gateway.emoji_snapshot,
                &gateway.emoji.views,
            ),
            Expressions::Gif => gif::view(&self.ui.gif)
                .map_or_else(|| text("").into(), |panel| panel.map(Message::Gif)),
            Expressions::Sticker => sticker::view(&self.ui.sticker)
                .map_or_else(|| text("").into(), |panel| panel.map(Message::Sticker)),
        };
        let full = gateway.emoji_picker.reaction().is_none()
            && gateway.composer.editing().is_none()
            && gateway.timeline.can_send;
        // One window of one size whatever the tab: the tabs and the cross
        // never move under the pointer. A reaction or an edit offers emoji
        // only, under a title instead of the tabs.
        let head: Element<'a, Message> = if full {
            ui::tabs(
                &[
                    (Expressions::Emoji, i18n::tr("Emoji")),
                    (Expressions::Gif, "GIF"),
                    (Expressions::Sticker, i18n::tr("Stickers")),
                ],
                tab,
                Message::Expressions,
            )
        } else {
            text(if gateway.emoji_picker.reaction().is_some() {
                i18n::tr("React with emoji")
            } else {
                i18n::tr("Emoji")
            })
            .size(16)
            .font(ui::BOLD)
            .into()
        };
        let close = match tab {
            Expressions::Emoji => Message::EmojiPicker(emoji_picker::Event::Close),
            Expressions::Gif => Message::Gif(gif::Event::Toggle),
            Expressions::Sticker => Message::Sticker(sticker::Event::Toggle),
        };
        container(
            column![
                row![
                    container(head).width(Length::Fill),
                    icons::close(Some(close)),
                ]
                .spacing(8)
                .height(36)
                .align_y(iced::Alignment::Center),
                container(body).height(Length::Fill),
            ]
            .spacing(10),
        )
        .padding(12)
        .width(EXPRESSIONS_SIZE.width)
        .height(EXPRESSIONS_SIZE.height)
        .style(style::card)
        .into()
    }

    /// The "+" menu of the message box.
    fn composer_menu<'a>(&'a self, gateway: &'a GatewayPanel) -> Element<'a, Message> {
        let pick = |message| Some(Message::MenuPick(Box::new(message)));
        let can_send = gateway.timeline.can_send;
        let mut rows = column![
            ui::item(
                Some(icons::Icon::Attach),
                i18n::tr("Attach a file"),
                can_send
                    .then(|| pick(Message::Composer(composer::Event::Attach)))
                    .flatten(),
            ),
            ui::item(
                Some(icons::Icon::Paste),
                i18n::tr("Paste image"),
                can_send
                    .then(|| pick(Message::Composer(composer::Event::PasteImage)))
                    .flatten(),
            ),
            ui::item(
                Some(icons::Icon::Poll),
                i18n::tr("Create poll"),
                can_send
                    .then(|| pick(Message::Composer(composer::Event::Toggle(
                        composer::Picker::Poll
                    ))))
                    .flatten(),
            ),
        ]
        .spacing(1);
        if self.audio_preferences.voice_recording {
            rows = rows.push(ui::item(
                Some(icons::Icon::Record),
                i18n::tr("Voice message"),
                can_send
                    .then(|| pick(Message::Voice(voice_record::Event::Start)))
                    .flatten(),
            ));
        }
        ui::popover(
            ui::menu_card(rows, 232.0),
            ui::Anchor::BottomLeft(
                navigation::RAIL_WIDTH + navigation::SIDEBAR_WIDTH + 12.0,
                64.0,
            ),
            Message::OpenMenu(None),
        )
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
        if let Phase::Account { gateway, .. } = &mut self.phase {
            gateway.emoji_picker.close();
        }
    }

    /// What closes the dialog on top, if one is open (the inner `None`: it
    /// waits for a request and cannot be closed now).
    fn dialog_close(&self) -> Option<Option<Message>> {
        let Phase::Account {
            session, gateway, ..
        } = &self.phase
        else {
            return None;
        };
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
        if let Phase::Account { gateway, .. } = &mut self.phase {
            // Drafts live in memory only; logout forgets them at once.
            gateway.composer.clear();
        }
        self.ui.reset();
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
        let (task, action) = self.ui.account_panel.update(event, &session.client);
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
        let account = session.user.id;
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
        Task::batch([task, volume_events])
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
            self.ui
                .forum
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
                if let Some((guild, thread)) = self.ui.threads.archived_thread(index) {
                    self.ui.threads.close();
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
                self.ui
                    .threads
                    .set_state((thread, archived, locked), allowed, &session.client)
                    .map(Message::Threads)
            }
            threads::Event::Created(Ok(thread)) => {
                self.ui.threads.close();
                if let Some(guild) = thread.guild_id {
                    gateway.controls.open_thread(guild, *thread);
                }
                Task::none()
            }
            event => self
                .ui
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
            if let Some(url) = self.ui.gif.url(index).map(str::to_owned)
                && gateway.composer.insert_emoji(&url)
            {
                self.ui.gif.close();
                return iced::widget::operation::focus(composer::EDITOR_ID);
            }
            return Task::none();
        }
        if matches!(event, gif::Event::Toggle) && !self.ui.gif.open && !gateway.timeline.can_send {
            return Task::none();
        }
        self.ui
            .gif
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
        let task = self.ui.assets.update(event, &env);
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
            .ui
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
            .ui
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
                open_link(&url);
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
        let task = self.ui.announce.update(
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
        self.ui
            .forward
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
        self.ui
            .slash
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
        let (task, used) = self.ui.slash.update(event, &env);
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
        self.ui
            .poll
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
        let (task, file) = self.ui.voice.update(event, &context);
        if let Some(file) = file {
            if gateway.controls.send_message_with_files(
                channel,
                String::new(),
                vec![file],
                false,
                None,
            ) {
                self.ui.voice.sent();
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
        match self.ui.soundboard.take_dirty_prefs() {
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
        self.ui.soundboard.sync_call(env.voice, env.deafened);
        let mut tasks = Vec::new();
        for cue in &controls.sound_cues {
            tasks.push(
                self.ui
                    .soundboard
                    .update(soundboard::Event::Cue(*cue), &env),
            );
        }
        if let Some(guild) = self
            .ui
            .soundboard
            .new_catalog_change(controls.sound_catalog_change)
        {
            tasks.push(
                self.ui
                    .soundboard
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
                self.ui.stage.finish(serial, result);
                return Task::none();
            }
            Event::OpenTopic(editing) => {
                let draft = if editing {
                    stage.topic.clone().unwrap_or_default()
                } else {
                    String::new()
                };
                self.ui.stage.topic = Some((editing, draft));
                return Task::none();
            }
            Event::Topic(value) => {
                if let Some((_, draft)) = &mut self.ui.stage.topic {
                    *draft = value
                        .chars()
                        .take(fastcord_discord::MAX_TOPIC_CHARS)
                        .collect();
                }
                return Task::none();
            }
            Event::CloseTopic => {
                self.ui.stage.topic = None;
                return Task::none();
            }
            Event::SubmitTopic => match self.ui.stage.submission() {
                Some(action) => action,
                None => return Task::none(),
            },
            Event::Do(action) => action,
        };
        self.ui.member_menu.close();
        let (Some(guild), Some(channel)) = (stage.guild, stage.channel) else {
            return Task::none();
        };
        if session.client.writes_restricted() {
            return Task::none();
        }
        let Some(serial) = self.ui.stage.begin() else {
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
        let task = self.ui.soundboard.update(event, &env);
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
        self.ui
            .sticker
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
        let task =
            self.ui
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
            .ui
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
        let task = self.ui.events.update(
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
        let task = self.ui.discover.update(event, &session.client);
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
        let task = self.ui.invite.update(
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
            self.ui
                .community
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
            // The rail's right click opens the same menu as the server's name.
            guild_admin::Event::Menu(guild) => {
                self.close_menus();
                self.ui.menu = Some(Menu::Server(guild));
                return Task::none();
            }
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
                .ui
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
            let _ = self.ui.guild_admin.update(
                guild_admin::Event::Close,
                &gateway.navigation,
                &session.client,
            );
            return self.community_event(community::Event::Open(guild));
        }
        if let guild_admin::Event::Leave(guild) = event {
            // The menu hands over to the leave confirmation of the invite module.
            let _ = self
                .ui
                .guild_admin
                .update(event, &gateway.navigation, &session.client);
            return self.invite_event(invite::Event::AskLeave(guild));
        }
        if let guild_admin::Event::AutoMod(guild) = event {
            // The menu hands over to the AutoMod panel.
            let _ = self
                .ui
                .guild_admin
                .update(event, &gateway.navigation, &session.client);
            return self.automod_event(automod::Event::Open(guild));
        }
        if let guild_admin::Event::Bans(guild) | guild_admin::Event::Audit(guild) = event {
            // The menu hands over to the moderation panels.
            let audit = matches!(event, guild_admin::Event::Audit(_));
            let _ = self
                .ui
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
            .ui
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
            .ui
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
            .ui
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
        let task = self.ui.identity.update(
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
                self.ui.member_menu.target = Some(member_menu::Target {
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
                self.ui.member_menu.notice = None;
            }
            Event::Close => {
                self.ui.member_menu.close();
                if let Some(writer) = &self.volume_writer {
                    writer.flush();
                }
            }
            Event::Done(serial, result) => {
                self.ui.member_menu.done(
                    serial,
                    result.map_or_else(moderation::failure, |()| "Done.".into()),
                );
            }
            Event::Sub(sub) => {
                if let Some(target) = &mut self.ui.member_menu.target {
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
                if let Some(target) = &mut self.ui.member_menu.target {
                    target.reason = fastcord_discord::cut_reason(text);
                }
            }
            Event::Nick(text) => {
                if let Some(target) = &mut self.ui.member_menu.target {
                    target.nick = text
                        .chars()
                        .take(fastcord_discord::MAX_NICK_CHARS)
                        .collect();
                }
            }
            Event::BanWindow(seconds) => {
                if let Some(target) = &mut self.ui.member_menu.target {
                    target.ban_window = seconds;
                }
            }
            Event::Select(action) => {
                if self.ui.member_menu.pending.is_some() {
                    return Task::none();
                }
                let Some(target) = &mut self.ui.member_menu.target else {
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
                    .ui
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
        let Some(target) = &self.ui.member_menu.target else {
            return Task::none();
        };
        let Some(guild) = target
            .guild
            .filter(|id| Some(*id) == gateway.navigation.selected_guild)
        else {
            return Task::none();
        };
        if self.ui.member_menu.pending.is_some() || session.client.writes_restricted() {
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
            self.ui.member_menu.notice =
                Some("This action is no longer available for this member.".into());
            return Task::none();
        }
        let reason = Some(target.reason.trim().to_owned()).filter(|r| !r.is_empty());
        let (bridge, user) = (gateway.controls.clone(), target.user.id);
        let serial = self.ui.member_menu.begin(user);
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
                .ui
                .report
                .begin(subject, &session.client)
                .map(Message::Report);
        }
        // The review is judged against what the timeline holds right now.
        self.ui
            .report
            .sync(self.ui.report.watched().and_then(|(channel, message)| {
                timeline::is_retained(&gateway.timeline, channel, message)
            }));
        let rejected = report::State::is_rejected(&event);
        let mut task = self
            .ui
            .report
            .update(event, &session.client)
            .map(Message::Report);
        if let Some((action, user)) = follow {
            task = Task::batch([
                task,
                self.ui
                    .friends
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
            .ui
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
            && let Some(link) = self.ui.switcher.wanted().and_then(desktop::parse)
        {
            self.ui.switcher.close();
            return self.open_link(link);
        }
        let Phase::Account { gateway, .. } = &mut self.phase else {
            return Task::none();
        };
        match self.ui.switcher.update(event, &gateway.navigation.quick) {
            switcher::Action::None => Task::none(),
            switcher::Action::Open(entry) => {
                // Exactly a sidebar click: the conversation opens (a voice
                // channel's chat, never its call) and its draft comes back.
                self.ui.threads.close();
                self.ui.friends.close();
                if self.ui.inbox.is_open() {
                    self.ui.inbox.clear();
                    gateway.controls.inbox_rows(self.ui.inbox.rows_wanted());
                }
                gateway.directs = entry.guild.is_none();
                gateway.controls.open_channel(entry.channel);
                self.ui.nav_pending = true;
                iced::widget::operation::focus(composer::EDITOR_ID)
            }
        }
    }

    /// A navigation chord (see [`switcher`]).
    fn navigation_shortcut(&mut self, shortcut: switcher::Shortcut) -> Task<Message> {
        use switcher::Shortcut;
        if let Shortcut::Escape { .. } = shortcut {
            // Whatever is on top closes first: a popover, a dialog, the
            // expressions window, a settings screen, then the side panels.
            if self.ui.status.open || self.mic_menu || self.ui.menu.is_some() {
                self.close_menus();
                return Task::none();
            }
            if self.ui.new_dm.open {
                self.ui.new_dm.update(new_dm::Event::Close);
                return Task::none();
            }
            if let Some(close) = self.dialog_close() {
                return close.map_or_else(Task::none, |close| self.handle(close));
            }
            if let Phase::Account { gateway, .. } = &self.phase
                && self.expressions_tab(gateway).is_some()
            {
                self.close_expressions();
                return iced::widget::operation::focus(composer::EDITOR_ID);
            }
            if self.server_page().is_some() {
                return self.close_server_pages();
            }
            if self.audio_settings.open {
                return self.handle(Message::Settings(settings::Event::Close));
            }
        }
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
                self.ui.switcher.toggle();
                if self.ui.switcher.is_open() {
                    iced::widget::operation::focus(switcher::INPUT_ID)
                } else {
                    composer()
                }
            }
            Shortcut::Help => {
                self.ui.switcher.toggle_help();
                composer()
            }
            Shortcut::Move(delta) => {
                if self.ui.switcher.is_open() {
                    self.ui
                        .switcher
                        .update(switcher::Event::Move(delta), &gateway.navigation.quick);
                }
                Task::none()
            }
            Shortcut::Step(step) => {
                if self.ui.switcher.is_open() || self.ui.switcher.help_open() {
                    return Task::none();
                }
                self.ui.threads.close();
                self.ui.friends.close();
                if self.ui.inbox.is_open() {
                    self.ui.inbox.clear();
                    gateway.controls.inbox_rows(self.ui.inbox.rows_wanted());
                }
                if step.scope == fastcord_discord::state::navigation::StepScope::Server {
                    gateway.directs = false;
                }
                gateway.controls.step(step);
                self.ui.nav_pending = true;
                Task::none()
            }
            Shortcut::Escape { captured } => {
                // Then the side panels, the topmost first.
                if self.ui.inbox.is_open() {
                    return Task::done(Message::Inbox(inbox::Event::Toggle));
                }
                if self.ui.notify_settings.is_open() {
                    return Task::done(Message::NotifySettings(notify_settings::Event::Toggle));
                }
                if self.ui.search.open {
                    return Task::done(Message::Search(search::Event::Toggle));
                }
                if gateway
                    .timeline
                    .channel_id
                    .is_some_and(|channel| self.ui.pins.shows(channel))
                {
                    return Task::done(Message::Pins(pins::Event::Toggle));
                }
                if self.ui.member_menu.target.is_some() {
                    self.ui.member_menu.close();
                    return Task::none();
                }
                if self.ui.switcher.close() {
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
        let task = self.ui.notify_settings.update(
            event,
            &gateway.navigation.notifications,
            &session.client,
        );
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
            inbox::Event::MarkRead(channel) => {
                if let Some(row) = rows.iter().find(|row| row.channel == channel) {
                    gateway.controls.mark_read(row.guild, row.channel);
                }
                return Task::none();
            }
            inbox::Event::MarkAllRead(shown) => {
                gateway.controls.mark_read_many(shown);
                return Task::none();
            }
            _ => {}
        }
        let rejected = matches!(
            &event,
            inbox::Event::Loaded(_, Err(RestError::AuthenticationRequired))
                | inbox::Event::Muted(_, Err(RestError::AuthenticationRequired))
        );
        let (task, action) =
            self.ui
                .inbox
                .update(event, &gateway.navigation.inbox, &session.client);
        gateway.controls.inbox_rows(self.ui.inbox.rows_wanted());
        if let inbox::Action::Open(channel, target) = action {
            if gateway.timeline.channel_id == Some(channel) {
                inbox_jump(&gateway.controls, channel, target);
            } else {
                // Only a conversation the account can open is selected;
                // otherwise the wait ends with a notice.
                self.ui.inbox.set_pending(channel, target);
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
            if self.ui.pins.shows(channel)
                && let Some(pin) = self.ui.pins.pin(index)
            {
                gateway.controls.timeline_intent(history::Intent::JumpTo {
                    channel,
                    message: pin.message,
                });
            }
            return Task::none();
        }
        self.ui
            .pins
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
            if let Some(hit) = self.ui.search.hit(index).cloned() {
                if gateway.timeline.channel_id == Some(hit.channel) {
                    gateway.controls.timeline_intent(history::Intent::JumpTo {
                        channel: hit.channel,
                        message: hit.message,
                    });
                } else {
                    self.ui.search.set_pending_jump(hit.channel, hit.message);
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
        self.ui.search.update(event, &context).map(Message::Search)
    }

    fn view(&self) -> Element<'_, Message> {
        let body = if self.audio_settings.open {
            self.settings_view()
        } else {
            self.view_body()
        };
        #[cfg(feature = "fake-discord")]
        if let Some(banner) = fake::banner() {
            return column![banner, body].into();
        }
        body
    }

    /// The settings screen with its current page.
    fn settings_view(&self) -> Element<'_, Message> {
        let account = match &self.phase {
            Phase::Account {
                session,
                persistence,
                gateway,
            } => Some((session, persistence, gateway)),
            _ => None,
        };
        let section = match self.settings_section {
            s if s.needs_account() && account.is_none() => settings::Section::Voice,
            s => s,
        };
        let controls = account.map(|(_, _, gateway)| gateway.controls.voice_controls());
        let missing = || -> Element<'_, Message> { ui::note(i18n::tr("Not available yet.")) };
        let content: Element<'_, Message> = match (section, account) {
            (settings::Section::Profile, Some((session, _, gateway))) => profile_edit::view(
                &self.ui.identity,
                &gateway.navigation,
                own_user(gateway, session),
            )
            .map_or_else(missing, |panel| panel.map(Message::Identity)),
            (settings::Section::Accounts, Some((session, persistence, _))) => account_panel::view(
                &self.ui.account_panel,
                &self.account_index,
                session.user.id,
                *persistence == Persistence::Saved,
            )
            .map_or_else(missing, |panel| panel.map(Message::Account)),
            (settings::Section::Privacy, Some((_, _, gateway))) => {
                safety::view(&self.ui.safety, &gateway.navigation)
                    .map_or_else(missing, |panel| panel.map(Message::Safety))
            }
            (settings::Section::Chat, _) => chat_prefs::view(
                self.audio_preferences.chat,
                &chat_prefs::SpeechView {
                    prefs: &self.audio_preferences.speech,
                    voices: &self.speech_voices,
                    error: speech::error(),
                    speaking: speech::speaking(),
                },
            ),
            (settings::Section::Notifications, _) => {
                audio_devices::notifications(&self.audio_preferences)
            }
            (settings::Section::Network, _) => network_ui::view(
                &self.network_settings,
                &self.network,
                matches!(self.phase, Phase::Login),
            ),
            (settings::Section::App, _) => column![
                desktop::view(
                    self.audio_preferences.close_to_tray,
                    &self.tray,
                    self.autostart,
                ),
                audio_devices::experimental(&self.audio_preferences),
                unverified::view(
                    account.map(|(session, _, _)| &session.client),
                    self.ui.unverified_open,
                ),
                audio_devices::diagnostics(
                    controls.as_ref().and_then(|c| c.audio_stats),
                    controls.as_ref().map_or_else(String::new, |c| {
                        audio_devices::transmit_summary(
                            c.audio_stats,
                            c.transport_stats,
                            c.capture_enabled(),
                        )
                    }),
                ),
            ]
            .spacing(28)
            .into(),
            _ => column![
                audio_devices::devices(
                    &self.audio_settings,
                    &self.audio_preferences,
                    controls.as_ref().and_then(|c| c.input_error),
                    controls.as_ref().and_then(|c| c.output_error),
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
                shortcuts::view(
                    &self.audio_preferences.voice_input,
                    &self.shortcuts,
                    controls.as_ref().is_some_and(|c| c.vad_forbidden),
                    self.call_active(),
                ),
                audio_devices::processing(
                    &self.audio_preferences,
                    controls.as_ref().and_then(|c| c.noise_status),
                ),
            ]
            .spacing(28)
            .into(),
        };
        let content = match self.notice.as_deref() {
            Some(notice) if account.is_none() => column![ui::error(i18n::tr(notice)), content]
                .spacing(16)
                .into(),
            _ => content,
        };
        settings::view(section, account.is_some(), content)
    }

    fn view_body(&self) -> Element<'_, Message> {
        if self.compact_call
            && let Phase::Account { gateway, .. } = &self.phase
        {
            return desktop::compact_view(&gateway.controls.voice_controls());
        }
        if let Phase::Account { gateway, .. } = &self.phase
            && let Some((guild, section)) = self.server_page()
        {
            return self.server_screen(gateway, guild, section);
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
                    search::view(&self.ui.search, guild.is_some()).map(Message::Search)
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
                    settings_open: self.ui.voice_details,
                    can_leave: gateway.voice_guild.or(gateway.voice_call).is_some(),
                    share_text: share.text.clone(),
                    ping_ms: voice_controls.voice_ping_ms,
                })
            });
            let voice_settings: Option<Element<'_, Message>> = (in_voice && self.ui.voice_details)
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
                        volume_notice: self.volume_notice.as_deref(),
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
                    i18n::trf(
                        "Microphone stopped: {}. Open the voice settings to try again.",
                        &[&i18n::tr(&error.to_string())],
                    ),
                    true,
                );
            }
            if let Some(error) = voice_controls.output_error {
                say(
                    i18n::trf(
                        "Playback stopped: {}. Open the voice settings to try again.",
                        &[&i18n::tr(&error.to_string())],
                    ),
                    true,
                );
            }
            say(self.notice.clone().unwrap_or_default(), false);
            say(gateway.attachment_notice.unwrap_or("").to_owned(), false);
            // Closed, search is a tool in the conversation header.
            let has_search = self.ui.search.open && !search_bar_is_empty(gateway);
            let mut banner_column = column![].spacing(2);
            for (label, role) in [
                ("Voice calls", fastcord_net::Role::Voice),
                ("Stream", fastcord_net::Role::Stream),
            ] {
                let status = self.network.controller().service_status(role);
                if matches!(
                    status,
                    fastcord_net::selector::ServiceStatus::UdpDiscovering { .. }
                        | fastcord_net::selector::ServiceStatus::UdpUnreachable
                        | fastcord_net::selector::ServiceStatus::Failed(
                            fastcord_net::NetworkError::ProxyUdpClosed
                                | fastcord_net::NetworkError::ProxyUdpUnavailable
                        )
                ) {
                    banner_column = banner_column.push(
                        row![
                            text(format!(
                                "{}: {}",
                                i18n::tr(label),
                                network_ui::status_text(status)
                            ))
                            .size(13),
                            button(text(i18n::tr("Network settings")).size(12))
                                .on_press(Message::Network(network_ui::Action::Open))
                                .style(style::secondary_button(false)),
                        ]
                        .spacing(8),
                    );
                }
            }
            if has_search {
                banner_column = banner_column.push(search_bar);
            }
            if let Some(channel) = gateway.timeline.channel_id
                && self.ui.pins.shows(channel)
            {
                banner_column = banner_column.push(pins::view(&self.ui.pins).map(Message::Pins));
            }
            if let Some(panel) = soundboard::view(&self.ui.soundboard) {
                banner_column = banner_column.push(panel.map(Message::Soundboard));
            }
            if let Some(panel) = stage::banner(
                &self.ui.stage,
                &voice_controls,
                session.client.writes_restricted(),
            ) {
                banner_column = banner_column.push(panel);
            }
            if let Some(channel) = gateway.timeline.channel_id
                && let Some(panel) = slash::view(
                    &self.ui.slash,
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
                && self.ui.poll.is_open_for(channel)
                && let Some(panel) = poll_create::view(&self.ui.poll)
            {
                banner_column = banner_column.push(panel.map(Message::Poll));
            }
            if let Some(channel) = gateway.timeline.channel_id
                && let Some(panel) =
                    voice_record::view(&self.ui.voice, &self.voice_context(gateway, channel))
            {
                banner_column = banner_column.push(panel.map(Message::Voice));
            }
            // Forms open as dialogs over the window; only their reminder
            // lines and notices stay above the conversation.
            let mut dialogs: Vec<(Element<'_, Message>, Option<Message>)> = Vec::new();
            if let Some(panel) =
                onboarding::view(&self.ui.onboarding, &gateway.navigation, &gateway.revealed)
            {
                let panel = panel.map(Message::Onboarding);
                if self.ui.onboarding.is_open() {
                    dialogs.push((panel, Some(Message::Onboarding(onboarding::Event::Close))));
                } else {
                    banner_column = banner_column.push(panel);
                }
            }
            if let Some(panel) = scheduled_events::view(&self.ui.events, &gateway.navigation) {
                let panel = panel.map(Message::Events);
                if self
                    .ui
                    .events
                    .is_open_for(gateway.navigation.selected_guild)
                {
                    dialogs.push((panel, Some(Message::Events(scheduled_events::Event::Close))));
                } else {
                    banner_column = banner_column.push(panel);
                }
            }
            if let Some(panel) = guild_admin::view(&self.ui.guild_admin, &gateway.navigation) {
                let panel = panel.map(Message::GuildAdmin);
                if self.ui.guild_admin.is_dialog() {
                    dialogs.push((panel, Some(Message::GuildAdmin(guild_admin::Event::Close))));
                } else {
                    banner_column = banner_column.push(panel);
                }
            }
            if let Some(panel) = announce::view(&self.ui.announce, &gateway.navigation) {
                dialogs.push((
                    panel.map(Message::Announce),
                    Some(Message::Announce(announce::Event::Close)),
                ));
            }
            if let Some(panel) = apps::view(&self.ui.apps, &gateway.navigation) {
                // The account's authorized apps; server pages are a screen.
                dialogs.push((
                    panel.map(Message::Apps),
                    Some(Message::Apps(apps::Event::Close)),
                ));
            }
            if let Some(panel) = invite::view(&self.ui.invite, localtime::now()) {
                let panel = panel.map(Message::Invite);
                if self.ui.invite.is_dialog() {
                    dialogs.push((panel, self.ui.invite.dismiss().map(Message::Invite)));
                } else {
                    banner_column = banner_column.push(panel);
                }
            }
            if let Some(panel) = discover::view(
                &self.ui.discover,
                &gateway.navigation,
                &gateway.avatars.views,
            ) {
                dialogs.push((
                    panel.map(Message::Discover),
                    Some(Message::Discover(discover::Event::Toggle)),
                ));
            }
            if let Some(panel) =
                group_dm::view(&self.ui.group, &gateway.navigation, session.user.id)
            {
                dialogs.push((
                    panel.map(Message::Group),
                    Some(Message::Group(group_dm::Event::Close)),
                ));
            }
            if let Some(panel) = report::view(&self.ui.report, &gateway.navigation) {
                dialogs.push((
                    panel.map(Message::Report),
                    Some(Message::Report(report::Event::Close)),
                ));
            }
            if self.ui.forward.is_open()
                && let Some(panel) = forward::view(&self.ui.forward, &forward_candidates(gateway))
            {
                dialogs.push((
                    panel.map(Message::Forward),
                    Some(Message::Forward(forward::Event::Close)),
                ));
            }
            // One at a time: the first open one.
            let dialog = dialogs
                .into_iter()
                .next()
                .map(|(panel, close)| ui::modal_maybe(ui::sheet(panel, 640.0), close));
            if let Some(panel) = threads::view(&self.ui.threads) {
                banner_column = banner_column.push(panel.map(Message::Threads));
            }
            for (line, danger) in notes {
                let line = text(i18n::tr(&line).to_owned()).size(12);
                banner_column = banner_column.push(if danger {
                    line.style(text::danger)
                } else {
                    line.color(crate::style::muted())
                });
            }
            if matches!(
                &gateway.status,
                gateway::GatewayStatus::Stopped(reason) if reason.allows_manual_reconnect()
            ) {
                banner_column = banner_column.push(
                    button(text(i18n::tr("Reconnect")).size(12))
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
                            text(i18n::trf("Incoming call: {}", &[&call.label]))
                                .size(13)
                                .font(bold)
                                .width(Length::Fill),
                            primary_button(text(i18n::tr("Accept")).size(12))
                                .on_press(Message::AcceptCall(call.channel_id))
                                .style(style::action_button(false)),
                            button(text(i18n::tr("Decline")).size(12))
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
                banner_column = banner_column.push(text(i18n::tr("Discord requires account verification. Sending is paused; open the official Discord client, complete its check, then reconnect fastcord.")).size(13).style(text::danger));
            }
            let banner: Option<Element<'_, Message>> = Some(banner_column.into());

            // ---- the user's panel at the foot of the sidebar ----
            // Avatar and name (status, profile, accounts behind a click),
            // microphone with its device menu, headphones, and the settings.
            let me = own_user(gateway, session);
            let display = me.display_name().to_owned();
            let own = gateway.navigation.presences.get(&session.user.id);
            let own_status = self.ui.status.status(own);
            let mut foot = column![];
            if let Some(settings) = voice_settings {
                foot = foot.push(settings);
            }
            if let Some(panel) = stage::own(
                &self.ui.stage,
                &voice_controls,
                session.client.writes_restricted(),
            ) {
                foot = foot.push(panel);
            }
            if let Some(panel) = voice_panel {
                foot = foot.push(panel);
            }
            let identity = button(
                row![
                    avatars::view(
                        &gateway.avatars.views,
                        &avatars::AvatarKey::user(me.id, me.avatar.as_deref()),
                        32.0,
                        || ui::initials(&display, session.user.id.0, 32.0),
                    ),
                    column![
                        text(display.clone())
                            .size(13)
                            .font(bold)
                            .wrapping(text::Wrapping::None),
                        row![
                            status::dot(own_status),
                            text(
                                self.ui
                                    .status
                                    .custom(own)
                                    .or(own_status.map(status::label))
                                    .unwrap_or(i18n::tr("Set status"))
                                    .to_owned()
                            )
                            .size(11)
                            .color(style::MUTED)
                            .wrapping(text::Wrapping::None),
                        ]
                        .spacing(5)
                        .align_y(iced::alignment::Vertical::Center),
                    ]
                    .width(Length::Fill),
                ]
                .spacing(8)
                .align_y(iced::alignment::Vertical::Center),
            )
            .on_press(Message::Status(status::Event::ToggleMenu))
            .width(Length::Fill)
            .padding([2, 4])
            .style(style::row_button(self.ui.status.open, false));
            let action = |icon, on_press, active, danger, tip: &str| icons::Action {
                icon,
                on_press,
                active,
                danger,
                tip: tip.to_owned(),
                badge: 0,
            };
            let mic = icons::action(action(
                icons::Icon::Mic {
                    muted: voice_controls.self_muted,
                },
                running.then_some(Message::SetSelfMute(!voice_controls.self_muted)),
                !voice_controls.self_muted,
                voice_controls.self_muted,
                if !running {
                    i18n::tr("Microphone (available in a call)")
                } else if voice_controls.self_muted {
                    i18n::tr("Turn microphone on")
                } else {
                    i18n::tr("Turn microphone off")
                },
            ));
            let devices = iced::widget::tooltip(
                button(icons::sized(icons::Icon::ChevronUp, style::MUTED, 12.0))
                    .on_press(Message::MicMenu(!self.mic_menu))
                    .padding([8, 1])
                    .style(style::tool_button(self.mic_menu, false)),
                container(text(i18n::tr("Input and output devices")).size(11))
                    .padding([3, 6])
                    .style(container::rounded_box),
                iced::widget::tooltip::Position::Top,
            );
            let headphones = icons::action(action(
                icons::Icon::Headphones {
                    deafened: voice_controls.deafened,
                },
                running.then_some(Message::SetDeafen(!voice_controls.deafened)),
                !voice_controls.deafened,
                voice_controls.deafened,
                if !running {
                    i18n::tr("Sound (available in a call)")
                } else if voice_controls.deafened {
                    i18n::tr("Turn sound on")
                } else {
                    i18n::tr("Turn sound off")
                },
            ));
            let gear = icons::action(action(
                icons::Icon::Settings,
                Some(Message::Settings(settings::Event::Open(
                    self.settings_section,
                ))),
                true,
                false,
                i18n::tr("Settings"),
            ));
            foot = foot.push(
                container(
                    row![
                        container(identity).width(Length::Fill).clip(true),
                        row![mic, devices, headphones, gear]
                            .spacing(0)
                            .align_y(iced::alignment::Vertical::Center),
                    ]
                    .spacing(2)
                    .align_y(iced::alignment::Vertical::Center),
                )
                .padding([6, 6])
                .style(style::footer),
            );
            // The menu behind the name: status, profile, accounts, log out.
            let account_menu: Option<Element<'_, Message>> = self.ui.status.open.then(|| {
                let pick = |message| Message::MenuPick(Box::new(message));
                ui::popover(
                    ui::menu_card(
                        column![
                            row![
                                ui::initials(&display, session.user.id.0, 40.0),
                                column![
                                    text(display.clone()).size(15).font(bold),
                                    text(format!("@{}", me.username))
                                        .size(12)
                                        .color(style::MUTED),
                                ],
                            ]
                            .spacing(10)
                            .align_y(iced::alignment::Vertical::Center)
                            .padding(6),
                            self.ui
                                .status
                                .menu(
                                    own,
                                    if *persistence == Persistence::Saved {
                                        i18n::tr("Saved login")
                                    } else {
                                        i18n::tr("Not saved on this device")
                                    },
                                )
                                .map(Message::Status),
                            ui::separator(),
                            ui::item(
                                Some(icons::Icon::Edit),
                                i18n::tr("Edit profile"),
                                Some(pick(Message::Settings(settings::Event::Open(
                                    settings::Section::Profile
                                )))),
                            ),
                            ui::item(
                                Some(icons::Icon::Group),
                                i18n::tr("Switch account"),
                                Some(pick(Message::Settings(settings::Event::Open(
                                    settings::Section::Accounts
                                )))),
                            ),
                            ui::separator(),
                            ui::danger_item(
                                Some(icons::Icon::Exit),
                                i18n::tr("Log out"),
                                Some(pick(Message::Account(account_panel::Event::Logout))),
                            ),
                        ]
                        .spacing(4),
                        300.0,
                    ),
                    ui::Anchor::BottomLeft(navigation::RAIL_WIDTH + 8.0, 60.0),
                    Message::Status(status::Event::ToggleMenu),
                )
            });
            // Quick device choice above the microphone.
            let mic_menu: Option<Element<'_, Message>> = self.mic_menu.then(|| {
                ui::popover(
                    ui::menu_card(
                        column![
                            audio_devices::devices(
                                &self.audio_settings,
                                &self.audio_preferences,
                                voice_controls.input_error,
                                voice_controls.output_error,
                            ),
                            ui::separator(),
                            ui::item(
                                Some(icons::Icon::Settings),
                                i18n::tr("Voice settings"),
                                Some(Message::MenuPick(Box::new(Message::Settings(
                                    settings::Event::Open(settings::Section::Voice)
                                )))),
                            ),
                        ]
                        .spacing(8)
                        .padding(8),
                        320.0,
                    ),
                    ui::Anchor::BottomLeft(navigation::RAIL_WIDTH + 8.0, 60.0),
                    Message::MicMenu(false),
                )
            });

            let account = container(navigation::view(
                &gateway.navigation,
                &gateway.timeline,
                &gateway.composer,
                {
                    let mut interaction = gateway.interaction();
                    interaction.publishing = self.ui.announce.publishing();
                    interaction
                },
                timeline::Attachments {
                    account: session.user.id,
                    views: &gateway.attachment_views,
                    media: gateway.media.as_ref(),
                    emojis: &gateway.emoji.views,
                    avatars: &gateway.avatars.views,
                    revealed: &gateway.revealed,
                    invites: &self.ui.invite.cache,
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
                    wide: self.wide(),
                    forum: gateway.navigation.forum.as_ref().map(|forum| {
                        forum::view(
                            &self.ui.forum,
                            forum,
                            self.ui.threads.forum_archive(forum.channel),
                            session.user.id,
                            localtime::now(),
                        )
                    }),
                    inbox: self.ui.notify_settings.is_open().then(|| {
                        notify_settings::view(
                            &self.ui.notify_settings,
                            &gateway.navigation.notifications,
                            gateway.directs,
                        )
                        .map(Message::NotifySettings)
                    }),
                    friends: if self.ui.requests_open && self.ui.safety.is_open() {
                        safety::view(&self.ui.safety, &gateway.navigation)
                            .map(|panel| container(panel.map(Message::Safety)).padding(16).into())
                    } else {
                        None
                    }
                    .or_else(|| {
                        self.ui.friends.open.then(|| {
                            friends::view(
                                &self.ui.friends,
                                &gateway.navigation.relationships,
                                &gateway.navigation.presences,
                            )
                            .map(Message::Friends)
                        })
                    }),
                },
            ))
            .height(Length::Fill);
            // A profile popup covers whatever is under it.
            let account: Element<'_, Message> = match friends::profile_view(
                &self.ui.friends,
                &gateway.navigation.relationships,
                session.user.id,
                friends::MemberContext::of(
                    self.ui.friends.profile_user(),
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
                None if self.expressions_tab(gateway).is_some() => iced::widget::stack![
                    account,
                    container(self.expressions_view(gateway))
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
                &self.ui.member_menu,
                &gateway.navigation,
                voice_controls.clone(),
                session.client.writes_restricted(),
                self.volume_notice.as_deref(),
            ) {
                Some(menu) => iced::widget::stack![screen, menu].into(),
                None => screen,
            };
            let menu = match self.ui.menu {
                Some(Menu::Server(guild)) => Some(self.server_menu(gateway, guild)),
                Some(Menu::Conversation) => Some(self.conversation_menu(gateway)),
                Some(Menu::Composer) => Some(self.composer_menu(gateway)),
                None => None,
            };
            // The inbox floats beside the rail; the conversation stays put.
            let inbox_panel: Option<Element<'_, Message>> = self.ui.inbox.is_open().then(|| {
                ui::popover(
                    container(
                        inbox::view(&self.ui.inbox, &gateway.navigation.inbox).map(Message::Inbox),
                    )
                    .max_height(640)
                    .into(),
                    ui::Anchor::TopLeft(navigation::RAIL_WIDTH + 4.0, 8.0),
                    Message::Inbox(inbox::Event::Toggle),
                )
            });
            let screen = match account_menu
                .or(mic_menu)
                .or(menu)
                .or(inbox_panel)
                .or_else(|| {
                    new_dm::view(
                        &self.ui.new_dm,
                        &gateway.navigation.relationships,
                        gateway.private.notice.as_deref(),
                    )
                }) {
                Some(menu) => iced::widget::stack![screen, menu].into(),
                None => screen,
            };
            let screen = match dialog {
                Some(dialog) => iced::widget::stack![screen, dialog].into(),
                None => screen,
            };
            let overlay: Option<Element<'_, Message>> = if self.ui.switcher.is_open() {
                Some(
                    switcher::view(&self.ui.switcher, &gateway.navigation.quick, |channel| {
                        gateway.composer.has_draft(channel)
                    })
                    .map(Message::Switcher),
                )
            } else if self.ui.switcher.help_open() {
                Some(switcher::help_view().map(Message::Switcher))
            } else {
                None
            };
            return match overlay {
                Some(overlay) => iced::widget::stack![screen, overlay].into(),
                None => screen,
            };
        }
        let stage = match &self.phase {
            Phase::Starting => login_screen::Stage::Starting,
            Phase::Login => login_screen::Stage::Form,
            Phase::Qr(attempt) => login_screen::Stage::Qr(&attempt.stage),
            Phase::Mfa(_) => login_screen::Stage::Mfa,
            Phase::Authenticating { saved_account } => login_screen::Stage::Checking {
                restoring: saved_account.is_some(),
            },
            Phase::MemoryConsent { session, error } => login_screen::Stage::MemoryConsent {
                name: format!(
                    "{} (@{})",
                    session.user.display_name(),
                    session.user.username
                ),
                error: i18n::tr(&error.to_string()).to_owned(),
            },
            Phase::Account { .. } => unreachable!("account view returned above"),
            Phase::Deleting { .. } => login_screen::Stage::Deleting,
            Phase::DeleteFailed { error, .. } => login_screen::Stage::DeleteFailed {
                error: i18n::tr(&error.to_string()).to_owned(),
            },
        };
        login_screen::view(
            &self.login_ui,
            login_screen::Inputs {
                stage,
                saved: self
                    .saved_accounts
                    .iter()
                    .map(|id| {
                        (
                            *id,
                            self.account_index.get(*id).map_or_else(
                                || i18n::trf("Account {}", &[id]),
                                accounts::Entry::label,
                            ),
                        )
                    })
                    .collect(),
                acknowledged: self.acknowledged,
                memory_only: self.memory_only,
                token: self.token.expose_secret(),
                login: &self.password_login,
                password: self.password.expose_secret(),
                notice: self.notice.as_deref().map(i18n::tr),
                can_login: self.can_login(),
                can_password: self.can_password_login(),
                store_failed: self.store.is_none(),
            },
        )
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
    let mut seen: std::collections::HashSet<avatars::AvatarKey> = wanted.iter().cloned().collect();
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
        if seen.insert(key.clone()) {
            wanted.push(key);
        }
    }
    wanted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn participant_volume_change_survives_account_teardown_without_release() {
        let mut app = login_app();
        let session = authenticate(&mut app, Persistence::MemoryOnly);
        let path = std::env::temp_dir()
            .join(format!(
                "fastcord-app-volume-{}-{}",
                std::process::id(),
                session.user.id
            ))
            .join("voice-settings.csv");
        let (writer, _) = voice_settings::Writer::at(Some(path.clone()), session.user.id);
        app.volume_writer = Some(writer);
        let _ = app.handle(Message::SetParticipantVolume(Snowflake(99), 67));
        let _ = app.handle(Message::PresetParticipantVolume(Snowflake(98), 6));
        let _ = app.handle(Message::SetParticipantVolume(Snowflake(99), 62));
        if let Phase::Account { gateway, .. } = &app.phase {
            assert_eq!(
                gateway
                    .controls
                    .voice_controls()
                    .volume_percent(Snowflake(99)),
                62
            );
        }
        app.teardown_account();
        let restored = gateway::NavigationBridge::new();
        restored.set_volume_settings(voice_settings::load_from(&path, session.user.id));
        assert_eq!(restored.voice_controls().volume_percent(Snowflake(99)), 62);
        assert_eq!(restored.voice_controls().volume_percent(Snowflake(98)), 6);
        assert!(voice_settings::load_from(&path, Snowflake(session.user.id.0 + 1)).is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

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
    fn an_inactive_window_drops_decoded_animations() {
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        let frame = attachments::DecodedImage {
            width: 1,
            height: 1,
            rgba: bytes::Bytes::from_static(&[255, 0, 0, 255]),
        };
        gateway.emoji.views.insert(
            emoji::Asset::Custom {
                id: Snowflake(12345),
                animated: true,
            },
            emoji::View::from_decoded(attachments::DecodedAnimation {
                frames: vec![frame.clone(), frame],
                delays: vec![std::time::Duration::from_millis(20); 2],
            }),
        );
        let _ = app.update(Message::WindowActive(false));
        let Phase::Account { gateway, .. } = &app.phase else {
            panic!("account");
        };
        assert!(gateway.emoji.views.is_empty());
    }

    #[test]
    fn tcp_proxy_explains_media_before_any_join_or_ring_intent() {
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        app.network = app.network.with_mode(fastcord_net::settings::Mode::Proxy);
        for message in [
            Message::JoinVoice(Snowflake(1), Snowflake(2)),
            Message::StartCall(Snowflake(3)),
            Message::AcceptCall(Snowflake(3)),
        ] {
            drop(app.update(message));
            assert!(app.network_settings.open);
            assert!(app.network_settings.notice.unwrap().contains("TCP only"));
            let Phase::Account { gateway, .. } = &app.phase else {
                panic!("account expected")
            };
            assert_eq!(
                gateway.controls.voice_audio_state(),
                gateway::VoiceAudioState::Idle
            );
            assert!(gateway.voice_call.is_none());
            assert!(gateway.voice_guild.is_none());
        }
    }

    #[test]
    fn proxy_password_ui_messages_are_redacted() {
        let message = Message::Network(network_ui::Action::ProxyPassword(Arc::new(
            UserToken::new("proxy-canary-secret".into()),
        )));
        assert!(!format!("{message:?}").contains("proxy-canary-secret"));
    }

    #[test]
    fn a_configured_socks5_proxy_allows_an_explicit_call_intent() {
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        app.network = app
            .network
            .with_proxy(
                fastcord_net::ProxyConfig {
                    kind: fastcord_net::ProxyKind::Socks5,
                    host: "127.0.0.1".into(),
                    port: 1080,
                    username: None,
                },
                zeroize::Zeroizing::new(String::new()),
            )
            .unwrap();
        drop(app.update(Message::StartCall(Snowflake(3))));
        assert!(!app.network_settings.open);
        let Phase::Account { gateway, .. } = &app.phase else {
            panic!("account expected")
        };
        assert_eq!(gateway.voice_call, Some(Snowflake(3)));
        assert_eq!(
            gateway.controls.voice_audio_state(),
            gateway::VoiceAudioState::Connecting
        );
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
        // A draft in another conversation is kept across the reconnect.
        if let Phase::Account { gateway, .. } = &mut app.phase {
            gateway.composer.select(Some(Snowflake(41)));
            assert!(gateway.composer.restore("half-written"));
            gateway.composer.select(Some(Snowflake(42)));
            assert!(gateway.composer.has_draft(Snowflake(41)));
        }
        drop(app.update(Message::ReconnectGateway));
        let Phase::Account { gateway, .. } = &app.phase else {
            panic!("account");
        };
        assert!(gateway.composer.has_draft(Snowflake(41)), "draft survives");
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
        assert!(app.ui.notify_settings.is_open());
        let _ = app.update(Message::Gateway(
            id,
            GatewayStatus::Stopped(fastcord_discord::gateway::StopReason::ActionRequired(
                "REQUIRE_VERIFIED_EMAIL".to_owned(),
            )),
        ));
        client.authentication_required().await;
        assert!(matches!(app.phase, Phase::Login));
        assert!(!app.ui.notify_settings.is_open());
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
        assert!(app.ui.unverified_open);
        let _ = app.update(Message::Logout);
        assert!(!app.ui.unverified_open);
        let _ = app.update(Message::Deleted(Ok(())));
        // The next sign-in has a new client with the defaults, not the old switch.
        let next = authenticate(&mut app, Persistence::MemoryOnly);
        assert!(!next.client.write_group_enabled(WriteGroup::Reports));
        assert!(next.client.write_group_enabled(WriteGroup::Typing));
    }

    #[tokio::test]
    async fn the_allow_all_switch_turns_every_group_on_until_logout() {
        use fastcord_discord::WriteGroup;
        let mut app = login_app();
        let session = authenticate(&mut app, Persistence::Saved);
        assert!(!session.client.write_group_enabled(WriteGroup::Reports));
        let _ = app.update(Message::UnverifiedWriteAll(true));
        assert!(
            unverified::rows(Some(&session.client))
                .iter()
                .all(|(_, on)| *on)
        );
        let _ = app.update(Message::UnverifiedWriteAll(false));
        assert!(
            unverified::rows(Some(&session.client))
                .iter()
                .all(|(_, on)| !*on)
        );
        let _ = app.update(Message::UnverifiedWriteAll(true));
        drop(session);
        let _ = app.update(Message::Logout);
        let _ = app.update(Message::Deleted(Ok(())));
        // The next sign-in starts from the default groups, not from "allow all".
        let next = authenticate(&mut app, Persistence::MemoryOnly);
        assert!(
            unverified::rows(Some(&next.client))
                .iter()
                .all(|(group, on)| *on == group.default_on())
        );
        assert!(!next.client.write_group_enabled(WriteGroup::Reports));
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
        assert!(app.ui.account_panel.is_open());

        let _ = app.update(Message::SwitchAccount(other));
        // The old gateway and voice are gone, panels cleared, and the token
        // of the old account has stopped working before the new login starts.
        client.authentication_required().await;
        assert!(
            matches!(app.phase, Phase::Authenticating { saved_account: Some(a) } if a == other)
        );
        assert!(!app.ui.account_panel.is_open());
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
    fn leaving_or_reentering_an_account_resets_every_panel_of_it() {
        // Before #169 two hand-written lists disagreed: the new-DM selection
        // and the open menu survived a switch, search hits a logout.
        let mut app = login_app();
        let session = authenticate(&mut app, Persistence::Saved);
        app.ui.new_dm.update(new_dm::Event::Open);
        app.ui.new_dm.update(new_dm::Event::Toggle(Snowflake(7)));
        app.ui.menu = Some(Menu::Conversation);
        app.ui.requests_open = true;
        assert_eq!(app.ui.new_dm.recipients(), "7");
        assert!(app.teardown_account().is_some());
        assert_eq!(app.ui.new_dm.recipients(), "");
        assert_eq!(app.ui.menu, None);
        assert!(!app.ui.requests_open);
        // Entering an account (a switch, or Reconnect) starts clean as well.
        app.ui.voice_details = true;
        let _ = app.enter_account(session, Persistence::Saved);
        assert!(!app.ui.voice_details);
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
        assert!(app.ui.switcher.is_open());
        let _ = app.update(Message::Shortcut(Shortcut::Help));
        assert!(app.ui.switcher.help_open() && !app.ui.switcher.is_open());
        // Escape closes the overlay first and leaves the reply alone.
        let _ = app.update(Message::Shortcut(Shortcut::Escape { captured: false }));
        assert!(!app.ui.switcher.help_open());
        let replying = |app: &App| matches!(&app.phase, Phase::Account { gateway, .. } if gateway.composer.replying().is_some());
        assert!(replying(&app));
        // A focused composer already handled its own Escape.
        let _ = app.update(Message::Shortcut(Shortcut::Escape { captured: true }));
        assert!(replying(&app));
        let _ = app.update(Message::Shortcut(Shortcut::Escape { captured: false }));
        assert!(!replying(&app));
        // Plain arrows do nothing while the switcher is closed.
        let _ = app.update(Message::Shortcut(Shortcut::Move(1)));
        assert!(!app.ui.switcher.is_open());
    }

    #[test]
    fn escape_closes_the_topmost_layer_first_and_settings_before_sign_in() {
        use switcher::Shortcut;
        let escape = Message::Shortcut(Shortcut::Escape { captured: false });
        // Before sign-in the settings screen closes too.
        let mut app = login_app();
        let _ = app.update(Message::Settings(settings::Event::Open(
            settings::Section::Network,
        )));
        assert!(app.audio_settings.open && app.network_settings.open);
        let _ = app.update(escape.clone());
        assert!(!app.audio_settings.open && !app.network_settings.open);
        // Signed in: a menu over the settings goes first, then the screen.
        authenticate(&mut app, Persistence::MemoryOnly);
        let _ = app.update(Message::Settings(settings::Event::Open(
            settings::Section::Chat,
        )));
        app.mic_menu = true;
        let _ = app.update(escape.clone());
        assert!(!app.mic_menu && app.audio_settings.open);
        let _ = app.update(escape);
        assert!(!app.audio_settings.open);
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
            assert!(app.ui.switcher.is_open());
            app.ui.nav_pending = false;
            let _ = app.update(Message::Switcher(switcher::Event::Pick(index)));
            assert!(!app.ui.switcher.is_open());
            assert!(app.ui.nav_pending, "focus returns once the chat is open");
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
    fn a_snapshot_carrying_the_last_status_still_looks_up_visible_invites() {
        let mut app = login_app();
        authenticate(&mut app, Persistence::MemoryOnly);
        let Phase::Account { gateway, .. } = &mut app.phase else {
            panic!("account");
        };
        let (_, worker) = iced::Task::<()>::none().abortable();
        let id = gateway.id;
        **gateway = GatewayPanel::new(id, worker, gateway::NavigationBridge::new());
        let message: fastcord_model::Message = serde_json::from_value(serde_json::json!({
            "id": "42",
            "channel_id": "500",
            "author": {"id": "2", "username": "alt"},
            "content": "join https://discord.gg/abc123",
            "timestamp": "2026-10-24T12:00:00.000000+00:00",
        }))
        .unwrap();
        // Every snapshot after sign-in carries the last status; it must not
        // cut short the rest of the update.
        gateway.controls.publish(
            fastcord_discord::state::navigation::NavigationSnapshot::default(),
            timeline::Snapshot {
                channel_id: Some(Snowflake(500)),
                rows: vec![timeline::Row::new(Arc::new(message), 1)],
                ..timeline::Snapshot::default()
            },
            emoji_picker::Snapshot::default(),
            Some(GatewayStatus::SigningIn),
        );
        let _ = app.update(Message::Navigation(id));
        let Phase::Account { gateway, .. } = &app.phase else {
            panic!("the status must not end the account");
        };
        assert_eq!(gateway.status, GatewayStatus::SigningIn);
        assert_eq!(
            app.ui.invite.cache.get("abc123").map(|entry| &entry.card),
            Some(&invite::Card::Loading),
            "the invite is looked up"
        );
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
        // The footer's own actions are there all along: the account menu,
        // the device menu and the settings.
        for name in ["Status", "MicMenu", "Settings"] {
            assert!(idle.contains(name), "{name}: {idle:?}");
        }
        let _ = app.update(Message::JoinVoice(guild, voice));
        let joined = bottom(&app);
        for name in [
            "OpenCall",
            "LeaveVoice",
            "ToggleVoiceDetails",
            "Status",
            "Settings",
        ] {
            assert!(joined.contains(name), "{name}: {joined:?}");
        }
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
