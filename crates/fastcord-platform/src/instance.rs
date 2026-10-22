//! The single-instance guard (issue #105). Two fastcord processes on one token
//! are never started: the first process holds a per-session named mutex and
//! owns a hidden message-only window; a later launch finds that window, hands
//! its command-line argument over with `WM_COPYDATA`, and tells the caller to
//! exit. The window thread blocks in `GetMessageW`, so the guard costs no idle
//! CPU. Where it cannot be built (a non-Windows target), [`acquire`] reports a
//! primary instance without a guard.

use std::thread::JoinHandle;

/// Longest forwarded argument, in bytes (a deep link is far shorter).
pub const MAX_ARG_BYTES: usize = 2048;
/// `COPYDATASTRUCT::dwData` tag of a fastcord hand-off ("FC" + version 1).
const TAG: usize = 0x4643_0001;

/// What a second launch asked the running instance to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Forwarded {
    /// Bring the window forward.
    Raise,
    /// Bring the window forward and open this link.
    Open(String),
}

/// The payload for a hand-off. An absent or empty argument means "raise". A
/// longer-than-allowed argument, or one with control characters, is refused:
/// the receiver would drop it anyway.
pub fn encode(arg: Option<&str>) -> Option<Vec<u8>> {
    let arg = arg.unwrap_or("");
    valid(arg.as_bytes()).then(|| arg.as_bytes().to_vec())
}

/// The request in a received message, or `None` for anything that is not an
/// exact fastcord hand-off (another program's message, oversize, not UTF-8).
pub fn decode(tag: usize, payload: &[u8]) -> Option<Forwarded> {
    if tag != TAG || !valid(payload) {
        return None;
    }
    let text = std::str::from_utf8(payload).ok()?;
    Some(if text.is_empty() {
        Forwarded::Raise
    } else {
        Forwarded::Open(text.to_owned())
    })
}

fn valid(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_ARG_BYTES
        && std::str::from_utf8(bytes).is_ok_and(|text| !text.chars().any(char::is_control))
}

/// The running instance's guard; keep it alive until the process exits.
pub struct Instance {
    #[cfg_attr(not(windows), allow(dead_code))]
    thread: Option<(u32, JoinHandle<()>)>,
}

pub enum Start {
    /// This process is the (only) instance; forwarded requests reach the sink.
    Primary(Instance),
    /// Another instance runs; `arg` was handed to it (or could not be). The
    /// caller must exit without starting a session.
    Secondary,
}

/// Becomes the primary instance or hands `arg` to the one that exists. The
/// sink runs on the guard's thread and must only enqueue.
pub fn acquire(arg: Option<&str>, sink: impl FnMut(Forwarded) + Send + 'static) -> Start {
    sys::acquire(arg, Box::new(sink))
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 mutex and window FFI, confined here.
mod sys {
    use std::cell::RefCell;
    use std::sync::mpsc;
    use std::time::Duration;

    use windows::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM,
    };
    use windows::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        ASFW_ANY, AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DestroyWindow,
        FindWindowExW, GetMessageW, HWND_MESSAGE, MSG, PostThreadMessageW, RegisterClassW,
        SMTO_ABORTIFHUNG, SendMessageTimeoutW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COPYDATA, WM_QUIT,
        WNDCLASSW,
    };
    use windows::core::{PCWSTR, w};

    use super::{Forwarded, Instance, Start, TAG, decode, encode};

    type Sink = Box<dyn FnMut(Forwarded) + Send>;

    const CLASS: PCWSTR = w!("FastcordSingleInstance");

    thread_local! {
        static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
    }

    pub fn acquire(arg: Option<&str>, sink: Sink) -> Start {
        // The mutex lives for the whole process: it is deliberately never closed.
        // SAFETY: plain named-mutex creation; the name is a NUL-terminated literal.
        let mutex = unsafe { CreateMutexW(None, false, w!(r"Local\fastcord-single-instance")) };
        // SAFETY: reads the calling thread's last error, set by the call above.
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if mutex.is_err() {
            // Without the mutex nothing proves we are alone; refuse to run a second session.
            return Start::Secondary;
        }
        if exists {
            forward(arg);
            return Start::Secondary;
        }
        let (ready, started) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("fastcord-instance".into())
            .spawn(move || run(sink, ready))
            .ok();
        let id = started.recv().ok().flatten();
        Start::Primary(Instance {
            thread: thread.zip(id).map(|(thread, id)| (id, thread)),
        })
    }

    /// Delivers `arg` to the primary's window, waiting briefly for a window that
    /// is still being created. A failure is not retried further: the caller
    /// exits either way, because starting a second session is the worse outcome.
    fn forward(arg: Option<&str>) {
        let Some(payload) = encode(arg) else { return };
        for _ in 0..20 {
            // SAFETY: looks up a top-level message-only window by class name.
            let found = unsafe { FindWindowExW(Some(HWND_MESSAGE), None, CLASS, None) };
            if let Ok(window) = found {
                let data = COPYDATASTRUCT {
                    dwData: TAG,
                    cbData: payload.len() as u32,
                    lpData: payload.as_ptr() as *mut _,
                };
                // SAFETY: `data` and `payload` outlive the synchronous send; the
                // receiver only reads them. The foreground right lets the primary
                // raise its window for this launch.
                unsafe {
                    let _ = AllowSetForegroundWindow(ASFW_ANY);
                    let _ = SendMessageTimeoutW(
                        window,
                        WM_COPYDATA,
                        WPARAM(0),
                        LPARAM(&data as *const _ as isize),
                        SMTO_ABORTIFHUNG,
                        2000,
                        None,
                    );
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn run(sink: Sink, ready: mpsc::SyncSender<Option<u32>>) {
        // SAFETY: registers a class whose procedure is a valid `extern "system"` fn
        // and creates a message-only window owned by this thread.
        let window = unsafe {
            let module = GetModuleHandleW(None).ok().map(Into::into);
            let class = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: module.unwrap_or_default(),
                lpszClassName: CLASS,
                ..Default::default()
            };
            RegisterClassW(&class);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!("fastcord"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                module,
                None,
            )
        };
        let Ok(window) = window else {
            let _ = ready.send(None);
            return;
        };
        SINK.with(|slot| *slot.borrow_mut() = Some(sink));
        // SAFETY: reads the calling thread's id.
        let _ = ready.send(Some(unsafe { GetCurrentThreadId() }));
        let mut message = MSG::default();
        // SAFETY: blocks until a message arrives: no CPU while nothing is forwarded.
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
            // SAFETY: the window procedure below handles every message of this window.
            unsafe { windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&message) };
        }
        // SAFETY: destroys the window created above on its own thread.
        let _ = unsafe { DestroyWindow(window) };
    }

    unsafe extern "system" fn procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_COPYDATA && lparam.0 != 0 {
            // SAFETY: for WM_COPYDATA `lparam` points to a COPYDATASTRUCT that is
            // valid for the duration of the call; the length is checked before use.
            let request = unsafe {
                let data = &*(lparam.0 as *const COPYDATASTRUCT);
                if data.cbData as usize <= super::MAX_ARG_BYTES
                    && (data.cbData == 0 || !data.lpData.is_null())
                {
                    let bytes = if data.cbData == 0 {
                        &[][..]
                    } else {
                        std::slice::from_raw_parts(data.lpData as *const u8, data.cbData as usize)
                    };
                    decode(data.dwData, bytes)
                } else {
                    None
                }
            };
            if let Some(request) = request {
                SINK.with(|sink| {
                    if let Ok(mut sink) = sink.try_borrow_mut()
                        && let Some(sink) = sink.as_mut()
                    {
                        sink(request);
                    }
                });
                return LRESULT(1);
            }
            return LRESULT(0);
        }
        // SAFETY: default handling of every other message.
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            if let Some((id, thread)) = self.thread.take() {
                // SAFETY: posts a plain quit to the guard thread, joined below.
                unsafe {
                    let _ = PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0));
                }
                let _ = thread.join();
            }
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use super::{Forwarded, Instance, Start};

    pub fn acquire(_: Option<&str>, _: Box<dyn FnMut(Forwarded) + Send>) -> Start {
        // ponytail: no guard off Windows yet (a lock file or D-Bus name per target).
        Start::Primary(Instance { thread: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handoff_round_trips_and_an_empty_one_raises_the_window() {
        let link = "discord://-/channels/1/2/3";
        let payload = encode(Some(link)).unwrap();
        assert_eq!(
            decode(TAG, &payload),
            Some(Forwarded::Open(link.to_owned()))
        );
        assert_eq!(decode(TAG, &encode(None).unwrap()), Some(Forwarded::Raise));
        assert_eq!(
            decode(TAG, &encode(Some("")).unwrap()),
            Some(Forwarded::Raise)
        );
    }

    #[test]
    fn foreign_oversize_and_malformed_messages_are_dropped() {
        assert_eq!(decode(TAG + 1, b"x"), None, "another program's tag");
        assert_eq!(decode(TAG, &[0xff, 0xfe]), None, "not UTF-8");
        assert_eq!(decode(TAG, b"a\nb"), None, "control character");
        let long = "a".repeat(MAX_ARG_BYTES + 1);
        assert_eq!(decode(TAG, long.as_bytes()), None);
        assert_eq!(encode(Some(&long)), None);
        assert!(encode(Some(&"a".repeat(MAX_ARG_BYTES))).is_some());
    }
}
