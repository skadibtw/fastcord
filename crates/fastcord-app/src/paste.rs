//! Turning a pasted clipboard image into a file that can be attached.
//!
//! Only an explicit "Paste image" action reads the clipboard. The bitmap is
//! re-encoded as PNG into a private temporary directory; the file is deleted
//! when the last reference to it goes (see `UploadFile::inspect_temporary`), and
//! anything left behind by a crash is removed at the next start.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use image::{DynamicImage, ImageFormat, ImageReader, Limits};

const DIR: &str = "fastcord-pastes";
/// Largest decoded bitmap, in bytes of RGB pixels, a paste may expand to.
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;

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

/// PNG bytes for a `CF_DIB` bitmap (a `BITMAPINFOHEADER`, optional colors and
/// masks, then pixels). The alpha channel is dropped: screenshots put zero in
/// the unused byte of 32-bit bitmaps, which would otherwise come out invisible.
pub fn png_from_dib(dib: &[u8]) -> Result<Vec<u8>, &'static str> {
    const BAD: &str = "The clipboard image could not be read.";
    let word = |at: usize| -> Option<u32> {
        dib.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let header = word(0).ok_or(BAD)? as usize;
    if header < 40 || header > dib.len() {
        return Err(BAD);
    }
    let bits = dib
        .get(14..16)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or(BAD)?;
    let compression = word(16).ok_or(BAD)?;
    let used = word(32).ok_or(BAD)? as usize;
    let mut colors = if bits <= 8 {
        if used == 0 { 1usize << bits } else { used }
    } else {
        used
    };
    colors = colors.min(256) * 4;
    // BI_BITFIELDS keeps three masks after a 40-byte header.
    let masks = if compression == 3 && header == 40 {
        12
    } else {
        0
    };
    let offset = 14 + header + colors + masks;
    let mut bmp = Vec::with_capacity(dib.len() + 14);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((dib.len() + 14) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    let mut reader = ImageReader::with_format(Cursor::new(bmp), ImageFormat::Bmp);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| BAD)?;
    let rgb = DynamicImage::ImageRgb8(decoded.to_rgb8());
    let mut png = Cursor::new(Vec::new());
    rgb.write_to(&mut png, ImageFormat::Png).map_err(|_| BAD)?;
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
    let dib = fastcord_platform::clipboard::image_dib().map_err(|error| match error {
        fastcord_platform::clipboard::ClipboardError::Unsupported => {
            "Pasting an image is not supported on this platform yet."
        }
        fastcord_platform::clipboard::ClipboardError::NoImage => {
            "The clipboard does not hold an image."
        }
        fastcord_platform::clipboard::ClipboardError::Busy => {
            "The clipboard is in use by another program. Try again."
        }
        fastcord_platform::clipboard::ClipboardError::TooLarge => {
            "The clipboard image is too large to attach."
        }
    })?;
    let png = png_from_dib(&dib)?;
    save_png(&png).map_err(|_| "The pasted image could not be saved.")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x2 bottom-up 24-bit DIB: red, green / blue, white.
    fn dib() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&24u16.to_le_bytes());
        d.extend_from_slice(&[0; 24]);
        // Rows are padded to 4 bytes; BGR order; the bottom row comes first.
        d.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]); // blue, white
        d.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]); // red, green
        d
    }

    #[test]
    fn a_clipboard_bitmap_becomes_a_png_with_the_same_pixels() {
        let png = png_from_dib(&dib()).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let image = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(image.dimensions(), (2, 2));
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(image.get_pixel(1, 0).0, [0, 255, 0]);
        assert_eq!(image.get_pixel(0, 1).0, [0, 0, 255]);
        assert_eq!(image.get_pixel(1, 1).0, [255, 255, 255]);
    }

    #[test]
    fn garbage_and_truncated_bitmaps_are_refused_not_panicked_on() {
        for bad in [&b""[..], &[0u8; 10][..], &[1u8; 200][..]] {
            assert!(png_from_dib(bad).is_err());
        }
        let mut cut = dib();
        cut.truncate(50);
        assert!(png_from_dib(&cut).is_err());
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
