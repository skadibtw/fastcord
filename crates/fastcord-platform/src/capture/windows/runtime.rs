//! The native owner thread, the Win32 events it waits on, the WinRT apartment
//! guard and the machine's monotonic clock.

use std::any::Any;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, LazyLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, HANDLE, RO_E_CLOSED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{Error, PCWSTR};

use super::super::{CaptureError, CaptureState, CaptureStatus};

/// How long stopping waits for the owner thread to release its native objects
/// before giving up on joining it (a hung GPU driver must not hang the app).
pub(super) const STOP_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) trait OrCapture<T> {
    fn capture(self, operation: &'static str) -> Result<T, CaptureError>;
}

impl<T> OrCapture<T> for windows::core::Result<T> {
    fn capture(self, operation: &'static str) -> Result<T, CaptureError> {
        self.map_err(|error| native_error(operation, &error))
    }
}

pub(super) fn native_error(operation: &'static str, error: &Error) -> CaptureError {
    match error.code() {
        E_ACCESSDENIED => CaptureError::PermissionDenied,
        // The capture item or session was closed under us.
        RO_E_CLOSED => CaptureError::SourceClosed,
        code => CaptureError::Native {
            operation,
            code: code.0,
        },
    }
}

/// An owned kernel handle. Sharing does not transfer ownership; the `Arc` keeps
/// it alive until the last native waiter or callback is gone.
pub(super) struct Handle(pub HANDLE);

// SAFETY: kernel event handles are valid across threads, and `CloseHandle`
// runs only after every `Arc` reference has been released.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns one successful Win32 handle creation.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(super) fn event(manual_reset: bool) -> Result<Arc<Handle>, CaptureError> {
    // SAFETY: an unnamed event with default security; the returned handle is
    // owned by `Handle` and stays alive throughout each wait.
    unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) }
        .map(|handle| Arc::new(Handle(handle)))
        .capture("CreateEventW")
}

pub(super) fn signal(event: &Handle) -> Result<(), CaptureError> {
    // SAFETY: the borrowed owned event outlives the call.
    unsafe { SetEvent(event.0) }.capture("SetEvent")
}

/// Waits for any of `handles`; returns its index (the lowest signaled one), or
/// `None` on timeout.
pub(super) fn wait(handles: &[HANDLE], timeout: Duration) -> Result<Option<usize>, CaptureError> {
    let milliseconds = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    // SAFETY: callers hold every handle through this bounded wait.
    let result = unsafe { WaitForMultipleObjects(handles, false, milliseconds) };
    if result == WAIT_TIMEOUT {
        return Ok(None);
    }
    if result == WAIT_FAILED {
        return Err(native_error(
            "WaitForMultipleObjects",
            &Error::from_thread(),
        ));
    }
    let index = result.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
    if index >= handles.len() {
        return Err(CaptureError::InvalidSample(
            "unexpected native capture wait result",
        ));
    }
    Ok(Some(index))
}

/// Keeps the process's multithreaded apartment alive once WinRT has been used.
///
/// The `windows` crate caches WinRT activation factories in statics. Without
/// this, the apartment would be torn down when the last capture thread ended,
/// and the next session would call into factories of a dead apartment (an
/// access violation on the second share). The usage count is never released.
static MTA_USAGE: LazyLock<Result<(), CaptureError>> = LazyLock::new(|| {
    // SAFETY: a plain process-wide reference count on the MTA.
    unsafe { CoIncrementMTAUsage() }
        .map(|_cookie| ())
        .capture("CoIncrementMTAUsage")
});

/// WinRT initialized for this thread's lifetime. Not `Send`: it must be
/// dropped on the thread that created it.
pub(super) struct Apartment {
    _owner: PhantomData<Rc<()>>,
}

impl Apartment {
    /// The multithreaded apartment, which Media Foundation's asynchronous
    /// transforms also require; an existing incompatible apartment is an error.
    pub(super) fn mta() -> Result<Self, CaptureError> {
        MTA_USAGE.clone()?;
        // SAFETY: initializes this thread; the `!Send` guard balances every
        // successful call on the same thread.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.capture("RoInitialize")?;
        Ok(Self {
            _owner: PhantomData,
        })
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances this guard's successful `RoInitialize`.
        unsafe { RoUninitialize() };
    }
}

/// The owner thread of one capture session.
pub(in crate::capture) struct Native {
    stop: Arc<Handle>,
    poke: Arc<Handle>,
    done: mpsc::Receiver<Result<(), CaptureError>>,
    thread: Option<JoinHandle<()>>,
}

impl Native {
    /// Runs `run` on a new thread named `name`. `run` receives the stop event
    /// (manual reset: once signaled it stays signaled) and the wake event
    /// (auto reset). Whatever it returns, and a panic, becomes the terminal
    /// state, published after `run` has released everything it owned.
    pub(super) fn spawn(
        name: &'static str,
        status: CaptureStatus,
        run: impl FnOnce(&Arc<Handle>, &Arc<Handle>) -> Result<(), CaptureError> + Send + 'static,
    ) -> Result<Self, CaptureError> {
        let stop = event(true)?;
        let poke = event(false)?;
        let (done_tx, done) = mpsc::sync_channel(1);
        let (thread_stop, thread_poke) = (Arc::clone(&stop), Arc::clone(&poke));
        let thread = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| run(&thread_stop, &thread_poke)))
                    .unwrap_or_else(|panic| Err(panicked(&*panic)));
                status.set(match &result {
                    Ok(()) => CaptureState::Stopped,
                    Err(error) => CaptureState::Failed(error.clone()),
                });
                let _ = done_tx.send(result);
            })
            .map_err(|_| CaptureError::Worker("could not create the capture thread"))?;
        Ok(Self {
            stop,
            poke,
            done,
            thread: Some(thread),
        })
    }

    /// A closure that wakes the thread from any other thread.
    pub(in crate::capture) fn waker(&self) -> Box<dyn Fn() + Send + Sync> {
        let poke = Arc::clone(&self.poke);
        Box::new(move || {
            let _ = signal(&poke);
        })
    }

    /// Stops the thread and returns its terminal result.
    pub(in crate::capture) fn stop(mut self) -> Result<(), CaptureError> {
        self.stop_within(STOP_TIMEOUT)
    }

    pub(super) fn stop_within(&mut self, timeout: Duration) -> Result<(), CaptureError> {
        let _ = signal(&self.stop);
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        match self.done.recv_timeout(timeout) {
            Ok(result) => {
                let _ = thread.join();
                result
            }
            // Detach rather than hang: the thread frees what it owns if it
            // ever returns.
            Err(RecvTimeoutError::Timeout) => Err(CaptureError::Worker(
                "the capture thread did not stop in time",
            )),
            Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                Err(CaptureError::Worker("the capture thread ended abnormally"))
            }
        }
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        let _ = self.stop_within(STOP_TIMEOUT);
    }
}

fn panicked(panic: &(dyn Any + Send)) -> CaptureError {
    // The payload may carry file paths or user data; only its presence matters.
    let _ = panic;
    CaptureError::Worker("the capture thread panicked")
}

/// Ticks per second of the performance counter, fixed at boot. Windows XP and
/// later never fail the query, so a zero is only an unreachable sentinel.
static FREQUENCY: LazyLock<i64> = LazyLock::new(|| {
    let mut frequency = 0i64;
    // SAFETY: the output pointer refers to an initialized i64.
    let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
    frequency
});

/// QPC-derived time since boot: the clock of WGC's `SystemRelativeTime`, not
/// time since this session opened.
pub(super) fn monotonic_time() -> Result<Duration, CaptureError> {
    let frequency = *FREQUENCY;
    let mut ticks = 0i64;
    // SAFETY: the output pointer refers to an initialized i64.
    unsafe { QueryPerformanceCounter(&mut ticks) }.capture("QueryPerformanceCounter")?;
    if ticks < 0 || frequency <= 0 {
        return Err(CaptureError::InvalidSample(
            "invalid native monotonic clock",
        ));
    }
    let nanos = ticks as u128 * 1_000_000_000 / frequency as u128;
    u64::try_from(nanos)
        .map(Duration::from_nanos)
        .map_err(|_| CaptureError::InvalidSample("native timestamp overflow"))
}

/// 100-nanosecond units (WinRT `TimeSpan`) as a `Duration`.
pub(super) fn from_hns(time: i64) -> Duration {
    Duration::from_nanos(u64::try_from(time).unwrap_or(0).saturating_mul(100))
}
