//! Reading an image from the system clipboard, only when the user asks.
//!
//! The clipboard is never watched or read in the background: [`image_dib`] is
//! called from an explicit "Paste image" action. It returns the device-independent
//! bitmap (`CF_DIB`) bytes unchanged; turning them into a file is the caller's
//! job. May block briefly (another program can hold the clipboard), so it
//! belongs on a worker thread.
#![allow(unsafe_code)]

use std::fmt;

/// Largest bitmap read from the clipboard.
pub const MAX_DIB_BYTES: usize = 64 * 1024 * 1024;

/// Why no image was read. None of them carries clipboard contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardError {
    /// This platform has no clipboard image support yet.
    Unsupported,
    /// The clipboard holds no image.
    NoImage,
    /// Another program holds the clipboard; try again.
    Busy,
    /// The image is larger than [`MAX_DIB_BYTES`].
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

/// The clipboard's bitmap as `CF_DIB` bytes (a `BITMAPINFOHEADER` followed by
/// the pixels).
#[cfg(target_os = "windows")]
pub fn image_dib() -> Result<Vec<u8>, ClipboardError> {
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    const CF_DIB: u32 = 8;

    /// Closes the clipboard however the read ends.
    struct Open;
    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: only constructed after `OpenClipboard` succeeded on this thread.
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    let mut opened = false;
    for _ in 0..10 {
        // SAFETY: a null owner window associates the clipboard with this task.
        if unsafe { OpenClipboard(None) }.is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !opened {
        return Err(ClipboardError::Busy);
    }
    let _open = Open;
    // SAFETY: the clipboard is open on this thread for all calls below.
    unsafe {
        if IsClipboardFormatAvailable(CF_DIB).is_err() {
            return Err(ClipboardError::NoImage);
        }
        let handle: HANDLE = GetClipboardData(CF_DIB).map_err(|_| ClipboardError::NoImage)?;
        let global = HGLOBAL(handle.0);
        let size = GlobalSize(global);
        if size == 0 {
            return Err(ClipboardError::NoImage);
        }
        if size > MAX_DIB_BYTES {
            return Err(ClipboardError::TooLarge);
        }
        let pointer = GlobalLock(global);
        if pointer.is_null() {
            return Err(ClipboardError::Busy);
        }
        // The block is `size` readable bytes while locked; copy before unlocking.
        let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(global);
        Ok(bytes)
    }
}

#[cfg(not(target_os = "windows"))]
pub fn image_dib() -> Result<Vec<u8>, ClipboardError> {
    Err(ClipboardError::Unsupported)
}
