//! Explicit, per-user registration of the app-owned `fastcord` URL scheme.
//! Blocking OS work: call only from a worker. Never changes `discord` handlers.
//! There is deliberately no uninstall or implicit registration operation.

#[cfg(any(target_os = "linux", windows, test))]
use std::path::Path;

#[cfg(target_os = "linux")]
#[path = "link_handler/linux.rs"]
mod linux;
#[cfg(windows)]
#[path = "link_handler/windows.rs"]
mod windows;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Unsupported,
    Conflict,
    UnsafeExecutable,
    Failed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "Link registration is not supported on this desktop.",
            Self::Conflict => "Another handler or installation owns fastcord links. Change it in your system settings.",
            Self::UnsafeExecutable => "This executable path cannot be registered safely.",
            Self::Failed => "Link registration failed. Check your system settings before trying again.",
        })
    }
}
impl std::error::Error for Error {}

/// The user must explicitly request this operation; never call during startup.
pub fn register() -> Result<(), Error> {
    let exe = std::env::current_exe().map_err(|_| Error::Failed)?;
    #[cfg(target_os = "linux")]
    {
        linux::register(&exe)
    }
    #[cfg(windows)]
    {
        windows::register(&exe)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = exe;
        Err(Error::Unsupported)
    }
}

// Restrict rather than interpolate shell/desktop field-code metacharacters.
// Spaces and Unicode remain supported. All launchers use an absolute path.
#[cfg(any(target_os = "linux", windows, test))]
fn executable(path: &Path) -> Result<&str, Error> {
    let text = path.to_str().ok_or(Error::UnsafeExecutable)?;
    if text.len() > 2048
        || !path.is_absolute()
        || text.contains(|c: char| c.is_control() || matches!(c, '"' | '%' | '$' | '`'))
    {
        return Err(Error::UnsafeExecutable);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_rejects_field_codes_and_command_metacharacters() {
        for path in [
            "relative",
            "/tmp/a%u",
            "/tmp/a\"b",
            "/tmp/a\nb",
            "/tmp/$HOME",
            "/tmp/`id`",
        ] {
            assert_eq!(executable(Path::new(path)), Err(Error::UnsafeExecutable));
        }
        let path = std::env::temp_dir().join("Fast Cord Ω").join("fastcord");
        assert!(executable(&path).is_ok());
    }
}
