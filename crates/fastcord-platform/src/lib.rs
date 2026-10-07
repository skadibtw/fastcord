//! Native credential storage. All methods may block and belong on a worker,
//! never the UI thread. No default/global keyring store or plaintext fallback.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use fastcord_model::Snowflake;
use keyring_core::{CredentialStore, Entry};
use zeroize::{Zeroize, Zeroizing};

const SERVICE: &str = "fastcord";

/// The user's primary locale as a BCP 47 tag (for example `en-US`), reported in
/// the Gateway client profile. Falls back to `en-US` when the OS reports none
/// or something that is not a plausible tag.
pub fn system_locale() -> String {
    sys_locale::get_locale()
        .filter(|tag| is_locale_tag(tag))
        .unwrap_or_else(|| "en-US".to_owned())
}

fn is_locale_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 35
        && tag.split('-').all(|part| {
            (1..=8).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

/// Deliberately categorical: native errors may contain secret data or attributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreError {
    Locked,
    Unavailable,
    InvalidCredential,
    Ambiguous,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Locked => "The native credential store is locked or access was denied.",
            Self::Unavailable => "The native credential store is unavailable.",
            Self::InvalidCredential => "The saved credential has an invalid format.",
            Self::Ambiguous => "Multiple credentials match this account in the native store.",
        })
    }
}

impl std::error::Error for StoreError {}

fn store_error(error: keyring_core::Error) -> StoreError {
    match error {
        keyring_core::Error::NoStorageAccess(_) => StoreError::Locked,
        keyring_core::Error::BadEncoding(mut bytes)
        | keyring_core::Error::BadDataFormat(mut bytes, _) => {
            bytes.zeroize();
            StoreError::InvalidCredential
        }
        keyring_core::Error::Invalid(_, _) | keyring_core::Error::TooLong(_, _) => {
            StoreError::InvalidCredential
        }
        keyring_core::Error::Ambiguous(_) => StoreError::Ambiguous,
        _ => StoreError::Unavailable,
    }
}

/// A loaded secret: not serializable, not displayable, redacted, zeroized on drop.
pub struct StoredCredential(Zeroizing<String>);

impl StoredCredential {
    pub fn into_secret(self) -> Zeroizing<String> {
        self.0
    }
}

impl fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoredCredential([REDACTED])")
    }
}

/// Exactly one native backend per target. Clones serialize store operations:
/// native backends do not guarantee simultaneous same-entry writes are ordered.
#[derive(Clone)]
pub struct NativeCredentialStore {
    inner: Arc<CredentialStore>,
    access: Arc<Mutex<()>>,
    service: &'static str,
}

impl fmt::Debug for NativeCredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeCredentialStore")
    }
}

impl NativeCredentialStore {
    pub fn new() -> Result<Self, StoreError> {
        #[cfg(target_os = "windows")]
        let inner = windows_native_keyring_store::Store::new().map_err(store_error)?;
        #[cfg(target_os = "macos")]
        let inner = apple_native_keyring_store::keychain::Store::new().map_err(store_error)?;
        #[cfg(target_os = "linux")]
        let inner = zbus_secret_service_keyring_store::Store::new().map_err(store_error)?;
        #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
        {
            Ok(Self {
                inner,
                access: Arc::new(Mutex::new(())),
                service: SERVICE,
            })
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        Err(StoreError::Unavailable)
    }

    /// Discover account IDs from native metadata without reading any secrets.
    /// No separate configuration file or non-account credential is necessary.
    pub fn accounts(&self) -> Result<Vec<Snowflake>, StoreError> {
        let _guard = self.access.lock().map_err(|_| StoreError::Unavailable)?;
        #[cfg(target_os = "windows")]
        let pattern = format!(r"^[0-9]{{1,20}}\.{}$", self.service);
        #[cfg(target_os = "windows")]
        let search = HashMap::from([("pattern", pattern.as_str())]);
        #[cfg(not(target_os = "windows"))]
        let search = HashMap::from([("service", self.service)]);
        let entries = self.inner.search(&search).map_err(store_error)?;
        let mut accounts = Vec::with_capacity(entries.len());
        for entry in entries {
            if let Some((service, account)) = entry.get_specifiers()
                && service == self.service
                && let Ok(id) = account.parse::<Snowflake>()
                && id.0 != 0
            {
                accounts.push(id);
            }
        }
        accounts.sort_unstable();
        accounts.dedup();
        Ok(accounts)
    }

    pub fn load(&self, account: Snowflake) -> Result<Option<StoredCredential>, StoreError> {
        let _guard = self.access.lock().map_err(|_| StoreError::Unavailable)?;
        match self.entry(account)?.get_password() {
            Ok(secret) => Ok(Some(StoredCredential(Zeroizing::new(secret)))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }

    /// Call only after GET /users/@me has validated this token and account ID.
    pub fn save(&self, account: Snowflake, token: &str) -> Result<(), StoreError> {
        let _guard = self.access.lock().map_err(|_| StoreError::Unavailable)?;
        self.entry(account)?
            .set_password(token)
            .map_err(store_error)
    }

    /// Idempotent. A missing credential is already logged out locally.
    pub fn delete(&self, account: Snowflake) -> Result<(), StoreError> {
        let _guard = self.access.lock().map_err(|_| StoreError::Unavailable)?;
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(store_error(error)),
        }
    }

    fn entry(&self, account: Snowflake) -> Result<Entry, StoreError> {
        if account.0 == 0 {
            return Err(StoreError::InvalidCredential);
        }
        self.inner
            .build(self.service, &account.to_string(), None)
            .map_err(store_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_tags_are_validated_before_being_reported() {
        for good in ["en-US", "ru", "zh-Hant-TW", "sr-Latn-RS"] {
            assert!(is_locale_tag(good), "{good}");
        }
        for bad in [
            "",
            "en_US.UTF-8",
            "en US",
            "-",
            "en--US",
            "waytoolongsubtag-x",
        ] {
            assert!(!is_locale_tag(bad), "{bad}");
        }
        assert!(is_locale_tag(&system_locale()));
    }

    fn mock_store() -> NativeCredentialStore {
        NativeCredentialStore {
            inner: keyring_core::mock::Store::new().unwrap(),
            access: Arc::new(Mutex::new(())),
            service: SERVICE,
        }
    }

    #[test]
    fn save_load_overwrite_and_idempotent_logout_use_account_id() {
        let store = mock_store();
        let account = Snowflake(123);
        assert!(store.load(account).unwrap().is_none());
        store.save(account, "dummy-offline-secret").unwrap();
        assert_eq!(store.accounts().unwrap(), [account]);
        let loaded = store.clone().load(account).unwrap().unwrap();
        assert_eq!(format!("{loaded:?}"), "StoredCredential([REDACTED])");
        assert!(loaded.into_secret().as_str() == "dummy-offline-secret");
        store.save(account, "replacement-dummy-secret").unwrap();
        assert!(
            store.load(account).unwrap().unwrap().into_secret().as_str()
                == "replacement-dummy-secret"
        );
        store.delete(account).unwrap();
        store.delete(account).unwrap();
        assert!(store.load(account).unwrap().is_none());
    }

    #[test]
    fn discovery_ignores_other_services_and_non_account_metadata() {
        let store = mock_store();
        for (service, account) in [
            ("fastcord-other", "4"),
            (SERVICE, "metadata"),
            (SERVICE, "0"),
            (SERVICE, "+123"),
            (SERVICE, "18446744073709551616"),
        ] {
            store
                .inner
                .build(service, account, None)
                .unwrap()
                .set_password("dummy")
                .unwrap();
        }
        store.save(Snowflake(10), "dummy").unwrap();
        store.save(Snowflake(2), "dummy").unwrap();
        assert_eq!(store.accounts().unwrap(), [Snowflake(2), Snowflake(10)]);
        assert_eq!(
            store.save(Snowflake(0), "dummy"),
            Err(StoreError::InvalidCredential)
        );
    }

    #[test]
    fn locked_store_never_silently_saves_or_reports_a_successful_logout() {
        let store = mock_store();
        let account = Snowflake(123);
        let entry = store.entry(account).unwrap();
        let mock = entry
            .as_any()
            .downcast_ref::<keyring_core::mock::Cred>()
            .unwrap();
        mock.set_error(keyring_core::Error::NoStorageAccess(Box::new(
            std::io::Error::other("secret-context"),
        )));
        assert_eq!(store.save(account, "dummy"), Err(StoreError::Locked));
        assert!(store.load(account).unwrap().is_none());
        store.save(account, "dummy").unwrap();
        mock.set_error(keyring_core::Error::NoStorageAccess(Box::new(
            std::io::Error::other("secret-context"),
        )));
        assert_eq!(store.delete(account), Err(StoreError::Locked));
        assert!(store.load(account).unwrap().is_some());
    }

    #[test]
    fn native_error_details_and_malformed_secret_bytes_are_redacted() {
        let errors = [
            keyring_core::Error::BadEncoding(b"secret-context".to_vec()),
            keyring_core::Error::Invalid("secret-context".into(), "secret-context".into()),
            keyring_core::Error::PlatformFailure(Box::new(std::io::Error::other("secret-context"))),
            keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other("secret-context"))),
        ];
        for error in errors {
            let error = store_error(error);
            assert!(!format!("{error:?} {error}").contains("secret-context"));
        }
        assert_eq!(format!("{:?}", mock_store()), "NativeCredentialStore");
    }

    /// Manual only: operates on a separate test service, never a real account.
    #[test]
    #[ignore = "requires an unlocked native store in an interactive desktop session"]
    fn native_dummy_save_restart_restore_delete() {
        let account = Snowflake(5);
        let mut store = NativeCredentialStore::new().unwrap();
        store.service = "fastcord-m5-test";
        store.delete(account).unwrap();
        let result = (|| -> Result<(), StoreError> {
            store.save(account, "dummy-native-milestone-five")?;
            let mut restarted = NativeCredentialStore::new()?;
            restarted.service = "fastcord-m5-test";
            assert_eq!(restarted.accounts()?, [account]);
            let restored = restarted.load(account)?.unwrap();
            assert!(restored.into_secret().as_str() == "dummy-native-milestone-five");
            restarted.delete(account)?;
            assert!(restarted.load(account)?.is_none());
            assert!(restarted.accounts()?.is_empty());
            Ok(())
        })();
        store.delete(account).unwrap();
        result.unwrap();
    }
}
