//! Audio a screen share carries (SPEC §8.4, §8.7; milestone 35): WASAPI
//! loopback on Windows, delivered as 48 kHz interleaved stereo `f32`.
//!
//! One [`Loopback`] owns one dedicated native thread. Everything that touches
//! the operating system's audio objects, and the handler, runs there; the
//! handler is the real-time callback of this path and must not block,
//! allocate, lock or do I/O (it moves samples into a preallocated ring).
//! Dropping the [`Loopback`] stops and joins the thread.
//!
//! Windows has two mechanisms:
//!
//! - **Process loopback** (build 20348 and later) records what one application
//!   and its child processes render ([`LoopbackScope::Application`]) or
//!   everything except fastcord itself
//!   ([`LoopbackScope::SystemExcludingFastcord`]), so the call is never
//!   retransmitted.
//! - **Classic loopback** records everything the default output device plays,
//!   the call included ([`LoopbackScope::SystemIncludingFastcord`]); the UI
//!   must call it the broad one.
//!
//! Elsewhere every entry point reports [`LoopbackError::Unsupported`] (SPEC
//! milestones 36 and 37).

use std::fmt;

use crate::capture::{SourceId, SourceKind};

#[cfg(not(windows))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
use unsupported as native;
#[cfg(windows)]
use windows as native;

/// Sample rate and channel count of every block the handler receives.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

/// What a capture records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopbackScope {
    /// One application and the processes it started.
    Application { process_id: u32 },
    /// Everything the system plays except fastcord's own output.
    SystemExcludingFastcord,
    /// Everything the default output plays, fastcord's call included.
    SystemIncludingFastcord,
}

impl LoopbackScope {
    /// Whether viewers could hear the call through this capture.
    pub fn includes_fastcord(self) -> bool {
        matches!(self, Self::SystemIncludingFastcord)
    }
}

/// Which mechanisms this machine offers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopbackSupport {
    pub process_loopback: bool,
    pub system_loopback: bool,
}

/// Cheap; never opens a device.
pub fn support() -> LoopbackSupport {
    native::support()
}

/// The scope a share of `source` captures: a window's own application, a
/// display's everything-but-fastcord and, where the OS cannot exclude
/// fastcord, everything. `None` when this machine cannot capture audio.
pub fn scope_for(source: SourceId, support: LoopbackSupport) -> Option<LoopbackScope> {
    if support.process_loopback {
        return Some(match (source.kind(), native::window_process(source)) {
            (SourceKind::Window, Some(process_id)) => LoopbackScope::Application { process_id },
            _ => LoopbackScope::SystemExcludingFastcord,
        });
    }
    support
        .system_loopback
        .then_some(LoopbackScope::SystemIncludingFastcord)
}

/// Receives interleaved stereo blocks at [`SAMPLE_RATE`] on the capture
/// thread, in order. Silence the OS reported arrives as zeros. Real-time.
pub type Handler = Box<dyn FnMut(&[f32]) + Send>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopbackError {
    Unsupported(&'static str),
    TargetNotFound,
    NoOutputDevice,
    PermissionDenied,
    /// A native call failed with this platform status.
    Native {
        operation: &'static str,
        code: i32,
    },
    Worker(&'static str),
}

impl fmt::Display for LoopbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(detail) | Self::Worker(detail) => f.write_str(detail),
            Self::TargetNotFound => f.write_str("the application to capture audio from is gone"),
            Self::NoOutputDevice => f.write_str("there is no audio output device to capture"),
            Self::PermissionDenied => {
                f.write_str("the system refused to capture audio (a privacy setting or policy)")
            }
            Self::Native { operation, code } => {
                write!(f, "{operation} failed (0x{:08X})", *code as u32)
            }
        }
    }
}

impl std::error::Error for LoopbackError {}

/// A running capture. Dropping it stops the thread and waits for it.
pub struct Loopback {
    native: Option<native::Native>,
}

impl fmt::Debug for Loopback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Loopback")
    }
}

impl Loopback {
    /// Whether the capture thread is still running (false once the device was
    /// lost or the application ended).
    pub fn is_running(&self) -> bool {
        self.native.as_ref().is_some_and(native::Native::is_running)
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        if let Some(native) = self.native.take() {
            native.stop();
        }
    }
}

/// Starts capturing `scope`, returning once audio flows or the start failed.
/// May block for a moment: call it on a worker.
pub fn start(scope: LoopbackScope, handler: Handler) -> Result<Loopback, LoopbackError> {
    native::start(scope, handler).map(|native| Loopback {
        native: Some(native),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> SourceId {
        SourceId::from_raw(SourceKind::Window, 0)
    }

    #[test]
    fn scope_follows_what_the_machine_offers() {
        let none = LoopbackSupport::default();
        assert_eq!(scope_for(window(), none), None);
        let broad = LoopbackSupport {
            process_loopback: false,
            system_loopback: true,
        };
        let scope = scope_for(window(), broad).unwrap();
        assert_eq!(scope, LoopbackScope::SystemIncludingFastcord);
        assert!(scope.includes_fastcord());
        let full = LoopbackSupport {
            process_loopback: true,
            system_loopback: true,
        };
        let display = SourceId::from_raw(SourceKind::Display, 1);
        assert_eq!(
            scope_for(display, full),
            Some(LoopbackScope::SystemExcludingFastcord)
        );
        // A window that has no process (stale handle) still excludes fastcord.
        assert!(!scope_for(window(), full).unwrap().includes_fastcord());
    }
}
