//! Platforms without stream-audio capture yet (SPEC milestones 36 and 37).

use super::{Handler, LoopbackError, LoopbackScope, LoopbackSupport};
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

pub(super) fn start(_: LoopbackScope, _: Handler) -> Result<Native, LoopbackError> {
    Err(LoopbackError::Unsupported(
        "sharing audio is only implemented for Windows so far",
    ))
}
