//! Native desktop notifications (SPEC milestone 42): one text toast whose
//! Open or MarkRead action runs one callback. Policy (what notifies, coalescing, previews) belongs
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

/// Runs, at most once, when the user chooses a known toast action. It runs on a native
/// thread and must only hand the click on (send it to a queue).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Open,
    MarkRead,
}

impl Action {
    #[cfg_attr(not(any(windows, target_os = "linux", test)), allow(dead_code))]
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "default" => Some(Self::Open),
            "mark-read" => Some(Self::MarkRead),
            _ => None,
        }
    }
}

pub type OnClick = Box<dyn FnOnce(Action) + Send>;

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
    open_label: &str,
    mark_read_label: &str,
    on_click: impl FnOnce(Action) + Send + 'static,
) -> Result<(), NotifyError> {
    native(
        &clip(title, MAX_TITLE_CHARS),
        &clip(body, MAX_BODY_CHARS),
        &clip(open_label, MAX_TITLE_CHARS),
        &clip(mark_read_label, MAX_TITLE_CHARS),
        Box::new(on_click),
    )
}

#[cfg(target_os = "linux")]
use linux::show as native;
#[cfg(target_os = "macos")]
use macos::show as native;

#[cfg(any(windows, test))]
fn toast_xml(title: &str, body: &str, mark_read_label: &str) -> String {
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    format!(
        "<toast launch=\"default\"><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual><actions><action content=\"{}\" arguments=\"mark-read\" activationType=\"foreground\"/></actions></toast>",
        escape(title),
        escape(body),
        escape(mark_read_label)
    )
}

#[cfg(windows)]
fn native(
    title: &str,
    body: &str,
    _open_label: &str,
    mark_read_label: &str,
    on_click: OnClick,
) -> Result<(), NotifyError> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{
        ToastActivatedEventArgs, ToastNotification, ToastNotificationManager,
    };
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::core::{HSTRING, IInspectable, Interface};

    /// A toast's `Activated` handler lives only as long as the toast object,
    /// so the last few toasts are kept alive here.
    static SHOWN: Mutex<Pending<u64>> = Mutex::new(Pending::new());
    static TOASTS: Mutex<VecDeque<ToastNotification>> = Mutex::new(VecDeque::new());
    static NEXT: AtomicU64 = AtomicU64::new(0);

    // An already-initialized thread reports a harmless error.
    // SAFETY: plain WinRT apartment initialization of the calling thread.
    #[allow(unsafe_code)]
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let xml = toast_xml(title, body, mark_read_label);
    let key = NEXT.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut shown) = SHOWN.lock() {
        shown.insert(key, on_click);
    }
    let show = || -> windows::core::Result<ToastNotification> {
        let document = XmlDocument::new()?;
        document.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&document)?;
        toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
            move |_, args| {
                let Some(action) = args
                    .as_ref()
                    .and_then(|args| args.cast::<ToastActivatedEventArgs>().ok())
                    .and_then(|args| args.Arguments().ok())
                    .filter(|args| args.len() <= 32)
                    .and_then(|args| Action::from_id(&args.to_string_lossy()))
                else {
                    return Ok(());
                };
                let click = SHOWN.lock().ok().and_then(|mut shown| shown.take(&key));
                if let Some(click) = click {
                    click(action);
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
fn native(
    _title: &str,
    _body: &str,
    _open_label: &str,
    _mark_read_label: &str,
    _on_click: OnClick,
) -> Result<(), NotifyError> {
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
    fn toast_payload_has_one_fixed_read_action_and_escaped_user_text() {
        let xml = toast_xml("Ann & <QA>", "body", "Read \"now\"");
        assert!(xml.contains("Ann &amp; &lt;QA&gt;"));
        assert!(xml.contains("Read &quot;now&quot;"));
        assert_eq!(xml.matches("arguments=\"mark-read\"").count(), 1);
        assert!(xml.contains("launch=\"default\""));
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_xml_accepts_the_action_payload_without_showing_a_toast() {
        use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
        #[allow(unsafe_code)]
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        let document = windows::Data::Xml::Dom::XmlDocument::new().unwrap();
        document
            .LoadXml(&windows::core::HSTRING::from(toast_xml(
                "QA",
                "body",
                "Mark as read",
            )))
            .unwrap();
        let actions = document
            .GetElementsByTagName(&windows::core::HSTRING::from("action"))
            .unwrap();
        assert_eq!(actions.Length().unwrap(), 1);
    }

    #[test]
    fn action_ids_are_exact_and_never_parse_arbitrary_navigation_or_writes() {
        assert_eq!(Action::from_id("default"), Some(Action::Open));
        assert_eq!(Action::from_id("mark-read"), Some(Action::MarkRead));
        for unknown in [
            "",
            "MARK-READ",
            "mark-read\0",
            "mark-read?channel=3",
            "discord://-/channels/@me/3/4",
        ] {
            assert_eq!(Action::from_id(unknown), None);
        }
    }

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
                Box::new(move |_| {
                    runs.fetch_add(1, Ordering::Relaxed);
                }),
            );
        }
        assert!(pending.take(&0).is_none(), "the oldest was forgotten");
        assert!(pending.take(&1).is_none());
        pending.take(&2).unwrap()(Action::Open);
        assert!(pending.take(&2).is_none(), "a click runs once");
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        assert_eq!(pending.entries.len(), MAX_PENDING - 1);
    }

    #[test]
    fn markup_is_escaped() {
        assert_eq!(escape_markup("a<b>&c"), "a&lt;b&gt;&amp;c");
    }
}
