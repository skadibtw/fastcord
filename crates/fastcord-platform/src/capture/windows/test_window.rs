//! A real Win32 window with known pixels, for the hardware capture tests of
//! this crate and its dependents (behind the `test-support` feature, never in
//! a normal build). It lives on its own thread with its own message loop, so a
//! test thread can resize, freeze and close it while the capture runs.
//!
//! The client area is red on the left half and blue on the right half; a small
//! square in the top-left corner flips between green and white on a 60 Hz timer
//! so the compositor keeps producing frames. The process opts into per-monitor
//! DPI awareness first, so window sizes are physical pixels.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Once};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, HGDIOBJ, InvalidateRect,
    PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DispatchMessageW, GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW, KillTimer, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, SWP_NOACTIVATE, SWP_NOZORDER, SetTimer,
    SetWindowLongPtrW, SetWindowPos, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY,
    WM_NCCREATE, WM_PAINT, WM_SIZE, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use super::super::{SourceId, SourceKind};

static REGISTER: Once = Once::new();
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// RGB of the left half of the client area.
pub const LEFT_RGB: [u8; 3] = [255, 0, 0];
/// RGB of the right half of the client area.
pub const RIGHT_RGB: [u8; 3] = [0, 0, 255];

struct State {
    animated: AtomicBool,
    flip: AtomicBool,
}

pub struct TestWindow {
    hwnd: isize,
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
}

impl TestWindow {
    /// Opens the window with a `client_width` × `client_height` client area at
    /// (`left`, `top`), animating.
    pub fn open(client_width: i32, client_height: i32, left: i32, top: i32) -> Self {
        // Already set (or refused) is fine: only the first call can change it.
        // SAFETY: a plain process-wide setting.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let state = Arc::new(State {
            animated: AtomicBool::new(true),
            flip: AtomicBool::new(false),
        });
        let (ready_tx, ready) = mpsc::channel();
        let thread_state = Arc::clone(&state);
        let title = format!(
            "fastcord capture test {} {}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let thread = thread::Builder::new()
            .name("fastcord-test-window".to_owned())
            .spawn(move || {
                run(
                    thread_state,
                    &title,
                    client_width,
                    client_height,
                    left,
                    top,
                    ready_tx,
                )
            })
            .expect("the test window thread starts");
        let hwnd = ready.recv().expect("the test window was created");
        Self {
            hwnd,
            state,
            thread: Some(thread),
        }
    }

    pub fn id(&self) -> SourceId {
        SourceId::from_raw(SourceKind::Window, self.hwnd as usize as u64)
    }

    pub fn raw(&self) -> HWND {
        HWND(self.hwnd as usize as *mut c_void)
    }

    /// Resizes the client area (keeping the position).
    pub fn resize(&self, client_width: i32, client_height: i32) {
        let outer = outer_size(client_width, client_height);
        // SAFETY: a plain cross-thread window query on a live window.
        let _ = unsafe {
            SetWindowPos(
                self.raw(),
                None,
                0,
                0,
                outer.0,
                outer.1,
                SWP_NOZORDER | SWP_NOACTIVATE | windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE,
            )
        };
    }

    /// Starts or stops the 60 Hz corner animation (a frozen window produces
    /// no new frames at all).
    pub fn set_animated(&self, animated: bool) {
        self.state.animated.store(animated, Ordering::Relaxed);
    }

    /// Closes the window and joins its thread.
    pub fn close(&mut self) {
        // SAFETY: posting a message to a window handle is always allowed; a
        // destroyed window just rejects it.
        let _ = unsafe { PostMessageW(Some(self.raw()), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        self.close();
    }
}

fn outer_size(client_width: i32, client_height: i32) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: client_width,
        bottom: client_height,
    };
    // SAFETY: a plain computation on a valid rectangle.
    let _ = unsafe { AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false) };
    (rect.right - rect.left, rect.bottom - rect.top)
}

fn run(
    state: Arc<State>,
    title: &str,
    client_width: i32,
    client_height: i32,
    left: i32,
    top: i32,
    ready: mpsc::Sender<isize>,
) {
    // SAFETY: standard window class registration and creation on this thread;
    // `state` outlives the window because the owner joins this thread before
    // dropping it, and the window procedure only reads it.
    unsafe {
        let module = GetModuleHandleW(None).expect("the module handle");
        let instance = HINSTANCE(module.0);
        let class = w!("FastcordCaptureTestWindow");
        REGISTER.call_once(|| {
            let window_class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&window_class);
        });
        let (outer_width, outer_height) = outer_size(client_width, client_height);
        let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            if left == i32::MIN {
                CW_USEDEFAULT
            } else {
                left
            },
            if top == i32::MIN { CW_USEDEFAULT } else { top },
            outer_width,
            outer_height,
            None,
            None,
            Some(instance),
            Some(Arc::as_ptr(&state).cast::<c_void>()),
        )
        .expect("the test window is created");
        SetTimer(Some(hwnd), 1, 16, None);
        let _ = ready.send(hwnd.0 as usize as isize);
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the user data is the `State` pointer passed at creation, valid
    // until the window thread ends; messages arrive only on that thread.
    unsafe {
        if message == WM_NCCREATE {
            let create = lparam.0 as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*create).lpCreateParams as isize);
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State;
        match message {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let middle = client.left + (client.right - client.left) / 2;
                let fill = |rect: RECT, [r, g, b]: [u8; 3]| {
                    let brush = CreateSolidBrush(COLORREF(
                        u32::from(r) | u32::from(g) << 8 | u32::from(b) << 16,
                    ));
                    FillRect(dc, &rect, brush);
                    let _ = DeleteObject(HGDIOBJ(brush.0));
                };
                fill(
                    RECT {
                        right: middle,
                        ..client
                    },
                    LEFT_RGB,
                );
                fill(
                    RECT {
                        left: middle,
                        ..client
                    },
                    RIGHT_RGB,
                );
                let flipped = !state.is_null() && (*state).flip.load(Ordering::Relaxed);
                fill(
                    RECT {
                        left: 8,
                        top: 8,
                        right: 40,
                        bottom: 40,
                    },
                    if flipped {
                        [0, 255, 0]
                    } else {
                        [255, 255, 255]
                    },
                );
                let _ = EndPaint(hwnd, &paint);
                LRESULT(0)
            }
            WM_TIMER => {
                if !state.is_null() && (*state).animated.load(Ordering::Relaxed) {
                    (*state).flip.fetch_xor(true, Ordering::Relaxed);
                    let corner = RECT {
                        left: 8,
                        top: 8,
                        right: 40,
                        bottom: 40,
                    };
                    let _ = InvalidateRect(Some(hwnd), Some(&corner), false);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                DefWindowProcW(hwnd, message, wparam, lparam)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), 1);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }
}
