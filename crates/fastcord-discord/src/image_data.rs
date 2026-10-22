//! The one bounded image-to-`data:` URI helper shared by every request that
//! uploads a small picture inside a JSON body (group icons, profile avatars).
//! The type comes from the file's signature, never its name, and the base64
//! form of the largest accepted image stays inside the 1 MiB request budget.

use base64::Engine;

/// Largest image read. The base64 form must fit the 1 MiB request budget.
pub const MAX_IMAGE_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// Empty, not a recognized format, or a format this use does not allow.
    Unsupported,
    TooLarge,
}

/// Encodes a PNG or JPEG (and a GIF when `allow_gif`) of at most
/// [`MAX_IMAGE_BYTES`] as `data:<mime>;base64,...`.
pub(crate) fn data_uri(bytes: &[u8], allow_gif: bool) -> Result<String, ImageError> {
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if allow_gif && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        "image/gif"
    } else {
        return Err(ImageError::Unsupported);
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ImageError::TooLarge);
    }
    Ok(encode(mime, bytes))
}

fn encode(mime: &str, bytes: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("data:{mime};base64,{encoded}")
}

/// Largest custom emoji file Discord accepts.
pub const MAX_EMOJI_BYTES: usize = 256 * 1024;

/// A custom emoji image: PNG, JPEG, GIF or WebP of at most [`MAX_EMOJI_BYTES`],
/// judged by signature.
pub(crate) fn emoji_data_uri(bytes: &[u8]) -> Result<String, ImageError> {
    let webp = bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP";
    let mime = if webp {
        "image/webp"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else {
        return Err(ImageError::Unsupported);
    };
    if bytes.len() > MAX_EMOJI_BYTES {
        return Err(ImageError::TooLarge);
    }
    Ok(encode(mime, bytes))
}

/// A soundboard clip: MP3 or Ogg of at most `limit` bytes, judged by signature.
pub(crate) fn sound_data_uri(bytes: &[u8], limit: usize) -> Result<String, ImageError> {
    let mp3 = bytes.starts_with(b"ID3")
        || (bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0);
    let mime = if bytes.starts_with(b"OggS") {
        "audio/ogg"
    } else if mp3 {
        "audio/mpeg"
    } else {
        return Err(ImageError::Unsupported);
    };
    if bytes.len() > limit {
        return Err(ImageError::TooLarge);
    }
    Ok(encode(mime, bytes))
}
