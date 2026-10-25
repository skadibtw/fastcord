//! The operating-system facts the Discord desktop profile reports, read the way
//! Electron/Node reads them. Blocking but cheap; called once per login.

/// Node's `os.release()` plus the per-OS extras of the desktop profile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OsRelease {
    /// `10.0.26100` on Windows, the Darwin release on macOS, the kernel release
    /// on Linux; empty when the OS does not say.
    pub version: String,
    /// Windows: the build number.
    pub sdk_version: Option<String>,
    /// Linux: `XDG_CURRENT_DESKTOP,GDMSESSION`, `unknown` for a missing one.
    pub window_manager: Option<String>,
    /// Linux: `PRETTY_NAME` from os-release.
    pub distro: Option<String>,
}

#[cfg(windows)]
pub fn os_release() -> OsRelease {
    let (major, minor, build) = windows_version();
    OsRelease {
        version: format!("{major}.{minor}.{build}"),
        sdk_version: Some(build.to_string()),
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
        ..OsRelease::default()
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn os_release() -> OsRelease {
    OsRelease::default()
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
        }
        if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
            assert!(!release.version.is_empty());
        }
    }
}
