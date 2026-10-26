//! Turning a pasted clipboard image into a file that can be attached.
//!
//! Only an explicit "Paste image" action reads the clipboard. The bitmap is
//! re-encoded as PNG into a private temporary directory; the file is deleted
//! when the last reference to it goes (see `UploadFile::inspect_temporary`), and
//! anything left behind by a crash is removed at the next start.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::i18n::tr;
use image::ImageFormat;

const DIR: &str = "fastcord-pastes";
fn directory() -> PathBuf {
    std::env::temp_dir().join(DIR)
}

/// Removes pasted images an earlier run left behind. Only files untouched for
/// twelve hours go, so a second running instance keeps its own.
pub fn purge_stale() {
    let Ok(entries) = std::fs::read_dir(directory()) else {
        return;
    };
    let limit = std::time::Duration::from_secs(12 * 60 * 60);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > limit);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// PNG bytes for straight RGBA pixels, alpha kept. A bitmap whose alpha is
/// zero everywhere (a 32-bit screenshot leaves the spare byte empty) is made
/// opaque, or it would come out invisible.
pub fn png_from_rgba(width: u32, height: u32, mut rgba: Vec<u8>) -> Result<Vec<u8>, &'static str> {
    const BAD: &str = "The clipboard image could not be read.";
    let (pixels, _) = rgba.as_chunks_mut::<4>();
    if pixels.iter().all(|pixel| pixel[3] == 0) {
        pixels.iter_mut().for_each(|pixel| pixel[3] = u8::MAX);
    }
    let image = image::RgbaImage::from_raw(width, height, rgba).ok_or_else(|| tr(BAD))?;
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|_| tr(BAD))?;
    Ok(png.into_inner())
}

/// Writes `png` to a new file in the private directory.
pub fn save_png(png: &[u8]) -> std::io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = directory();
    std::fs::create_dir_all(&dir)?;
    let name = format!(
        "pasted-{}-{}.png",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(name);
    std::fs::write(&path, png)?;
    Ok(path)
}

/// Reads the clipboard image and saves it as a PNG; blocking.
pub fn paste_image() -> Result<PathBuf, &'static str> {
    let image = fastcord_platform::clipboard::image_rgba().map_err(|error| match error {
        fastcord_platform::clipboard::ClipboardError::Unsupported => {
            tr("Pasting an image is not supported on this platform yet.")
        }
        fastcord_platform::clipboard::ClipboardError::NoImage => {
            tr("The clipboard does not hold an image.")
        }
        fastcord_platform::clipboard::ClipboardError::Busy => {
            tr("The clipboard is in use by another program. Try again.")
        }
        fastcord_platform::clipboard::ClipboardError::TooLarge => {
            tr("The clipboard image is too large to attach.")
        }
    })?;
    let png = png_from_rgba(image.width, image.height, image.rgba)?;
    save_png(&png).map_err(|_| tr("The pasted image could not be saved."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_pixels_become_a_png_that_keeps_transparency() {
        // red, half-transparent green / blue, fully transparent white
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 128, //
            0, 0, 255, 255, 255, 255, 255, 0,
        ];
        let png = png_from_rgba(2, 2, rgba).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let image = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (2, 2));
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [0, 255, 0, 128]);
        assert_eq!(image.get_pixel(0, 1).0, [0, 0, 255, 255]);
        assert_eq!(image.get_pixel(1, 1).0, [255, 255, 255, 0]);
    }

    #[test]
    fn a_bitmap_with_no_alpha_at_all_is_made_opaque() {
        let png = png_from_rgba(1, 2, vec![10, 20, 30, 0, 40, 50, 60, 0]).unwrap();
        let image = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(image.get_pixel(0, 1).0, [40, 50, 60, 255]);
    }

    #[test]
    fn a_short_buffer_is_refused_not_panicked_on() {
        assert!(png_from_rgba(2, 2, vec![0; 15]).is_err());
    }

    #[test]
    fn saved_pastes_live_in_the_private_directory_and_purge_keeps_fresh_ones() {
        let path = save_png(b"png").unwrap();
        assert!(path.starts_with(directory()));
        purge_stale();
        assert!(path.exists(), "a fresh paste belongs to a running instance");
        std::fs::remove_file(path).unwrap();
    }
}
