#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod attachment_ui;
mod attachments;
mod audio_devices;
mod captcha;
mod changes;
mod composer;
mod emoji;
mod emoji_picker;
mod gateway;
mod history;
mod login;
mod navigation;
mod outbox;
mod password;
mod paste;
mod playback;
mod qr;
mod reactions;
mod render;
mod rich;
mod search;
mod style;
mod timeline;
mod variable_list;
mod virtual_list;
mod voice_settings;

use crate::attachments::AttachmentKey;
use std::sync::Arc;

use fastcord_discord::{RemoteAuthEvent, UserToken};
use fastcord_model::{Snowflake, VoiceStateRequest};
use fastcord_platform::{NativeCredentialStore, StoreError};
use gateway::{AttachmentAction, GatewayPanel, GatewayStatus};
use iced::widget::{
    button, center, checkbox, column, container, qr_code, row, scrollable, slider, text, text_input,
};
use iced::{Color, Element, Length, Task};
use login::{LoginError, LoginOutcome, Persistence, Session, StoreReady};
use qr::{Applied, QrAttempt, QrStage};
use zeroize::Zeroize;

const MAX_PRIVATE_RECIPIENT_INPUT_BYTES: usize = 256;
const MAX_PRIVATE_RECIPIENTS: usize = fastcord_discord::MAX_PRIVATE_RECIPIENTS;

fn search_bar_is_empty(gateway: &GatewayPanel) -> bool {
    App::search_target(gateway).is_none()
}

fn main() -> iced::Result {
    render::application(App::boot, App::update, App::view)
        .title("fastcord")
        .theme(App::theme)
        .subscription(App::subscription)
        .window_size((1080, 640))
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
    qr_attempts: u64,
    password_login: String,
    password: Arc<UserToken>,
    captcha_solved: bool,
    gateway_sessions: u64,
    attachment_purge_failed: bool,
    audio_preferences: audio_devices::Preferences,
    audio_settings: audio_devices::Settings,
    audio_saving: Option<audio_devices::Preferences>,
    window_active: bool,
    window_drawable: bool,
    /// The per-person volume sliders are open.
    voice_details: bool,
    search: search::State,
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
    Forget(Snowflake),
    RetryDelete,
    Deleted(Result<(), StoreError>),
    Gateway(u64, GatewayStatus),
    Navigation(u64),
    SelectGuild(Snowflake),
    SelectChannel(Snowflake, Snowflake),
    LeaveVoice,
    WatchStream(String),
    UnwatchStream,
    ShowStream(bool),
    VideoReady(u64),
    OpenSharePicker,
    ShareSources(Result<Vec<fastcord_platform::capture::CaptureSource>, String>),
    CloseSharePicker,
    StartShare(fastcord_platform::capture::SourceId),
    StopShare,
    GuildViewport(virtual_list::Window),
    ChannelViewport(virtual_list::Window),
    SetSelfMute(bool),
    SetDeafen(bool),
    SetParticipantVolume(Snowflake, u16),
    CommitParticipantVolume(Snowflake),
    PresetParticipantVolume(Snowflake, u16),
    OpenAudioSettings,
    CloseAudioSettings,
    RefreshAudioDevices,
    AudioDevicesListed(audio_devices::Inventory),
    Initialized(audio_devices::Preferences, Result<StoreReady, StoreError>),
    SelectAudioDevice(fastcord_audio::Direction, audio_devices::DeviceOption),
    NoiseSuppressionChanged(fastcord_audio::NoiseSuppression),
    EchoCancellationChanged(bool),
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
    AttachmentActionFinished(
        u64,
        AttachmentKey,
        AttachmentAction,
        attachment_ui::ActionOutcome,
    ),
    CloseViewer,
    Composer(composer::Event),
    Search(search::Event),
    EmojiPicker(emoji_picker::Event),
    EmojiVisibility(emoji::Occurrence, bool),
    EmojiLoaded(
        u64,
        emoji::Asset,
        u64,
        Result<attachments::DecodedAnimation, &'static str>,
    ),
    EmojiTick(std::time::Instant),
    WindowActive(bool),
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
                qr_attempts: 0,
                password_login: String::new(),
                password: Arc::new(UserToken::new(String::new())),
                captcha_solved: false,
                gateway_sessions: 0,
                attachment_purge_failed: false,
                audio_preferences: audio_devices::Preferences::default(),
                audio_settings: audio_devices::Settings::default(),
                audio_saving: None,
                window_active: true,
                window_drawable: true,
                voice_details: false,
                search: search::State::default(),
            },
            Task::perform(
                async {
                    paste::purge_stale();
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

    fn update(&mut self, message: Message) -> Task<Message> {
        let resynchronize = !matches!(
            message,
            Message::EmojiTick(_) | Message::EmojiLoaded(..) | Message::VideoReady(_)
        );
        let task = self.handle(message);
        if resynchronize {
            self.synchronize_video();
            Task::batch([task, self.synchronize_emoji()])
        } else {
            task
        }
    }

    fn synchronize_emoji(&mut self) -> Task<Message> {
        let Phase::Account { gateway, .. } = &mut self.phase else {
            return Task::none();
        };
        gateway
            .controls
            .emoji_request(&gateway.emoji_picker.request);
        gateway.emoji.synchronize(
            self.window_active
                && self.window_drawable
                && !self.audio_settings.open
                && !gateway.watch_open
                && gateway.attachment_viewer.is_none(),
            gateway.emoji_picker.request.open,
            gateway.id,
            gateway.attachments.as_ref(),
        )
    }

    fn synchronize_video(&self) {
        if let Phase::Account { gateway, .. } = &self.phase
            && let Some(session) = gateway.controls.watched()
        {
            session.set_visible(
                self.window_active
                    && self.window_drawable
                    && gateway.watch_open
                    && !self.audio_settings.open
                    && gateway.attachment_viewer.is_none()
                    && !gateway.emoji_picker.request.open,
            );
        }
    }

    fn video_subscription(&self) -> iced::Subscription<Message> {
        if let Phase::Account { gateway, .. } = &self.phase
            && let Some(session) = gateway.controls.watched()
        {
            session.subscription().map(Message::VideoReady)
        } else {
            iced::Subscription::none()
        }
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        let activity = iced::event::listen_with(|event, _, _| match event {
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowSize(size))
            }
            iced::Event::Window(iced::window::Event::FileDropped(path)) => {
                Some(Message::FileDropped(path))
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
        ])
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
                self.audio_preferences = preferences;
                return self.handle(Message::StoreReady(store));
            }
            Message::OpenAudioSettings | Message::RefreshAudioDevices => {
                self.audio_settings.open = true;
                if !self.audio_settings.probing {
                    self.audio_settings.probing = true;
                    return Task::perform(audio_devices::enumerate(), Message::AudioDevicesListed);
                }
            }
            Message::CloseAudioSettings => self.audio_settings.open = false,
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
                        self.notice = None;
                        return self.enter_account(session, persistence);
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
                if status == GatewayStatus::AuthenticationRequired {
                    // Discord rejected the token: stop all of this account's
                    // work and return to login.
                    let cache = if let Phase::Account { gateway, .. } = &self.phase {
                        gateway.attachments.clone()
                    } else {
                        None
                    };
                    if let Phase::Account { session, .. } = &self.phase {
                        session.client.stop_authenticated_work();
                    }
                    self.reset_login();
                    self.notice = Some(
                        "Discord no longer accepts this login (the token was revoked, expired, or reset). Log in again. A saved copy of the old token can be removed with Forget saved login.".to_owned(),
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
            Message::CloseViewer => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.attachment_viewer = None;
                }
            }
            Message::Navigation(id) => {
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
            }
            Message::ShowDirects(on) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.directs = on;
                }
            }
            Message::ToggleVoiceDetails => self.voice_details = !self.voice_details,
            Message::Search(event) => return self.search_event(event),
            Message::SelectGuild(guild) => {
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
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    let join = gateway.navigation.selected_guild == Some(guild)
                        && gateway.navigation.channels.rows.iter().any(|row| {
                            row.id == channel
                                && row.kind
                                    == fastcord_discord::state::navigation::ChannelKind::Voice
                                && row.enabled
                        });
                    gateway.controls.select_channel(guild, channel);
                    if join {
                        gateway.voice_guild = Some(guild);
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
            Message::LeaveVoice => {
                if let Phase::Account { gateway, .. } = &mut self.phase
                    && let Some(guild) = gateway.voice_guild.take()
                {
                    gateway
                        .controls
                        .request_voice_state(VoiceStateRequest::leave(Some(guild)));
                }
                if let Phase::Account { gateway, .. } = &mut self.phase
                    && let Some(session) = gateway.controls.watched()
                {
                    if !gateway.controls.unwatch_stream(session.key.clone()) {
                        self.notice = Some(
                            "Could not queue Stop watching; retry before leaving voice.".into(),
                        );
                    }
                    gateway.watch_open = false;
                }
            }
            Message::WatchStream(key) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    let controls = gateway.controls.voice_controls();
                    if gateway.controls.voice_audio_state() == gateway::VoiceAudioState::Running
                        && controls.streams.iter().any(|offer| offer.key == key)
                        && gateway.controls.watch_stream(key)
                    {
                        gateway.watch_open = true;
                        self.notice = None;
                    } else {
                        self.notice = Some("Could not watch this stream. Join its voice channel, or stop the current watch and retry.".into());
                    }
                }
            }
            Message::UnwatchStream => {
                if let Phase::Account { gateway, .. } = &mut self.phase
                    && let Some(session) = gateway.controls.watched()
                {
                    if gateway.controls.unwatch_stream(session.key.clone()) {
                        gateway.watch_open = false;
                        self.notice = None;
                    } else {
                        self.notice = Some("Could not queue Stop watching; retry.".into());
                    }
                }
            }
            Message::ShowStream(open) => {
                if let Phase::Account { gateway, .. } = &mut self.phase {
                    gateway.watch_open = open && gateway.controls.watched().is_some();
                }
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
                                fastcord_platform::capture::list_sources()
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
                        Ok(sources) => gateway::SharePicker::Listed(sources),
                        Err(error) => gateway::SharePicker::Failed(error),
                    };
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
                        gateway.controls.start_share(source, request);
                        self.notice = None;
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
                if let Phase::Account { gateway, .. } = &self.phase
                    && let Some(session) = gateway.controls.watched()
                {
                    session.consume(id);
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
                if let Phase::Account { gateway, .. } = &mut self.phase {
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
            }
            Message::WindowSize(size) => {
                self.window_drawable = size.width > 0.0 && size.height > 0.0;
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
                        timeline::Event::OpenLink(url) => {
                            if rich::safe_url(&url) {
                                let _ = fastcord_platform::open_url(&url);
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
                        | timeline::Event::RetryChange { .. }
                        | timeline::Event::DismissChange { .. }
                        | timeline::Event::ToggleReaction { .. }
                        | timeline::Event::InspectReaction { .. }
                        | timeline::Event::CloseReactionUsers
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
                    composer::update(
                        &mut gateway.composer,
                        event,
                        &gateway.timeline,
                        &gateway.controls,
                    );
                    if refocus {
                        return iced::widget::operation::focus(composer::EDITOR_ID);
                    }
                }
            }
            Message::Logout => {
                if let Phase::Account {
                    session, gateway, ..
                } = &self.phase
                {
                    let account = session.user.id;
                    let cache = gateway.attachments.clone();
                    session.client.stop_authenticated_work();
                    // Dropping the account phase aborts attachment workers before purge starts.
                    let delete = self.start_delete(account);
                    return match cache {
                        Some(cache) => Task::batch([
                            delete,
                            Task::perform(async move { cache.purge().await }, |result| {
                                Message::AttachmentPurged(result.is_ok())
                            }),
                        ]),
                        None => delete,
                    };
                }
            }
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
                            let purge_failed = self.attachment_purge_failed;
                            self.attachment_purge_failed = false;
                            self.reset_login();
                            let voice_purge_failed = voice_settings::purge(account).is_err();
                            self.notice = Some(if purge_failed || voice_purge_failed {
                                "Logged out locally. The saved credential was removed, but some local attachment cache or saved voice volumes could not be cleared. Other Discord sessions were not revoked."
                            } else {
                                "Logged out locally. The saved credential was removed and account state was cleared. Other Discord sessions were not revoked."
                            }.to_owned());
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

    /// The overlay that lists the screens and windows that can be shared.
    fn share_picker_view(picker: &gateway::SharePicker) -> Element<'_, Message> {
        use fastcord_platform::capture::SourceKind;
        let mut content = column![
            row![
                text("Share your screen").size(22).width(Length::Fill),
                button("Cancel").on_press(Message::CloseSharePicker),
            ]
            .spacing(12),
            text("Choose a screen or a window. Windows draws a border around what is shared, and your voice stays connected.").size(12),
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
        self.clear_token();
        self.qr_attempts += 1;
        let id = self.qr_attempts;
        let (task, worker) =
            Task::run(qr::events(), move |event| Message::Qr(id, event)).abortable();
        self.phase = Phase::Qr(QrAttempt::new(id, worker));
        task
    }

    fn save_audio_preferences(&mut self) -> Task<Message> {
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
        let controls = gateway::NavigationBridge::new();
        controls.set_account_id(session.user.id);
        controls.set_volume_settings(voice_settings::load(session.user.id));
        controls.set_audio_devices(&self.audio_preferences);
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
        if self.audio_settings.open {
            let (input_error, output_error, noise_status, audio_stats) = match &self.phase {
                Phase::Account { gateway, .. } => {
                    let controls = gateway.controls.voice_controls();
                    (
                        controls.input_error,
                        controls.output_error,
                        controls.noise_status,
                        controls.audio_stats,
                    )
                }
                _ => (None, None, None, None),
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
            let in_voice = voice_audio != gateway::VoiceAudioState::Idle;
            let running = voice_audio == gateway::VoiceAudioState::Running;
            let bold = iced::Font {
                weight: iced::font::Weight::Bold,
                ..iced::Font::DEFAULT
            };

            // ---- the voice bar: who is in the call, Go Live ----
            let voice_bar: Option<Element<'_, Message>> = (in_voice
                || !voice_controls.streams.is_empty()
                || watching.is_some())
            .then(|| {
                let mut people = row![].spacing(8).align_y(iced::alignment::Vertical::Center);
                for participant in &voice_controls.participants {
                    let user = participant.user_id;
                    let name = format!("User {}", user.0 % 10_000);
                    people = people.push(
                        container(
                            row![
                                container(text(style::initial(&name)).size(12).font(bold))
                                    .center(24)
                                    .style(style::avatar(style::avatar_color(user.0))),
                                text(name).size(13),
                            ]
                            .spacing(6)
                            .align_y(iced::alignment::Vertical::Center),
                        )
                        .padding([3, 8])
                        .style(style::speaking(participant.speaking)),
                    );
                }
                if voice_controls.participants.is_empty() {
                    people = people.push(
                        text(if in_voice {
                            "Waiting for others to speak…"
                        } else {
                            ""
                        })
                        .size(12)
                        .style(text::secondary),
                    );
                }
                let mut actions = row![
                    button(
                        text(if self.voice_details {
                            "Hide volume"
                        } else {
                            "Volume"
                        })
                        .size(12)
                    )
                    .on_press(Message::ToggleVoiceDetails)
                    .style(style::tool_button(self.voice_details, false)),
                ]
                .spacing(8)
                .align_y(iced::alignment::Vertical::Center);
                if let Some(watching) = &watching {
                    actions = actions
                        .push(text(watching.description()).size(12))
                        .push(
                            button(text("Show stream").size(12))
                                .on_press(Message::ShowStream(true))
                                .style(style::action_button(false)),
                        )
                        .push(
                            button(text("Stop watching").size(12))
                                .on_press(Message::UnwatchStream)
                                .style(style::action_button(true)),
                        );
                } else {
                    for offer in voice_controls.streams.iter() {
                        actions = actions.push(
                            button(text(format!("Watch User {}", offer.owner.0 % 10_000)).size(12))
                                .on_press_maybe(
                                    running.then(|| Message::WatchStream(offer.key.clone())),
                                )
                                .style(style::action_button(false)),
                        );
                    }
                }
                let share = gateway.controls.share_view();
                if share.active {
                    actions = actions.push(text(share.text.clone()).size(12)).push(
                        button(text("Stop sharing").size(12))
                            .on_press(Message::StopShare)
                            .style(style::action_button(true)),
                    );
                } else if cfg!(windows) {
                    actions = actions.push(
                        button(text("Share screen").size(12))
                            .on_press_maybe(
                                (running
                                    && gateway.voice_guild.is_some()
                                    && !gateway.share_picker.is_open())
                                .then_some(Message::OpenSharePicker),
                            )
                            .style(style::action_button(false)),
                    );
                    if !share.text.is_empty() {
                        actions = actions.push(text(share.text.clone()).size(12));
                    }
                }
                let mut bar = column![
                    scrollable(people).direction(scrollable::Direction::Horizontal(
                        scrollable::Scrollbar::default()
                    )),
                    scrollable(actions).direction(scrollable::Direction::Horizontal(
                        scrollable::Scrollbar::default()
                    )),
                ]
                .spacing(6);
                if self.voice_details {
                    for participant in &voice_controls.participants {
                        let user = participant.user_id;
                        let volume = voice_controls.volume_percent(user);
                        bar = bar.push(
                            row![
                                text(format!("User {}", user.0 % 10_000))
                                    .size(12)
                                    .width(Length::Fixed(110.0)),
                                slider(0.0..=200.0, volume as f32, move |value| {
                                    Message::SetParticipantVolume(user, value.round() as u16)
                                })
                                .on_release(Message::CommitParticipantVolume(user))
                                .width(Length::Fixed(220.0)),
                                button(text("0%").size(11))
                                    .on_press(Message::PresetParticipantVolume(user, 0))
                                    .style(style::tool_button(false, false)),
                                button(text("100%").size(11))
                                    .on_press(Message::PresetParticipantVolume(user, 100))
                                    .style(style::tool_button(false, false)),
                                text(format!("{volume}%")).size(12),
                            ]
                            .spacing(8)
                            .align_y(iced::alignment::Vertical::Center),
                        );
                    }
                    bar = bar.push(
                        text(
                            "Server restrictions: ".to_owned()
                                + if voice_controls.server_muted {
                                    "muted. "
                                } else {
                                    "none to mute. "
                                }
                                + if voice_controls.server_deafened {
                                    "Deafened."
                                } else {
                                    ""
                                },
                        )
                        .size(11)
                        .style(text::secondary),
                    );
                }
                container(bar)
                    .padding(8)
                    .width(Length::Fill)
                    .style(style::card)
                    .into()
            });

            // ---- connection and notices above the conversation ----
            let mut notes: Vec<(String, bool)> = Vec::new();
            let mut say = |line: String, danger: bool| {
                if !line.is_empty() {
                    notes.push((line, danger));
                }
            };
            if !matches!(gateway.status, gateway::GatewayStatus::Ready(_)) {
                say(gateway.status.describe(), false);
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
            for (line, danger) in notes {
                let line = text(line).size(12);
                banner_column = banner_column.push(if danger {
                    line.style(text::danger)
                } else {
                    line.style(text::secondary)
                });
            }
            let banner: Option<Element<'_, Message>> = Some(banner_column.into());

            // ---- the user's panel at the foot of the sidebar ----
            let display = session.user.display_name().to_owned();
            let mut foot = column![];
            if in_voice {
                foot = foot.push(
                    container(
                        row![
                            column![
                                text(match voice_audio {
                                    gateway::VoiceAudioState::Connecting => "Connecting…",
                                    gateway::VoiceAudioState::EngineFailed => "Audio failed",
                                    _ => "Voice connected",
                                })
                                .size(13)
                                .font(bold)
                                .color(match voice_audio {
                                    gateway::VoiceAudioState::EngineFailed => style::RED,
                                    gateway::VoiceAudioState::Connecting => style::YELLOW,
                                    _ => style::GREEN,
                                }),
                            ]
                            .width(Length::Fill),
                            button(text("Leave").size(12))
                                .on_press_maybe(gateway.voice_guild.map(|_| Message::LeaveVoice),)
                                .style(style::action_button(true)),
                        ]
                        .align_y(iced::alignment::Vertical::Center),
                    )
                    .padding([8, 10])
                    .style(style::footer),
                );
            }
            foot = foot.push(
                container(
                    row![
                        container(text(style::initial(&display)).size(14).font(bold))
                            .center(32)
                            .style(style::avatar(style::avatar_color(session.user.id.0))),
                        column![
                            text(display)
                                .size(13)
                                .font(bold)
                                .wrapping(text::Wrapping::None),
                            text(if *persistence == Persistence::Saved {
                                "Saved login"
                            } else {
                                "Memory-only"
                            })
                            .size(11)
                            .style(text::secondary),
                        ]
                        .width(Length::Fill),
                        button(
                            text(if voice_controls.self_muted {
                                "Muted"
                            } else {
                                "Mic"
                            })
                            .size(12)
                        )
                        .on_press_maybe(
                            running.then_some(Message::SetSelfMute(!voice_controls.self_muted)),
                        )
                        .style(style::tool_button(
                            !voice_controls.self_muted,
                            voice_controls.self_muted
                        )),
                        button(
                            text(if voice_controls.deafened {
                                "Deaf"
                            } else {
                                "Sound"
                            })
                            .size(12)
                        )
                        .on_press_maybe(
                            running.then_some(Message::SetDeafen(!voice_controls.deafened)),
                        )
                        .style(style::tool_button(
                            !voice_controls.deafened,
                            voice_controls.deafened
                        )),
                        button(text("Audio").size(12))
                            .on_press(Message::OpenAudioSettings)
                            .style(style::tool_button(true, false)),
                        button(text("Exit").size(12))
                            .on_press(Message::Logout)
                            .style(style::tool_button(false, false)),
                    ]
                    .spacing(2)
                    .align_y(iced::alignment::Vertical::Center),
                )
                .padding([8, 8])
                .style(style::footer),
            );

            let account = container(navigation::view(
                &gateway.navigation,
                &gateway.timeline,
                &gateway.composer,
                gateway.interaction(),
                navigation::PrivateConversation {
                    recipients: &gateway.private.recipients,
                    notice: gateway.private.notice.as_deref(),
                },
                timeline::Attachments {
                    account: session.user.id,
                    views: &gateway.attachment_views,
                    emojis: &gateway.emoji.views,
                    revealed: &gateway.revealed,
                },
                voice_audio,
                navigation::Chrome {
                    directs: gateway.directs,
                    footer: foot.into(),
                    banner,
                    voice_bar,
                },
            ))
            .height(Length::Fill);
            // The larger image view covers the account screen, which stays in
            // the widget tree underneath so the timeline keeps its scroll state.
            return match &gateway.attachment_viewer {
                None if gateway.watch_open
                    && watching.is_some()
                    && !gateway.emoji_picker.request.open =>
                {
                    let session = watching.expect("watch pane has a session");
                    let pane = container(
                        column![
                            row![
                                text(session.description()).width(Length::Fill),
                                button("Back to messages").on_press(Message::ShowStream(false)),
                                button("Stop watching").on_press(Message::UnwatchStream),
                            ]
                            .spacing(12),
                            text(self.notice.as_deref().unwrap_or("")).size(12),
                            playback::view::<Message>(session),
                        ]
                        .spacing(12),
                    )
                    .padding(20)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(|_| container::Style {
                        background: Some(Color::from_rgb8(24, 25, 30).into()),
                        ..Default::default()
                    });
                    iced::widget::stack![account, pane].into()
                }
                None if gateway.share_picker.is_open() && !gateway.emoji_picker.request.open => {
                    iced::widget::stack![account, Self::share_picker_view(&gateway.share_picker)]
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
                None => account.into(),
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
            text("fastcord").size(36),
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
                    button(text(format!("Restore account {account}")))
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
        assert!(app.token.expose_secret().is_empty());
        // The saved credential is not silently deleted; the user decides.
        assert!(app.saved_accounts.contains(&account));
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
        assert!(bridge.watch_stream("guild:41:127:99"));
        let watched = bridge.watched().unwrap();
        assert_eq!(app.video_subscription().units(), 0);
        let _ = app.update(Message::ShowStream(true));
        assert_eq!(app.video_subscription().units(), 1);
        let _ = app.update(Message::WindowActive(false));
        assert_eq!(app.video_subscription().units(), 0);
        assert!(!watched.visible.load(Ordering::Acquire));
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
        let _ = app.update(Message::UnwatchStream);
        assert_eq!(app.video_subscription().units(), 0);
        assert!(bridge.watched().is_none());
        assert!(!watched.active.load(Ordering::Acquire));
        assert!(watched.frame().is_none());
    }
}
