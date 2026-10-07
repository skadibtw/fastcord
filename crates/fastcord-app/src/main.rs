#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod login;
mod qr;

use std::sync::Arc;

use fastcord_discord::{RemoteAuthEvent, UserToken};
use fastcord_model::Snowflake;
use fastcord_platform::{NativeCredentialStore, StoreError};
use iced::widget::{
    button, center, checkbox, column, container, qr_code, row, scrollable, text, text_input,
};
use iced::{Color, Element, Length, Task};
use zeroize::Zeroize;

use login::{LoginError, LoginOutcome, Persistence, Session, StoreReady};
use qr::{Applied, QrAttempt, QrStage};

fn main() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title("fastcord")
        .window_size((760, 640))
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
    },
    Deleting {
        account: Snowflake,
    },
    DeleteFailed {
        account: Snowflake,
        error: StoreError,
    },
    Qr(QrAttempt),
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
            },
            Task::perform(login::initialize_store(), Message::StoreReady),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
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
            Message::TokenEdited(token) if matches!(self.phase, Phase::Login) => {
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
                        self.phase = Phase::Account {
                            session,
                            persistence,
                        };
                        self.notice = None;
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
                    self.phase = Phase::Account {
                        session: Arc::clone(session),
                        persistence: Persistence::MemoryOnly,
                    };
                }
            }
            Message::CancelLogin if matches!(self.phase, Phase::MemoryConsent { .. }) => {
                self.reset_login();
            }
            Message::Logout => {
                if let Phase::Account { session, .. } = &self.phase {
                    let account = session.user.id;
                    session.client.stop_authenticated_work();
                    // Always remove this account's credential, even when this
                    // session was memory-only: discovery may have failed while
                    // the store was locked and an older saved login may exist.
                    return self.start_delete(account);
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
                            self.reset_login();
                            self.notice = Some("Logged out locally. The saved credential was removed and account state was cleared. Other Discord sessions were not revoked.".to_owned());
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

    /// Starts a fresh attempt (new key pair, new socket). Replacing the phase
    /// drops any previous attempt, which aborts its worker.
    fn start_qr(&mut self) -> Task<Message> {
        self.clear_token();
        self.qr_attempts += 1;
        let id = self.qr_attempts;
        let (task, worker) =
            Task::run(qr::events(), move |event| Message::Qr(id, event)).abortable();
        self.phase = Phase::Qr(QrAttempt::new(id, worker));
        task
    }

    fn view(&self) -> Element<'_, Message> {
        let content: Element<'_, Message> = match &self.phase {
            Phase::Starting => text("Looking for a saved login in the native credential store…").into(),
            Phase::Login => self.login_view(),
            Phase::Qr(attempt) => self.qr_view(attempt),
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
            Phase::Account { session, persistence } => column![
                text("Logged in").size(32),
                text(session.user.display_name()).size(26),
                text(format!("@{}", session.user.username)),
                text(format!("Account ID: {}", session.user.id)),
                text(match persistence {
                    Persistence::Saved => "Saved securely in the native credential store. This account will be restored on launch.",
                    Persistence::MemoryOnly => "Memory-only session: this token was not saved. Login is required next launch unless a previously saved credential still exists.",
                }),
                button("Log out").on_press(Message::Logout),
            ].spacing(20).into(),
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
            text("Warning: third-party Discord clients violate Discord’s Terms of Service and may get your account banned. Use an alternate account. Never share your token; it grants access to your account. fastcord never asks for your password."),
            checkbox(self.acknowledged).label("I understand the risk and am using an alternate account").on_toggle(Message::Acknowledge),
            checkbox(self.memory_only).label("Memory-only session (do not save this login)").on_toggle(Message::MemoryOnly),
            text("Otherwise, the login is saved securely using Windows Credential Manager, macOS Keychain, or Linux Secret Service. Native-store and network operations do not run on the UI thread."),
            text("Log in with QR code (recommended)").size(24),
            text("Open Discord on your phone, go to Settings, choose Scan QR Code, and scan the code fastcord shows. You never handle the token yourself."),
            button("Show QR code").on_press_maybe(self.acknowledged.then_some(Message::StartQr)),
            text("Token login (advanced)").size(24),
            text_input("Paste your alt-account token", self.token.expose_secret())
                .id("token-input")
                .secure(true)
                .on_input(token_edited)
                .on_submit_maybe(self.can_login().then_some(Message::Login))
                .padding(12),
            button("Log in with token").on_press_maybe(self.can_login().then_some(Message::Login)),
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
}
