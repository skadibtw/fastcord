//! The operating-system facts the Discord desktop profile reports, read the way
//! Electron/Node reads them. Blocking but cheap; called once per login.

/// Node's `os.release()` plus the per-OS extras of the desktop profile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OsRelease {
    /// `10.0.26100` on Windows, the Darwin release on macOS, the kernel release
    /// on Linux; empty when the OS does not say.
    pub version: String,
    /// Windows: build number; macOS: Darwin major version.
    pub sdk_version: Option<String>,
    /// Linux: `XDG_CURRENT_DESKTOP,GDMSESSION`, `unknown` for a missing one.
    pub window_manager: Option<String>,
    /// Linux: `PRETTY_NAME` from os-release.
    pub distro: Option<String>,
    /// The IANA time zone (`Europe/Moscow`), when the OS names one.
    pub timezone: Option<String>,
}

#[cfg(windows)]
pub fn os_release() -> OsRelease {
    let (major, minor, build) = windows_version();
    // WinRT's calendar reports the IANA name, as Chromium's ICU does.
    let timezone = windows::Globalization::Calendar::new()
        .and_then(|calendar| calendar.GetTimeZone())
        .ok()
        .map(|zone| zone.to_string_lossy());
    OsRelease {
        version: format!("{major}.{minor}.{build}"),
        sdk_version: Some(build.to_string()),
        timezone,
        ..OsRelease::default()
    }
}

/// `(major, minor, build)` from `RtlGetVersion`, which, unlike
/// `GetVersionEx`, is not capped by the manifest. Zeros if it fails.
#[cfg(windows)]
#[allow(unsafe_code)] // One NT version query, confined here.
pub(crate) fn windows_version() -> (u32, u32, u32) {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    static VERSION: std::sync::OnceLock<(u32, u32, u32)> = std::sync::OnceLock::new();
    *VERSION.get_or_init(|| {
        let mut info = OSVERSIONINFOW {
            dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a correctly sized, writable OSVERSIONINFOW.
        if unsafe { RtlGetVersion(&mut info) }.is_ok() {
            (info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber)
        } else {
            (0, 0, 0)
        }
    })
}

#[cfg(target_os = "linux")]
pub fn os_release() -> OsRelease {
    let read = |path| std::fs::read_to_string(path).unwrap_or_default();
    let os_release = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .unwrap_or_default();
    let env = |name| std::env::var(name).ok().filter(|v| !v.is_empty());
    OsRelease {
        version: read("/proc/sys/kernel/osrelease").trim().to_owned(),
        sdk_version: None,
        window_manager: Some(window_manager(
            env("XDG_CURRENT_DESKTOP"),
            env("GDMSESSION"),
        )),
        distro: pretty_name(&os_release),
        timezone: unix_time_zone(),
    }
}

#[cfg(target_os = "macos")]
pub fn os_release() -> OsRelease {
    let version = std::process::Command::new("/usr/bin/uname")
        .arg("-r")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .unwrap_or_default();
    OsRelease {
        version: version.trim().to_owned(),
        sdk_version: darwin_sdk(&version),
        timezone: unix_time_zone(),
        ..OsRelease::default()
    }
}

/// The documented macOS desktop profile sends the Darwin major as its SDK.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn darwin_sdk(release: &str) -> Option<String> {
    let major = release.trim().split('.').next()?;
    (!major.is_empty() && major.bytes().all(|b| b.is_ascii_digit())).then(|| major.to_owned())
}

/// `TZ` when it names a zone, else the zone `/etc/localtime` links to.
#[cfg(unix)]
fn unix_time_zone() -> Option<String> {
    let from_env = std::env::var("TZ").ok().and_then(|tz| zone_name(&tz));
    from_env.or_else(|| {
        let target = std::fs::read_link("/etc/localtime").ok()?;
        zone_name(target.to_str()?)
    })
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn os_release() -> OsRelease {
    OsRelease::default()
}

/// The zone in a `TZ` value or a zoneinfo path: `Europe/Moscow` from
/// `Europe/Moscow`, `:Europe/Moscow` or `/usr/share/zoneinfo/Europe/Moscow`.
#[cfg_attr(not(unix), allow(dead_code))]
fn zone_name(value: &str) -> Option<String> {
    let value = value.strip_prefix(':').unwrap_or(value);
    let zone = match value.rsplit_once("zoneinfo/") {
        Some((_, zone)) => zone,
        None if !value.starts_with('/') => value,
        None => return None,
    };
    let valid = (1..=64).contains(&zone.len())
        && zone
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+'));
    valid.then(|| zone.to_owned())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn window_manager(desktop: Option<String>, session: Option<String>) -> String {
    let unknown = || "unknown".to_owned();
    format!(
        "{},{}",
        desktop.unwrap_or_else(unknown),
        session.unwrap_or_else(unknown)
    )
}

/// `PRETTY_NAME` of an os-release file, unquoted.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn pretty_name(os_release: &str) -> Option<String> {
    let value = os_release
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))?
        .trim();
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value);
    (!value.is_empty() && value.len() <= 128).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_sdk_is_the_darwin_major() {
        assert_eq!(darwin_sdk("24.2.0\n").as_deref(), Some("24"));
        for invalid in ["", "\n", "Darwin", ".2.0"] {
            assert_eq!(darwin_sdk(invalid), None);
        }
    }

    #[test]
    fn linux_extras_are_read_like_the_desktop_client() {
        let file = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.4 LTS\"\nID=ubuntu\n";
        assert_eq!(pretty_name(file).as_deref(), Some("Ubuntu 24.04.4 LTS"));
        assert_eq!(
            pretty_name("PRETTY_NAME='Arch Linux'").as_deref(),
            Some("Arch Linux")
        );
        assert_eq!(pretty_name("PRETTY_NAME=Gentoo").as_deref(), Some("Gentoo"));
        assert_eq!(pretty_name("PRETTY_NAME=\"\""), None);
        assert_eq!(pretty_name("ID=debian"), None);
        assert_eq!(
            window_manager(Some("GNOME".into()), Some("ubuntu".into())),
            "GNOME,ubuntu"
        );
        assert_eq!(
            window_manager(Some("Hyprland".into()), None),
            "Hyprland,unknown"
        );
    }

    #[test]
    fn zone_names_are_read_from_tz_values_and_zoneinfo_paths() {
        assert_eq!(zone_name("Europe/Moscow").as_deref(), Some("Europe/Moscow"));
        assert_eq!(
            zone_name(":America/New_York").as_deref(),
            Some("America/New_York")
        );
        assert_eq!(
            zone_name("/usr/share/zoneinfo/Etc/GMT+3").as_deref(),
            Some("Etc/GMT+3")
        );
        assert_eq!(
            zone_name("/var/db/timezone/zoneinfo/Asia/Tokyo").as_deref(),
            Some("Asia/Tokyo")
        );
        assert_eq!(zone_name("/etc/custom"), None);
        assert_eq!(zone_name(""), None);
        assert_eq!(zone_name("bad zone"), None);
    }

    #[test]
    fn the_running_os_reports_a_release() {
        let release = os_release();
        if cfg!(windows) {
            let parts: Vec<u32> = release
                .version
                .split('.')
                .map(|p| p.parse().unwrap())
                .collect();
            assert_eq!(parts.len(), 3);
            assert!(parts[0] >= 10, "{}", release.version);
            assert_eq!(release.sdk_version, Some(parts[2].to_string()));
            let zone = release.timezone.clone().expect("Windows names a zone");
            assert_eq!(zone_name(&zone).as_deref(), Some(zone.as_str()));
            println!("os {} zone {zone}", release.version);
        }
        if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
            assert!(!release.version.is_empty());
        }
    }
}
