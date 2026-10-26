//! Native credential storage, and screen capture (`capture`). All methods may
//! block and belong on a worker, never the UI thread. No default/global keyring
//! store or plaintext fallback.

pub mod autostart;
pub mod capture;
pub mod clipboard;
pub mod host;
pub mod instance;
pub mod loopback;
pub mod network;
pub mod notify;
pub mod realtime;
pub mod shortcut;
pub mod speech;
pub mod tray;

pub use host::{OsRelease, os_release};

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use fastcord_model::Snowflake;
use keyring_core::{CredentialStore, Entry};
use zeroize::{Zeroize, Zeroizing};

const SERVICE: &str = "fastcord";
const PROXY_SERVICE: &str = "fastcord-proxy";
/// Apply a portable one-byte length cap. An empty stored password is distinct
/// from a missing credential and permits HTTP Basic's empty-password case.
pub const MAX_PROXY_PASSWORD_BYTES: usize = 255;

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
    launch(path.as_os_str())
}

/// Opens an `http(s)` link with the OS default browser. Anything else (other
/// schemes, whitespace or control characters, no host) is refused, and the URL
/// is handed over whole, never parsed by a shell. May block: call it off the UI
/// thread.
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
    launch(std::ffi::OsStr::new(url))
}

/// Hands a checked file or URL to the OS default handler (#140). Windows uses
/// `ShellExecuteExW`, so nothing parses a command line (explorer.exe split
/// arguments on commas) and a missing handler is an error.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn launch(target: &std::ffi::OsStr) -> Result<(), FileOpenError> {
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
    };
    use windows::Win32::UI::Shell::{
        SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};

    // Some handlers are COM objects; an already-initialized thread reports
    // a harmless error.
    // SAFETY: plain apartment initialization of the calling thread.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let file = HSTRING::from(target);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("open"),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` is initialized with its size and lives across the call;
    // `file` outlives it.
    unsafe { ShellExecuteExW(&mut info) }.map_err(|_| FileOpenError)
}

/// Unix: `open`/`xdg-open`, reaped on its own thread so no zombie is left;
/// some `xdg-open` fallbacks stay until the viewer exits, so never inline.
#[cfg(unix)]
fn launch(target: &std::ffi::OsStr) -> Result<(), FileOpenError> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    let mut child = std::process::Command::new(program)
        .arg(target)
        .spawn()
        .map_err(|_| FileOpenError)?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(not(any(target_os = "windows", unix)))]
fn launch(_: &std::ffi::OsStr) -> Result<(), FileOpenError> {
    Err(FileOpenError)
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

/// Marks an existing file as downloaded from the Internet: on Windows the
/// `Zone.Identifier` alternate data stream with `ZoneId=3` (it fails when the
/// volume has no alternate streams: FAT, exFAT, network shares), on macOS the
/// `com.apple.quarantine` attribute Gatekeeper checks before running it
/// (#137). It fails when the file does not exist; callers decide whether that
/// is fatal. A no-op elsewhere.
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
    #[cfg(target_os = "macos")]
    {
        // `0083`: a web download Gatekeeper must check, as browsers set it.
        // The system tool, so no FFI: called once per download, on a worker.
        std::fs::metadata(path)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .ok_or(MarkError)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let status = std::process::Command::new("/usr/bin/xattr")
            .arg("-w")
            .arg("com.apple.quarantine")
            .arg(format!("0083;{now:08x};fastcord;"))
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|_| MarkError)?;
        status.success().then_some(()).ok_or(MarkError)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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

    /// Loads the proxy password bound to the selected configuration's 64-digit
    /// lowercase SHA-256 key, in a native namespace separate from every
    /// Discord account. Endpoint-bound entries prevent a failed settings save
    /// from offering a new proxy's password to the previous proxy on restart.
    /// Worker-thread only;
    /// fake mode must not call this or any other native proxy-store method.
    pub fn load_proxy_password(&self, key: &str) -> Result<Option<StoredCredential>, StoreError> {
        let _guard = self.access.lock().map_err(|_| StoreError::Unavailable)?;
        match self.proxy_entry(key)?.get_password() {
            Ok(secret) => {
                let secret = Zeroizing::new(secret);
                if !valid_proxy_password(&secret) {
                    return Err(StoreError::InvalidCredential);
                }
                Ok(Some(StoredCredential(secret)))
            }
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }

    /// Saves only to the selected native backend; a refused write never falls
    /// back to network-settings.json or another plaintext location. The caller
    /// owns and zeroizes the input; neither the password nor backend details
    /// can be formatted through this API's return values.
    pub fn save_proxy_password(&self, key: &str, password: &str) -> Result<(), StoreError> {
        if !valid_proxy_password(password) {
            return Err(StoreError::InvalidCredential);
        }
        let _guard = self.access.lock().map_err(|_| StoreError::Unavailable)?;
        self.proxy_entry(key)?
            .set_password(password)
            .map_err(store_error)
    }

    /// Idempotent and unrelated to account logout. Removing the proxy password
    /// never removes a Discord token or changes account discovery metadata.
    pub fn delete_proxy_password(&self, key: &str) -> Result<(), StoreError> {
        let _guard = self.access.lock().map_err(|_| StoreError::Unavailable)?;
        match self.proxy_entry(key)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(store_error(error)),
        }
    }

    fn proxy_entry(&self, key: &str) -> Result<Entry, StoreError> {
        if key.len() != 64
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(StoreError::InvalidCredential);
        }
        self.inner
            .build(PROXY_SERVICE, key, None)
            .map_err(store_error)
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

fn valid_proxy_password(password: &str) -> bool {
    password.len() <= MAX_PROXY_PASSWORD_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PROXY_KEY: &str = concat!(
        "0123456789abcdef",
        "0123456789abcdef",
        "0123456789abcdef",
        "0123456789abcdef"
    );
    const OTHER_PROXY_KEY: &str = concat!(
        "abcdef0123456789",
        "abcdef0123456789",
        "abcdef0123456789",
        "abcdef0123456789"
    );

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
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            assert_eq!(mark_downloaded(&missing), Err(MarkError));
            assert!(!missing.exists());
            assert_eq!(open_local_file(&missing), Err(FileOpenError));
            assert!(!missing.exists());
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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

    #[cfg(target_os = "macos")]
    #[test]
    fn marking_sets_the_quarantine_attribute_gatekeeper_checks() {
        let path = scratch_file("quarantined.command");
        assert_eq!(mark_downloaded(&path), Ok(()));
        let read = std::process::Command::new("/usr/bin/xattr")
            .args(["-p", "com.apple.quarantine"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(read.status.success());
        let value = String::from_utf8(read.stdout).unwrap();
        assert!(value.starts_with("0083;") && value.trim_end().ends_with(";fastcord;"));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(mark_downloaded(&path), Err(MarkError), "a missing file");
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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

    #[test]
    fn proxy_password_uses_its_own_namespace_without_appearing_as_an_account() {
        let store = mock_store();
        let account = Snowflake(123);
        store.save(account, "dummy-account-token").unwrap();
        assert!(store.load_proxy_password(TEST_PROXY_KEY).unwrap().is_none());
        store
            .save_proxy_password(TEST_PROXY_KEY, "dummy-proxy-password")
            .unwrap();
        let proxy_entry = store.proxy_entry(TEST_PROXY_KEY).unwrap();
        assert_eq!(
            proxy_entry.get_specifiers(),
            Some((PROXY_SERVICE.to_owned(), TEST_PROXY_KEY.to_owned()))
        );
        assert_eq!(store.accounts().unwrap(), [account]);
        let loaded = store
            .clone()
            .load_proxy_password(TEST_PROXY_KEY)
            .unwrap()
            .unwrap();
        assert_eq!(format!("{loaded:?}"), "StoredCredential([REDACTED])");
        assert!(loaded.into_secret().as_str() == "dummy-proxy-password");
        assert!(
            store.load(account).unwrap().unwrap().into_secret().as_str() == "dummy-account-token"
        );
        store
            .save_proxy_password(TEST_PROXY_KEY, "replacement-proxy-password")
            .unwrap();
        assert!(
            store
                .load_proxy_password(TEST_PROXY_KEY)
                .unwrap()
                .unwrap()
                .into_secret()
                .as_str()
                == "replacement-proxy-password"
        );
        store.delete_proxy_password(TEST_PROXY_KEY).unwrap();
        store.delete_proxy_password(TEST_PROXY_KEY).unwrap();
        assert!(store.load_proxy_password(TEST_PROXY_KEY).unwrap().is_none());
        assert_eq!(store.accounts().unwrap(), [account]);
        assert!(store.load(account).unwrap().is_some());
    }

    #[test]
    fn account_logout_and_other_native_services_do_not_change_the_proxy_password() {
        let store = mock_store();
        store
            .inner
            .build("fastcord-other-proxy", TEST_PROXY_KEY, None)
            .unwrap()
            .set_password("other-service-password")
            .unwrap();
        store
            .inner
            .build(PROXY_SERVICE, "other-entry", None)
            .unwrap()
            .set_password("other-entry-password")
            .unwrap();
        store.save(Snowflake(1), "dummy-account-token").unwrap();
        store
            .save_proxy_password(TEST_PROXY_KEY, "dummy-proxy-password")
            .unwrap();
        store.delete(Snowflake(1)).unwrap();
        assert!(store.load(Snowflake(1)).unwrap().is_none());
        assert!(
            store
                .load_proxy_password(TEST_PROXY_KEY)
                .unwrap()
                .unwrap()
                .into_secret()
                .as_str()
                == "dummy-proxy-password"
        );
        store.delete_proxy_password(TEST_PROXY_KEY).unwrap();
        assert!(
            store
                .inner
                .build("fastcord-other-proxy", TEST_PROXY_KEY, None)
                .unwrap()
                .get_password()
                .unwrap()
                == "other-service-password"
        );
        assert!(
            store
                .inner
                .build(PROXY_SERVICE, "other-entry", None)
                .unwrap()
                .get_password()
                .unwrap()
                == "other-entry-password"
        );
    }

    #[test]
    fn proxy_password_byte_limit_is_checked_on_save_and_native_load() {
        let store = mock_store();
        store.save_proxy_password(TEST_PROXY_KEY, "").unwrap();
        assert!(
            store
                .load_proxy_password(TEST_PROXY_KEY)
                .unwrap()
                .unwrap()
                .into_secret()
                .is_empty()
        );
        assert_eq!(
            store.save_proxy_password(TEST_PROXY_KEY, &"x".repeat(256)),
            Err(StoreError::InvalidCredential)
        );
        // Limits are bytes, rather than Unicode scalar/character counts.
        assert_eq!(
            store.save_proxy_password(TEST_PROXY_KEY, &"é".repeat(128)),
            Err(StoreError::InvalidCredential)
        );
        let largest = format!("{}x", "é".repeat(127));
        assert_eq!(largest.len(), 255);
        store.save_proxy_password(TEST_PROXY_KEY, &largest).unwrap();
        assert!(
            store
                .load_proxy_password(TEST_PROXY_KEY)
                .unwrap()
                .unwrap()
                .into_secret()
                .as_str()
                == largest
        );
        for invalid in ["x".repeat(256), "é".repeat(128)] {
            store
                .proxy_entry(TEST_PROXY_KEY)
                .unwrap()
                .set_password(&invalid)
                .unwrap();
            assert!(matches!(
                store.load_proxy_password(TEST_PROXY_KEY),
                Err(StoreError::InvalidCredential)
            ));
        }
    }

    #[test]
    fn proxy_password_slots_are_endpoint_bound_and_reject_unbound_keys() {
        let store = mock_store();
        store
            .save_proxy_password(TEST_PROXY_KEY, "first-proxy-password")
            .unwrap();
        store
            .save_proxy_password(OTHER_PROXY_KEY, "second-proxy-password")
            .unwrap();
        assert!(
            store
                .load_proxy_password(TEST_PROXY_KEY)
                .unwrap()
                .unwrap()
                .into_secret()
                .as_str()
                == "first-proxy-password"
        );
        assert!(
            store
                .load_proxy_password(OTHER_PROXY_KEY)
                .unwrap()
                .unwrap()
                .into_secret()
                .as_str()
                == "second-proxy-password"
        );
        store.delete_proxy_password(TEST_PROXY_KEY).unwrap();
        assert!(store.load_proxy_password(TEST_PROXY_KEY).unwrap().is_none());
        assert!(
            store
                .load_proxy_password(OTHER_PROXY_KEY)
                .unwrap()
                .is_some()
        );
        assert!(store.accounts().unwrap().is_empty());
        for invalid in [
            "",
            "default",
            "123",
            "proxy.example:1080",
            &"A".repeat(64),
            &"a".repeat(63),
            &"a".repeat(65),
        ] {
            assert_eq!(
                store.save_proxy_password(invalid, "canary-proxy-password"),
                Err(StoreError::InvalidCredential)
            );
            assert!(matches!(
                store.load_proxy_password(invalid),
                Err(StoreError::InvalidCredential)
            ));
            assert_eq!(
                store.delete_proxy_password(invalid),
                Err(StoreError::InvalidCredential)
            );
        }
    }

    #[test]
    fn locked_proxy_store_refuses_save_load_and_delete_without_fallback() {
        let store = mock_store();
        let entry = store.proxy_entry(TEST_PROXY_KEY).unwrap();
        let mock = entry
            .as_any()
            .downcast_ref::<keyring_core::mock::Cred>()
            .unwrap();
        let locked = || {
            keyring_core::Error::NoStorageAccess(Box::new(std::io::Error::other(
                "canary-proxy-secret-context",
            )))
        };
        mock.set_error(locked());
        assert_eq!(
            store.save_proxy_password(TEST_PROXY_KEY, "dummy-proxy-password"),
            Err(StoreError::Locked)
        );
        assert!(store.load_proxy_password(TEST_PROXY_KEY).unwrap().is_none());
        store
            .save_proxy_password(TEST_PROXY_KEY, "dummy-proxy-password")
            .unwrap();
        mock.set_error(locked());
        let error = store.load_proxy_password(TEST_PROXY_KEY).unwrap_err();
        assert_eq!(error, StoreError::Locked);
        assert!(!format!("{error:?} {error}").contains("canary"));
        mock.set_error(locked());
        assert_eq!(
            store.delete_proxy_password(TEST_PROXY_KEY),
            Err(StoreError::Locked)
        );
        assert!(store.load_proxy_password(TEST_PROXY_KEY).unwrap().is_some());
        mock.set_error(keyring_core::Error::BadEncoding(
            b"canary-invalid-proxy-password".to_vec(),
        ));
        let error = store.load_proxy_password(TEST_PROXY_KEY).unwrap_err();
        assert_eq!(error, StoreError::InvalidCredential);
        assert!(!format!("{error:?} {error}").contains("canary"));
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
