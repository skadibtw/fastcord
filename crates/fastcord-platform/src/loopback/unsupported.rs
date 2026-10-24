//! Platforms without stream-audio capture yet (SPEC milestone 37).

use super::{AudioSource, Handler, LoopbackError, LoopbackScope, LoopbackSupport};
use crate::capture::SourceId;

pub(super) struct Native;

impl Native {
    pub(super) fn is_running(&self) -> bool {
        false
    }

    pub(super) fn stop(self) {}
}

pub(super) fn support() -> LoopbackSupport {
    LoopbackSupport::default()
}

pub(super) fn window_process(_: SourceId) -> Option<u32> {
    None
}

pub(super) fn list_targets() -> Result<Vec<AudioSource>, LoopbackError> {
    Ok(Vec::new())
}

pub(super) fn start(_: LoopbackScope, _: Handler) -> Result<Native, LoopbackError> {
    Err(LoopbackError::Unsupported(
        "sharing audio is only implemented for Windows and Linux so far",
    ))
}
