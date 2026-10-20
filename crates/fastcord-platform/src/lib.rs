//! Native credential storage, and screen capture (`capture`). All methods may
//! block and belong on a worker, never the UI thread. No default/global keyring
//! store or plaintext fallback.

pub mod capture;
pub mod clipboard;
pub mod notify;
pub mod realtime;
pub mod shortcut;

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
/// A native file handler could not be started. OS error details are kept out of
/// user-visible messages because they can contain private paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileOpenError;

impl fmt::Display for FileOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("The attachment could not be opened by the operating system.")
    }
}

impl std::error::Error for FileOpenError {}

/// Opens an already-downloaded local file with the OS default application.
/// This deliberately never routes through a shell or accepts a remote URL, and
/// opens only passive media/document types (`is_passive_file`): an attachment
/// is untrusted content, and a denylist cannot keep up with everything an OS
/// will run, install or mount, so every other type can only be saved, never
/// launched from here. On Windows the file is first marked as downloaded from
/// the Internet zone (`mark_downloaded`); if that fails the file is not opened.
pub fn open_local_file(path: &std::path::Path) -> Result<(), FileOpenError> {
    if !is_passive_file(path) {
        return Err(FileOpenError);
    }
    mark_downloaded(path).map_err(|_| FileOpenError)?;
    #[cfg(target_os = "windows")]
    let (program, args) = ("explorer.exe", vec![path.as_os_str()]);
    #[cfg(target_os = "macos")]
    let (program, args) = ("open", vec![path.as_os_str()]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let (program, args) = ("xdg-open", vec![path.as_os_str()]);
    #[cfg(not(any(target_os = "windows", unix)))]
    return Err(FileOpenError);

    std::process::Command::new(program)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|_| FileOpenError)
}

/// Opens an `http(s)` link with the OS default browser. Anything else (other
/// schemes, whitespace or control characters, no host) is refused, and the URL
/// is one argument of a spawned program, never parsed by a shell.
pub fn open_url(url: &str) -> Result<(), FileOpenError> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or(FileOpenError)?;
    if url.len() > 2048
        || rest.starts_with(['/', '?', '#'])
        || rest.is_empty()
        || url.contains(|c: char| c.is_whitespace() || c.is_control())
    {
        return Err(FileOpenError);
    }
    #[cfg(target_os = "windows")]
    let program = "explorer.exe";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    #[cfg(not(any(target_os = "windows", unix)))]
    return Err(FileOpenError);

    std::process::Command::new(program)
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|_| FileOpenError)
}

/// The only extensions `open_local_file` hands to the OS: the image, audio and
/// video types the app names for content types, plus PDF and plain text. The
/// match is on the final extension only, ASCII case-insensitively, so `a.pdf.exe`
/// and `a.exe.` are refused and anything unknown (archives, disk images,
/// installers, scripts) is Download-only.
const PASSIVE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "mp3", "ogg", "wav", "m4a", "mp4", "webm", "pdf",
    "txt",
];

/// Whether the file name has a passive type that may be opened with the OS
/// default application.
pub fn is_passive_file(path: &std::path::Path) -> bool {
    let Some(extension) = path.extension().and_then(std::ffi::OsStr::to_str) else {
        return false;
    };
    PASSIVE_EXTENSIONS
        .iter()
        .any(|passive| extension.eq_ignore_ascii_case(passive))
}

/// The contents of a Windows `Zone.Identifier` stream for a file from the
/// Internet zone (zone 3), which is what makes SmartScreen and Office
/// Protected View treat the file as untrusted.
#[cfg(target_os = "windows")]
const ZONE_IDENTIFIER: &[u8] = b"[ZoneTransfer]\r\nZoneId=3\r\n";

/// The Mark-of-the-Web could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkError;

impl fmt::Display for MarkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("The downloaded file could not be marked as coming from the Internet.")
    }
}

impl std::error::Error for MarkError {}

/// Marks an existing file as downloaded from the Internet (Windows
/// `Zone.Identifier` alternate data stream with `ZoneId=3`). It fails when the
/// file does not exist or its volume has no alternate streams (FAT, exFAT,
/// network shares); callers decide whether that is fatal. A no-op elsewhere.
pub fn mark_downloaded(path: &std::path::Path) -> Result<(), MarkError> {
    #[cfg(target_os = "windows")]
    {
        use std::io::Write;
        // Creating the stream of a missing file would create the file itself.
        std::fs::metadata(path)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .ok_or(MarkError)?;
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(stream)
            .and_then(|mut file| file.write_all(ZONE_IDENTIFIER))
            .map_err(|_| MarkError)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Ok(())
    }
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
    fn only_passive_media_and_documents_are_opened_everything_else_is_refused() {
        for refused in [
            "setup.exe",
            "RUN.BAT",
            "x.Cmd",
            "script.ps1",
            "a.lnk",
            "installer.msi",
            "tool.sh",
            "page.hta",
            "evil.JS",
            // Not in the old denylist.
            "disk.iso",
            "disk.img",
            "disk.vhdx",
            "help.chm",
            "script.py",
            "pkg.msix",
            "Thing.workflow",
            "Thing.terminal",
            "archive.zip",
            "unknown.bin",
            "noextension",
            // Only the final extension counts.
            "a.pdf.exe",
            "photo.png.scr",
            "a.exe.",
            "a.exe ",
            ".png",
        ] {
            assert!(!is_passive_file(std::path::Path::new(refused)), "{refused}");
            // Refused before any process is started, so no handler is invoked.
            assert_eq!(
                open_local_file(std::path::Path::new(refused)),
                Err(FileOpenError)
            );
        }
        for passive in [
            "photo.png",
            "photo.JPG",
            "photo.Jpeg",
            "anim.gif",
            "pic.webp",
            "pic.bmp",
            "song.mp3",
            "song.ogg",
            "song.wav",
            "song.m4a",
            "clip.mp4",
            "clip.WEBM",
            "doc.pdf",
            "notes.txt",
            "a.exe.png",
            "a.zip.pdf",
        ] {
            assert!(is_passive_file(std::path::Path::new(passive)), "{passive}");
        }
    }

    fn scratch_file(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("fastcord-platform-{}-{name}", std::process::id()));
        std::fs::write(&path, b"data").unwrap();
        path
    }

    #[test]
    fn marking_a_missing_file_fails_without_creating_it() {
        let missing = std::env::temp_dir().join(format!(
            "fastcord-platform-{}-never-created.png",
            std::process::id()
        ));
        #[cfg(target_os = "windows")]
        {
            assert_eq!(mark_downloaded(&missing), Err(MarkError));
            assert!(!missing.exists());
            assert_eq!(open_local_file(&missing), Err(FileOpenError));
            assert!(!missing.exists());
        }
        #[cfg(not(target_os = "windows"))]
        assert_eq!(mark_downloaded(&missing), Ok(()));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn marking_writes_the_internet_zone_stream_and_overwrites_a_stale_one() {
        let path = scratch_file("marked.png");
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        std::fs::write(
            &stream,
            b"[ZoneTransfer]\r\nZoneId=0\r\nHostUrl=about:internet\r\n",
        )
        .unwrap();
        assert_eq!(mark_downloaded(&path), Ok(()));
        assert_eq!(std::fs::read(&stream).unwrap(), ZONE_IDENTIFIER);
        assert_eq!(std::fs::read(&path).unwrap(), b"data");
        std::fs::remove_file(&path).unwrap();
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn marking_is_a_no_op_outside_windows() {
        let path = scratch_file("unmarked.png");
        assert_eq!(mark_downloaded(&path), Ok(()));
        assert_eq!(std::fs::read(&path).unwrap(), b"data");
        std::fs::remove_file(&path).unwrap();
    }

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
