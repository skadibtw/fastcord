//! Capture on operating systems that have no backend yet (SPEC milestones 33
//! and 34): every entry point reports why, so the UI can say so instead of
//! offering a share that cannot work.

use std::sync::Arc;

use super::{
    CaptureError, CaptureSource, CaptureStatus, Control, HandlerFactory, SourceId, StartOptions,
};

const UNSUPPORTED: CaptureError =
    CaptureError::Unsupported("screen capture is only implemented for Windows so far");

/// No session can exist here.
pub(super) enum Native {}

impl Native {
    pub(super) fn waker(&self) -> Box<dyn Fn() + Send + Sync> {
        match *self {}
    }

    pub(super) fn stop(self) -> Result<(), CaptureError> {
        match self {}
    }
}

pub(super) fn list_sources() -> Result<Vec<CaptureSource>, CaptureError> {
    Err(UNSUPPORTED)
}

pub(super) fn availability() -> Result<(), CaptureError> {
    Err(UNSUPPORTED)
}

pub(super) fn start(
    _source: SourceId,
    _options: &StartOptions,
    _control: Arc<Control>,
    _status: CaptureStatus,
    _handler: HandlerFactory,
) -> Result<Native, CaptureError> {
    Err(UNSUPPORTED)
}
