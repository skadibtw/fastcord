//! Start-at-sign-in (issue #105). Windows writes or removes one value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`; it is touched only when
//! the user toggles the setting. Elsewhere [`AutostartError::Unsupported`].

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
            Self::Unsupported => "Starting at sign-in is only supported on Windows in this build.",
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

/// Whether the `Run` value exists.
pub fn is_enabled() -> bool {
    sys::is_enabled()
}

/// Writes (for the running executable) or removes the `Run` value.
pub fn set(enabled: bool) -> Result<(), AutostartError> {
    if !enabled {
        return sys::remove();
    }
    let exe = std::env::current_exe().map_err(|_| AutostartError::Failed)?;
    sys::write(&run_value(&exe).ok_or(AutostartError::Failed)?)
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

#[cfg(not(windows))]
mod sys {
    use super::AutostartError;

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
}
