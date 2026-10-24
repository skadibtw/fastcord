//! Native desktop notifications (SPEC milestone 42): one text toast whose
//! click runs a callback. Policy (what notifies, coalescing, previews) belongs
//! to the caller; this only shows a toast, may block, and belongs on a worker.
//! Windows uses a WinRT `ToastNotification`, Linux the freedesktop
//! `org.freedesktop.Notifications` D-Bus service (`notify/linux.rs`), macOS
//! `UNUserNotificationCenter` (`notify/macos.rs`, packaged `.app` only).
//! At most [`MAX_PENDING`] clicks are waited for; an older toast's click is
//! forgotten.

use std::collections::VecDeque;
use std::fmt;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

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

/// Toasts whose click is still waited for.
pub const MAX_PENDING: usize = 8;

/// Runs, at most once, when the user clicks the toast. It runs on a native
/// thread and must only hand the click on (send it to a queue).
pub type OnClick = Box<dyn FnOnce() + Send>;

/// The clicks waited for, oldest first, bounded by [`MAX_PENDING`].
struct Pending<K> {
    entries: VecDeque<(K, OnClick)>,
}

impl<K: PartialEq> Pending<K> {
    const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    fn insert(&mut self, key: K, click: OnClick) {
        if self.entries.len() == MAX_PENDING {
            self.entries.pop_front();
        }
        self.entries.push_back((key, click));
    }

    fn take(&mut self, key: &K) -> Option<OnClick> {
        let index = self.entries.iter().position(|(k, _)| k == key)?;
        self.entries.remove(index).map(|(_, click)| click)
    }
}

/// Shows one toast. The text is clipped here too, so no caller can exceed the bound.
pub fn show(
    title: &str,
    body: &str,
    on_click: impl FnOnce() + Send + 'static,
) -> Result<(), NotifyError> {
    native(
        &clip(title, MAX_TITLE_CHARS),
        &clip(body, MAX_BODY_CHARS),
        Box::new(on_click),
    )
}

#[cfg(target_os = "linux")]
use linux::show as native;
#[cfg(target_os = "macos")]
use macos::show as native;

#[cfg(windows)]
fn native(title: &str, body: &str, on_click: OnClick) -> Result<(), NotifyError> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::core::{HSTRING, IInspectable};

    /// A toast's `Activated` handler lives only as long as the toast object,
    /// so the last few toasts are kept alive here.
    static SHOWN: Mutex<Pending<u64>> = Mutex::new(Pending::new());
    static TOASTS: Mutex<VecDeque<ToastNotification>> = Mutex::new(VecDeque::new());
    static NEXT: AtomicU64 = AtomicU64::new(0);

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
    let key = NEXT.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut shown) = SHOWN.lock() {
        shown.insert(key, on_click);
    }
    let show = || -> windows::core::Result<ToastNotification> {
        let document = XmlDocument::new()?;
        document.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&document)?;
        toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
            move |_, _| {
                let click = SHOWN.lock().ok().and_then(|mut shown| shown.take(&key));
                if let Some(click) = click {
                    click();
                }
                Ok(())
            },
        ))?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from("fastcord"))?
            .Show(&toast)?;
        Ok(toast)
    };
    match show() {
        Ok(toast) => {
            if let Ok(mut toasts) = TOASTS.lock() {
                if toasts.len() == MAX_PENDING {
                    toasts.pop_front();
                }
                toasts.push_back(toast);
            }
            Ok(())
        }
        Err(_) => {
            let _ = SHOWN.lock().map(|mut shown| shown.take(&key));
            Err(NotifyError::Failed)
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn native(_title: &str, _body: &str, _on_click: OnClick) -> Result<(), NotifyError> {
    Err(NotifyError::Unsupported)
}

/// Body text for a notification server that reads it as markup.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

    #[test]
    fn pending_clicks_are_bounded_and_taken_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let runs = Arc::new(AtomicUsize::new(0));
        let mut pending = Pending::new();
        for key in 0..MAX_PENDING as u32 + 2 {
            let runs = runs.clone();
            pending.insert(
                key,
                Box::new(move || {
                    runs.fetch_add(1, Ordering::Relaxed);
                }),
            );
        }
        assert!(pending.take(&0).is_none(), "the oldest was forgotten");
        assert!(pending.take(&1).is_none());
        pending.take(&2).unwrap()();
        assert!(pending.take(&2).is_none(), "a click runs once");
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        assert_eq!(pending.entries.len(), MAX_PENDING - 1);
    }

    #[test]
    fn markup_is_escaped() {
        assert_eq!(escape_markup("a<b>&c"), "a&lt;b&gt;&amp;c");
    }
}
