//! OpenH264 encoding into one Annex B access unit per picture.

use std::time::Duration;

use super::native;
use crate::codec::{CodecError, EncodedFrame, EncoderConfig, VideoEncoder};
use crate::frame::Nv12;
use crate::h264::{ParameterSets, nal_units};

/// The wrapper's encoder settings: screen-share usage, honest bitrate rate
/// control, single-threaded low-complexity Baseline encoding at the
/// configured GOP length, no frame skipping (one output per submitted
/// picture keeps the timestamps mapped), and explicit BT.709 limited-range
/// VUI so the SPS carries the color our defaults already claim.
fn native_config(config: &EncoderConfig) -> native::encoder::EncoderConfig {
    native::encoder::EncoderConfig::new()
        .bitrate(native::encoder::BitRate::from_bps(config.bitrate))
        .max_frame_rate(native::encoder::FrameRate::from_hz(
            config.frame_rate as f32,
        ))
        .usage_type(native::encoder::UsageType::ScreenContentRealTime)
        .rate_control_mode(native::encoder::RateControlMode::Bitrate)
        .profile(native::encoder::Profile::Baseline)
        .complexity(native::encoder::Complexity::Low)
        .num_threads(1)
        .skip_frames(false)
        .intra_frame_period(native::encoder::IntraFramePeriod::from_num_frames(
            config.keyframe_interval,
        ))
        .vui(native::encoder::VuiConfig::bt709())
}

/// An OpenH264 software H.264 encoder (see [`super::open_encoder`]).
///
/// The bundled encoder is configurable only at open time through the safe
/// wrapper API (its raw option API would need `unsafe`), so a bitrate request
/// while running keeps the contract's default [`CodecError::Unsupported`]
/// answer and the caller reopens the encoder at the new rate.
pub struct SoftwareEncoder {
    config: EncoderConfig,
    backend: super::Backend,
    native: native::encoder::Encoder,
    parameter_sets: ParameterSets,
    planes: Planes,
}

impl SoftwareEncoder {
    pub(super) fn open(config: EncoderConfig) -> Result<Self, CodecError> {
        config.validate()?;
        // The bundled source rejects pictures beyond 3840x2160 on either side
        // as it initializes (the smaller side at most 2160), below the codec
        // contract's 4096x2304 ceiling: answering here makes the failed
        // candidate part of the selection report instead of a first-picture
        // failure.
        if config.width.max(config.height) > 3840 || config.width.min(config.height) > 2160 {
            return Err(CodecError::Unsupported(
                "OpenH264 encodes pictures at most 3840x2160",
            ));
        }
        let planes = Planes::new(&config)?;
        let native = native::encoder::Encoder::with_api_config(
            native::OpenH264API::from_source(),
            native_config(&config),
        )
        .map_err(super::native_error("OpenH264 create encoder"))?;
        Ok(Self {
            config,
            backend: super::backend(),
            native,
            parameter_sets: ParameterSets::default(),
            planes,
        })
    }
}

/// Per-frame scratch, exactly sized once at open: three I420 planes for one
/// picture in the wrapper's input format, plus one buffer that collects the
/// wrapper's own layer/NAL byte stream. Encoding only ever replaces their
/// contents, so no buffer grows between frames; their size follows the
/// validated config (at most 3840x2160 through this backend), not the
/// wrapper's output.
struct Planes {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    /// The wrapper's bitstream output as its layers carry it, start codes
    /// included, before the canonical Annex B re-assembly.
    raw: Vec<u8>,
}

impl Planes {
    /// Sizes every plane from the validated config: luma at one byte per
    /// pixel, separate chroma planes a quarter of the size (even dimensions
    /// are required by the config validation).
    fn new(config: &EncoderConfig) -> Result<Self, CodecError> {
        let (width, height) = (config.width as usize, config.height as usize);
        let luma = width
            .checked_mul(height)
            .ok_or(CodecError::Unsupported("picture is larger than 4096x2304"))?;
        let chroma = (width / 2)
            .checked_mul(height / 2)
            .ok_or(CodecError::Unsupported("picture is larger than 4096x2304"))?;
        Ok(Self {
            y: vec![0; luma],
            u: vec![0; chroma],
            v: vec![0; chroma],
            raw: Vec::new(),
        })
    }

    /// Copies the visible pixels of `frame` into the planes as three separate
    /// I420 planes. The config's even dimensions make the plane sizes exact,
    /// so every write is in bounds; `Nv12`'s row accessors already clip a
    /// padded last row to its visible bytes.
    fn fill(&mut self, frame: &Nv12<'_>, width: usize) {
        for (row, target) in self.y.chunks_exact_mut(width).enumerate() {
            target.copy_from_slice(frame.luma_row(row));
        }
        // NV12 interleaves U and V per chroma row; I420 keeps them apart.
        let chroma_width = width / 2;
        for (row, target) in self.u.chunks_exact_mut(chroma_width).enumerate() {
            let source = frame.chroma_row(row);
            for (index, sample) in target.iter_mut().enumerate() {
                *sample = source[2 * index];
            }
        }
        for (row, target) in self.v.chunks_exact_mut(chroma_width).enumerate() {
            let source = frame.chroma_row(row);
            for (index, sample) in target.iter_mut().enumerate() {
                *sample = source[2 * index + 1];
            }
        }
    }
}

const START_CODE: [u8; 4] = [0, 0, 0, 1];

impl VideoEncoder for SoftwareEncoder {
    fn backend(&self) -> &super::Backend {
        &self.backend
    }

    fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn encode(
        &mut self,
        frame: &Nv12<'_>,
        timestamp: Duration,
        keyframe: bool,
        out: &mut Vec<EncodedFrame>,
    ) -> Result<(), CodecError> {
        // Nothing of the wrapper's state may follow a resize upstream: the
        // shared path reopens the encoder instead (and so would a caller).
        let expected = (self.config.width, self.config.height);
        let actual = (frame.width(), frame.height());
        if actual != expected {
            return Err(CodecError::FrameSize { expected, actual });
        }
        // The wrapper turns the milliseconds into an i64 and panics beyond
        // that range; durations are non-negative, so one bound is enough.
        let milliseconds = timestamp.as_millis();
        if milliseconds > i64::MAX as u128 {
            return Err(CodecError::Unsupported(
                "timestamp is beyond the encoder range",
            ));
        }

        self.planes.fill(frame, self.config.width as usize);
        if keyframe {
            self.native.force_intra_frame();
        }

        let source = native::formats::YUVSlices::new(
            (&self.planes.y, &self.planes.u, &self.planes.v),
            (self.config.width as usize, self.config.height as usize),
            (
                self.config.width as usize,
                (self.config.width / 2) as usize,
                (self.config.width / 2) as usize,
            ),
        );
        let encoded = self
            .native
            .encode_at(&source, native::Timestamp::from_millis(milliseconds as u64))
            .map_err(super::native_error("OpenH264 encode frame"))?;

        // The wrapper's layers concatenate their NAL units with their own
        // start codes; splitting the whole output and re-emitting it gives
        // every access unit exactly one canonical four-byte prefix per NAL
        // instead of depending on which prefixes the encoder picked.
        self.planes.raw.clear();
        let mut layer_index = 0;
        while let Some(layer) = encoded.layer(layer_index) {
            layer_index += 1;
            let mut nal_index = 0;
            while let Some(nal) = layer.nal_unit(nal_index) {
                nal_index += 1;
                self.planes.raw.extend_from_slice(nal);
            }
        }
        if self.planes.raw.is_empty() {
            // No bytes for the picture (a rate-controlled skip): frame
            // skipping is off, so most likely the encoder has nothing to say
            // about this picture yet. There is no access unit to report.
            return Ok(());
        }

        let mut annex_b = Vec::new();
        for nal in nal_units(&self.planes.raw) {
            annex_b.extend_from_slice(&START_CODE);
            annex_b.extend_from_slice(nal);
        }
        // ConstantId does not prepend the parameter sets to later IDR
        // pictures, but every keyframe a receiver joins on must carry them.
        match self.parameter_sets.complete(annex_b) {
            Ok(Some((data, idr))) => out.push(EncodedFrame {
                data,
                timestamp,
                keyframe: idr,
            }),
            // Only parameter sets were produced; they are remembered for the
            // next picture.
            Ok(None) => {}
            // An IDR without previous SPS/PPS is a bookkeeping bug; surface
            // it instead of emitting a useless access unit.
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn flush(&mut self, out: &mut Vec<EncodedFrame>) -> Result<(), CodecError> {
        // The wrapper encodes synchronously, one layer set per submitted
        // picture, and the Baseline profile carries no B-frames: nothing
        // accumulates between encode calls, so nothing is pending here and
        // `out` stays untouched.
        let _ = out;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{API, SoftwareEncoderSource};
    use super::*;
    use crate::codec::BackendKind;
    use crate::h264::{NAL_SPS, inspect, nal_unit_type, parse_sps};
    use crate::test_vectors::moving_pattern;

    const CONFIG: EncoderConfig = EncoderConfig {
        width: 128,
        height: 96,
        frame_rate: 30,
        bitrate: 500_000,
        keyframe_interval: 150,
    };

    fn frame_time(index: usize) -> Duration {
        Duration::from_millis(33) * index as u32
    }

    /// Encodes `count` moving-pattern pictures with a forced keyframe at
    /// `forced` and returns the access units.
    fn encode_pattern(
        encoder: &mut SoftwareEncoder,
        count: usize,
        forced: usize,
    ) -> Vec<EncodedFrame> {
        let mut encoded = Vec::new();
        for index in 0..count {
            let picture = moving_pattern(CONFIG.width, CONFIG.height, index as u32);
            let frame = Nv12::packed(CONFIG.width, CONFIG.height, &picture).unwrap();
            encoder
                .encode(&frame, frame_time(index), index == forced, &mut encoded)
                .unwrap_or_else(|error| panic!("picture {index}: {error}"));
        }
        encoder.flush(&mut encoded).unwrap();
        encoded
    }

    #[test]
    fn settings_fail_before_any_codec_is_built() {
        for config in [
            EncoderConfig {
                width: 127,
                ..CONFIG
            },
            EncoderConfig {
                width: 16,
                height: 15,
                ..CONFIG
            },
            EncoderConfig {
                frame_rate: 0,
                ..CONFIG
            },
            EncoderConfig {
                bitrate: 63_999,
                ..CONFIG
            },
            EncoderConfig {
                keyframe_interval: 0,
                ..CONFIG
            },
        ] {
            assert!(
                matches!(
                    SoftwareEncoderSource.open(config),
                    Err(CodecError::InvalidConfig(_))
                ),
                "{config:?}"
            );
        }
        // The bundled source stops at 3840x2160, below the contract's ceiling.
        let large = EncoderConfig {
            width: 4096,
            height: 2304,
            ..CONFIG
        };
        assert!(matches!(
            SoftwareEncoderSource.open(large),
            Err(CodecError::Unsupported(_))
        ));
    }

    #[test]
    fn timestamps_beyond_the_native_range_are_rejected() {
        let mut encoder = SoftwareEncoderSource.open(CONFIG).unwrap();
        let picture = moving_pattern(CONFIG.width, CONFIG.height, 0);
        let frame = Nv12::packed(CONFIG.width, CONFIG.height, &picture).unwrap();
        let beyond = Duration::from_millis(i64::MAX as u64 + 1);
        assert_eq!(
            encoder.encode(&frame, beyond, false, &mut Vec::new()),
            Err(CodecError::Unsupported(
                "timestamp is beyond the encoder range"
            ))
        );
    }

    #[test]
    fn frames_of_another_size_are_rejected_before_encoding() {
        let mut encoder = SoftwareEncoderSource.open(CONFIG).unwrap();
        let small = moving_pattern(64, 64, 0);
        let frame = Nv12::packed(64, 64, &small).unwrap();
        assert_eq!(
            encoder.encode(&frame, Duration::ZERO, false, &mut Vec::new()),
            Err(CodecError::FrameSize {
                expected: (CONFIG.width, CONFIG.height),
                actual: (64, 64)
            })
        );
        // The encoder is unaffected and still encodes the configured size.
        let picture = moving_pattern(CONFIG.width, CONFIG.height, 0);
        let frame = Nv12::packed(CONFIG.width, CONFIG.height, &picture).unwrap();
        let mut encoded = Vec::new();
        encoder
            .encode(&frame, frame_time(0), false, &mut encoded)
            .unwrap();
        assert!(encoded[0].keyframe);
    }

    #[test]
    fn output_is_canonical_annex_b_with_self_contained_keyframes() {
        let mut encoder = SoftwareEncoderSource.open(CONFIG).unwrap();
        assert_eq!(encoder.backend().kind, BackendKind::Software);
        assert_eq!(encoder.backend().api, API);
        let forced = 3;
        let encoded = encode_pattern(&mut encoder, 6, forced);

        // No reordering (no B-frames): decoding order is presentation order,
        // and every submitted picture answers with its own access unit.
        let times: Vec<Duration> = encoded.iter().map(|frame| frame.timestamp).collect();
        assert_eq!(times, (0..6).map(frame_time).collect::<Vec<_>>());

        let mut since_keyframe = 0;
        for (index, frame) in encoded.iter().enumerate() {
            let summary = inspect(&frame.data).unwrap();
            assert!(summary.vcl, "picture {index}");
            assert_eq!(frame.keyframe, summary.idr, "picture {index}");
            if frame.keyframe {
                // Every keyframe is self-contained for a joining receiver.
                assert!(summary.sps && summary.pps, "keyframe {index}");
                let sps = nal_units(&frame.data)
                    .find(|nal| nal_unit_type(nal) == NAL_SPS)
                    .unwrap();
                let sps = parse_sps(sps).unwrap();
                assert_eq!((sps.width, sps.height), (CONFIG.width, CONFIG.height));
                assert_eq!(sps.chroma_format_idc, 1);
                assert_eq!((sps.bit_depth_luma, sps.bit_depth_chroma), (8, 8));
                since_keyframe = 0;
            } else {
                since_keyframe += 1;
                assert!(
                    since_keyframe < CONFIG.keyframe_interval,
                    "picture {index}: no keyframe for {since_keyframe} frames"
                );
            }
        }
        assert!(encoded[0].keyframe, "the stream starts with a keyframe");
        assert!(encoded[forced].keyframe, "a requested keyframe is produced");
        // Only the forced IDR (besides the first picture) is a keyframe.
        assert_eq!(encoded.iter().filter(|frame| frame.keyframe).count(), 2);
    }

    #[test]
    fn automatic_keyframes_honor_the_configured_interval() {
        let config = EncoderConfig {
            keyframe_interval: 3,
            ..CONFIG
        };
        let mut encoder = SoftwareEncoderSource.open(config).unwrap();
        let encoded = encode_pattern(&mut encoder, 8, usize::MAX);
        assert!(encoded[0].keyframe);
        let mut since_keyframe = 0;
        for (index, frame) in encoded.iter().enumerate() {
            let summary = inspect(&frame.data).unwrap();
            assert_eq!(frame.keyframe, summary.idr, "picture {index}");
            assert!(
                since_keyframe < config.keyframe_interval,
                "picture {index}: no keyframe for {since_keyframe} frames"
            );
            if frame.keyframe {
                since_keyframe = 0;
            } else {
                since_keyframe += 1;
            }
        }
    }

    #[test]
    fn nv12_is_split_into_exact_i420_planes() {
        // 4x2 luma with padding, one interleaved chroma row of two pairs.
        let y = [0, 1, 2, 3, 0xEE, 4, 5, 6, 7, 0xEE];
        let uv = [8, 24, 9, 25, 10, 26, 11, 27];
        let frame = Nv12::new(4, 2, &y, 5, &uv, 8).unwrap();
        let mut planes = Planes {
            y: vec![0; 8],
            u: vec![0; 2],
            v: vec![0; 2],
            raw: Vec::new(),
        };
        planes.fill(&frame, 4);
        assert_eq!(planes.y, [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(planes.u, [8, 9]);
        assert_eq!(planes.v, [24, 25]);
    }
}
