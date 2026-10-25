//! The shared Gateway and REST identity (ADR 0005, revision 2026-10-25): the
//! official Discord desktop (Electron) client on the stable channel.
//!
//! One versioned module owns the identity sent to the Gateway and REST API. The
//! profile is built once at login and reused unchanged across requests and
//! reconnects: nothing here is randomized per connection.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use uuid::Uuid;

/// Bump whenever the properties below change shape or meaning.
pub const PROFILE_VERSION: u32 = 4;

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
/// Windows/Linux metadata fallback observed on 2026-10-25. macOS has its
/// own fallback; the wire field is nullable for hosts without native metadata.
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

/// The analytics heartbeat session is regenerated this often.
const HEARTBEAT_ROTATION: Duration = Duration::from_secs(30 * 60);
/// An installation id is a fingerprint (`<snowflake>.<hash>`); anything longer
/// or with other characters is not kept or sent.
const MAX_INSTALLATION_BYTES: usize = 128;

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
    pub native_build_number: Option<u32>,
    /// The web build the desktop client loads, from `https://discord.com/app`.
    pub client_build_number: u32,
}

impl Versions {
    /// The values bundled at release time, per platform and architecture.
    pub fn bundled(os: HostOs) -> Self {
        Self::bundled_for(os, APP_ARCH)
    }

    pub(super) fn bundled_for(os: HostOs, arch: &str) -> Self {
        let client_version = match (os, arch) {
            (HostOs::Windows, "arm64") => "1.0.57",
            (HostOs::Windows, _) => PIN_HOST_VERSION,
            (HostOs::MacOs, _) => "0.0.419",
            (HostOs::Linux, _) => "1.0.166",
        };
        Self {
            client_version: client_version.to_owned(),
            native_build_number: Some(match os {
                HostOs::MacOs => 96_312,
                _ => BUNDLED_NATIVE_BUILD_NUMBER,
            }),
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
            versions.native_build_number = Some(release.metadata_version);
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
    fastcord_bypass::route_http(reqwest::Client::builder())
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
    // The page sets the Cloudflare cookies later API requests carry.
    crate::cookies::store(response.url(), response.headers());
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
    /// Windows: build number; macOS: Darwin major version.
    pub os_sdk_version: Option<String>,
    /// Linux only: `XDG_CURRENT_DESKTOP,GDMSESSION`.
    pub window_manager: Option<String>,
    /// Linux only: `PRETTY_NAME` of os-release.
    pub distro: Option<String>,
    /// The IANA time zone, for `x-discord-timezone`.
    pub timezone: Option<String>,
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
            timezone: None,
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
    pub native_build_number: Option<u32>,
    pub client_event_source: Option<&'static str>,
    pub client_heartbeat_session_id: String,
    /// The installation id Discord issued (READY `apex_experiments`),
    /// persisted by the app; absent until the first session supplies one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<String>,
    /// Not a property: the IANA zone the REST headers report.
    #[serde(skip)]
    pub timezone: Option<String>,
}

/// Explicit REST allowlist. Gateway-only fields must never enter this view
/// (Client Properties footnote 3); installation uses its own REST header.
#[derive(Serialize)]
struct RestProperties<'a> {
    os: &'a str,
    browser: &'a str,
    release_channel: &'a str,
    client_version: &'a str,
    os_version: &'a str,
    os_arch: &'a str,
    app_arch: &'a str,
    system_locale: &'a str,
    has_client_mods: bool,
    client_launch_id: &'a str,
    launch_signature: &'a str,
    browser_user_agent: &'a str,
    browser_version: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    os_sdk_version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_manager: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    distro: Option<&'a str>,
    client_build_number: u32,
    native_build_number: Option<u32>,
    client_event_source: Option<&'a str>,
    client_heartbeat_session_id: &'a str,
}

impl<'a> From<&'a ClientProperties> for RestProperties<'a> {
    fn from(p: &'a ClientProperties) -> Self {
        Self {
            os: p.os,
            browser: p.browser,
            release_channel: p.release_channel,
            client_version: &p.client_version,
            os_version: &p.os_version,
            os_arch: p.os_arch,
            app_arch: p.app_arch,
            system_locale: &p.system_locale,
            has_client_mods: p.has_client_mods,
            client_launch_id: &p.client_launch_id,
            launch_signature: &p.launch_signature,
            browser_user_agent: &p.browser_user_agent,
            browser_version: p.browser_version,
            os_sdk_version: p.os_sdk_version.as_deref(),
            window_manager: p.window_manager.as_deref(),
            distro: p.distro.as_deref(),
            client_build_number: p.client_build_number,
            native_build_number: p.native_build_number,
            client_event_source: p.client_event_source,
            client_heartbeat_session_id: &p.client_heartbeat_session_id,
        }
    }
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
            installation_id: None,
            timezone: host.timezone.clone().filter(|zone| is_time_zone(zone)),
        }
    }

    /// `sec-ch-ua` as Chromium builds it for an embedder without a brand
    /// (Electron): the GREASE brand and Chromium, ordered and spelled by the
    /// major version (Chromium's `GenerateBrandVersionList`).
    pub fn sec_ch_ua(&self) -> String {
        let major: u32 = CHROMIUM_VERSION
            .split('.')
            .next()
            .and_then(|major| major.parse().ok())
            .unwrap_or(0);
        const GREASE_CHARS: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
        const GREASE_VERSIONS: [&str; 3] = ["8", "99", "24"];
        let seed = major as usize;
        let grease = format!(
            "\"Not{}A{}Brand\";v=\"{}\"",
            GREASE_CHARS[seed % GREASE_CHARS.len()],
            GREASE_CHARS[(seed + 1) % GREASE_CHARS.len()],
            GREASE_VERSIONS[seed % GREASE_VERSIONS.len()]
        );
        let chromium = format!("\"Chromium\";v=\"{major}\"");
        if seed.is_multiple_of(2) {
            format!("{grease}, {chromium}")
        } else {
            format!("{chromium}, {grease}")
        }
    }

    /// `sec-ch-ua-platform` without its quotes.
    pub fn ua_platform(&self) -> &'static str {
        match self.os {
            "Windows" => "Windows",
            "Mac OS X" => "macOS",
            _ => "Linux",
        }
    }

    /// Chromium's `Accept-Language` for the locale: the tag, then its language
    /// at q=0.9 when the tag has a region.
    pub fn accept_language(&self) -> String {
        match self.system_locale.split_once('-') {
            Some((language, _)) => format!("{},{language};q=0.9", self.system_locale),
            None => self.system_locale.clone(),
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
    /// per Identify and never per reconnect; [`Identity`] rotates the heartbeat.
    pub fn with_launch_identity(mut self) -> Self {
        self.client_launch_id = Uuid::new_v4().to_string();
        self.launch_signature = launch_signature().to_string();
        self.client_heartbeat_session_id = Uuid::new_v4().to_string();
        self
    }
}

/// An IANA zone name (`Europe/Moscow`, `UTC`): what `x-discord-timezone` may carry.
fn is_time_zone(zone: &str) -> bool {
    (1..=64).contains(&zone.len())
        && zone
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+'))
}

/// Whether `id` has the shape of an installation id: `<digits>.<base64url>`.
pub fn is_installation_id(id: &str) -> bool {
    id.len() <= MAX_INSTALLATION_BYTES
        && id.split_once('.').is_some_and(|(snowflake, hash)| {
            !snowflake.is_empty()
                && snowflake.bytes().all(|b| b.is_ascii_digit())
                && !hash.is_empty()
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// The profile one session's REST client and Gateway share. It is fixed for
/// the run except the analytics heartbeat session, regenerated every 30
/// minutes, and the installation id Discord issues in READY. Clones share one
/// value.
#[derive(Clone, Debug)]
pub struct Identity(Arc<Mutex<Current>>);

#[derive(Debug)]
struct Current {
    properties: ClientProperties,
    heartbeat_since: Instant,
    /// `x-super-properties`, re-encoded only when the profile changes.
    encoded: String,
}

impl Identity {
    pub fn new(properties: ClientProperties) -> Self {
        let encoded = encode(&properties);
        Self(Arc::new(Mutex::new(Current {
            properties,
            heartbeat_since: Instant::now(),
            encoded,
        })))
    }

    /// The profile as Identify sends it now.
    pub fn properties(&self) -> ClientProperties {
        self.with_current(|current| current.properties.clone())
    }

    /// `(x-super-properties, X-Installation-ID)` for a request sent now.
    pub(crate) fn headers(&self) -> (String, Option<String>) {
        self.with_current(|current| {
            (
                current.encoded.clone(),
                current.properties.installation_id.clone(),
            )
        })
    }

    pub fn installation_id(&self) -> Option<String> {
        self.with_current(|current| current.properties.installation_id.clone())
    }

    /// Adopts the installation id Discord issued; a malformed one is ignored.
    pub fn set_installation_id(&self, id: &str) {
        if !is_installation_id(id) {
            return;
        }
        self.with_current(|current| {
            if current.properties.installation_id.as_deref() != Some(id) {
                current.properties.installation_id = Some(id.to_owned());
            }
        });
    }

    fn with_current<R>(&self, f: impl FnOnce(&mut Current) -> R) -> R {
        let mut current = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if !current.properties.client_heartbeat_session_id.is_empty()
            && now.duration_since(current.heartbeat_since) >= HEARTBEAT_ROTATION
        {
            current.properties.client_heartbeat_session_id = Uuid::new_v4().to_string();
            current.heartbeat_since = now;
            current.encoded = encode(&current.properties);
        }
        f(&mut current)
    }
}

impl From<ClientProperties> for Identity {
    fn from(properties: ClientProperties) -> Self {
        Self::new(properties)
    }
}

/// The REST properties view, standard-base64 encoded. Identify serializes
/// `ClientProperties` directly and includes its Gateway-only fields.
pub(crate) fn encode(properties: &ClientProperties) -> String {
    use base64::Engine;
    let json = serde_json::to_string(&RestProperties::from(properties))
        .expect("plain strings and numbers serialize");
    base64::engine::general_purpose::STANDARD.encode(json)
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
            ..Host::bare(HostOs::Windows)
        };
        let versions = Versions {
            client_version: "1.0.9999".to_owned(),
            native_build_number: Some(100_001),
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
        assert_eq!(windows.native_build_number, Some(100_001));
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
    fn browser_headers_follow_chromium() {
        let properties = ClientProperties::bundled(HostOs::Windows, "ru-RU");
        // Chromium 148: seed 148 -> '/' and ')', version "99", GREASE first. The same
        // rule gives Chrome 120 "Not_A Brand";v="8" and 124 "Not-A.Brand";v="99".
        assert_eq!(
            properties.sec_ch_ua(),
            "\"Not/A)Brand\";v=\"99\", \"Chromium\";v=\"148\""
        );
        assert!(properties.sec_ch_ua().contains(&format!(
            "\"Chromium\";v=\"{}\"",
            CHROMIUM_VERSION.split('.').next().unwrap()
        )));
        assert_eq!(properties.ua_platform(), "Windows");
        assert_eq!(
            ClientProperties::bundled(HostOs::MacOs, "en").ua_platform(),
            "macOS"
        );
        assert_eq!(properties.accept_language(), "ru-RU,ru;q=0.9");
        assert_eq!(
            ClientProperties::bundled(HostOs::Linux, "en").accept_language(),
            "en"
        );
        let zoned = |zone: &str| {
            let host = Host {
                timezone: Some(zone.to_owned()),
                ..Host::bare(HostOs::Linux)
            };
            ClientProperties::desktop(&host, "en-US", &Versions::bundled(HostOs::Linux)).timezone
        };
        assert_eq!(zoned("Europe/Moscow").as_deref(), Some("Europe/Moscow"));
        assert_eq!(zoned("Etc/GMT+3").as_deref(), Some("Etc/GMT+3"));
        assert_eq!(zoned("bad zone\r\n"), None);
        // Not a property: never in Identify or x-super-properties.
        let json = serde_json::to_string(&ClientProperties {
            timezone: Some("UTC".to_owned()),
            ..properties
        })
        .unwrap();
        assert!(!json.contains("timezone"));
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

    #[tokio::test(start_paused = true)]
    async fn the_heartbeat_session_rotates_every_30_minutes_and_reaches_the_headers() {
        let identity =
            Identity::new(ClientProperties::bundled(HostOs::Linux, "en-US").with_launch_identity());
        let first = identity.properties();
        let (header, installation) = identity.headers();
        assert_eq!(installation, None);
        tokio::time::advance(HEARTBEAT_ROTATION - Duration::from_secs(1)).await;
        assert_eq!(identity.properties(), first);
        assert_eq!(identity.headers().0, header);
        tokio::time::advance(Duration::from_secs(1)).await;
        let rotated = identity.properties();
        assert_ne!(
            rotated.client_heartbeat_session_id,
            first.client_heartbeat_session_id
        );
        assert_eq!(rotated.client_heartbeat_session_id.len(), 36);
        // Only the heartbeat changed, and the header carries the new value.
        assert_eq!(
            ClientProperties {
                client_heartbeat_session_id: first.client_heartbeat_session_id.clone(),
                ..rotated.clone()
            },
            first
        );
        assert_eq!(identity.headers().0, encode(&rotated));
        // Clones share the one value.
        assert_eq!(identity.clone().properties(), rotated);
        // A profile without a launch identity has no heartbeat to rotate.
        let bare = Identity::new(ClientProperties::bundled(HostOs::Linux, "en-US"));
        tokio::time::advance(HEARTBEAT_ROTATION * 3).await;
        assert_eq!(bare.properties().client_heartbeat_session_id, "");
    }

    #[test]
    fn installation_is_gateway_only_with_a_separate_rest_header() {
        use base64::Engine;
        let identity = Identity::new(ClientProperties::bundled(HostOs::Linux, "en-US"));
        let before = identity.headers().0;
        let long = format!("1.{}", "a".repeat(200));
        for bad in [
            "",
            ".",
            "123.",
            ".abc",
            "12a.abc",
            "123.ab c",
            "123.ab\r\nX: y",
            long.as_str(),
        ] {
            identity.set_installation_id(bad);
            assert_eq!(identity.installation_id(), None, "{bad:?}");
        }
        let id = "1084179945133187083.JQddgNMmwJPghoBtFmaH7jTmdsw";
        identity.set_installation_id(id);
        assert_eq!(identity.installation_id().as_deref(), Some(id));
        assert_eq!(identity.properties().installation_id.as_deref(), Some(id));
        let (header, installation) = identity.headers();
        assert_eq!(installation.as_deref(), Some(id));
        assert_eq!(header, encode(&identity.properties()));
        assert_eq!(header, before, "installation never changes the REST view");
        let rest: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(header)
                .unwrap(),
        )
        .unwrap();
        let mut gateway = serde_json::to_value(identity.properties()).unwrap();
        assert_eq!(
            gateway.as_object_mut().unwrap().remove("installation_id"),
            Some(id.into())
        );
        assert_eq!(rest, gateway, "all common fields agree");
        for gateway_only in ["installation_id", "is_fast_connect", "version"] {
            assert!(rest.get(gateway_only).is_none());
        }
        let json = serde_json::to_string(&identity.properties()).unwrap();
        assert!(json.ends_with(&format!("\"installation_id\":\"{id}\"}}")));
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

    #[test]
    fn native_metadata_is_nullable_in_both_wire_views_and_platform_specific() {
        use base64::Engine;
        assert_eq!(
            Versions::bundled(HostOs::MacOs).native_build_number,
            Some(96_312)
        );
        assert_eq!(
            Versions::bundled(HostOs::Windows).native_build_number,
            Some(96_160)
        );
        let versions = Versions {
            native_build_number: None,
            ..Versions::bundled(HostOs::MacOs)
        };
        let host = Host {
            os_version: "24.2.0".into(),
            os_sdk_version: Some("24".into()),
            ..Host::bare(HostOs::MacOs)
        };
        let profile = ClientProperties::desktop(&host, "en-US", &versions);
        let gateway = serde_json::to_value(&profile).unwrap();
        let rest: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(encode(&profile))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(rest, gateway);
        assert!(rest.get("native_build_number").unwrap().is_null());
        assert_eq!(rest["os_sdk_version"], "24");
    }

    /// The pin check (`.github/workflows/identity-pin.yml`; manual:
    /// `cargo test -p fastcord-discord live_version_chain --locked -- --ignored --nocapture`).
    /// Fetches all supported platform/architecture manifests and the web
    /// client; fails on host/native drift. Runtime-pair evidence outside
    /// Windows x64 is a separate capture gate. Never downloads Discord.
    #[tokio::test]
    #[ignore = "contacts updates.discord.com and discord.com"]
    async fn live_version_chain_matches_the_pin() {
        // Explicit opt-in for the maintainer's filtered route. The same
        // fetch_client used in production must retain route_http after merges.
        if let Ok(strategy) = std::env::var("FASTCORD_IDENTITY_BYEDPI") {
            fastcord_bypass::enable(&strategy).expect("the local byedpi engine starts");
        }
        let build = fetch_build_number()
            .await
            .expect("discord.com/app carried a build number");
        let cases = [
            (HostOs::Windows, "x64"),
            (HostOs::Windows, "arm64"),
            (HostOs::MacOs, "x64"),
            (HostOs::MacOs, "arm64"),
            (HostOs::Linux, "x64"),
        ];
        let mut tasks = tokio::task::JoinSet::new();
        for (os, arch) in cases {
            tasks.spawn(async move { (os, arch, fetch_manifest(os, arch).await) });
        }
        // Check every target even when one drifts, so CI shows the full matrix.
        let mut drift = Vec::new();
        while let Some(result) = tasks.join_next().await {
            let (os, arch, release) = result.unwrap();
            let Some(release) = release else {
                drift.push(format!("{os:?}/{arch}: manifest unavailable"));
                continue;
            };
            println!(
                "{os:?}/{arch}: host {} native {} web {build}",
                release.host_version, release.metadata_version
            );
            let bundled = Versions::bundled_for(os, arch);
            if release.host_version != bundled.client_version
                || Some(release.metadata_version) != bundled.native_build_number
            {
                drift.push(format!(
                    "{os:?}/{arch}: host/native pin changed; review runtime versions"
                ));
            }
            let versions = Versions {
                client_version: release.host_version,
                native_build_number: Some(release.metadata_version),
                client_build_number: build,
            };
            let profile = ClientProperties::desktop(&Host::bare(os), "en-US", &versions);
            assert!(
                profile
                    .browser_user_agent
                    .contains(&format!("discord/{} ", profile.client_version))
            );
            assert!(
                profile
                    .browser_user_agent
                    .contains(&format!("Electron/{} ", profile.browser_version))
            );
            assert_eq!(profile.native_build_number, versions.native_build_number);
            assert_eq!(profile.client_build_number, build);
        }
        assert!(drift.is_empty(), "{}", drift.join("\n"));
    }
}
