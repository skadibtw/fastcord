//! fastcord's real-time audio engine (SPEC §7).
//!
//! - [`device`]: cpal device enumeration, stable IDs, explicit selection.
//! - [`AudioEngine`]: an owner thread per call that opens the device streams,
//!   converts between device formats/rates and 48 kHz, and runs Opus.
//! - [`codec`]: Opus encode/decode with FEC and concealment.
//! - [`audit`]: the callback marker used by allocation instrumentation.
//!
//! Device callbacks only convert samples and move them through preallocated
//! lock-free rings; they never allocate, lock, or perform I/O.

pub mod audit;
mod callback;
mod channels;
pub mod codec;
pub mod device;
mod engine;
mod error;
mod jitter;
mod pipeline;
mod resample;
mod ring;

#[cfg(test)]
mod alloc_counter;

pub use codec::{CodecError, EncodedPacket, VoiceDecoder, VoiceEncoder};
pub use device::{DeviceChoice, DeviceErrorKind, DeviceInfo, DeviceKey, Direction, list_devices};
pub use engine::{
    AudioEngine, DeviceFormat, EngineChannels, EngineConfig, EngineEvent, EngineStats,
};
pub use error::AudioError;
pub use pipeline::{CapturedFrame, RemoteFrame};
pub use resample::UnsupportedRate;
