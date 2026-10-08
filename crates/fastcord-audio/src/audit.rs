//! Real-time callback marker for allocation instrumentation.
//!
//! Every audio data callback runs inside a [`CallbackScope`]. A profiling
//! build (or a test binary) can install a counting `#[global_allocator]` that
//! consults [`in_audio_callback`] and records any allocation made while a
//! device callback is running. The marker is a const-initialized thread-local
//! `Cell`, so checking or setting it never allocates or locks.

use std::cell::Cell;

thread_local! {
    static IN_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

/// Whether the current thread is executing an audio device data callback.
pub fn in_audio_callback() -> bool {
    IN_CALLBACK.try_with(Cell::get).unwrap_or(false)
}

/// Marks the current thread as inside an audio callback until dropped.
pub(crate) struct CallbackScope {
    previous: bool,
}

impl CallbackScope {
    pub(crate) fn enter() -> Self {
        Self {
            previous: IN_CALLBACK.with(|flag| flag.replace(true)),
        }
    }
}

impl Drop for CallbackScope {
    fn drop(&mut self) {
        IN_CALLBACK.with(|flag| flag.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_marks_only_the_current_thread_and_restores() {
        assert!(!in_audio_callback());
        {
            let _outer = CallbackScope::enter();
            assert!(in_audio_callback());
            std::thread::spawn(|| assert!(!in_audio_callback()))
                .join()
                .unwrap();
            {
                let _inner = CallbackScope::enter();
                assert!(in_audio_callback());
            }
            assert!(in_audio_callback());
        }
        assert!(!in_audio_callback());
    }
}
