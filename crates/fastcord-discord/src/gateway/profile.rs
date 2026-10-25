//! The shared Gateway and REST identity (ADR 0005, revision 2026-10-25): the
//! official Discord desktop (Electron) client on the stable channel.
//!
//! One versioned module owns the identity sent to the Gateway and REST API. The
//! profile is built once at login and reused unchanged across requests and
//! reconnects: nothing here is randomized per connection.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

/// Bump whenever the properties below change shape or meaning.
pub const PROFILE_VERSION: u32 = 3;

/// Electron and Chromium of the official desktop client. Not in the update
/// manifest: read from the `Discord.exe` of the stable Windows x64 host
/// `PIN_HOST_VERSION` (its `package.json` holds the same values). The
/// scheduled `identity-pin` check fails once the manifest moves past that host,
/// and the pair is reviewed before the pin and the bundled values are bumped.
pub const ELECTRON_VERSION: &str = "42.11.15";
pub const CHROMIUM_VERSION: &str = "148.0.7778.280";
pub const PIN_HOST_VERSION: &str = "1.0.9265";

/// `client_build_number` of discord.com's web client observed on 2026-10-25.
/// Used only when the live value cannot be fetched.
pub const BUNDLED_BUILD_NUMBER: u32 = 649_389;
/// The update manifest's `metadata_version` observed on 2026-10-25 (Windows
/// and Linux; macOS answered 96312). Used only when the manifest cannot be
/// fetched.
pub const BUNDLED_NATIVE_BUILD_NUMBER: u32 = 96_160;

const APP_URL: &str = "https://discord.com/app";
const MANIFEST_URL: &str = "https://updates.discord.com/distributions/app/manifests/latest";
const BUILD_MARKER: &[u8] = b"\"BUILD_NUMBER\":\"";
/// The marker sits in a script near the top of the page; never read more.
const MAX_PAGE_BYTES: usize = 1024 * 1024;
/// A manifest is about 10 KiB; anything near this is not one.
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Plausibility window for a web build number; anything else is page drift.
const BUILD_RANGE: std::ops::RangeInclusive<u32> = 100_000..=9_999_999;
/// Plausibility window for the native metadata version.
const NATIVE_RANGE: std::ops::RangeInclusive<u32> = 1_000..=9_999_999;

/// The launch-signature bits Discord reads as detected client mods (JQUERY,
/// BETTERDISCORD, RAMBOX, VENDETTA, VENCORD, REPLUGGED, LEGCORD, DORION,
/// GOOFCORD, OPENASAR, SHELTER, MOONLIGHT); a clean client sends them clear.
const CLIENT_MOD_BITS: u128 = 1 << 119
    | 1 << 108
    | 1 << 100
    | 1 << 91
    | 1 << 84
    | 1 << 75
    | 1 << 61
    | 1 << 55
    | 1 << 48
    | 1 << 38
    | 1 << 24
    | 1 << 11;

/// The three version values that change with Discord's releases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Versions {
    /// The desktop host version, `full.host_version` of the update manifest.
    pub client_version: String,
    /// The manifest's `metadata_version`.
    pub native_build_number: u32,
    /// The web build the desktop client loads, from `https://discord.com/app`.
    pub client_build_number: u32,
}

impl Versions {
    /// The values bundled at release time, per platform and architecture.
    pub fn bundled(os: HostOs) -> Self {
        let client_version = match (os, APP_ARCH) {
            (HostOs::Windows, "arm64") => "1.0.57",
            (HostOs::Windows, _) => PIN_HOST_VERSION,
            (HostOs::MacOs, _) => "0.0.419",
            (HostOs::Linux, _) => "1.0.166",
        };
        Self {
            client_version: client_version.to_owned(),
            native_build_number: BUNDLED_NATIVE_BUILD_NUMBER,
            client_build_number: BUNDLED_BUILD_NUMBER,
        }
    }
}

/// Resolves the version chain: the update manifest for the host version and
/// native build, discord.com's web client for the web build, each falling back
/// to its bundled value on any failure (offline, page drift, redirect). The
/// host version and native build come from the same manifest or are both
/// bundled. The requests are unauthenticated and carry no account data. They
/// run at most once per process: the answer, fetched or fallback, is kept for
/// every later validate, restore, account switch and login step.
pub async fn resolve_versions() -> Versions {
    static RESOLVED: tokio::sync::OnceCell<Versions> = tokio::sync::OnceCell::const_new();
    let os = HostOs::current();
    #[cfg(feature = "fake-discord")]
    if crate::fake_origin::active() {
        return Versions::bundled(os);
    }
    resolve_once(&RESOLVED, async move {
        let (manifest, build) = tokio::join!(fetch_manifest(os, APP_ARCH), fetch_build_number());
        let mut versions = Versions::bundled(os);
        if let Some(release) = manifest {
            versions.client_version = release.host_version;
            versions.native_build_number = release.metadata_version;
        }
        if let Some(build) = build {
            versions.client_build_number = build;
        }
        versions
    })
    .await
}

/// The cached value, or `fetch` run once (concurrent callers share that run).
async fn resolve_once<T: Clone>(
    cell: &tokio::sync::OnceCell<T>,
    fetch: impl Future<Output = T>,
) -> T {
    cell.get_or_init(|| fetch).await.clone()
}

fn fetch_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()
}

async fn fetch_build_number() -> Option<u32> {
    let mut response = fetch_client()?
        .get(APP_URL)
        .header(
            reqwest::header::USER_AGENT,
            ClientProperties::bundled(HostOs::current(), "en-US").browser_user_agent,
        )
        .header(reqwest::header::ACCEPT, "text/html")
        .send()
        .await
        .ok()?;
    if response.status() != reqwest::StatusCode::OK {
        return None;
    }
    let mut page = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        page.extend_from_slice(&chunk);
        if let Some(value) = parse_build_number(&page) {
            return Some(value);
        }
        if page.len() > MAX_PAGE_BYTES {
            return None;
        }
    }
    None
}

/// Extracts `"BUILD_NUMBER":"<digits>"` from the web client's
/// `window.GLOBAL_ENV` script. Requires the closing quote, so a chunk boundary
/// in the middle of the number yields `None` rather than a truncated value.
pub(crate) fn parse_build_number(page: &[u8]) -> Option<u32> {
    let start = page
        .windows(BUILD_MARKER.len())
        .position(|window| window == BUILD_MARKER)?
        + BUILD_MARKER.len();
    let digits = &page[start..];
    let end = digits.iter().position(|b| !b.is_ascii_digit())?;
    if end == 0 || digits[end] != b'"' {
        return None;
    }
    let value: u32 = std::str::from_utf8(&digits[..end]).ok()?.parse().ok()?;
    BUILD_RANGE.contains(&value).then_some(value)
}

/// What the update manifest says about the current stable host.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HostRelease {
    host_version: String,
    metadata_version: u32,
}

/// Asks the updater endpoint the desktop client polls; never downloads the
/// package it points to.
async fn fetch_manifest(os: HostOs, arch: &str) -> Option<HostRelease> {
    // ponytail: a fresh install_id per process; persist it with the
    // installation id once that exists.
    let install_id = Uuid::new_v4().to_string();
    let mut response = fetch_client()?
        .get(MANIFEST_URL)
        .query(&[
            ("channel", "stable"),
            ("platform", os.update_platform()),
            ("arch", arch),
            ("install_id", &install_id),
        ])
        .send()
        .await
        .ok()?;
    if response.status() != reqwest::StatusCode::OK {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_MANIFEST_BYTES {
            return None;
        }
    }
    parse_manifest(&body)
}

fn parse_manifest(body: &[u8]) -> Option<HostRelease> {
    #[derive(Deserialize)]
    struct Manifest {
        full: Full,
        metadata_version: u32,
    }
    #[derive(Deserialize)]
    struct Full {
        host_version: [u16; 3],
    }
    let manifest: Manifest = serde_json::from_slice(body).ok()?;
    let [major, minor, patch] = manifest.full.host_version;
    NATIVE_RANGE
        .contains(&manifest.metadata_version)
        .then(|| HostRelease {
            host_version: format!("{major}.{minor}.{patch}"),
            metadata_version: manifest.metadata_version,
        })
}

/// The build's architecture as Electron names it (`app_arch`; `os_arch` too,
/// since fastcord ships native builds per architecture).
const APP_ARCH: &str = if cfg!(target_arch = "aarch64") {
    "arm64"
} else if cfg!(target_arch = "x86") {
    "ia32"
} else {
    "x64"
};

/// Operating systems the desktop profile can describe truthfully.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOs {
    Windows,
    MacOs,
    Linux,
}

impl HostOs {
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }

    /// `(os, UA platform token)` as Electron reports them: Chromium's reduced
    /// UA is frozen (Windows NT 10.0 covers Windows 11 and ARM; macOS stays
    /// 10_15_7).
    const fn desktop_identity(self) -> (&'static str, &'static str) {
        match self {
            Self::Windows => ("Windows", "Windows NT 10.0; Win64; x64"),
            Self::MacOs => ("Mac OS X", "Macintosh; Intel Mac OS X 10_15_7"),
            Self::Linux => ("Linux", "X11; Linux x86_64"),
        }
    }

    /// The updater's `platform` value.
    const fn update_platform(self) -> &'static str {
        match self {
            Self::Windows => "win",
            Self::MacOs => "osx",
            Self::Linux => "linux",
        }
    }
}

/// The host facts the profile reports; the app reads them from the OS
/// (`fastcord_platform::os_release`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub os: HostOs,
    /// Node's `os.release()`: `10.0.26100`, the Darwin release, the kernel.
    pub os_version: String,
    /// Windows only: the build number.
    pub os_sdk_version: Option<String>,
    /// Linux only: `XDG_CURRENT_DESKTOP,GDMSESSION`.
    pub window_manager: Option<String>,
    /// Linux only: `PRETTY_NAME` of os-release.
    pub distro: Option<String>,
}

impl Host {
    /// Only the OS family known: offline tests and pre-login fetches.
    pub fn bare(os: HostOs) -> Self {
        Self {
            os,
            os_version: String::new(),
            os_sdk_version: None,
            window_manager: None,
            distro: None,
        }
    }
}

/// Identify `properties`, field names, order and value shapes as the desktop
/// client sends them. Serialization order is declaration order. `device`,
/// `referrer*` and `client_app_state` are absent from the published desktop
/// examples and stay out until a sanitized official capture shows them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClientProperties {
    pub os: &'static str,
    pub browser: &'static str,
    pub release_channel: &'static str,
    pub client_version: String,
    pub os_version: String,
    pub os_arch: &'static str,
    pub app_arch: &'static str,
    pub system_locale: String,
    pub has_client_mods: bool,
    pub client_launch_id: String,
    pub launch_signature: String,
    pub browser_user_agent: String,
    pub browser_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_sdk_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_manager: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    pub client_build_number: u32,
    pub native_build_number: u32,
    pub client_event_source: Option<&'static str>,
    pub client_heartbeat_session_id: String,
}

impl ClientProperties {
    /// The profile itself: everything Identify sends that is fixed for the run.
    /// Pure, so equal inputs give an equal profile.
    pub fn desktop(host: &Host, locale: &str, versions: &Versions) -> Self {
        let (os, platform) = host.os.desktop_identity();
        let client_version = versions.client_version.clone();
        Self {
            os,
            browser: "Discord Client",
            release_channel: "stable",
            browser_user_agent: format!(
                "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) \
                 discord/{client_version} Chrome/{CHROMIUM_VERSION} \
                 Electron/{ELECTRON_VERSION} Safari/537.36"
            ),
            client_version,
            os_version: host.os_version.clone(),
            os_arch: APP_ARCH,
            app_arch: APP_ARCH,
            system_locale: locale.to_owned(),
            has_client_mods: false,
            client_launch_id: String::new(),
            launch_signature: String::new(),
            browser_version: ELECTRON_VERSION,
            os_sdk_version: host.os_sdk_version.clone(),
            window_manager: host.window_manager.clone(),
            distro: host.distro.clone(),
            client_build_number: versions.client_build_number,
            native_build_number: versions.native_build_number,
            client_event_source: None,
            client_heartbeat_session_id: String::new(),
        }
    }

    /// The desktop profile with bundled versions and only the OS family known:
    /// offline tests and diagnostics.
    pub fn bundled(os: HostOs, locale: &str) -> Self {
        Self::desktop(&Host::bare(os), locale, &Versions::bundled(os))
    }

    /// Stamps the one-off launch identity the desktop client generates when the
    /// app starts: a launch id, its signature with every client-mod bit clear,
    /// and the analytics heartbeat session. Generated once per launch, never
    /// per Identify and never per reconnect.
    // ponytail: the heartbeat session is not yet rotated every 30 minutes;
    // that needs the swappable REST header (IDENTITY_AUDIT A2).
    pub fn with_launch_identity(mut self) -> Self {
        self.client_launch_id = Uuid::new_v4().to_string();
        self.launch_signature = launch_signature().to_string();
        self.client_heartbeat_session_id = Uuid::new_v4().to_string();
        self
    }
}

/// A UUIDv4 with the client-mod bits cleared, as the official generator does.
/// None of them overlap the version or variant bits, so it is still a v4 UUID.
fn launch_signature() -> Uuid {
    Uuid::from_u128(Uuid::new_v4().as_u128() & !CLIENT_MOD_BITS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = include_str!("../../../../fixtures/gateway/app-page-excerpt.html");
    const MANIFEST: &str =
        include_str!("../../../../fixtures/gateway/update-manifest-excerpt.json");

    #[tokio::test]
    async fn the_versions_are_fetched_once_whether_they_succeed_or_fall_back() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let fetches = AtomicU32::new(0);
        let counted = |answer| {
            let fetches = &fetches;
            async move {
                fetches.fetch_add(1, Ordering::Relaxed);
                answer
            }
        };
        let cell = tokio::sync::OnceCell::new();
        assert_eq!(resolve_once(&cell, counted(123_456)).await, 123_456);
        assert_eq!(resolve_once(&cell, counted(999_999)).await, 123_456);
        assert_eq!(fetches.load(Ordering::Relaxed), 1, "one fetch per cell");
    }

    #[test]
    fn build_number_is_read_from_the_global_env_script() {
        assert_eq!(parse_build_number(PAGE.as_bytes()), Some(631_730));
    }

    #[test]
    fn truncated_missing_and_implausible_values_are_rejected() {
        let marker = std::str::from_utf8(BUILD_MARKER).unwrap();
        for page in [
            String::new(),
            "no marker here".to_owned(),
            // A chunk boundary inside the number must not yield a prefix.
            format!("{marker}6317"),
            format!("{marker}\""),
            format!("{marker}abc\""),
            format!("{marker}99999\""),
            format!("{marker}99999999999999999999\""),
            format!("{marker}12345678\""),
            format!("{marker}631730,"),
        ] {
            assert_eq!(parse_build_number(page.as_bytes()), None, "{page}");
        }
        assert_eq!(
            parse_build_number(format!("{marker}700000\"").as_bytes()),
            Some(700_000)
        );
    }

    #[test]
    fn host_version_and_native_build_are_read_from_the_update_manifest() {
        assert_eq!(
            parse_manifest(MANIFEST.as_bytes()),
            Some(HostRelease {
                host_version: "1.0.9265".to_owned(),
                metadata_version: 96_160,
            })
        );
        for body in [
            "",
            "{}",
            r#"{"full":{"host_version":[1,0]},"metadata_version":96160}"#,
            r#"{"full":{"host_version":[1,0,"x"]},"metadata_version":96160}"#,
            r#"{"full":{"host_version":[1,0,9265]}}"#,
            r#"{"full":{"host_version":[1,0,9265]},"metadata_version":0}"#,
        ] {
            assert_eq!(parse_manifest(body.as_bytes()), None, "{body}");
        }
    }

    #[test]
    fn desktop_profile_is_truthful_consistent_and_stable() {
        let host = Host {
            os: HostOs::Windows,
            os_version: "10.0.26100".to_owned(),
            os_sdk_version: Some("26100".to_owned()),
            window_manager: None,
            distro: None,
        };
        let versions = Versions {
            client_version: "1.0.9999".to_owned(),
            native_build_number: 100_001,
            client_build_number: 700_001,
        };
        let windows = ClientProperties::desktop(&host, "ru-RU", &versions);
        assert_eq!(windows.os, "Windows");
        assert_eq!(windows.browser, "Discord Client");
        assert_eq!(windows.release_channel, "stable");
        assert_eq!(windows.system_locale, "ru-RU");
        assert_eq!(windows.os_version, "10.0.26100");
        assert_eq!(windows.os_sdk_version.as_deref(), Some("26100"));
        assert_eq!(windows.os_arch, windows.app_arch);
        assert!(!windows.has_client_mods);
        // Every version value in one place, and the UA agrees with all of them.
        assert_eq!(windows.client_version, "1.0.9999");
        assert_eq!(windows.native_build_number, 100_001);
        assert_eq!(windows.client_build_number, 700_001);
        assert_eq!(windows.browser_version, ELECTRON_VERSION);
        assert_eq!(
            windows.browser_user_agent,
            format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 discord/1.0.9999 Chrome/{CHROMIUM_VERSION} Electron/{ELECTRON_VERSION} Safari/537.36"
            )
        );
        let mac = ClientProperties::bundled(HostOs::MacOs, "en-US");
        assert_eq!(mac.os, "Mac OS X");
        assert!(mac.browser_user_agent.contains("Macintosh"));
        assert!(mac.browser_user_agent.contains("discord/0.0.419 "));
        let linux = Host {
            window_manager: Some("GNOME,ubuntu".to_owned()),
            distro: Some("Ubuntu 24.04.4 LTS".to_owned()),
            ..Host::bare(HostOs::Linux)
        };
        let linux = ClientProperties::desktop(&linux, "en-US", &Versions::bundled(HostOs::Linux));
        assert_eq!(linux.os, "Linux");
        assert!(linux.browser_user_agent.contains("X11; Linux x86_64"));
        // Optional fields appear only where the host has them, in the
        // published desktop order.
        let windows_json = serde_json::to_string(&windows).unwrap();
        let linux_json = serde_json::to_string(&linux).unwrap();
        let at = |json: &str, name: &str| json.find(&format!("\"{name}\":"));
        assert!(at(&windows_json, "os_sdk_version").is_some());
        assert!(at(&windows_json, "distro").is_none());
        assert!(at(&linux_json, "os_sdk_version").is_none());
        assert!(at(&linux_json, "browser_version") < at(&linux_json, "window_manager"));
        assert!(at(&linux_json, "distro") < at(&linux_json, "client_build_number"));
        assert!(at(&windows_json, "release_channel") < at(&windows_json, "client_version"));
        for absent in ["device", "referrer", "client_app_state"] {
            assert!(at(&windows_json, absent).is_none(), "{absent}");
        }
        // Never randomized: identical inputs give an identical profile.
        assert_eq!(
            ClientProperties::desktop(&host, "ru-RU", &versions),
            windows
        );
        // The launch identity is the one exception: fresh per launch, then
        // fixed for every Identify that follows.
        assert_eq!(windows.client_launch_id, "");
        let launched = windows.clone().with_launch_identity();
        assert_eq!(launched.client_launch_id.len(), 36);
        assert_eq!(launched.launch_signature.len(), 36);
        assert_eq!(launched.client_heartbeat_session_id.len(), 36);
        assert_ne!(launched.launch_signature, launched.client_launch_id);
        let other = windows.clone().with_launch_identity();
        assert_ne!(other.client_launch_id, launched.client_launch_id);
        assert_eq!(other.browser_user_agent, launched.browser_user_agent);
        assert_eq!(
            HostOs::current(),
            if cfg!(windows) {
                HostOs::Windows
            } else if cfg!(target_os = "macos") {
                HostOs::MacOs
            } else {
                HostOs::Linux
            }
        );
    }

    #[test]
    fn launch_signatures_never_claim_a_client_mod() {
        assert_eq!(CLIENT_MOD_BITS.count_ones(), 12);
        for _ in 0..256 {
            let signature = launch_signature();
            assert_eq!(signature.as_u128() & CLIENT_MOD_BITS, 0);
            assert_eq!(signature.get_version_num(), 4);
            assert_eq!(signature.get_variant(), uuid::Variant::RFC4122);
        }
    }

    #[test]
    fn bundled_values_are_plausible_and_match_the_pin() {
        assert!(BUILD_RANGE.contains(&BUNDLED_BUILD_NUMBER));
        assert!(NATIVE_RANGE.contains(&BUNDLED_NATIVE_BUILD_NUMBER));
        // A release cannot bump the bundled host without reviewing the
        // Electron/Chromium pair that belongs to it.
        if APP_ARCH == "x64" {
            assert_eq!(
                Versions::bundled(HostOs::Windows).client_version,
                PIN_HOST_VERSION
            );
        }
        let chromium_major = CHROMIUM_VERSION.split('.').next().unwrap();
        assert!(chromium_major.parse::<u32>().unwrap() >= 100);
        assert_eq!(ELECTRON_VERSION.split('.').count(), 3);
    }

    /// The pin check (`.github/workflows/identity-pin.yml`; manual:
    /// `cargo test -p fastcord-discord live_version_chain --locked -- --ignored --nocapture`).
    /// Fetches the stable Windows x64 manifest and the web client, builds the
    /// profile from them, and fails when the host moved past the pinned
    /// Electron/Chromium pair. Never downloads Discord.
    #[tokio::test]
    #[ignore = "contacts updates.discord.com and discord.com"]
    async fn live_version_chain_matches_the_pin() {
        let release = fetch_manifest(HostOs::Windows, "x64")
            .await
            .expect("the update manifest answered");
        let build = fetch_build_number()
            .await
            .expect("discord.com/app carried a build number");
        println!(
            "host {} native {} web build {build}",
            release.host_version, release.metadata_version
        );
        let versions = Versions {
            client_version: release.host_version.clone(),
            native_build_number: release.metadata_version,
            client_build_number: build,
        };
        let profile = ClientProperties::desktop(&Host::bare(HostOs::Windows), "en-US", &versions);
        let ua = &profile.browser_user_agent;
        assert!(ua.contains(&format!("discord/{} ", profile.client_version)));
        assert!(ua.contains(&format!("Electron/{} ", profile.browser_version)));
        assert_eq!(profile.native_build_number, release.metadata_version);
        assert_eq!(profile.client_build_number, build);
        assert_eq!(
            release.host_version, PIN_HOST_VERSION,
            "the stable Windows host moved: read Electron/Chromium from the new host \
             (Discord.exe strings or package.json), update ELECTRON_VERSION and \
             CHROMIUM_VERSION if they changed, then bump PIN_HOST_VERSION and the \
             bundled values"
        );
    }
}
