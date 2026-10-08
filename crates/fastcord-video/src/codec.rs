//! The platform-neutral codec contract every backend implements.

use std::fmt;
use std::time::Duration;

use crate::frame::{FrameError, Nv12};

/// Largest accepted picture width or height, in pixels.
pub const MAX_DIMENSION: u32 = 4096;
/// Largest accepted picture area: 4096x2304 (covers 3840x2160 and 4096x2160).
pub const MAX_PIXELS: u64 = 4096 * 2304;

/// Rejects empty pictures and pictures over [`MAX_DIMENSION`]/[`MAX_PIXELS`].
pub fn check_dimensions(width: u32, height: u32) -> Result<(), CodecError> {
    if width == 0 || height == 0 {
        return Err(CodecError::InvalidConfig(
            "picture dimensions must be nonzero",
        ));
    }
    if width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(CodecError::Unsupported("picture is larger than 4096x2304"));
    }
    Ok(())
}

/// H.264 encoder settings. The bitstream is always a single layer in the
/// constrained-baseline-compatible Baseline profile (no B-frames), configured
/// for low latency, emitted as Annex B with SPS/PPS before every IDR picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Even picture width in pixels.
    pub width: u32,
    /// Even picture height in pixels.
    pub height: u32,
    /// Nominal frames per second.
    pub frame_rate: u32,
    /// Constant target bitrate in bits per second.
    pub bitrate: u32,
    /// Maximum frames between automatic IDR pictures. Keyframes can also be
    /// requested per frame (receiver PLI).
    pub keyframe_interval: u32,
}

impl EncoderConfig {
    /// The screen-share send baseline (SPEC §8.2): 1280x720 at 30 fps, starting
    /// at 2.5 Mbit/s, with an IDR at least every five seconds.
    pub const SCREEN_SHARE_720P30: Self = Self {
        width: 1280,
        height: 720,
        frame_rate: 30,
        bitrate: 2_500_000,
        keyframe_interval: 150,
    };

    pub fn validate(&self) -> Result<(), CodecError> {
        check_dimensions(self.width, self.height)?;
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err(CodecError::InvalidConfig(
                "4:2:0 encoding needs even dimensions",
            ));
        }
        if self.width < 16 || self.height < 16 {
            return Err(CodecError::InvalidConfig("pictures must be at least 16x16"));
        }
        if !(1..=120).contains(&self.frame_rate) {
            return Err(CodecError::InvalidConfig("frame rate must be 1-120 fps"));
        }
        if !(64_000..=50_000_000).contains(&self.bitrate) {
            return Err(CodecError::InvalidConfig(
                "bitrate must be 64 kbit/s to 50 Mbit/s",
            ));
        }
        if self.keyframe_interval == 0 {
            return Err(CodecError::InvalidConfig(
                "keyframe interval must be at least one frame",
            ));
        }
        Ok(())
    }

    /// Nominal duration of one frame.
    pub fn frame_duration(&self) -> Duration {
        Duration::from_secs(1) / self.frame_rate.max(1)
    }
}

/// One encoded H.264 access unit in Annex B format.
#[derive(Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    /// Presentation time of the source frame.
    pub timestamp: Duration,
    /// An IDR picture, preceded by its SPS and PPS.
    pub keyframe: bool,
}

impl fmt::Debug for EncodedFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedFrame")
            .field("bytes", &self.data.len())
            .field("timestamp", &self.timestamp)
            .field("keyframe", &self.keyframe)
            .finish()
    }
}

/// A decoded picture, borrowed from the decoder for the duration of the
/// callback that receives it. Copy what must outlive the callback.
#[derive(Clone, Copy, Debug)]
pub struct DecodedFrame<'a> {
    /// The display area (cropping already applied).
    pub image: Nv12<'a>,
    pub timestamp: Duration,
}

/// Where a codec runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Hardware,
    Software,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Hardware => "hardware",
            Self::Software => "software",
        })
    }
}

/// The backend a codec actually uses, for display and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backend {
    pub kind: BackendKind,
    /// Native API, for example "Media Foundation".
    pub api: &'static str,
    /// The concrete implementation, for example the transform's name and GPU.
    pub name: String,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.api, self.kind, self.name)
    }
}

/// Which backends automatic selection may use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackendPreference {
    /// Hardware first, then software; the fallback is reported.
    #[default]
    Auto,
    /// Hardware only; failure is returned rather than hidden.
    Hardware,
    /// Software only.
    Software,
}

impl BackendPreference {
    pub fn allows(self, kind: BackendKind) -> bool {
        match self {
            Self::Auto => true,
            Self::Hardware => kind == BackendKind::Hardware,
            Self::Software => kind == BackendKind::Software,
        }
    }
}

/// An H.264 encoder. Codec objects belong to the thread that opened them.
pub trait VideoEncoder {
    /// The backend in use.
    fn backend(&self) -> &Backend;

    fn config(&self) -> &EncoderConfig;

    /// Submits one frame of exactly the configured size and appends every
    /// access unit that is ready to `out`, in decoding order. A hardware
    /// encoder may return a frame's output on a later call; [`flush`]
    /// collects everything still pending. `keyframe` forces an IDR picture.
    ///
    /// [`flush`]: VideoEncoder::flush
    fn encode(
        &mut self,
        frame: &Nv12<'_>,
        timestamp: Duration,
        keyframe: bool,
        out: &mut Vec<EncodedFrame>,
    ) -> Result<(), CodecError>;

    /// Appends all pending access units to `out`. The encoder stays usable.
    fn flush(&mut self, out: &mut Vec<EncodedFrame>) -> Result<(), CodecError>;
}

/// An H.264 decoder. Codec objects belong to the thread that opened them.
pub trait VideoDecoder {
    /// The backend in use. A hardware decoder that the driver turns down for a
    /// particular stream reports the software path it fell back to.
    fn backend(&self) -> &Backend;

    /// Decodes one complete Annex B access unit and passes each picture that
    /// is ready to `out`.
    fn decode(
        &mut self,
        access_unit: &[u8],
        timestamp: Duration,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError>;

    /// Passes all pending pictures to `out`. The decoder stays usable.
    fn flush(&mut self, out: &mut dyn FnMut(DecodedFrame<'_>)) -> Result<(), CodecError>;
}

/// Codec failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// The native codec framework is not installed (for example Windows N
    /// editions or Windows Server without Media Foundation).
    PlatformUnavailable(&'static str),
    /// A native call failed; `code` is the platform status (HRESULT, OSStatus).
    Platform {
        operation: &'static str,
        code: i32,
    },
    /// The backend cannot provide something this codec contract requires.
    Unsupported(&'static str),
    InvalidConfig(&'static str),
    InvalidFrame(FrameError),
    /// A frame does not match the configured size.
    FrameSize {
        expected: (u32, u32),
        actual: (u32, u32),
    },
    /// The input is not a well-formed H.264 access unit.
    InvalidBitstream(&'static str),
    /// The native codec stopped answering (for example a hung GPU driver).
    /// The codec should be dropped and reopened.
    Stalled(&'static str),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlatformUnavailable(what) => write!(f, "{what} is not available"),
            Self::Platform { operation, code } => {
                write!(f, "{operation} failed (0x{:08X})", *code as u32)
            }
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
            Self::InvalidConfig(what) => write!(f, "invalid encoder settings: {what}"),
            Self::InvalidFrame(error) => write!(f, "invalid frame: {error}"),
            Self::FrameSize { expected, actual } => write!(
                f,
                "frame is {}x{}, the encoder expects {}x{}",
                actual.0, actual.1, expected.0, expected.1
            ),
            Self::InvalidBitstream(what) => write!(f, "invalid H.264 data: {what}"),
            Self::Stalled(what) => f.write_str(what),
        }
    }
}

impl std::error::Error for CodecError {}

impl From<FrameError> for CodecError {
    fn from(error: FrameError) -> Self {
        Self::InvalidFrame(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_share_baseline_is_valid() {
        let config = EncoderConfig::SCREEN_SHARE_720P30;
        config.validate().unwrap();
        assert_eq!(config.frame_duration(), Duration::from_nanos(33_333_333));
    }

    #[test]
    fn invalid_encoder_settings_are_rejected() {
        let base = EncoderConfig::SCREEN_SHARE_720P30;
        for config in [
            EncoderConfig {
                width: 1279,
                ..base
            },
            EncoderConfig { height: 8, ..base },
            EncoderConfig {
                frame_rate: 0,
                ..base
            },
            EncoderConfig {
                frame_rate: 121,
                ..base
            },
            EncoderConfig {
                bitrate: 1_000,
                ..base
            },
            EncoderConfig {
                keyframe_interval: 0,
                ..base
            },
        ] {
            assert!(matches!(
                config.validate(),
                Err(CodecError::InvalidConfig(_))
            ));
        }
        assert!(matches!(
            EncoderConfig {
                width: 4098,
                ..base
            }
            .validate(),
            Err(CodecError::Unsupported(_))
        ));
    }

    #[test]
    fn dimension_limits_cover_4k_but_not_more() {
        check_dimensions(3840, 2160).unwrap();
        check_dimensions(4096, 2304).unwrap();
        check_dimensions(2304, 4096).unwrap();
        assert!(check_dimensions(4096, 4096).is_err());
        assert!(check_dimensions(4112, 16).is_err());
        assert!(check_dimensions(0, 16).is_err());
    }

    #[test]
    fn preference_filters_backend_kinds() {
        use BackendKind::*;
        assert!(BackendPreference::Auto.allows(Hardware));
        assert!(BackendPreference::Auto.allows(Software));
        assert!(!BackendPreference::Hardware.allows(Software));
        assert!(!BackendPreference::Software.allows(Hardware));
    }

    #[test]
    fn platform_errors_show_the_status_code() {
        let error = CodecError::Platform {
            operation: "IMFTransform::ProcessInput",
            code: 0xC00D6D72_u32 as i32,
        };
        assert_eq!(
            error.to_string(),
            "IMFTransform::ProcessInput failed (0xC00D6D72)"
        );
    }
}
