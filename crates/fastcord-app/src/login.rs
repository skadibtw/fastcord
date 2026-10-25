use std::fmt;
use std::sync::Arc;

use fastcord_discord::gateway::{ClientProperties, Host, HostOs};
use fastcord_discord::{
    NetworkFailure, Priority, RestClient, RestError, RetryableFailure, UserToken,
};
use fastcord_model::{Snowflake, User};
use fastcord_platform::{NativeCredentialStore, StoreError};

use crate::i18n::{tr, trf};

pub const MAX_TOKEN_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Persistence {
    Saved,
    MemoryOnly,
}

/// This is also the handoff for QR login: both methods must call `validate`
/// followed by `persist`, rather than constructing an authenticated UI directly.
pub struct Session {
    pub user: User,
    pub client: RestClient,
    token: Arc<UserToken>,
}
impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("account_id", &self.user.id)
            .field("token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl Session {
    /// The validated token, for the Gateway's Identify. Redacted everywhere it
    /// is held; never formatted.
    pub fn token(&self) -> Arc<UserToken> {
        Arc::clone(&self.token)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.client.stop_authenticated_work();
    }
}

#[derive(Clone, Debug)]
pub enum LoginOutcome {
    Authenticated(Arc<Session>, Persistence),
    NeedsMemoryConsent(Arc<Session>, StoreError),
}

#[derive(Clone, Debug)]
pub struct StoreReady {
    pub store: NativeCredentialStore,
    pub accounts: Vec<Snowflake>,
    /// Labels of the saved accounts, reconciled with `accounts`.
    pub index: crate::accounts::Index,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginError {
    Rest(RestError),
    Store(StoreError),
    CredentialMissing,
    AccountMismatch,
    InvalidInput,
    WorkerStopped,
}

impl LoginError {
    /// Only typed categories enter the login notice; raw transport errors may
    /// contain URLs, headers or credentials and are never formatted here.
    pub fn notice(self, network: &fastcord_net::NetworkContext) -> String {
        if matches!(
            self,
            Self::Rest(RestError::Retryable(RetryableFailure::Network(_)))
        ) && let fastcord_net::selector::ServiceStatus::Failed(category) = network
            .controller()
            .service_status(fastcord_net::Role::Rest)
        {
            return format!("Could not reach Discord: {category} Login stopped; try again.");
        }
        self.to_string()
    }
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rest(RestError::AuthenticationRequired | RestError::InvalidToken) => {
                f.write_str(tr("Discord rejected this token. Login stopped; paste a valid alt-account token."))
            }
            Self::Rest(RestError::Retryable(RetryableFailure::Server(status))) => {
                write!(f, "Discord returned HTTP {}. Login stopped; try again later.", status.as_u16())
            }
            Self::Rest(RestError::Retryable(RetryableFailure::Network(failure))) => {
                let reason = match failure {
                    NetworkFailure::Timeout => "the request timed out",
                    NetworkFailure::Connection => "the connection failed",
                    NetworkFailure::Body => "the response could not be read",
                    NetworkFailure::Other => "the request failed",
                };
                write!(f, "Could not reach Discord: {reason}. Login stopped; try again.")
            }
            Self::Rest(error) => f.write_str(&trf("Login stopped: {}.", &[&tr(&error.to_string())])),
            Self::Store(error) => f.write_str(&trf("{} Unlock it to restore your saved login, or explicitly choose a memory-only session.", &[&tr(&error.to_string())])),
            Self::CredentialMissing => f.write_str(tr("The saved token is no longer in the native credential store.")),
            Self::AccountMismatch => f.write_str(tr("The saved token belongs to a different account. Login stopped; forget this saved login.")),
            Self::InvalidInput => f.write_str(tr("Paste a nonempty token of at most 4096 bytes. No passwords are accepted.")),
            Self::WorkerStopped => f.write_str(tr("The login worker stopped. No login was established; try again.")),
        }
    }
}

impl std::error::Error for LoginError {}

pub async fn initialize_store() -> Result<StoreReady, StoreError> {
    // Fake mode never opens the native credential store.
    #[cfg(feature = "fake-discord")]
    if crate::fake::active() {
        return Err(StoreError::Unavailable);
    }
    tokio::task::spawn_blocking(|| {
        let store = NativeCredentialStore::new()?;
        let accounts = store.accounts()?;
        let mut index = crate::accounts::load();
        index.reconcile(&accounts);
        Ok(StoreReady {
            store,
            accounts,
            index,
        })
    })
    .await
    .map_err(|_| StoreError::Unavailable)?
}

/// The official desktop-client identity shared by password login and the
/// validated session (ADR 0005 revision 2026-10-25).
pub async fn client_properties(network: &fastcord_net::NetworkContext) -> ClientProperties {
    let versions = fastcord_discord::gateway::resolve_versions(network).await;
    let (release, installation_id) =
        tokio::task::spawn_blocking(|| (fastcord_platform::os_release(), load_installation()))
            .await
            .unwrap_or_default();
    let host = Host {
        os: HostOs::current(),
        os_version: release.version,
        os_sdk_version: release.sdk_version,
        window_manager: release.window_manager,
        distro: release.distro,
        timezone: release.timezone,
    };
    let properties = ClientProperties {
        installation_id,
        ..ClientProperties::desktop(&host, &fastcord_platform::system_locale(), &versions)
    }
    .with_launch_identity();
    fastcord_media::set_handshake_identity(
        &properties.browser_user_agent,
        &properties.accept_language(),
    );
    properties
}

const MAX_INSTALLATION_FILE_BYTES: u64 = 256;

/// The installation id Discord issued to this installation: one non-secret
/// file shared by every account, as the official client keeps it.
fn installation_path() -> Option<std::path::PathBuf> {
    crate::audio_devices::data_dir().map(|dir| dir.join("installation-id"))
}

/// Blocking; call from a worker.
fn load_installation() -> Option<String> {
    let path = installation_path()?;
    if !std::fs::metadata(&path).is_ok_and(|meta| meta.len() <= MAX_INSTALLATION_FILE_BYTES) {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let id = text.trim();
    fastcord_discord::gateway::is_installation_id(id).then(|| id.to_owned())
}

fn save_installation(id: &str) -> std::io::Result<()> {
    let path = installation_path().ok_or_else(|| std::io::Error::other("no data folder"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, id)?;
    std::fs::rename(temporary, path)
}

/// Keeps the installation id READY issued (the Gateway already adopted it on
/// the shared identity) for later launches. Writes only when it changed.
pub fn remember_installation(client: &RestClient) {
    let Some(id) = client.identity().installation_id() else {
        return;
    };
    tokio::task::spawn_blocking(move || {
        if load_installation().as_deref() != Some(id.as_str()) {
            // Best effort: without the file the next session is issued a new one.
            let _ = save_installation(&id);
        }
    });
}

/// Exactly one validation request before an authenticated account is displayed.
/// Tokens and raw error bodies never enter a displayable diagnostic.
pub async fn validate(
    network: fastcord_net::NetworkContext,
    token: Arc<UserToken>,
    expected_account: Option<Snowflake>,
) -> Result<Arc<Session>, LoginError> {
    if token.expose_secret().is_empty() || token.expose_secret().len() > MAX_TOKEN_BYTES {
        return Err(LoginError::InvalidInput);
    }
    let client = RestClient::new(
        network.clone(),
        UserToken::new(token.expose_secret().to_owned()),
        client_properties(&network).await,
    )
    .map_err(LoginError::Rest)?;
    let user: User = client
        .get("/users/@me", Priority::UserRead)
        .await
        .map_err(LoginError::Rest)?;
    check_account(&user, expected_account)?;
    Ok(Arc::new(Session {
        user,
        client,
        token,
    }))
}

fn check_account(user: &User, expected: Option<Snowflake>) -> Result<(), LoginError> {
    if user.id.0 == 0 || expected.is_some_and(|id| id != user.id) {
        Err(LoginError::AccountMismatch)
    } else {
        Ok(())
    }
}

pub async fn persist(
    session: Arc<Session>,
    store: Option<NativeCredentialStore>,
    remember: bool,
) -> LoginOutcome {
    if !remember {
        return LoginOutcome::Authenticated(session, Persistence::MemoryOnly);
    }
    let Some(store) = store else {
        return LoginOutcome::NeedsMemoryConsent(session, StoreError::Unavailable);
    };
    let saving = Arc::clone(&session);
    let result = tokio::task::spawn_blocking(move || {
        store.save(saving.user.id, saving.token.expose_secret())
    })
    .await
    .unwrap_or(Err(StoreError::Unavailable));
    match result {
        Ok(()) => LoginOutcome::Authenticated(session, Persistence::Saved),
        Err(error) => LoginOutcome::NeedsMemoryConsent(session, error),
    }
}

pub async fn token_login(
    network: fastcord_net::NetworkContext,
    token: Arc<UserToken>,
    store: Option<NativeCredentialStore>,
    remember: bool,
) -> Result<LoginOutcome, LoginError> {
    let session = validate(network, token, None).await?;
    Ok(persist(session, store, remember).await)
}

pub async fn restore(
    network: fastcord_net::NetworkContext,
    store: NativeCredentialStore,
    account: Snowflake,
) -> Result<LoginOutcome, LoginError> {
    let credential = tokio::task::spawn_blocking(move || store.load(account))
        .await
        .map_err(|_| LoginError::WorkerStopped)?
        .map_err(LoginError::Store)?
        .ok_or(LoginError::CredentialMissing)?;
    let mut secret = credential.into_secret();
    let token = Arc::new(UserToken::new(std::mem::take(&mut *secret)));
    let session = validate(network, token, Some(account)).await?;
    Ok(LoginOutcome::Authenticated(session, Persistence::Saved))
}

pub async fn delete_saved(
    store: Option<NativeCredentialStore>,
    account: Snowflake,
) -> Result<(), StoreError> {
    #[cfg(feature = "fake-discord")]
    if crate::fake::active() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || {
        let store = match store {
            Some(store) => store,
            None => NativeCredentialStore::new()?,
        };
        store.delete(account)
    })
    .await
    .unwrap_or(Err(StoreError::Unavailable))
}

#[cfg(test)]
pub(crate) fn fixture_session() -> Arc<Session> {
    let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
    let properties = ClientProperties::bundled(HostOs::Linux, "en-US").with_launch_identity();
    Arc::new(Session {
        user: serde_json::from_str(include_str!("../../../fixtures/rest/current-user.json"))
            .unwrap(),
        client: RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("dummy-offline-secret".to_owned()),
            properties,
        )
        .unwrap(),
        token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_fixture_and_saved_account_identity_are_checked() {
        let session = fixture_session();
        assert_eq!(session.user.display_name(), "Alt Fixture");
        assert_eq!(session.user.username, "alt_fixture");
        assert_eq!(check_account(&session.user, Some(session.user.id)), Ok(()));
        assert_eq!(
            check_account(&session.user, Some(Snowflake(42))),
            Err(LoginError::AccountMismatch)
        );
        let mut zero_id = session.user.clone();
        zero_id.id = Snowflake(0);
        assert_eq!(
            check_account(&zero_id, None),
            Err(LoginError::AccountMismatch)
        );
    }

    #[tokio::test]
    async fn missing_store_requires_consent_and_memory_only_is_explicit() {
        let session = fixture_session();
        assert!(matches!(
            persist(Arc::clone(&session), None, true).await,
            LoginOutcome::NeedsMemoryConsent(_, StoreError::Unavailable)
        ));
        assert!(matches!(
            persist(session, None, false).await,
            LoginOutcome::Authenticated(_, Persistence::MemoryOnly)
        ));
    }

    #[tokio::test]
    async fn invalid_input_stops_before_a_network_request() {
        for value in [
            String::new(),
            "x".repeat(MAX_TOKEN_BYTES + 1),
            "bad\r\nheader".to_owned(),
        ] {
            let outcome = validate(
                fastcord_net::NetworkContext::off(),
                Arc::new(UserToken::new(value)),
                None,
            )
            .await;
            assert!(matches!(
                outcome,
                Err(LoginError::InvalidInput | LoginError::Rest(RestError::InvalidToken))
            ));
        }
    }

    #[test]
    fn sessions_results_and_errors_never_format_credentials() {
        let session = fixture_session();
        for output in [
            format!("{session:?}"),
            format!(
                "{:?}",
                LoginOutcome::NeedsMemoryConsent(session, StoreError::Locked)
            ),
        ] {
            assert!(!output.contains("dummy-offline-secret"));
        }
        assert!(
            LoginError::Rest(RestError::AuthenticationRequired)
                .to_string()
                .contains("rejected")
        );
        assert!(
            LoginError::Store(StoreError::Locked)
                .to_string()
                .contains("memory-only")
        );
    }

    #[test]
    fn login_notice_preserves_typed_transport_failures_and_server_statuses() {
        let network = fastcord_net::NetworkContext::off();
        network.controller().note_failure(
            fastcord_net::Role::Rest,
            fastcord_net::NetworkError::TlsReset,
        );
        let connection = LoginError::Rest(RestError::Retryable(RetryableFailure::Network(
            NetworkFailure::Connection,
        )));
        assert!(
            connection
                .notice(&network)
                .contains("TLS connection was reset")
        );
        let server = LoginError::Rest(RestError::Retryable(RetryableFailure::Server(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
        )));
        let text = server.notice(&network);
        assert!(text.contains("HTTP 503"));
        assert!(!text.contains("TLS"));
        let authentication = LoginError::Rest(RestError::AuthenticationRequired);
        assert!(
            authentication
                .notice(&network)
                .contains("rejected this token")
        );
    }

    #[test]
    fn login_notice_uses_the_current_request_category_after_a_successful_connection() {
        let network = fastcord_net::NetworkContext::off();
        network
            .controller()
            .note_service_reachable(fastcord_net::Role::Rest);
        for (failure, expected) in [
            (NetworkFailure::Timeout, "timed out"),
            (NetworkFailure::Body, "response could not be read"),
        ] {
            let error = LoginError::Rest(RestError::Retryable(RetryableFailure::Network(failure)));
            assert!(error.notice(&network).contains(expected));
        }
    }
}
