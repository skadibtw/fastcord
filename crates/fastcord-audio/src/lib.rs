//! fastcord's real-time audio engine (SPEC §7).
//!
//! - [`device`]: cpal device enumeration, stable IDs, explicit selection.
//! - [`AudioEngine`]: an owner thread per call that opens the device streams,
//!   converts between device formats/rates and 48 kHz, and runs Opus.
//! - [`codec`]: Opus encode/decode with FEC and concealment.
//! - [`denoise`]: microphone-only noise suppression (Off, Light, High quality) behind one
//!   [`Denoiser`] trait.
//! - `echo`: acoustic echo cancellation of the microphone against the played mix.
//! - [`audit`]: the callback marker used by allocation instrumentation.
//!
//! Device callbacks only convert samples and move them through preallocated
//! lock-free rings; they never allocate, lock, or perform I/O.

pub mod aac;
pub mod audit;
mod callback;
mod channels;
pub mod codec;
pub mod denoise;
pub mod device;
mod echo;
mod engine;
mod error;
pub mod file_player;
mod jitter;
pub mod ogg;
mod pipeline;
mod resample;
mod ring;

#[cfg(test)]
mod alloc_counter;

pub use codec::{CodecError, EncodedPacket, VoiceDecoder, VoiceEncoder};
pub use denoise::{DenoiseError, Denoiser, NoiseSuppression, NoiseSuppressionStatus};
pub use device::{DeviceChoice, DeviceErrorKind, DeviceInfo, DeviceKey, Direction, list_devices};
pub use engine::{
    AudioEngine, DeviceFormat, EngineChannels, EngineConfig, EngineControl, EngineEvent,
    EngineStats,
};
pub use error::AudioError;
pub use file_player::{
    FilePlayer, PacketSource, PlayerClock, PlayerError, PlayerFailure, PlayerSnapshot, PlayerState,
    Silence,
};
pub use pipeline::{CapturedFrame, RemoteFrame};
pub use resample::UnsupportedRate;
