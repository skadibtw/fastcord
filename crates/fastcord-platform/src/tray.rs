//! The notification-area icon (issue #105). Windows runs `Shell_NotifyIcon` on
//! its own thread with a hidden window, like the shortcut hook: it blocks in
//! `GetMessageW`, so it uses no CPU while nothing happens. Linux publishes a
//! StatusNotifierItem on the session bus (`tray/linux.rs`), macOS an
//! `NSStatusItem` in the menu bar (`tray/macos.rs`). The icon's menu and
//! tooltip follow a small [`State`] pushed with [`set_state`]; clicks come back
//! as [`Command`]s through the sink given to [`Tray::start`]. Elsewhere
//! [`TrayError::Unsupported`].

use std::fmt;
use std::sync::Mutex;

/// What a click on the icon or its menu asks the application to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Open,
    ToggleMute,
    Disconnect,
    Quit,
}

#[cfg_attr(
    not(any(windows, target_os = "linux", target_os = "macos")),
    allow(dead_code)
)]
impl Command {
    fn id(self) -> usize {
        self as usize + 1
    }

    fn from_id(id: usize) -> Option<Self> {
        [Self::Open, Self::ToggleMute, Self::Disconnect, Self::Quit]
            .into_iter()
            .find(|command| command.id() == id)
    }
}

/// What the icon shows: whether a call is connected, the microphone, and the
/// unread mention count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub in_call: bool,
    pub muted: bool,
    pub mentions: u32,
}

/// The tooltip text (Windows cuts it at 127 characters; this is far shorter).
pub fn tooltip(state: State) -> String {
    match state.mentions {
        0 => "fastcord".to_owned(),
        1 => "fastcord - 1 unread mention".to_owned(),
        n => format!("fastcord - {n} unread mentions"),
    }
}

/// The menu entries for `state`: the microphone and Disconnect exist only in a call.
pub fn menu(state: State) -> Vec<(Command, &'static str)> {
    let mut items = vec![(Command::Open, "Open fastcord")];
    if state.in_call {
        items.push((
            Command::ToggleMute,
            if state.muted {
                "Unmute microphone"
            } else {
                "Mute microphone"
            },
        ));
        items.push((Command::Disconnect, "Disconnect"));
    }
    items.push((Command::Quit, "Quit"));
    items
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayError {
    Unsupported,
    /// The desktop runs no tray host (Linux without a StatusNotifierWatcher).
    NoHost,
    Failed,
}

impl fmt::Display for TrayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "The tray icon is not supported on this operating system.",
            Self::NoHost => {
                "This desktop shows no tray icons (on GNOME, the AppIndicator extension adds them)."
            }
            Self::Failed => "The operating system refused the tray icon.",
        })
    }
}

impl std::error::Error for TrayError {}

static STATE: Mutex<State> = Mutex::new(State {
    in_call: false,
    muted: false,
    mentions: 0,
});

/// Publishes the state the icon shows; the icon updates only when it changed.
pub fn set_state(state: State) {
    let Ok(mut current) = STATE.lock() else {
        return;
    };
    if *current != state {
        *current = state;
        sys::refresh();
    }
}

#[cfg_attr(
    not(any(windows, target_os = "linux", target_os = "macos")),
    allow(dead_code)
)]
fn state() -> State {
    STATE.lock().map(|s| *s).unwrap_or_default()
}

/// The data of the ICO entry that is `size` pixels wide (the first if none is),
/// as `CreateIconFromResourceEx` takes it (PNG or DIB bytes of one image).
#[cfg_attr(
    not(any(windows, target_os = "linux", target_os = "macos")),
    allow(dead_code)
)]
fn ico_entry(ico: &[u8], size: u8) -> Option<&[u8]> {
    let count = usize::from(u16::from_le_bytes([*ico.get(4)?, *ico.get(5)?]));
    let entry = |n: usize| -> Option<(u8, &[u8])> {
        let e = ico.get(6 + 16 * n..22 + 16 * n)?;
        let len = u32::from_le_bytes(e[8..12].try_into().ok()?) as usize;
        let at = u32::from_le_bytes(e[12..16].try_into().ok()?) as usize;
        Some((e[0], ico.get(at..at.checked_add(len)?)?))
    };
    (0..count)
        .filter_map(entry)
        .find(|(width, _)| *width == size)
        .or_else(|| entry(0))
        .map(|(_, data)| data)
}

/// The running icon; dropping it removes the icon and ends its thread.
pub struct Tray {
    #[allow(dead_code)] // held for its Drop, which removes the icon
    inner: sys::Handle,
}

impl fmt::Debug for Tray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Tray")
    }
}

impl Tray {
    /// Adds the icon. The sink runs on the icon's thread and must only enqueue.
    pub fn start(sink: impl FnMut(Command) + Send + 'static) -> Result<Self, TrayError> {
        sys::start(Box::new(sink)).map(|inner| Self { inner })
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 shell and window FFI, confined here.
mod sys {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
        Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
        HICON, LR_DEFAULTCOLOR, MF_STRING, MSG, PostMessageW, PostThreadMessageW, RegisterClassW,
        RegisterWindowMessageW, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON,
        TrackPopupMenu, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_LBUTTONUP, WM_NULL, WM_QUIT,
        WM_RBUTTONUP, WNDCLASSW,
    };
    use windows::core::{HSTRING, PCWSTR, w};

    use super::{Command, TrayError, ico_entry, menu, state, tooltip};

    type Sink = Box<dyn FnMut(Command) + Send>;

    const CLASS: PCWSTR = w!("FastcordTray");
    /// The shell's callback for clicks on the icon.
    const WM_TRAY: u32 = WM_APP + 1;
    /// Posted by `set_state`: refresh the tooltip.
    const WM_REFRESH: u32 = WM_APP + 2;
    const ICON: &[u8] = include_bytes!("../../../assets/icons/app/fastcord.ico");

    /// The icon window, for `set_state` (0 while no icon runs).
    static WINDOW: AtomicIsize = AtomicIsize::new(0);

    thread_local! {
        static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
        static ICON_HANDLE: RefCell<HICON> = RefCell::new(HICON::default());
        static RECREATE: RefCell<u32> = const { RefCell::new(0) };
    }

    pub struct Handle {
        thread_id: u32,
        thread: Option<JoinHandle<()>>,
    }

    pub fn refresh() {
        let window = WINDOW.load(Ordering::Acquire);
        if window != 0 {
            // SAFETY: posts a plain message to the icon window; a stale handle just fails.
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(window as *mut _)),
                    WM_REFRESH,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }

    pub fn start(sink: Sink) -> Result<Handle, TrayError> {
        let (ready, started) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("fastcord-tray".into())
            .spawn(move || run(sink, ready))
            .map_err(|_| TrayError::Failed)?;
        match started.recv() {
            Ok(Some(thread_id)) => Ok(Handle {
                thread_id,
                thread: Some(thread),
            }),
            _ => {
                let _ = thread.join();
                Err(TrayError::Failed)
            }
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: posts a plain quit to the icon thread, joined below.
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn data(window: HWND, icon: HICON) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: icon,
            ..Default::default()
        };
        for (slot, unit) in data
            .szTip
            .iter_mut()
            .take(127)
            .zip(tooltip(state()).encode_utf16())
        {
            *slot = unit;
        }
        data
    }

    fn run(sink: Sink, ready: mpsc::SyncSender<Option<u32>>) {
        // SAFETY: registers a class with a valid window procedure, creates a hidden
        // top-level window (not message-only, so it receives `TaskbarCreated`) and
        // the icon from the embedded PNG entry; all owned by this thread.
        let created = unsafe {
            let module = GetModuleHandleW(None).ok().map(Into::into);
            let class = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: module.unwrap_or_default(),
                lpszClassName: CLASS,
                ..Default::default()
            };
            RegisterClassW(&class);
            let window = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!("fastcord tray"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                module,
                None,
            );
            let icon = ico_entry(ICON, 32).and_then(|bytes| {
                CreateIconFromResourceEx(bytes, true, 0x0003_0000, 32, 32, LR_DEFAULTCOLOR).ok()
            });
            window.ok().zip(icon)
        };
        let Some((window, icon)) = created else {
            let _ = ready.send(None);
            return;
        };
        // SAFETY: adds the icon for the window above.
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data(window, icon)) }.as_bool() {
            // SAFETY: releases what was created above.
            unsafe {
                let _ = DestroyIcon(icon);
                let _ = DestroyWindow(window);
            }
            let _ = ready.send(None);
            return;
        }
        SINK.with(|slot| *slot.borrow_mut() = Some(sink));
        ICON_HANDLE.with(|slot| *slot.borrow_mut() = icon);
        // SAFETY: registers the broadcast sent when Explorer restarts.
        RECREATE.with(|slot| {
            *slot.borrow_mut() = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
        });
        WINDOW.store(window.0 as isize, Ordering::Release);
        // SAFETY: reads the calling thread's id.
        let _ = ready.send(Some(unsafe { GetCurrentThreadId() }));
        let mut message = MSG::default();
        // SAFETY: blocks until a message arrives; the window procedure handles them.
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
            // SAFETY: dispatches to this thread's window procedure.
            unsafe { DispatchMessageW(&message) };
        }
        WINDOW.store(0, Ordering::Release);
        // SAFETY: removes the icon and releases the handles created above.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data(window, icon));
            let _ = DestroyIcon(icon);
            let _ = DestroyWindow(window);
        }
    }

    fn send(command: Command) {
        SINK.with(|sink| {
            if let Ok(mut sink) = sink.try_borrow_mut()
                && let Some(sink) = sink.as_mut()
            {
                sink(command);
            }
        });
    }

    fn popup(window: HWND) {
        // SAFETY: builds, shows and destroys a popup menu on this thread; the window
        // must be foreground for the menu to dismiss correctly.
        unsafe {
            let Ok(menu_handle) = CreatePopupMenu() else {
                return;
            };
            for (command, label) in menu(state()) {
                let _ = AppendMenuW(menu_handle, MF_STRING, command.id(), &HSTRING::from(label));
            }
            let mut at = POINT::default();
            let _ = GetCursorPos(&mut at);
            let _ = SetForegroundWindow(window);
            let chosen = TrackPopupMenu(
                menu_handle,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                at.x,
                at.y,
                None,
                window,
                None,
            );
            let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu_handle);
            if let Some(command) = Command::from_id(chosen.0 as usize) {
                send(command);
            }
        }
    }

    unsafe extern "system" fn procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_TRAY => match (lparam.0 & 0xffff) as u32 {
                WM_LBUTTONUP => send(Command::Open),
                WM_RBUTTONUP => popup(window),
                _ => {}
            },
            WM_REFRESH => {
                let icon = ICON_HANDLE.with(|slot| *slot.borrow());
                // SAFETY: modifies the icon added for this window.
                let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data(window, icon)) };
            }
            _ if message != 0 && RECREATE.with(|slot| *slot.borrow()) == message => {
                // Explorer restarted: the icon is gone, add it again.
                let icon = ICON_HANDLE.with(|slot| *slot.borrow());
                // SAFETY: re-adds the icon for this window.
                let _ = unsafe { Shell_NotifyIconW(NIM_ADD, &data(window, icon)) };
            }
            _ => {
                // SAFETY: default handling of every other message.
                return unsafe { DefWindowProcW(window, message, wparam, lparam) };
            }
        }
        LRESULT(0)
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as sys;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod sys {
    use super::{Command, TrayError};

    pub struct Handle;

    pub fn refresh() {}

    pub fn start(_: Box<dyn FnMut(Command) + Send>) -> Result<Handle, TrayError> {
        Err(TrayError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_has_microphone_and_disconnect_only_in_a_call() {
        let idle: Vec<_> = menu(State::default()).into_iter().map(|(c, _)| c).collect();
        assert_eq!(idle, [Command::Open, Command::Quit]);
        let call = State {
            in_call: true,
            ..State::default()
        };
        let labels: Vec<_> = menu(call).into_iter().map(|(_, l)| l).collect();
        assert_eq!(
            labels,
            ["Open fastcord", "Mute microphone", "Disconnect", "Quit"]
        );
        let muted = State {
            muted: true,
            ..call
        };
        assert_eq!(menu(muted)[1], (Command::ToggleMute, "Unmute microphone"));
    }

    #[test]
    fn menu_ids_map_back_to_commands_and_zero_is_a_dismissed_menu() {
        for (command, _) in menu(State {
            in_call: true,
            ..State::default()
        }) {
            assert_eq!(Command::from_id(command.id()), Some(command));
        }
        assert_eq!(Command::from_id(0), None);
        assert_eq!(Command::from_id(99), None);
    }

    #[test]
    fn the_tooltip_shows_the_unread_mention_count() {
        let state = |mentions| State {
            mentions,
            ..State::default()
        };
        assert_eq!(tooltip(state(0)), "fastcord");
        assert_eq!(tooltip(state(1)), "fastcord - 1 unread mention");
        assert_eq!(tooltip(state(12)), "fastcord - 12 unread mentions");
    }

    #[test]
    fn the_committed_icon_has_a_32_pixel_entry() {
        let ico = include_bytes!("../../../assets/icons/app/fastcord.ico");
        let entry = ico_entry(ico, 32).unwrap();
        assert!(entry.starts_with(b"\x89PNG"));
        assert_ne!(entry.len(), ico_entry(ico, 16).unwrap().len());
        assert_eq!(ico_entry(&ico[..8], 32), None, "truncated");
    }
}
