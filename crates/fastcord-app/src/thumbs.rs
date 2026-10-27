//! Persistent store of already-sized public images (avatars, server icons,
//! stickers, small embed pictures). A hit is one small file read of raw RGBA:
//! no network, no PNG/WebP decode, no resize. The identity is kind + id + CDN
//! hash + box, so a changed avatar (new hash) is a new file and entries never
//! need revalidation. Total size is bounded by [`THUMB_CACHE_LIMIT`], evicting
//! the least recently used file. Lives next to the attachment cache, per account.
use std::fs;
use std::path::Path;

use crate::disk_index;

use crate::attachments::{AttachmentError, DecodedImage};

pub const THUMB_CACHE_LIMIT: u64 = 64 * 1024 * 1024;
/// Larger pictures (big embeds) stay on the compressed attachment path.
pub const MAX_THUMB_BYTES: usize = 128 * 1024;
const MAGIC: &[u8; 4] = b"FCT1";
const HEADER: usize = 12;
/// A hit older than this has its recency refreshed (one metadata write).
const TOUCH_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// File name for one rendition; `hash` is reduced to name-safe characters.
pub fn name(kind: u8, id: u64, hash: &str, fit: (u32, u32)) -> String {
    let hash: String = hash
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(40)
        .collect();
    format!("{kind}-{id}-{hash}-{}x{}.thumb", fit.0, fit.1)
}

pub fn read(dir: &Path, name: &str) -> Option<DecodedImage> {
    let path = dir.join(name);
    let metadata = fs::metadata(&path).ok()?;
    if metadata.len() > (HEADER + MAX_THUMB_BYTES) as u64 {
        return None;
    }
    let data = fs::read(&path).ok()?;
    let image = parse(&data);
    if image.is_none() {
        let _ = fs::remove_file(&path);
        disk_index::forget(&path);
    } else {
        disk_index::touch(&path);
    }
    if image.is_some()
        && metadata
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > TOUCH_AFTER)
    {
        let _ = filetime::set_file_mtime(&path, filetime::FileTime::now());
    }
    image
}

fn parse(data: &[u8]) -> Option<DecodedImage> {
    if data.len() < HEADER || &data[..4] != MAGIC {
        return None;
    }
    let width = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let height = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let expected = u64::from(width) * u64::from(height) * 4;
    if width == 0 || height == 0 || expected != (data.len() - HEADER) as u64 {
        return None;
    }
    Some(DecodedImage {
        width,
        height,
        rgba: bytes::Bytes::copy_from_slice(&data[HEADER..]),
    })
}

/// Stores `image` (skipped when over [`MAX_THUMB_BYTES`]) and evicts the least
/// recently used files past [`THUMB_CACHE_LIMIT`] from the in-memory ledger.
pub fn write(dir: &Path, name: &str, image: &DecodedImage) -> Result<(), AttachmentError> {
    if image.rgba.len() > MAX_THUMB_BYTES {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|_| AttachmentError::CacheUnavailable)?;
    disk_index::ensure(dir.parent().unwrap_or(dir))?;
    let mut data = Vec::with_capacity(HEADER + image.rgba.len());
    data.extend_from_slice(MAGIC);
    data.extend_from_slice(&image.width.to_le_bytes());
    data.extend_from_slice(&image.height.to_le_bytes());
    data.extend_from_slice(&image.rgba);
    let destination = dir.join(name);
    fastcord_platform::paths::write_atomic(&destination, &data)
        .map_err(|_| AttachmentError::CacheUnavailable)?;
    let (victims, _) = disk_index::record(&destination, data.len() as u64, None, THUMB_CACHE_LIMIT);
    disk_index::delete(victims);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(fill: u8, side: u32) -> DecodedImage {
        DecodedImage {
            width: side,
            height: side,
            rgba: vec![fill; (side * side * 4) as usize].into(),
        }
    }

    fn scratch(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("fastcord-thumbs-{label}-{}", std::process::id()))
    }

    #[test]
    fn names_are_stable_and_change_with_hash_or_size() {
        let a = name(2, 9, "h1", (64, 64));
        assert_eq!(a, name(2, 9, "h1", (64, 64)));
        assert_ne!(a, name(2, 9, "h2", (64, 64)));
        assert_ne!(a, name(2, 9, "h1", (48, 48)));
        assert_ne!(a, name(3, 9, "h1", (64, 64)));
        assert!(!name(2, 9, "../x", (1, 1)).contains(['/', '\\']));
    }

    #[test]
    fn an_entry_survives_a_restart_and_corrupt_files_are_dropped() {
        let dir = scratch("restart").join("1");
        let _ = fs::remove_dir_all(&dir);
        let n = name(2, 9, "h1", (64, 64));
        write(&dir, &n, &image(7, 64)).unwrap();
        // A fresh read, as after a restart, needs nothing but the file.
        let hit = read(&dir, &n).unwrap();
        assert_eq!((hit.width, hit.height), (64, 64));
        assert!(hit.rgba.iter().all(|b| *b == 7));
        fs::write(dir.join(&n), b"FCT1garbage").unwrap();
        assert!(read(&dir, &n).is_none() && !dir.join(&n).exists());
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn the_ledger_evicts_least_recently_used_without_rescanning() {
        let base = scratch("lru");
        let dir = base.join("1");
        let _ = fs::remove_dir_all(&base);
        let names: Vec<_> = (0..4).map(|i| name(2, i, "h", (64, 64))).collect();
        for n in &names {
            write(&dir, n, &image(1, 64)).unwrap();
        }
        let one = 16 * 1024 + HEADER as u64;
        assert_eq!(disk_index::total(&base), 4 * one);
        // A file the ledger never saw proves later writes do not walk the disk.
        fs::write(dir.join("stranger.bin"), vec![0u8; 100_000]).unwrap();
        // Reading the oldest makes it the newest.
        assert!(read(&dir, &names[0]).is_some());
        let limit = 4 * one;
        let (victims, fits) = disk_index::record(&dir.join("new.thumb"), one, None, limit);
        assert!(fits);
        assert_eq!(victims, vec![dir.join(&names[1])]);
        assert_eq!(disk_index::total(&base), 4 * one);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn a_restart_counts_what_is_on_disk_and_still_enforces_the_budget() {
        let base = scratch("restart-budget");
        let dir = base.join("1");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..3 {
            let path = dir.join(format!("{i}.thumb"));
            fs::write(&path, vec![0u8; 10_000]).unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(100 + i, 0))
                .unwrap();
        }
        // No ledger yet (a new process): the first write scans once.
        write(&dir, &name(2, 1, "h", (64, 64)), &image(1, 64)).unwrap();
        assert_eq!(
            disk_index::total(&base),
            3 * 10_000 + 16 * 1024 + HEADER as u64
        );
        let (victims, fits) = disk_index::record(&dir.join("x.thumb"), 10_000, None, 40_000);
        assert!(fits);
        assert_eq!(victims[0], dir.join("0.thumb"));
        let _ = fs::remove_dir_all(base);
    }
}
