//! Reading an image from the system clipboard, only when the user asks.
//!
//! The clipboard is never watched or read in the background: [`image_rgba`]
//! is called from an explicit "Paste image" action. It returns straight RGBA
//! pixels through `arboard` (Windows, X11/Wayland, macOS); turning them into a
//! file is the caller's job. May block briefly (another program can hold the
//! clipboard, X11 starts a short-lived thread), so it belongs on a worker.

use std::fmt;

/// Largest bitmap read from the clipboard, in RGBA bytes.
pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// Why no image was read. None of them carries clipboard contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardError {
    /// This platform has no clipboard image support yet.
    Unsupported,
    /// The clipboard holds no image.
    NoImage,
    /// Another program holds the clipboard; try again.
    Busy,
    /// The image is larger than [`MAX_IMAGE_BYTES`].
    TooLarge,
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "Pasting an image is not supported on this platform yet.",
            Self::NoImage => "The clipboard does not hold an image.",
            Self::Busy => "The clipboard is in use by another program. Try again.",
            Self::TooLarge => "The clipboard image is too large to attach.",
        })
    }
}

impl std::error::Error for ClipboardError {}

/// A clipboard image: `width * height` straight RGBA pixels, rows top first.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The clipboard's image, if it holds one.
pub fn image_rgba() -> Result<Image, ClipboardError> {
    let mut clipboard = arboard::Clipboard::new().map_err(error)?;
    let image = clipboard.get_image().map_err(error)?;
    let (width, height) = (
        u32::try_from(image.width).map_err(|_| ClipboardError::TooLarge)?,
        u32::try_from(image.height).map_err(|_| ClipboardError::TooLarge)?,
    );
    if image.bytes.len() > MAX_IMAGE_BYTES {
        return Err(ClipboardError::TooLarge);
    }
    if width == 0 || height == 0 || image.bytes.len() != image.width * image.height * 4 {
        return Err(ClipboardError::NoImage);
    }
    Ok(Image {
        width,
        height,
        rgba: image.bytes.into_owned(),
    })
}

fn error(error: arboard::Error) -> ClipboardError {
    match error {
        arboard::Error::ContentNotAvailable | arboard::Error::ConversionFailure => {
            ClipboardError::NoImage
        }
        arboard::Error::ClipboardOccupied => ClipboardError::Busy,
        _ => ClipboardError::Unsupported,
    }
}

