//! The per-user settings folder and the one atomic file writer.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// `<config root>/<app>`: `%APPDATA%`, `~/Library/Application Support`, or
/// `$XDG_CONFIG_HOME` / `~/.config`. Unchanged from the former per-file copies,
/// so existing settings keep loading.
pub fn config_dir(app: &str) -> Option<PathBuf> {
    config_dir_from(|name| std::env::var_os(name), app)
}

/// `<cache root>/<app>`: `%LOCALAPPDATA%`, `~/Library/Caches`, or
/// `$XDG_CACHE_HOME` / `~/.cache`.
pub fn cache_dir(app: &str) -> Option<PathBuf> {
    cache_dir_from(|name| std::env::var_os(name), app)
}

/// The XDG spec: a relative base directory is invalid and ignored.
#[cfg(all(unix, not(target_os = "macos")))]
fn xdg(var: &impl Fn(&str) -> Option<OsString>, name: &str, fallback: &str) -> Option<PathBuf> {
    var(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(fallback)))
}

fn config_dir_from(var: impl Fn(&str) -> Option<OsString>, app: &str) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let root = var("APPDATA").map(PathBuf::from)?;
    #[cfg(target_os = "macos")]
    let root = var("HOME")
        .map(PathBuf::from)?
        .join("Library/Application Support");
    #[cfg(all(unix, not(target_os = "macos")))]
    let root = xdg(&var, "XDG_CONFIG_HOME", ".config")?;
    Some(root.join(app))
}

fn cache_dir_from(var: impl Fn(&str) -> Option<OsString>, app: &str) -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let root = var("LOCALAPPDATA").map(PathBuf::from)?;
    #[cfg(target_os = "macos")]
    let root = var("HOME").map(PathBuf::from)?.join("Library/Caches");
    #[cfg(all(unix, not(target_os = "macos")))]
    let root = xdg(&var, "XDG_CACHE_HOME", ".cache")?;
    Some(root.join(app))
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Creates the parent folder, writes `bytes` to a uniquely named sibling and
/// renames it over `path`; the rename is retried briefly because Windows
/// refuses it while a scanner or reader holds the target.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("settings parent unavailable"))?;
    std::fs::create_dir_all(parent)?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = parent.join(name);
    std::fs::write(&temporary, bytes)?;
    let mut result = std::fs::rename(&temporary, path);
    for _ in 0..3 {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        result = std::fs::rename(&temporary, path);
    }
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_uses_appdata() {
        let roaming = "C:\\Users\\u\\AppData\\Roaming";
        let dir = config_dir_from(env(&[("APPDATA", roaming)]), "fastcord");
        assert_eq!(dir, Some(PathBuf::from(roaming).join("fastcord")));
        assert_eq!(config_dir_from(env(&[("HOME", "C:\\h")]), "fastcord"), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_cache_uses_local_appdata() {
        let local = r"C:\Users\u\AppData\Local";
        let dir = cache_dir_from(env(&[("LOCALAPPDATA", local)]), "fastcord");
        assert_eq!(dir, Some(PathBuf::from(local).join("fastcord")));
        assert_eq!(
            cache_dir_from(env(&[("APPDATA", r"C:\a")]), "fastcord"),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_cache_uses_library_caches() {
        let dir = cache_dir_from(env(&[("HOME", "/Users/u")]), "fastcord");
        assert_eq!(dir, Some(PathBuf::from("/Users/u/Library/Caches/fastcord")));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_cache_prefers_xdg_then_home_and_ignores_relative() {
        let both = env(&[("XDG_CACHE_HOME", "/x"), ("HOME", "/h")]);
        assert_eq!(
            cache_dir_from(both, "fastcord"),
            Some(PathBuf::from("/x/fastcord"))
        );
        assert_eq!(
            cache_dir_from(env(&[("HOME", "/h")]), "fastcord"),
            Some(PathBuf::from("/h/.cache/fastcord"))
        );
        let relative = env(&[("XDG_CACHE_HOME", "rel"), ("HOME", "/h")]);
        assert_eq!(
            cache_dir_from(relative, "fastcord"),
            Some(PathBuf::from("/h/.cache/fastcord"))
        );
        let relative = env(&[("XDG_CONFIG_HOME", "rel"), ("HOME", "/h")]);
        assert_eq!(
            config_dir_from(relative, "fastcord"),
            Some(PathBuf::from("/h/.config/fastcord"))
        );
        assert_eq!(cache_dir_from(env(&[]), "fastcord"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_application_support() {
        let dir = config_dir_from(env(&[("HOME", "/Users/u")]), "fastcord");
        assert_eq!(
            dir,
            Some(PathBuf::from(
                "/Users/u/Library/Application Support/fastcord"
            ))
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_prefers_xdg_then_home() {
        let both = env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")]);
        assert_eq!(
            config_dir_from(both, "fastcord"),
            Some(PathBuf::from("/x/fastcord"))
        );
        assert_eq!(
            config_dir_from(env(&[("HOME", "/h")]), "fastcord"),
            Some(PathBuf::from("/h/.config/fastcord"))
        );
        assert_eq!(config_dir_from(env(&[]), "fastcord"), None);
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temporary() {
        let dir = std::env::temp_dir().join(format!("fastcord-paths-{}", std::process::id()));
        let file = dir.join("sub").join("a.json");
        write_atomic(&file, b"one").unwrap();
        write_atomic(&file, b"two").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        assert_eq!(
            std::fs::read_dir(file.parent().unwrap()).unwrap().count(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
