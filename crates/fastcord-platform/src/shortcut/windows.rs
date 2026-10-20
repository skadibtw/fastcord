//! The Windows listener: one `WH_KEYBOARD_LL` hook on its own thread.
//!
//! The thread blocks in `GetMessageW`; the hook runs on it only when a key moves and
//! only compares the key with the configured bindings. Keys are passed on unchanged and
//! nothing is stored but the held state of the configured bindings. While a binding is
//! held a 250 ms timer asks the OS whether its key is still down (the key-up can be lost
//! to a secure-desktop switch or a lock); it runs only during a press.
//!
//! Unsafe code is confined to this module: it is the Win32 FFI.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::sync::mpsc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, KillTimer, MSG, PostThreadMessageW,
    SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
    WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
};

use super::{Key, Matcher, ShortcutError, ShortcutEvent};

const RECHECK_MS: u32 = 250;

type Sink = Box<dyn FnMut(ShortcutEvent) + Send>;

struct State {
    matcher: Matcher,
    sink: Sink,
    timer: usize,
}

thread_local! {
    /// Owned by the hook thread; the hook procedure and the message loop both run on it.
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub(super) struct Hook {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Hook {
    pub(super) fn start(matcher: Matcher, sink: Sink) -> Result<Self, ShortcutError> {
        let (ready, installed) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("fastcord-shortcuts".into())
            .spawn(move || run(matcher, sink, ready))
            .map_err(|_| ShortcutError::Failed)?;
        match installed.recv() {
            Ok(Some(thread_id)) => Ok(Self {
                thread_id,
                thread: Some(thread),
            }),
            _ => {
                let _ = thread.join();
                Err(ShortcutError::Failed)
            }
        }
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        // SAFETY: posts a plain message to the hook thread, which is still joined below.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(matcher: Matcher, sink: Sink, ready: mpsc::SyncSender<Option<u32>>) {
    // SAFETY: both calls only read process state; the hook procedure is a valid
    // `extern "system"` function that lives for the whole process.
    let hook = unsafe {
        let module = GetModuleHandleW(None).ok().map(|m| m.into());
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(procedure), module, 0)
    };
    let Ok(hook) = hook else {
        let _ = ready.send(None);
        return;
    };
    STATE.with(|state| {
        *state.borrow_mut() = Some(State {
            matcher,
            sink,
            timer: 0,
        });
    });
    // SAFETY: reads the calling thread's id.
    let _ = ready.send(Some(unsafe { GetCurrentThreadId() }));
    let mut message = MSG::default();
    // SAFETY: `message` is a valid out-parameter; GetMessageW blocks until a message
    // arrives, so the thread uses no CPU while no key moves.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        if message.message == WM_TIMER {
            recheck();
        }
    }
    finish(hook);
}

/// Reports every still-held binding as cancelled, then removes the hook and timer.
fn finish(hook: HHOOK) {
    // SAFETY: `hook` was returned by `SetWindowsHookExW` on this thread.
    unsafe {
        let _ = UnhookWindowsHookEx(hook);
    }
    STATE.with(|state| {
        if let Some(mut state) = state.borrow_mut().take() {
            let State { matcher, sink, .. } = &mut state;
            matcher.cancel(&mut |event| sink(event));
            stop_timer(state.timer);
        }
    });
}

fn stop_timer(timer: usize) {
    if timer != 0 {
        // SAFETY: a thread timer created by `SetTimer` below.
        unsafe {
            let _ = KillTimer(None, timer);
        }
    }
}

/// Cancels held bindings whose key the OS no longer reports as down.
fn recheck() {
    STATE.with(|state| {
        if let Ok(mut guard) = state.try_borrow_mut()
            && let Some(State {
                matcher,
                sink,
                timer,
            }) = guard.as_mut()
        {
            // SAFETY: a plain query of the keyboard state.
            let down = |key: Key| unsafe { GetAsyncKeyState(i32::from(key.0)) } < 0;
            matcher.release_stale(down, &mut |event| sink(event));
            if !matcher.any_held() {
                stop_timer(*timer);
                *timer = 0;
            }
        }
    });
}

unsafe extern "system" fn procedure(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let message = wparam.0 as u32;
    let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
    let up = matches!(message, WM_KEYUP | WM_SYSKEYUP);
    if code == 0 && (down || up) {
        // SAFETY: for `HC_ACTION` on a low-level keyboard hook, `lparam` points to a
        // `KBDLLHOOKSTRUCT` valid for the duration of this call.
        let vk = unsafe { (*(lparam.0 as *const KBDLLHOOKSTRUCT)).vkCode } as u16;
        STATE.with(|state| {
            if let Ok(mut guard) = state.try_borrow_mut()
                && let Some(State {
                    matcher,
                    sink,
                    timer,
                }) = guard.as_mut()
            {
                matcher.key(Key(vk), down, &mut |event| sink(event));
                if matcher.any_held() && *timer == 0 {
                    // SAFETY: a thread timer; its WM_TIMER is read by this thread's loop.
                    *timer = unsafe { SetTimer(None, 0, RECHECK_MS, None) };
                } else if !matcher.any_held() {
                    stop_timer(*timer);
                    *timer = 0;
                }
            }
        });
    }
    // SAFETY: passes the unmodified arguments on, as the hook contract requires.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
