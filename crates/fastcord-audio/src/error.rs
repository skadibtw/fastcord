use crate::codec::CodecError;
use crate::device::{DeviceErrorKind, Direction};
use crate::resample::UnsupportedRate;

/// Audio engine and device errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// The OS reports no default device for this direction.
    NoDefaultDevice(Direction),
    /// The explicitly selected device is not present.
    DeviceNotFound(Direction),
    /// The device or OS audio service failed.
    Device(Direction, DeviceErrorKind),
    /// The engine has neither an input nor an output to run.
    NothingToRun,
    Codec(CodecError),
    Rate(UnsupportedRate),
    /// The engine thread could not be started or stopped unexpectedly.
    EngineThread,
}

impl AudioError {
    pub(crate) fn backend(direction: Direction, error: &cpal::Error) -> Self {
        Self::Device(direction, DeviceErrorKind::from_cpal(error.kind()))
    }
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let device = |direction: &Direction| match direction {
            Direction::Input => "input device",
            Direction::Output => "output device",
        };
        match self {
            Self::NoDefaultDevice(direction) => write!(f, "no default {}", device(direction)),
            Self::DeviceNotFound(direction) => {
                write!(f, "the selected {} is not connected", device(direction))
            }
            Self::Device(direction, kind) => write!(f, "{}: {kind}", device(direction)),
            Self::NothingToRun => f.write_str("no audio input or output requested"),
            Self::Codec(error) => error.fmt(f),
            Self::Rate(error) => error.fmt(f),
            Self::EngineThread => f.write_str("the audio engine thread failed"),
        }
    }
}

impl std::error::Error for AudioError {}

impl From<CodecError> for AudioError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

impl From<UnsupportedRate> for AudioError {
    fn from(error: UnsupportedRate) -> Self {
        Self::Rate(error)
    }
}
