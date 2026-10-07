//! The web-client Identify profile (ADR 0005).
//!
//! One versioned module owns every property sent to the Gateway. The profile is
//! built once per Gateway start and reused unchanged for every Identify across
//! reconnects: nothing here is randomized per connection.

use std::time::{Duration, SystemTime};

use serde::Serialize;

/// Bump whenever the properties below change shape or meaning.
pub const PROFILE_VERSION: u32 = 1;
/// Chrome stable at the time this profile was last reviewed (2026-10-07). The
/// UA and `browser_version` must agree; Chrome's reduced UA fixes the minor
/// components to zero.
const CHROME_MAJOR: u32 = 155;
/// `client_build_number` of discord.com's web client observed on 2026-10-07.
/// Used only when the live value cannot be fetched.
pub const BUNDLED_BUILD_NUMBER: u32 = 631_730;

const APP_URL: &str = "https://discord.com/app";
const BUILD_MARKER: &[u8] = b"\"BUILD_NUMBER\":\"";
/// The marker sits in a script near the top of the page; never read more.
const MAX_PAGE_BYTES: usize = 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Plausibility window for a web build number; anything else is page drift.
const BUILD_RANGE: std::ops::RangeInclusive<u32> = 100_000..=9_999_999;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildSource {
    /// Read from discord.com's current web client.
    Fetched,
    /// The value bundled at release time.
    Bundled,
}

/// The web client build number reported as `client_build_number`, timestamped
/// when it was resolved at Gateway start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildNumber {
    pub value: u32,
    pub source: BuildSource,
    pub resolved_at: SystemTime,
}

impl BuildNumber {
    pub fn bundled() -> Self {
        Self {
            value: BUNDLED_BUILD_NUMBER,
            source: BuildSource::Bundled,
            resolved_at: SystemTime::now(),
        }
    }

    pub fn fetched(value: u32) -> Self {
        Self {
            value,
            source: BuildSource::Fetched,
            resolved_at: SystemTime::now(),
        }
    }
}

/// Fetches the current build number from discord.com's web client, falling
/// back to the bundled value on any failure (offline, page drift, redirect).
/// The request is unauthenticated and carries no account data.
pub(crate) async fn resolve_build_number() -> BuildNumber {
    match fetch_build_number().await {
        Some(value) => BuildNumber::fetched(value),
        None => BuildNumber::bundled(),
    }
}

async fn fetch_build_number() -> Option<u32> {
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .user_agent(ClientProperties::web(HostOs::current(), "en-US", 0).browser_user_agent)
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;
    let mut response = client
        .get(APP_URL)
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

/// Operating systems the web profile can describe truthfully.
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

    /// `(os, os_version, UA platform token)` as Chrome reports them: the UA is
    /// frozen (Windows NT 10.0 covers Windows 11; macOS stays 10_15_7), and
    /// Chrome exposes no Linux version.
    const fn web_identity(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Windows => ("Windows", "10", "Windows NT 10.0; Win64; x64"),
            Self::MacOs => ("Mac OS X", "10.15.7", "Macintosh; Intel Mac OS X 10_15_7"),
            Self::Linux => ("Linux", "", "X11; Linux x86_64"),
        }
    }
}

/// Identify `properties`, field names and value shapes as sent by the web
/// client. Serialization order is declaration order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClientProperties {
    pub os: &'static str,
    pub browser: &'static str,
    pub device: &'static str,
    pub system_locale: String,
    pub has_client_mods: bool,
    pub browser_user_agent: String,
    pub browser_version: String,
    pub os_version: &'static str,
    pub referrer: &'static str,
    pub referring_domain: &'static str,
    pub referrer_current: &'static str,
    pub referring_domain_current: &'static str,
    pub release_channel: &'static str,
    pub client_build_number: u32,
    pub client_event_source: Option<&'static str>,
}

impl ClientProperties {
    pub fn web(host: HostOs, locale: &str, client_build_number: u32) -> Self {
        let (os, os_version, platform) = host.web_identity();
        Self {
            os,
            browser: "Chrome",
            device: "",
            system_locale: locale.to_owned(),
            has_client_mods: false,
            browser_user_agent: format!(
                "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/{CHROME_MAJOR}.0.0.0 Safari/537.36"
            ),
            browser_version: format!("{CHROME_MAJOR}.0.0.0"),
            os_version,
            referrer: "",
            referring_domain: "",
            referrer_current: "",
            referring_domain_current: "",
            release_channel: "stable",
            client_build_number,
            client_event_source: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = include_str!("../../../../fixtures/gateway/app-page-excerpt.html");

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
    fn web_profile_is_truthful_consistent_and_stable() {
        let windows = ClientProperties::web(HostOs::Windows, "ru-RU", 631_730);
        assert_eq!(windows.os, "Windows");
        assert_eq!(windows.browser, "Chrome");
        assert_eq!(windows.system_locale, "ru-RU");
        assert_eq!(windows.release_channel, "stable");
        assert_eq!(windows.client_build_number, 631_730);
        assert!(
            windows
                .browser_user_agent
                .contains(&format!("Chrome/{}", windows.browser_version))
        );
        assert!(windows.browser_user_agent.contains("Windows NT 10.0"));
        let mac = ClientProperties::web(HostOs::MacOs, "en-US", 1);
        assert_eq!(mac.os, "Mac OS X");
        assert!(mac.browser_user_agent.contains("Macintosh"));
        let linux = ClientProperties::web(HostOs::Linux, "en-US", 1);
        assert_eq!((linux.os, linux.os_version), ("Linux", ""));
        // Never randomized: identical inputs give an identical profile.
        assert_eq!(
            ClientProperties::web(HostOs::Windows, "ru-RU", 631_730),
            windows
        );
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
    fn bundled_build_number_is_inside_the_plausibility_window() {
        assert!(BUILD_RANGE.contains(&BUNDLED_BUILD_NUMBER));
        let bundled = BuildNumber::bundled();
        assert_eq!(bundled.source, BuildSource::Bundled);
        assert_eq!(bundled.value, BUNDLED_BUILD_NUMBER);
    }

    /// Manual: `cargo test -p fastcord-discord live_ --locked -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "contacts discord.com"]
    async fn live_build_number_is_fetched_from_the_web_client() {
        let build = resolve_build_number().await;
        println!("client_build_number {} ({:?})", build.value, build.source);
        assert_eq!(build.source, BuildSource::Fetched);
        assert!(BUILD_RANGE.contains(&build.value));
    }
}
