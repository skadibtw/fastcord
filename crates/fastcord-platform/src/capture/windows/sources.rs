//! Which displays and windows can be shared: monitor and top-level window
//! enumeration, and the filter that keeps desktop furniture, tool windows,
//! hidden and suspended windows, and this process's own windows out of it.

use std::ffi::c_void;
use std::mem::size_of;

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetClassNameW, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    MONITORINFOF_PRIMARY, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows::core::BOOL;

use super::super::{
    CaptureError, CaptureSource, MAX_SOURCES, MAX_TITLE_CHARS, SourceId, SourceKind,
};
use super::runtime::native_error;

/// Windows smaller than this in either direction are not offered: they are
/// controls, tooltips and shadows rather than something to share.
const MIN_WINDOW_EDGE: u32 = 48;

/// Shell windows that carry a title but are the desktop itself.
const DESKTOP_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
];

/// What the shareability filter looks at, gathered from the OS in one place so
/// the rule itself is a pure function.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct WindowInfo {
    pub visible: bool,
    pub minimized: bool,
    /// Suspended UWP hosts and windows on other virtual desktops are
    /// "cloaked": visible to the API, invisible to the user.
    pub cloaked: bool,
    pub own_process: bool,
    pub tool_window: bool,
    pub app_window: bool,
    pub title: String,
    pub class: String,
    pub width: u32,
    pub height: u32,
}

pub(super) fn shareable(window: &WindowInfo) -> bool {
    window.visible
        && !window.minimized
        && !window.cloaked
        && !window.own_process
        // A tool window is a palette or tooltip unless it asks for a taskbar
        // button.
        && (!window.tool_window || window.app_window)
        && !window.title.trim().is_empty()
        && !DESKTOP_CLASSES.contains(&window.class.as_str())
        && window.width >= MIN_WINDOW_EDGE
        && window.height >= MIN_WINDOW_EDGE
}

/// A title as the picker shows it: control characters become spaces, runs of
/// whitespace collapse, and the result is at most [`MAX_TITLE_CHARS`] scalars.
pub(super) fn sanitize_title(title: &str) -> String {
    let mut clean = String::with_capacity(title.len().min(MAX_TITLE_CHARS * 4));
    let mut pending_space = false;
    for character in title.chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !clean.is_empty();
            continue;
        }
        if pending_space {
            clean.push(' ');
            pending_space = false;
        }
        clean.push(character);
        if clean.chars().count() >= MAX_TITLE_CHARS {
            break;
        }
    }
    clean
}

pub(super) fn display_label(number: usize, width: u32, height: u32, primary: bool) -> String {
    format!(
        "Screen {number} - {width}x{height}{}",
        if primary { " (primary)" } else { "" }
    )
}

pub(in crate::capture) fn list_sources() -> Result<Vec<CaptureSource>, CaptureError> {
    let mut sources = displays()?;
    let remaining = MAX_SOURCES.saturating_sub(sources.len());
    sources.extend(windows(remaining));
    Ok(sources)
}

struct Monitor {
    handle: u64,
    width: u32,
    height: u32,
    primary: bool,
}

fn displays() -> Result<Vec<CaptureSource>, CaptureError> {
    let mut monitors: Vec<Monitor> = Vec::new();
    // SAFETY: the callback runs only during this call and `monitors` outlives
    // it; the pointer is the one the callback casts back.
    let enumerated = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut monitors as *mut Vec<Monitor> as isize),
        )
    };
    if !enumerated.as_bool() {
        return Err(CaptureError::Native {
            operation: "EnumDisplayMonitors",
            code: -1,
        });
    }
    // The primary display first, then in the system's order.
    monitors.sort_by_key(|monitor| !monitor.primary);
    Ok(monitors
        .into_iter()
        .take(MAX_SOURCES)
        .enumerate()
        .map(|(index, monitor)| CaptureSource {
            id: SourceId::from_raw(SourceKind::Display, monitor.handle),
            title: display_label(index + 1, monitor.width, monitor.height, monitor.primary),
            width: monitor.width,
            height: monitor.height,
            primary: monitor.primary,
        })
        .collect())
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _device: HDC,
    _bounds: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the `&mut Vec<Monitor>` `displays` passed, valid for
    // the whole enumeration.
    let monitors = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `info` is a valid MONITORINFOEXW whose size is declared; the
    // extended structure begins with a MONITORINFO.
    let described = unsafe {
        GetMonitorInfoW(
            monitor,
            (&mut info as *mut MONITORINFOEXW).cast::<MONITORINFO>(),
        )
    };
    if described.as_bool() {
        let bounds = info.monitorInfo.rcMonitor;
        monitors.push(Monitor {
            handle: monitor.0 as usize as u64,
            width: (bounds.right - bounds.left).max(0) as u32,
            height: (bounds.bottom - bounds.top).max(0) as u32,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    BOOL::from(true)
}

fn windows(limit: usize) -> Vec<CaptureSource> {
    let mut found: Vec<CaptureSource> = Vec::new();
    if limit == 0 {
        return found;
    }
    let mut context = (&mut found, limit);
    // SAFETY: the callback runs only during this call; `context` outlives it
    // and the pointer is the one the callback casts back. An error here means
    // the callback stopped the enumeration at the limit.
    let _ = unsafe {
        EnumWindows(
            Some(collect_window),
            LPARAM(&mut context as *mut (&mut Vec<CaptureSource>, usize) as isize),
        )
    };
    found
}

unsafe extern "system" fn collect_window(window: HWND, data: LPARAM) -> BOOL {
    // SAFETY: `data` is the context `windows` passed, valid for the whole
    // enumeration.
    let (found, limit) = unsafe { &mut *(data.0 as *mut (&mut Vec<CaptureSource>, usize)) };
    if found.len() >= *limit {
        return BOOL::from(false);
    }
    if let Some(info) = describe(window)
        && shareable(&info)
    {
        found.push(CaptureSource {
            id: SourceId::from_raw(SourceKind::Window, window.0 as usize as u64),
            title: sanitize_title(&info.title),
            width: info.width,
            height: info.height,
            primary: false,
        });
    }
    BOOL::from(true)
}

/// Gathers what the filter needs; `None` when the window vanished meanwhile.
fn describe(window: HWND) -> Option<WindowInfo> {
    // SAFETY: plain queries about a window handle that may be stale; each call
    // tolerates that and reports failure.
    unsafe {
        let visible = IsWindowVisible(window).as_bool();
        if !visible {
            return Some(WindowInfo::default());
        }
        let minimized = IsIconic(window).as_bool();
        let mut cloaked = 0u32;
        let cloaked = DwmGetWindowAttribute(
            window,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0;
        let style = GetWindowLongPtrW(window, GWL_EXSTYLE) as u32;
        let mut process = 0u32;
        GetWindowThreadProcessId(window, Some(&mut process));
        let length = GetWindowTextLengthW(window);
        let title = if length > 0 {
            // Titles beyond a few hundred units are cut by the sanitizer anyway.
            let mut buffer = vec![0u16; (length as usize + 1).min(1024)];
            let copied = GetWindowTextW(window, &mut buffer).max(0) as usize;
            String::from_utf16_lossy(&buffer[..copied.min(buffer.len())])
        } else {
            String::new()
        };
        let mut class = [0u16; 256];
        let copied = GetClassNameW(window, &mut class).max(0) as usize;
        let class = String::from_utf16_lossy(&class[..copied.min(class.len())]);
        let mut bounds = RECT::default();
        let framed = DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut bounds as *mut RECT).cast::<c_void>(),
            size_of::<RECT>() as u32,
        )
        .is_ok();
        if !framed && GetWindowRect(window, &mut bounds).is_err() {
            return None;
        }
        Some(WindowInfo {
            visible,
            minimized,
            cloaked,
            own_process: process == GetCurrentProcessId(),
            tool_window: style & WS_EX_TOOLWINDOW.0 != 0,
            app_window: style & WS_EX_APPWINDOW.0 != 0,
            title,
            class,
            width: (bounds.right - bounds.left).max(0) as u32,
            height: (bounds.bottom - bounds.top).max(0) as u32,
        })
    }
}

/// Whether `window` still names a window.
pub(super) fn window_exists(window: HWND) -> bool {
    // SAFETY: a plain handle query.
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(window)) }.as_bool()
}

/// Whether the window or display `source` still exists. The capture item's
/// own closed event is the primary signal; this catches a source that went
/// away without raising it.
pub(super) fn source_alive(source: SourceId) -> bool {
    match source.kind() {
        SourceKind::Window => window_exists(HWND(source.raw() as usize as *mut c_void)),
        SourceKind::Display => {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            // SAFETY: a plain query with a valid structure of its declared
            // size; a stale monitor handle makes it fail.
            unsafe {
                GetMonitorInfoW(
                    HMONITOR(source.raw() as usize as *mut c_void),
                    (&mut info as *mut MONITORINFOEXW).cast::<MONITORINFO>(),
                )
            }
            .as_bool()
        }
    }
}

/// Maps the OS status for a bad window or monitor handle to the stale-source
/// error, so a closed source reads as one rather than as a native failure.
pub(super) fn creation_error(
    operation: &'static str,
    error: &windows::core::Error,
) -> CaptureError {
    // E_INVALIDARG, ERROR_INVALID_WINDOW_HANDLE and ERROR_INVALID_MONITOR_HANDLE.
    const STALE: [u32; 3] = [0x8007_0057, 0x8007_0578, 0x8007_05B5];
    if STALE.contains(&(error.code().0 as u32)) {
        return CaptureError::SourceClosed;
    }
    native_error(operation, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> WindowInfo {
        WindowInfo {
            visible: true,
            title: "Document - Editor".to_owned(),
            class: "Editor".to_owned(),
            width: 800,
            height: 600,
            ..WindowInfo::default()
        }
    }

    #[test]
    fn ordinary_visible_titled_windows_are_offered() {
        assert!(shareable(&window()));
        // A tool window that asks for a taskbar button is a real window.
        assert!(shareable(&WindowInfo {
            tool_window: true,
            app_window: true,
            ..window()
        }));
    }

    #[test]
    fn furniture_hidden_suspended_and_own_windows_are_not() {
        for hidden in [
            WindowInfo {
                visible: false,
                ..window()
            },
            WindowInfo {
                minimized: true,
                ..window()
            },
            WindowInfo {
                cloaked: true,
                ..window()
            },
            WindowInfo {
                own_process: true,
                ..window()
            },
            WindowInfo {
                tool_window: true,
                ..window()
            },
            WindowInfo {
                title: "  \t".to_owned(),
                ..window()
            },
            WindowInfo {
                class: "Progman".to_owned(),
                title: "Program Manager".to_owned(),
                ..window()
            },
            WindowInfo {
                width: 47,
                ..window()
            },
            WindowInfo {
                height: 10,
                ..window()
            },
        ] {
            assert!(!shareable(&hidden), "{hidden:?}");
        }
    }

    #[test]
    fn titles_are_cleaned_and_bounded() {
        assert_eq!(sanitize_title("  a\tb\r\nc  "), "a b c");
        assert_eq!(sanitize_title("\u{0007}bell\u{0000}"), "bell");
        assert_eq!(sanitize_title("   "), "");
        let long = "é".repeat(500);
        assert_eq!(sanitize_title(&long).chars().count(), MAX_TITLE_CHARS);
        // Cutting never splits a scalar value.
        assert!(sanitize_title(&long).is_char_boundary(sanitize_title(&long).len()));
    }

    #[test]
    fn display_labels_name_the_screen_and_its_size() {
        assert_eq!(
            display_label(1, 2560, 1440, true),
            "Screen 1 - 2560x1440 (primary)"
        );
        assert_eq!(display_label(2, 1920, 1080, false), "Screen 2 - 1920x1080");
    }

    #[test]
    fn stale_handles_read_as_a_closed_source() {
        use windows::core::HRESULT;
        for code in [0x8007_0057u32, 0x8007_0578, 0x8007_05B5] {
            let error = windows::core::Error::from_hresult(HRESULT(code as i32));
            assert_eq!(
                creation_error("CreateForWindow", &error),
                CaptureError::SourceClosed
            );
        }
        let error = windows::core::Error::from_hresult(HRESULT(0x8000_4005u32 as i32));
        assert_eq!(
            creation_error("CreateForWindow", &error),
            CaptureError::Native {
                operation: "CreateForWindow",
                code: 0x8000_4005u32 as i32,
            }
        );
    }
}
