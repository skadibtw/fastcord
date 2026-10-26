//! fastcord video: OS-native H.264 and portable dav1d AV1 decoding behind one
//! platform-neutral codec trait (SPEC §8.2, §8.5; ADR 0001).
//!
//! [`VideoEncoder`] and [`VideoDecoder`] exchange borrowed NV12 images
//! ([`Nv12`]) with [`ColorDescription`]. [`FramePool`] retains immutable
//! [`FrameLease`] handles for rendering, with no RGB work on the GPU path.
//! H.264 uses Annex B access units; [`av1::Decoder`] receives sized
//! low-overhead OBU temporal units and never encodes AV1. Native H.264 backends
//! are selected at runtime (hardware first in automatic mode); the actual
//! backend is always reported ([`Backend`], [`Selected::rejected`]). FFmpeg is
//! not used. Windows uses Media Foundation ([`media_foundation`]); macOS uses
//! VideoToolbox ([`videotoolbox`]) with IOSurface-backed pixel buffers.
//!
//! [`share`] is the sending side of a screen share: it connects a platform
//! capture session to an encoder and hands finished access units to the
//! transport, with keyframe and bitrate control from the connection's feedback.

#[cfg(target_os = "linux")]
pub mod vaapi;
// The software H.264 codec itself is platform-neutral, but it is only
// registered as a candidate on Linux; Windows and macOS keep their native
// backends. Compiling it everywhere lets the identical round-trip tests run on
// every OS in CI (SPEC §8.2; ADR 0001).
pub mod av1;
#[cfg(any(target_os = "linux", test))]
mod bitwriter;
pub mod codec;
pub mod container;
pub mod effect;
pub mod frame;
pub mod h264;
#[cfg(windows)]
pub mod media_foundation;
pub mod openh264;
#[cfg(target_os = "linux")]
mod platform_h264;
mod select;
pub mod share;
#[cfg(test)]
mod test_vectors;
#[cfg(target_os = "macos")]
pub mod videotoolbox;

pub use codec::{
    Backend, BackendKind, BackendPreference, CodecError, DecodedFrame, EncodedFrame, EncoderConfig,
    VideoDecoder, VideoEncoder,
};
pub use frame::{
    ColorDescription, ColorMatrix, ColorRange, FrameError, FrameFormat, FrameLease, FramePool,
    LeaseError, Nv12,
};
pub use select::{Attempt, OpenError, Selected};
