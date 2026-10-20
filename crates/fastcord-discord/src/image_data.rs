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
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{mime};base64,{encoded}"))
}
