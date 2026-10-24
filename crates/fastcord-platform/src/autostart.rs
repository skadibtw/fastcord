//! Start-at-sign-in (issue #105). Windows writes or removes one value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, Linux one XDG autostart
//! entry (`~/.config/autostart/fastcord.desktop`), macOS one launch agent
//! (`~/Library/LaunchAgents/io.github.skadibtw.fastcord.plist`, read at the
//! next login). Each is touched only when the user toggles the setting.
//! Elsewhere [`AutostartError::Unsupported`].

use std::fmt;
use std::path::Path;

/// The registry value name under the per-user `Run` key.
pub const VALUE_NAME: &str = "fastcord";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutostartError {
    Unsupported,
    Failed,
}

impl fmt::Display for AutostartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "Starting at sign-in is not supported on this operating system.",
            Self::Failed => "The operating system refused the start-at-sign-in change.",
        })
    }
}

impl std::error::Error for AutostartError {}

/// The `Run` value for `exe`: its path in quotes. `None` for a path that is
/// not UTF-8, is empty, or holds a quote or control character (which would
/// change what the shell runs).
pub fn run_value(exe: &Path) -> Option<String> {
    let path = exe.to_str()?;
    (!path.is_empty() && !path.chars().any(|c| c == '"' || c.is_control()))
        .then(|| format!("\"{path}\""))
}

/// The XDG autostart entry for `exe`. `None` for a path the `Exec` key would
/// have to escape (quote, backslash, `$`, backtick, `%`) or that is not UTF-8,
/// is empty, or holds a control character.
pub fn desktop_entry(exe: &Path) -> Option<String> {
    let path = exe.to_str()?;
    (!path.is_empty()
        && !path
            .chars()
            .any(|c| matches!(c, '"' | '\\' | '$' | '`' | '%') || c.is_control()))
    .then(|| {
        format!(
            "[Desktop Entry]\nType=Application\nName=fastcord\nExec=\"{path}\"\n\
             X-GNOME-Autostart-enabled=true\n"
        )
    })
}

/// The launch agent property list for `exe`, which runs it once at login in
/// the graphical session. `None` for a path that is not UTF-8, is empty, or
/// holds a control character.
pub fn launch_agent(exe: &Path) -> Option<String> {
    let path = exe.to_str()?;
    if path.is_empty() || path.chars().any(char::is_control) {
        return None;
    }
    let path = path
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    Some(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{AGENT_LABEL}</string>
  <key>ProgramArguments</key><array><string>{path}</string></array>
  <key>RunAtLoad</key><true/>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
</dict>
</plist>
"#
    ))
}

/// The launch agent's label and file name (the bundle identifier).
pub const AGENT_LABEL: &str = "io.github.skadibtw.fastcord";

/// Whether the start-at-sign-in entry exists.
pub fn is_enabled() -> bool {
    sys::is_enabled()
}

/// Writes (for the running executable) or removes the start-at-sign-in entry.
pub fn set(enabled: bool) -> Result<(), AutostartError> {
    if !enabled {
        return sys::remove();
    }
    let exe = std::env::current_exe().map_err(|_| AutostartError::Failed)?;
    sys::write(&sys::value(&exe).ok_or(AutostartError::Failed)?)
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 registry FFI, confined here.
mod sys {
    use super::{AutostartError, VALUE_NAME};
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW,
    };
    use windows::core::{HSTRING, PCWSTR};

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub use super::run_value as value;

    fn open() -> Result<HKEY, AutostartError> {
        let mut key = HKEY::default();
        // SAFETY: `key` is a valid out-parameter; the key name is a NUL-terminated string.
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE | KEY_QUERY_VALUE,
                None,
                &mut key,
                None,
            )
        }
        .ok()
        .map_err(|_| AutostartError::Failed)?;
        Ok(key)
    }

    pub fn is_enabled() -> bool {
        let mut key = HKEY::default();
        // SAFETY: read-only open of an existing key into a valid out-parameter.
        let opened = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                None,
                KEY_QUERY_VALUE,
                &mut key,
            )
        };
        if opened.is_err() {
            return false;
        }
        // SAFETY: `key` is open; no data is requested, only existence.
        let found =
            unsafe { RegQueryValueExW(key, &HSTRING::from(VALUE_NAME), None, None, None, None) }
                .is_ok();
        // SAFETY: closes the key opened above.
        let _ = unsafe { RegCloseKey(key) };
        found
    }

    pub fn write(value: &str) -> Result<(), AutostartError> {
        let key = open()?;
        let wide: Vec<u8> = value
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        // SAFETY: `key` is open and `wide` is a NUL-terminated UTF-16 string.
        let result =
            unsafe { RegSetValueExW(key, &HSTRING::from(VALUE_NAME), None, REG_SZ, Some(&wide)) };
        // SAFETY: closes the key opened above.
        let _ = unsafe { RegCloseKey(key) };
        result.ok().map_err(|_| AutostartError::Failed)
    }

    pub fn remove() -> Result<(), AutostartError> {
        let key = open()?;
        // SAFETY: `key` is open.
        let result = unsafe { RegDeleteValueW(key, &HSTRING::from(VALUE_NAME)) };
        // SAFETY: closes the key opened above.
        let _ = unsafe { RegCloseKey(key) };
        // A value that is already gone is the wanted state.
        if result.is_ok() || result == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(AutostartError::Failed)
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod sys {
    use std::io::ErrorKind;
    use std::path::PathBuf;

    use super::AutostartError;

    #[cfg(target_os = "linux")]
    pub use super::desktop_entry as value;
    #[cfg(target_os = "macos")]
    pub use super::launch_agent as value;

    fn home() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
    }

    #[cfg(target_os = "linux")]
    fn file() -> Option<PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| home().map(|home| home.join(".config")))?;
        Some(config.join("autostart").join("fastcord.desktop"))
    }

    #[cfg(target_os = "macos")]
    fn file() -> Option<PathBuf> {
        Some(
            home()?
                .join("Library/LaunchAgents")
                .join(format!("{}.plist", super::AGENT_LABEL)),
        )
    }

    pub fn is_enabled() -> bool {
        file().is_some_and(|file| file.is_file())
    }

    pub fn write(value: &str) -> Result<(), AutostartError> {
        let file = file().ok_or(AutostartError::Failed)?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|_| AutostartError::Failed)?;
        }
        std::fs::write(file, value).map_err(|_| AutostartError::Failed)
    }

    pub fn remove() -> Result<(), AutostartError> {
        let file = file().ok_or(AutostartError::Failed)?;
        match std::fs::remove_file(file) {
            // An entry that is already gone is the wanted state.
            Err(error) if error.kind() != ErrorKind::NotFound => Err(AutostartError::Failed),
            _ => Ok(()),
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod sys {
    use std::path::Path;

    use super::AutostartError;

    pub fn value(_: &Path) -> Option<String> {
        None
    }
    pub fn is_enabled() -> bool {
        false
    }
    pub fn write(_: &str) -> Result<(), AutostartError> {
        Err(AutostartError::Unsupported)
    }
    pub fn remove() -> Result<(), AutostartError> {
        Err(AutostartError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_value_is_the_quoted_path_and_refuses_anything_that_could_change_the_command() {
        assert_eq!(
            run_value(Path::new(r"C:\Program Files\fastcord\fastcord.exe")).as_deref(),
            Some(r#""C:\Program Files\fastcord\fastcord.exe""#)
        );
        for bad in ["", "C:\\a\"b.exe", "C:\\a\nb.exe"] {
            assert_eq!(run_value(Path::new(bad)), None, "{bad:?}");
        }
    }

    #[test]
    fn the_desktop_entry_quotes_the_path_and_refuses_what_exec_would_escape() {
        let entry = desktop_entry(Path::new("/opt/fast cord/fastcord")).unwrap();
        assert!(entry.starts_with("[Desktop Entry]\nType=Application\n"));
        assert!(entry.contains("\nExec=\"/opt/fast cord/fastcord\"\n"));
        for bad in ["", "/a\"b", "/a\\b", "/a$b", "/a`b", "/a%b", "/a\nb"] {
            assert_eq!(desktop_entry(Path::new(bad)), None, "{bad:?}");
        }
    }

    #[test]
    fn the_launch_agent_runs_the_escaped_path_at_login() {
        let plist =
            launch_agent(Path::new("/Applications/a&b.app/Contents/MacOS/fastcord")).unwrap();
        assert!(plist.contains(
            "<array><string>/Applications/a&amp;b.app/Contents/MacOS/fastcord</string></array>"
        ));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
        assert!(plist.contains("<string>io.github.skadibtw.fastcord</string>"));
        assert_eq!(launch_agent(Path::new("/a\nb")), None);
        assert_eq!(launch_agent(Path::new("")), None);
    }
}
