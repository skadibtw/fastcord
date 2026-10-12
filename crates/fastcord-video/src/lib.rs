//! fastcord video: OS-native H.264 and portable dav1d AV1 decoding behind one
//! platform-neutral codec trait (SPEC §8.2, §8.5; ADR 0001).
//!
//! [`VideoEncoder`] and [`VideoDecoder`] exchange borrowed NV12 images
//! ([`Nv12`]). H.264 uses Annex B access units; [`av1::Decoder`] receives sized
//! low-overhead OBU temporal units and never encodes AV1. Native H.264 backends
//! are selected at runtime (hardware first in automatic mode); the actual
//! backend is always reported ([`Backend`], [`Selected::rejected`]). FFmpeg is
//! not used. Windows uses Media Foundation ([`media_foundation`]); macOS uses
//! VideoToolbox ([`videotoolbox`]) with IOSurface-backed pixel buffers.

pub mod av1;
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
