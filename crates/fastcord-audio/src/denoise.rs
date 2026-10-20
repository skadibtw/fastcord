//! Stateful 48 kHz mono microphone suppression. Stream/system audio never enters this module.
//!
//! [`Denoiser`] is the one engine interface: Off passes audio through, Light is nnnoiseless
//! (RNNoise, 0.5.2's i16-valued f32 API) and High quality is DeepFilterNet 3 on the pinned
//! upstream Rust/tract runtime with its bundled ONNX model. Off and Light are cheap and run on
//! the audio owner thread. The tract runtime is not `Send` and its graph takes far longer than
//! an audio tick to build, so a dedicated thread builds, runs and drops it (`worker`); the
//! owner only moves 480-sample blocks over bounded channels (`microphone`) and keeps the
//! previous engine running until the new one produces audio. The compressed model lives in
//! read-only executable data; its decompressed form, the graph, recurrent state and FFT
//! buffers exist only while High quality is selected.
use std::fmt;
use std::sync::Arc;

use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};
use nnnoiseless::DenoiseState;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod measure;
mod microphone;
#[cfg(test)]
pub(crate) mod quality;
#[cfg(test)]
pub(crate) mod synth;
#[cfg(test)]
mod tests;
mod worker;

pub(crate) use microphone::MicrophoneDenoiser;

pub const BLOCK_SAMPLES: usize = 480;
const PCM_SCALE: f32 = 32_768.0;

/// Builds a High quality engine on the thread that will own it.
pub(crate) type Factory = Arc<dyn Fn() -> Result<Box<dyn Denoiser>, DenoiseError> + Send + Sync>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NoiseSuppression {
    Off,
    #[default]
    Light,
    HighQuality,
}

impl NoiseSuppression {
    pub const ALL: [Self; 3] = [Self::Off, Self::Light, Self::HighQuality];

    pub const fn persisted(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Light => "light",
            Self::HighQuality => "high_quality",
        }
    }

    pub fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "light" => Some(Self::Light),
            "high_quality" => Some(Self::HighQuality),
            _ => None,
        }
    }
}

impl fmt::Display for NoiseSuppression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "Off",
            Self::Light => "Light (default)",
            Self::HighQuality => "High quality (DeepFilterNet 3)",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenoiseError {
    ModelLoad,
    UnsupportedModel,
    Inference,
    WorkerStopped,
    TooSlow,
}

impl fmt::Display for DenoiseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ModelLoad => {
                "Could not initialize DeepFilterNet 3. Select another mode or reselect High quality to retry."
            }
            Self::UnsupportedModel => {
                "The bundled denoiser model does not support 48 kHz mono / 480-sample blocks."
            }
            Self::Inference => "High quality denoising failed. Reselect High quality to retry.",
            Self::WorkerStopped => {
                "The High quality denoiser stopped unexpectedly. Reselect High quality to retry."
            }
            Self::TooSlow => {
                "High quality could not keep up with real time on this computer. Reselect High quality to retry."
            }
        })
    }
}
impl std::error::Error for DenoiseError {}

/// One block in, one block out, with an explicit algorithmic delay. The first delayed
/// output is represented by silence, never by dropping PCM or slowing the Opus clock.
/// An engine is built, used and dropped on one thread; its inference state need not be
/// `Send`.
pub trait Denoiser {
    fn mode(&self) -> NoiseSuppression;
    fn latency_samples(&self) -> usize;
    fn process(
        &mut self,
        input: &[f32; BLOCK_SAMPLES],
        output: &mut [f32; BLOCK_SAMPLES],
    ) -> Result<(), DenoiseError>;
    /// Clear delayed microphone audio without running inference (mute/device replacement).
    fn reset(&mut self) -> Result<(), DenoiseError>;
}

/// What the call is actually doing, as opposed to what was requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoiseSuppressionStatus {
    pub requested: NoiseSuppression,
    pub active: NoiseSuppression,
    /// High quality is requested and still being built, or its lookahead consumed.
    pub loading: bool,
    pub latency_samples: usize,
    pub error: Option<DenoiseError>,
}

impl Default for NoiseSuppressionStatus {
    fn default() -> Self {
        Self {
            requested: NoiseSuppression::Light,
            active: NoiseSuppression::Light,
            loading: false,
            latency_samples: BLOCK_SAMPLES,
            error: None,
        }
    }
}

struct Off;
impl Denoiser for Off {
    fn mode(&self) -> NoiseSuppression {
        NoiseSuppression::Off
    }
    fn latency_samples(&self) -> usize {
        0
    }
    fn process(
        &mut self,
        input: &[f32; BLOCK_SAMPLES],
        output: &mut [f32; BLOCK_SAMPLES],
    ) -> Result<(), DenoiseError> {
        output.copy_from_slice(input);
        Ok(())
    }
    fn reset(&mut self) -> Result<(), DenoiseError> {
        Ok(())
    }
}

struct Light {
    state: Box<DenoiseState<'static>>,
    scaled: [f32; BLOCK_SAMPLES],
    first: bool,
}
impl Light {
    fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            scaled: [0.0; BLOCK_SAMPLES],
            first: true,
        }
    }
}
impl Denoiser for Light {
    fn mode(&self) -> NoiseSuppression {
        NoiseSuppression::Light
    }
    fn latency_samples(&self) -> usize {
        BLOCK_SAMPLES
    }
    fn process(
        &mut self,
        input: &[f32; BLOCK_SAMPLES],
        output: &mut [f32; BLOCK_SAMPLES],
    ) -> Result<(), DenoiseError> {
        for (scaled, &sample) in self.scaled.iter_mut().zip(input) {
            *scaled = (sample * PCM_SCALE).clamp(-32_768.0, 32_767.0);
        }
        self.state.process_frame(output, &self.scaled);
        if self.first {
            // Upstream documents fade-in artifacts in the first, delayed result.
            output.fill(0.0);
            self.first = false;
        } else {
            for sample in output {
                *sample /= PCM_SCALE;
            }
        }
        Ok(())
    }
    fn reset(&mut self) -> Result<(), DenoiseError> {
        self.state = DenoiseState::new();
        self.scaled.fill(0.0);
        self.first = true;
        Ok(())
    }
}

struct HighQuality {
    runtime: DfTract,
    latency: usize,
}
impl HighQuality {
    /// Decompresses the bundled model and builds the tract graphs: hundreds of milliseconds
    /// of work, only ever done on a thread that owns the result.
    fn build() -> Result<Box<dyn Denoiser>, DenoiseError> {
        let runtime = std::panic::catch_unwind(|| {
            DfTract::new(DfParams::default(), &RuntimeParams::default_with_ch(1))
        })
        .map_err(|_| DenoiseError::ModelLoad)?
        .map_err(|_| DenoiseError::ModelLoad)?;
        if runtime.sr != 48_000 || runtime.ch != 1 || runtime.hop_size != BLOCK_SAMPLES {
            return Err(DenoiseError::UnsupportedModel);
        }
        // Upstream compensate-delay convention: STFT overlap + model lookahead.
        let latency = runtime.fft_size - runtime.hop_size + runtime.lookahead * runtime.hop_size;
        if latency > 4_800 {
            return Err(DenoiseError::UnsupportedModel);
        }
        Ok(Box::new(Self { runtime, latency }))
    }
}
impl Denoiser for HighQuality {
    fn mode(&self) -> NoiseSuppression {
        NoiseSuppression::HighQuality
    }
    fn latency_samples(&self) -> usize {
        self.latency
    }
    fn process(
        &mut self,
        input: &[f32; BLOCK_SAMPLES],
        output: &mut [f32; BLOCK_SAMPLES],
    ) -> Result<(), DenoiseError> {
        let input = ArrayView2::from_shape((1, BLOCK_SAMPLES), input.as_slice())
            .map_err(|_| DenoiseError::UnsupportedModel)?;
        let output = ArrayViewMut2::from_shape((1, BLOCK_SAMPLES), output.as_mut_slice())
            .map_err(|_| DenoiseError::UnsupportedModel)?;
        self.runtime
            .process(input, output)
            .map_err(|_| DenoiseError::Inference)?;
        Ok(())
    }
    fn reset(&mut self) -> Result<(), DenoiseError> {
        self.runtime.init().map_err(|_| DenoiseError::Inference)?;
        for state in &mut self.runtime.df_states {
            state.reset();
            state.init_norm_states(self.runtime.nb_df);
        }
        Ok(())
    }
}

fn off() -> Box<dyn Denoiser> {
    Box::new(Off)
}

fn light() -> Box<dyn Denoiser> {
    Box::new(Light::new())
}

/// The production High quality factory, run by the denoise worker thread.
pub(crate) fn high_quality_factory() -> Factory {
    Arc::new(HighQuality::build)
}

/// Synchronous constructor for offline fixtures and tools; the engine belongs to the
/// calling thread. A live call never builds High quality here: its worker thread does.
pub fn create(mode: NoiseSuppression) -> Result<Box<dyn Denoiser>, DenoiseError> {
    match mode {
        NoiseSuppression::Off => Ok(off()),
        NoiseSuppression::Light => Ok(light()),
        NoiseSuppression::HighQuality => HighQuality::build(),
    }
}
