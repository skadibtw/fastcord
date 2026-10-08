//! fastcord video: OS-native codecs behind one platform-neutral codec trait
//! (SPEC §8.2, §8.5; ADR 0001).
//!
//! [`VideoEncoder`] and [`VideoDecoder`] exchange NV12 images ([`Nv12`]) and
//! H.264 Annex B access units. Backends are selected at runtime: hardware
//! first, then software in automatic mode, and the backend actually in use is
//! always reported ([`Backend`], [`Selected::rejected`]). FFmpeg is not used.
//!
//! Windows uses Media Foundation ([`media_foundation`]); macOS uses VideoToolbox
//! ([`videotoolbox`]) with IOSurface-backed pixel buffers.

pub mod codec;
pub mod frame;
pub mod h264;
#[cfg(windows)]
pub mod media_foundation;
mod select;
#[cfg(test)]
mod test_vectors;
#[cfg(target_os = "macos")]
pub mod videotoolbox;

pub use codec::{
    Backend, BackendKind, BackendPreference, CodecError, DecodedFrame, EncodedFrame, EncoderConfig,
    VideoDecoder, VideoEncoder,
};
pub use frame::{FrameError, Nv12};
pub use select::{Attempt, OpenError, Selected};
