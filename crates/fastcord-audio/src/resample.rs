//! Streaming sample-rate conversion between device rates and 48 kHz.
//!
//! Equal rates pass through untouched. Otherwise a band-limited sinc
//! resampler (rubato) with fixed-size input chunks runs on preallocated
//! staging and output buffers, so steady-state processing does not allocate.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

/// Sinc filter length: a 256-tap Blackman-Harris filter preserves Opus
/// fullband (20 kHz) across 44.1/48 kHz conversion with 128 frames of delay.
const SINC_LEN: usize = 256;

/// A sample-rate converter could not be created for the given rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedRate {
    pub from: u32,
    pub to: u32,
}

impl std::fmt::Display for UnsupportedRate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot convert {} Hz to {} Hz", self.from, self.to)
    }
}

impl std::error::Error for UnsupportedRate {}

pub(crate) struct RateConverter {
    channels: usize,
    inner: Option<Converter>,
}

struct Converter {
    resampler: Async<f32>,
    /// Interleaved input staging of exactly one chunk.
    staging: Box<[f32]>,
    staged: usize,
    /// Interleaved output for one chunk, sized for the maximum output.
    output: Box<[f32]>,
}

impl RateConverter {
    /// Creates a converter for interleaved audio. `chunk_frames` is the input
    /// block size (typically 10 ms at the input rate).
    pub(crate) fn new(
        from: u32,
        to: u32,
        channels: usize,
        chunk_frames: usize,
    ) -> Result<Self, UnsupportedRate> {
        let error = UnsupportedRate { from, to };
        if from == 0 || to == 0 || channels == 0 || chunk_frames == 0 {
            return Err(error);
        }
        if from == to {
            return Ok(Self {
                channels,
                inner: None,
            });
        }
        let parameters =
            SincInterpolationParameters::new(SINC_LEN, WindowFunction::BlackmanHarris2)
                .interpolation(SincInterpolationType::Cubic);
        let resampler = Async::new_sinc(
            f64::from(to) / f64::from(from),
            1.0,
            &parameters,
            chunk_frames,
            channels,
            FixedAsync::Input,
        )
        .map_err(|_| error)?;
        let output_len = resampler.output_frames_max() * channels;
        Ok(Self {
            channels,
            inner: Some(Converter {
                resampler,
                staging: vec![0.0; chunk_frames * channels].into_boxed_slice(),
                staged: 0,
                output: vec![0.0; output_len].into_boxed_slice(),
            }),
        })
    }

    /// Whether input passes through unchanged.
    #[cfg(test)]
    pub(crate) fn is_passthrough(&self) -> bool {
        self.inner.is_none()
    }

    /// Feeds interleaved `input` (whole frames) and hands every converted
    /// block to `sink`. Input that does not complete a chunk is retained for
    /// the next call.
    pub(crate) fn process(&mut self, mut input: &[f32], sink: &mut impl FnMut(&[f32])) {
        debug_assert_eq!(input.len() % self.channels, 0, "partial frame");
        let Some(converter) = self.inner.as_mut() else {
            if !input.is_empty() {
                sink(input);
            }
            return;
        };
        let channels = self.channels;
        while !input.is_empty() {
            let take = (converter.staging.len() - converter.staged).min(input.len());
            converter.staging[converter.staged..converter.staged + take]
                .copy_from_slice(&input[..take]);
            converter.staged += take;
            input = &input[take..];
            if converter.staged < converter.staging.len() {
                break;
            }
            converter.staged = 0;
            let frames_in = converter.staging.len() / channels;
            let frames_out_max = converter.output.len() / channels;
            let (Ok(source), Ok(mut target)) = (
                InterleavedSlice::new(&converter.staging[..], channels, frames_in),
                InterleavedSlice::new_mut(&mut converter.output[..], channels, frames_out_max),
            ) else {
                unreachable!("buffers are sized from the resampler's own limits");
            };
            match converter
                .resampler
                .process_into_buffer(&source, &mut target, None)
            {
                Ok((_, produced)) => sink(&converter.output[..produced * channels]),
                Err(_) => unreachable!("buffers are sized from the resampler's own limits"),
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Magnitude of the `frequency` component of `signal` (Goertzel), as a
    /// fraction of a full-scale sine's amplitude.
    pub(crate) fn tone_level(signal: &[f32], rate: u32, frequency: f32) -> f32 {
        let omega = 2.0 * std::f32::consts::PI * frequency / rate as f32;
        let coeff = 2.0 * omega.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in signal {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        2.0 * power.max(0.0).sqrt() / signal.len() as f32
    }

    pub(crate) fn sine(rate: u32, frequency: f32, amplitude: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| {
                amplitude * (2.0 * std::f32::consts::PI * frequency * i as f32 / rate as f32).sin()
            })
            .collect()
    }

    fn convert_all(converter: &mut RateConverter, input: &[f32], block: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for chunk in input.chunks(block) {
            converter.process(chunk, &mut |converted| out.extend_from_slice(converted));
        }
        out
    }

    #[test]
    fn equal_rates_pass_through_unchanged() {
        let mut converter = RateConverter::new(48_000, 48_000, 2, 480).unwrap();
        assert!(converter.is_passthrough());
        let input = [0.1, 0.2, 0.3, 0.4];
        assert_eq!(convert_all(&mut converter, &input, 2), input);
    }

    #[test]
    fn invalid_rates_are_rejected() {
        assert!(RateConverter::new(0, 48_000, 1, 480).is_err());
        assert!(RateConverter::new(44_100, 48_000, 0, 441).is_err());
    }

    #[test]
    fn upsampling_preserves_tone_and_produces_the_right_frame_count() {
        let mut converter = RateConverter::new(44_100, 48_000, 1, 441).unwrap();
        let input = sine(44_100, 1_000.0, 0.5, 44_100);
        // Odd block size exercises chunk staging across calls.
        let out = convert_all(&mut converter, &input, 317);
        // One second in → one second out, minus the partial final chunk.
        assert!((47_500..=48_000).contains(&out.len()), "{}", out.len());
        let steady = &out[4_800..out.len() - 480];
        let level = tone_level(steady, 48_000, 1_000.0);
        assert!((level - 0.5).abs() < 0.02, "level {level}");
        assert!(steady.iter().all(|s| s.abs() <= 0.52));
    }

    #[test]
    fn twenty_kilohertz_passband_survives_44k_and_48k_conversion() {
        for (from, to, chunk_frames) in [(44_100, 48_000, 441), (48_000, 44_100, 480)] {
            let mut converter = RateConverter::new(from, to, 1, chunk_frames).unwrap();
            let input = sine(from, 20_000.0, 0.5, from as usize * 2);
            let output = convert_all(&mut converter, &input, chunk_frames);
            let level = tone_level(&output[4_800..], to, 20_000.0);
            assert!(
                level > 0.48,
                "{from}->{to} Hz 20 kHz level {level}, expected >0.48"
            );
        }
    }

    #[test]
    fn downsampling_stereo_keeps_channels_separate() {
        let mut converter = RateConverter::new(96_000, 48_000, 2, 960).unwrap();
        let left = sine(96_000, 440.0, 0.4, 96_000);
        let right = sine(96_000, 3_000.0, 0.2, 96_000);
        let input: Vec<f32> = left
            .iter()
            .zip(&right)
            .flat_map(|(l, r)| [*l, *r])
            .collect();
        let out = convert_all(&mut converter, &input, 960 * 2);
        let l: Vec<f32> = out.iter().step_by(2).copied().skip(4_800).collect();
        let r: Vec<f32> = out.iter().skip(1).step_by(2).copied().skip(4_800).collect();
        assert!((tone_level(&l, 48_000, 440.0) - 0.4).abs() < 0.02);
        assert!(
            tone_level(&l, 48_000, 3_000.0) < 0.01,
            "right leaked into left"
        );
        assert!((tone_level(&r, 48_000, 3_000.0) - 0.2).abs() < 0.02);
        assert!(
            tone_level(&r, 48_000, 440.0) < 0.01,
            "left leaked into right"
        );
    }

    #[test]
    fn downsampling_filters_content_above_the_new_nyquist() {
        let mut converter = RateConverter::new(96_000, 48_000, 1, 960).unwrap();
        // 30 kHz would alias to 18 kHz without an anti-aliasing filter.
        let input = sine(96_000, 30_000.0, 0.5, 96_000);
        let out = convert_all(&mut converter, &input, 960);
        assert!(tone_level(&out[4_800..], 48_000, 18_000.0) < 0.01);
    }
}
