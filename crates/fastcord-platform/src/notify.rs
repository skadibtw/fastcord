//! Native desktop notifications (SPEC milestone 42): one text toast. Policy
//! (what notifies, coalescing, previews) belongs to the caller; this only shows
//! a toast, may block, and belongs on a worker. Windows uses a WinRT
//! `ToastNotification`; elsewhere [`NotifyError::Unsupported`] until the
//! Linux/macOS adapters exist.

use std::fmt;

/// Title and body are cut to these many Unicode scalar values.
pub const MAX_TITLE_CHARS: usize = 64;
pub const MAX_BODY_CHARS: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyError {
    Unsupported,
    Failed,
}

impl fmt::Display for NotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "Desktop notifications are not supported on this platform.",
            Self::Failed => "The operating system refused the notification.",
        })
    }
}

impl std::error::Error for NotifyError {}

/// `text` cut to `max` scalar values (control characters dropped), with an
/// ellipsis when something was cut.
pub fn clip(text: &str, max: usize) -> String {
    let mut chars = text.chars().filter(|c| !c.is_control());
    let mut out: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// Shows one toast. The text is clipped here too, so no caller can exceed the bound.
pub fn show(title: &str, body: &str) -> Result<(), NotifyError> {
    native(&clip(title, MAX_TITLE_CHARS), &clip(body, MAX_BODY_CHARS))
}

#[cfg(windows)]
fn native(title: &str, body: &str) -> Result<(), NotifyError> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::core::HSTRING;

    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    // An already-initialized thread reports a harmless error.
    // SAFETY: plain WinRT apartment initialization of the calling thread.
    #[allow(unsafe_code)]
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let xml = format!(
        "<toast><visual><binding template=\"ToastText02\"><text id=\"1\">{}</text><text id=\"2\">{}</text></binding></visual></toast>",
        escape(title),
        escape(body)
    );
    let show = || -> windows::core::Result<()> {
        let document = XmlDocument::new()?;
        document.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&document)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from("fastcord"))?
            .Show(&toast)
    };
    show().map_err(|_| NotifyError::Failed)
}

#[cfg(not(windows))]
fn native(_title: &str, _body: &str) -> Result<(), NotifyError> {
    Err(NotifyError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_bounds_and_strips_control_characters() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("a\nb\u{7}c", 10), "abc");
        assert_eq!(
            clip(&"é".repeat(500), MAX_BODY_CHARS).chars().count(),
            MAX_BODY_CHARS + 1
        );
    }
}
