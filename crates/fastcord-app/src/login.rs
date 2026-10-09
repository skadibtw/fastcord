use std::fmt;
use std::sync::Arc;

use fastcord_discord::{Priority, RestClient, RestError, UserToken};
use fastcord_model::{Snowflake, User};
use fastcord_platform::{NativeCredentialStore, StoreError};

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

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rest(RestError::AuthenticationRequired | RestError::InvalidToken) => {
                f.write_str("Discord rejected this token. Login stopped; paste a valid alt-account token.")
            }
            Self::Rest(RestError::Retryable(_)) => {
                f.write_str("Could not reach Discord or Discord is temporarily unavailable. Login stopped; try again.")
            }
            Self::Rest(error) => write!(f, "Login stopped: {error}."),
            Self::Store(error) => write!(f, "{error} Unlock it to restore your saved login, or explicitly choose a memory-only session."),
            Self::CredentialMissing => f.write_str("The saved token is no longer in the native credential store."),
            Self::AccountMismatch => f.write_str("The saved token belongs to a different account. Login stopped; forget this saved login."),
            Self::InvalidInput => f.write_str("Paste a nonempty token of at most 4096 bytes. No passwords are accepted."),
            Self::WorkerStopped => f.write_str("The login worker stopped. No login was established; try again."),
        }
    }
}

impl std::error::Error for LoginError {}

pub async fn initialize_store() -> Result<StoreReady, StoreError> {
    tokio::task::spawn_blocking(|| {
        let store = NativeCredentialStore::new()?;
        let accounts = store.accounts()?;
        Ok(StoreReady { store, accounts })
    })
    .await
    .map_err(|_| StoreError::Unavailable)?
}

/// Exactly one validation request before an authenticated account is displayed.
/// Tokens and raw error bodies never enter a displayable diagnostic.
pub async fn validate(
    token: Arc<UserToken>,
    expected_account: Option<Snowflake>,
) -> Result<Arc<Session>, LoginError> {
    if token.expose_secret().is_empty() || token.expose_secret().len() > MAX_TOKEN_BYTES {
        return Err(LoginError::InvalidInput);
    }
    let client = RestClient::new(UserToken::new(token.expose_secret().to_owned()))
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
    token: Arc<UserToken>,
    store: Option<NativeCredentialStore>,
    remember: bool,
) -> Result<LoginOutcome, LoginError> {
    let session = validate(token, None).await?;
    Ok(persist(session, store, remember).await)
}

pub async fn restore(
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
    let session = validate(token, Some(account)).await?;
    Ok(LoginOutcome::Authenticated(session, Persistence::Saved))
}

pub async fn delete_saved(
    store: Option<NativeCredentialStore>,
    account: Snowflake,
) -> Result<(), StoreError> {
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
    Arc::new(Session {
        user: serde_json::from_str(include_str!("../../../fixtures/rest/current-user.json"))
            .unwrap(),
        client: RestClient::new(UserToken::new("dummy-offline-secret".to_owned())).unwrap(),
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
            let outcome = validate(Arc::new(UserToken::new(value)), None).await;
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
}
