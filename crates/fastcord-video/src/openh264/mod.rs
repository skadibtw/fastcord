//! OpenH264 software H.264 encode/decode (SPEC §8.2, §8.5; ADR 0001).
//!
//! Hardware encoders are absent on many Linux systems, and even a working
//! VA-API setup can refuse a particular stream, so every screen share needs a
//! software backend behind the hardware candidates in automatic mode, and an
//! explicit software preference needs one of its own. This module answers
//! both with Cisco's `cisco/openh264` source pinned by the `openh264-sys2`
//! crates (compiled once by that crate's build script): there is no runtime
//! loader, and the wrapper owns every native object and its lifetime, which
//! keeps this module free of `unsafe` code.
//!
//! The module itself is compiled on every platform — its tests run wherever
//! `fastcord-video` builds — but only the Linux platform glue registers the
//! candidates below; Windows keeps Media Foundation and macOS keeps
//! VideoToolbox, so their selection paths are unchanged.
//!
//! Codec objects belong to the thread that opened them (see the codec
//! contract). A running encoder cannot change its target bitrate: the
//! wrapper exposes mid-session option changes only through its unsafe raw
//! API, which this module denies, so [`VideoEncoder::set_bitrate`] keeps the
//! contract's default [`CodecError::Unsupported`] answer and the share
//! pipeline reopens the encoder at the new rate, resuming at a fresh keyframe.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
// Only Linux registers the candidates today; the attribute above keeps the
// Windows and macOS (unused) paths warning-free without changing anything.

mod decoder;
mod encoder;

use ::openh264 as native;

#[cfg(test)]
mod tests;

pub use decoder::SoftwareDecoder;
pub use encoder::SoftwareEncoder;

use crate::codec::{Backend, BackendKind, BackendPreference, CodecError, EncoderConfig};
use crate::select::{Candidate, OpenError, Selected, select};

/// Reported as [`Backend::api`].
pub const API: &str = "OpenH264";

/// Reported as [`Backend::name`]: the upstream source revision pinned by
/// `openh264-sys2` (recorded in the vendored crate's `upstream/VERSION`).
pub(crate) const NAME: &str = "cisco OpenH264 4a2615fa (bundled source)";

/// The bundled software encoder as a candidate source. The platform glue
/// keeps it at the end of its automatic candidate list; on the platforms that
/// do not register it, the whole module stays compiled but inert.
pub(crate) struct SoftwareEncoderSource;

/// The bundled software decoder as a candidate source.
pub(crate) struct SoftwareDecoderSource;
impl SoftwareEncoderSource {
    /// Opens the encoder after validating the settings.
    pub(crate) fn open(self, config: EncoderConfig) -> Result<SoftwareEncoder, CodecError> {
        encoder::SoftwareEncoder::open(config)
    }
}

impl SoftwareDecoderSource {
    /// Opens the decoder.
    pub(crate) fn open(self) -> Result<SoftwareDecoder, CodecError> {
        decoder::SoftwareDecoder::open()
    }
}

/// The single software encoder candidate.
pub(crate) fn encoder_candidates() -> Vec<Candidate<SoftwareEncoderSource>> {
    vec![Candidate {
        kind: BackendKind::Software,
        name: NAME.to_owned(),
        source: SoftwareEncoderSource,
    }]
}

/// The single software decoder candidate.
pub(crate) fn decoder_candidates() -> Vec<Candidate<SoftwareDecoderSource>> {
    vec![Candidate {
        kind: BackendKind::Software,
        name: NAME.to_owned(),
        source: SoftwareDecoderSource,
    }]
}

/// Opens the bundled OpenH264 encoder, as `preference` allows. Settings are
/// validated before any codec is built, so a rejected configuration is
/// reported as [`OpenError::Invalid`] without an attempt.
pub fn open_encoder(
    config: EncoderConfig,
    preference: BackendPreference,
) -> Result<Selected<SoftwareEncoder>, OpenError> {
    config.validate().map_err(OpenError::Invalid)?;
    select(preference, encoder_candidates(), |source| {
        source.open(config)
    })
}

/// Opens the bundled OpenH264 decoder, as `preference` allows.
pub fn open_decoder(preference: BackendPreference) -> Result<Selected<SoftwareDecoder>, OpenError> {
    select(
        preference,
        decoder_candidates(),
        SoftwareDecoderSource::open,
    )
}

/// The backend both codecs report.
fn backend() -> Backend {
    Backend {
        kind: BackendKind::Software,
        api: API,
        name: NAME.to_owned(),
    }
}

/// Maps a wrapper error onto the codec contract. The wrapper reports
/// OpenH264's native status (and its own message and decoding state), which
/// the typed codec errors have no free-text field for; the status code is the
/// honest portion and reaches logs through the codec error's `Display`.
pub(crate) fn native_error(
    operation: &'static str,
) -> impl FnOnce(native::Error) -> CodecError + 'static {
    move |error| CodecError::Platform {
        operation,
        code: i32::try_from(error.native_code()).unwrap_or(i32::MAX),
    }
}
