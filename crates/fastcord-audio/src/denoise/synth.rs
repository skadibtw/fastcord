//! Deterministic synthetic audio for the noise-suppression fixtures (`fixtures/audio/*.wav`).
//!
//! Everything is pure `f64` math driven by a xorshift PRNG, so the committed fixtures are
//! reproducible from the parameters recorded in `fixtures/audio/manifest.json`. No third-party
//! recordings, samples or datasets are involved.
//!
//! The talker is a Klatt-style source-filter voice: a differentiated Rosenberg glottal pulse
//! train (pitch contour plus 1.2 % period jitter, aspiration noise gated by the glottal flow)
//! drives a cascade of five formant resonators whose first three centres glide between vowel
//! targets. Short fricative bursts are mixed in between syllables. A zero-phase additive
//! harmonic stack was tried first and DeepFilterNet3 classifies it as noise; this source-filter
//! voice is accepted as speech by both engines.
use std::f64::consts::{FRAC_PI_2, PI, TAU};

pub const RATE: usize = 48_000;
const RATE_F: f64 = RATE as f64;

/// Seconds to a whole sample count. Rounded, so a 1-ulp difference cannot move a boundary.
fn samples(seconds: f64) -> usize {
    (seconds * RATE_F).round() as usize
}

fn ms_samples(ms: f64) -> usize {
    samples(ms / 1000.0)
}

/// xorshift64* generator; the sequence for a seed is fixed forever.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    /// Uniform in `[0, 1)` with 24 bits of resolution.
    pub fn uniform(&mut self) -> f64 {
        f64::from(self.next_u32() >> 8) / 16_777_216.0
    }

    /// Uniform in `[-1, 1)`.
    pub fn signed(&mut self) -> f64 {
        self.uniform() * 2.0 - 1.0
    }

    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.uniform()
    }
}

#[derive(Clone, Copy)]
enum Shape {
    Bandpass,
    Lowpass,
    Highpass,
}

/// RBJ-cookbook biquad in transposed direct form II.
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn new(shape: Shape, fc: f64, q: f64) -> Self {
        let w0 = TAU * fc / RATE_F;
        let alpha = w0.sin() / (2.0 * q);
        let c = w0.cos();
        let a0 = 1.0 + alpha;
        let b = match shape {
            Shape::Bandpass => [alpha, 0.0, -alpha],
            Shape::Lowpass => [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0],
            Shape::Highpass => [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0],
        };
        Self {
            b: b.map(|v| v / a0),
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// Klatt cascade formant resonator with unity DC gain.
#[derive(Clone, Copy, Default)]
struct Resonator {
    a: f64,
    b: f64,
    c: f64,
    y1: f64,
    y2: f64,
}

impl Resonator {
    fn tune(&mut self, freq: f64, bandwidth: f64) {
        let r = (-PI * bandwidth / RATE_F).exp();
        self.c = -(r * r);
        self.b = 2.0 * r * (TAU * freq / RATE_F).cos();
        self.a = 1.0 - self.b - self.c;
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.a * x + self.b * self.y1 + self.c * self.y2;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Rosenberg glottal flow over the normalized phase `t` in `[0, 1)`.
fn glottal(t: f64) -> f64 {
    const OPEN: f64 = 0.40;
    const CLOSE: f64 = 0.16;
    if t < OPEN {
        0.5 * (1.0 - (PI * t / OPEN).cos())
    } else if t < OPEN + CLOSE {
        ((t - OPEN) / CLOSE * FRAC_PI_2).cos()
    } else {
        0.0
    }
}

/// Vowel targets (F1, F2, F3) in Hz.
const VOWELS: [[f64; 3]; 6] = [
    [730.0, 1090.0, 2440.0],
    [270.0, 2290.0, 3010.0],
    [300.0, 870.0, 2240.0],
    [530.0, 1840.0, 2480.0],
    [570.0, 840.0, 2410.0],
    [660.0, 1720.0, 2410.0],
];

/// One synthetic talker. Equal parameters always render equal samples.
pub struct Talker {
    pub seed: u64,
    pub f0_hz: f64,
    /// Scales every formant (a vocal-tract length stand-in).
    pub formant_shift: f64,
    /// RMS over the speech-active samples after normalization.
    pub rms_dbfs: f64,
    fricative_prob: f64,
    syllable_ms: (f64, f64),
    gap_ms: (f64, f64),
}

impl Talker {
    /// The close, steady reference talker.
    pub fn foreground(seed: u64, f0_hz: f64, rms_dbfs: f64) -> Self {
        Self {
            seed,
            f0_hz,
            formant_shift: 1.0,
            rms_dbfs,
            fricative_prob: 0.3,
            syllable_ms: (110.0, 220.0),
            gap_ms: (20.0, 60.0),
        }
    }

    /// A faster, higher talker meant to be mixed in at a distance.
    pub fn background(seed: u64, f0_hz: f64, formant_shift: f64, rms_dbfs: f64) -> Self {
        Self {
            seed,
            f0_hz,
            formant_shift,
            rms_dbfs,
            fricative_prob: 0.25,
            syllable_ms: (90.0, 180.0),
            gap_ms: (15.0, 40.0),
        }
    }

    /// Renders speech only inside `spans` (seconds) of a `duration_s` clip. Returns the signal
    /// and a per-sample mask of the samples that carry voiced or fricative energy.
    pub fn render(&self, duration_s: f64, spans: &[(f64, f64)]) -> (Vec<f64>, Vec<bool>) {
        let n = samples(duration_s);
        let mut out = vec![0.0; n];
        let mut active = vec![false; n];
        let mut voice = Voice::new(self);
        for &(from_s, to_s) in spans {
            let end = samples(to_s).min(n);
            let mut pos = samples(from_s);
            while pos < end {
                if voice.rng.uniform() < self.fricative_prob {
                    pos += voice.fricative(&mut out[pos..end], &mut active[pos..end]);
                    if pos >= end {
                        break;
                    }
                }
                let (syllable, gap) = voice.syllable(&mut out[pos..end], &mut active[pos..end]);
                pos += syllable + gap;
            }
        }
        let (energy, count) = out
            .iter()
            .zip(&active)
            .filter(|(_, a)| **a)
            .fold((0.0, 0usize), |(e, c), (s, _)| (e + s * s, c + 1));
        let rms = (energy / count.max(1) as f64).sqrt();
        let gain = 10f64.powf(self.rms_dbfs / 20.0) / rms.max(1e-9);
        out.iter_mut().for_each(|s| *s *= gain);
        (out, active)
    }
}

/// Running synthesis state shared by every syllable of one render.
struct Voice<'a> {
    talker: &'a Talker,
    rng: Rng,
    formants: [f64; 3],
    resonators: [Resonator; 5],
    intonation: f64,
    phase: f64,
    previous_flow: f64,
    jitter: f64,
    aspiration: Biquad,
}

impl<'a> Voice<'a> {
    fn new(talker: &'a Talker) -> Self {
        let mut rng = Rng::new(talker.seed);
        let intonation = rng.range(0.0, 6.0);
        Self {
            talker,
            rng,
            formants: VOWELS[0],
            resonators: [Resonator::default(); 5],
            intonation,
            phase: 0.0,
            previous_flow: 0.0,
            jitter: 0.0,
            aspiration: Biquad::new(Shape::Lowpass, 4000.0, 0.7),
        }
    }

    /// Band-limited noise burst with a fast attack; returns its length in samples.
    fn fricative(&mut self, out: &mut [f64], active: &mut [bool]) -> usize {
        let len = ms_samples(self.rng.range(50.0, 120.0));
        let mut band = Biquad::new(Shape::Bandpass, self.rng.range(4500.0, 8000.0), 1.2);
        let mut high = Biquad::new(Shape::Highpass, 3000.0, 0.7);
        let amp = self.rng.range(0.012, 0.03);
        for (i, (o, a)) in out.iter_mut().zip(active.iter_mut()).take(len).enumerate() {
            let t = i as f64 / len as f64;
            let env = (t * 8.0).min(1.0) * (1.0 - t).powf(0.6);
            *o += high.step(band.step(self.rng.signed())) * amp * env;
            *a = true;
        }
        len
    }

    /// Voiced syllable gliding to a random vowel; returns `(length, following gap)` in samples.
    fn syllable(&mut self, out: &mut [f64], active: &mut [bool]) -> (usize, usize) {
        let talker = self.talker;
        let len = ms_samples(self.rng.range(talker.syllable_ms.0, talker.syllable_ms.1));
        let vowel = VOWELS[(self.rng.next_u32() % VOWELS.len() as u32) as usize];
        let target = vowel.map(|f| f * talker.formant_shift);
        let from = self.formants;
        let f0_start = talker.f0_hz * (1.0 + 0.12 * self.intonation.sin());
        self.intonation += self.rng.range(0.4, 1.3);
        let f0_end = talker.f0_hz * (1.0 + 0.12 * self.intonation.sin());
        let glide = (len as f64 * 0.3) as usize;
        let breath = self.rng.range(0.02, 0.06);
        for (i, (o, a)) in out.iter_mut().zip(active.iter_mut()).take(len).enumerate() {
            let t = i as f64 / len as f64;
            let mix = if i < glide {
                i as f64 / glide as f64
            } else {
                1.0
            };
            self.formants = std::array::from_fn(|k| from[k] + (target[k] - from[k]) * mix);
            let shift = talker.formant_shift;
            let [f1, f2, f3] = self.formants;
            self.resonators[0].tune(f1, 80.0 + 40.0 * (1.0 - t));
            self.resonators[1].tune(f2, 100.0);
            self.resonators[2].tune(f3, 150.0);
            self.resonators[3].tune(3500.0 * shift, 250.0);
            self.resonators[4].tune(4500.0 * shift, 300.0);
            let f0 = (f0_start + (f0_end - f0_start) * t) * (1.0 + self.jitter);
            self.phase += f0 / RATE_F;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
                self.jitter = self.rng.signed() * 0.012;
            }
            let flow = glottal(self.phase);
            let differentiated = flow - self.previous_flow;
            self.previous_flow = flow;
            let aspiration = self.aspiration.step(self.rng.signed()) * breath * flow.max(0.15);
            let envelope = (t * 12.0).min(1.0) * (1.0 - t).powf(0.35);
            let source = (differentiated * 40.0 + aspiration) * envelope;
            *o += self.resonators.iter_mut().fold(source, |x, r| r.step(x));
            *a = true;
        }
        (
            len,
            ms_samples(self.rng.range(talker.gap_ms.0, talker.gap_ms.1)),
        )
    }
}

/// Parameters of the clean reference talker shared by every fixture.
pub struct SpeechRecipe {
    pub duration_s: f64,
    pub intervals_s: Vec<(f64, f64)>,
    pub seed: u64,
    pub f0_hz: f64,
    pub rms_dbfs: f64,
}

impl SpeechRecipe {
    pub fn committed() -> Self {
        Self {
            duration_s: 3.0,
            intervals_s: vec![(0.45, 1.20), (1.75, 2.40)],
            seed: 1,
            f0_hz: 125.0,
            rms_dbfs: -24.0,
        }
    }

    /// The clean reference and its speech-active mask.
    pub fn render(&self) -> (Vec<f64>, Vec<bool>) {
        Talker::foreground(self.seed, self.f0_hz, self.rms_dbfs)
            .render(self.duration_s, &self.intervals_s)
    }
}

/// Pink noise (Paul Kellet filter) with a stable spectrum; roughly unit-scale before gain.
fn pink_noise(seed: u64, n: usize) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let mut b = [0.0f64; 7];
    (0..n)
        .map(|_| {
            let w = rng.signed();
            b[0] = 0.99886 * b[0] + w * 0.0555179;
            b[1] = 0.99332 * b[1] + w * 0.0750759;
            b[2] = 0.96900 * b[2] + w * 0.153_852;
            b[3] = 0.86650 * b[3] + w * 0.3104856;
            b[4] = 0.55000 * b[4] + w * 0.5329522;
            b[5] = -0.7616 * b[5] - w * 0.016898;
            let p = b[..6].iter().sum::<f64>() + b[6] + w * 0.5362;
            b[6] = w * 0.115926;
            p * 0.11
        })
        .collect()
}

/// Steady pink noise mixed at a fixed SNR over the speech-active samples.
pub struct NoisySpeechRecipe {
    pub noise_seed: u64,
    pub snr_db: f64,
}

impl NoisySpeechRecipe {
    pub fn committed() -> Self {
        Self {
            noise_seed: 7,
            snr_db: 5.0,
        }
    }

    pub fn render(&self, clean: &[f64], active: &[bool]) -> Vec<f64> {
        let noise = pink_noise(self.noise_seed, clean.len());
        let (speech, count) = clean
            .iter()
            .zip(active)
            .filter(|(_, a)| **a)
            .fold((0.0, 0usize), |(e, c), (s, _)| (e + s * s, c + 1));
        let speech_power = speech / count.max(1) as f64;
        let noise_power = noise.iter().map(|v| v * v).sum::<f64>() / noise.len().max(1) as f64;
        let gain = (speech_power / noise_power / 10f64.powf(self.snr_db / 10.0)).sqrt();
        clean
            .iter()
            .zip(&noise)
            .map(|(s, v)| s + v * gain)
            .collect()
    }
}

/// Keyboard clicks (`keys_per_s` scales the nominal burst-typing gap distribution, nominal 6).
///
/// Each key is a 2.2-5.2 kHz bandpass-noise click with a 2.2 ms decay plus a 140-260 Hz damped
/// "thock", 18 ms long, at 50-100 % of `peak`.
fn keyboard(seed: u64, n: usize, keys_per_s: f64, peak: f64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let mut out = vec![0.0; n];
    let mut t = rng.range(0.1, 0.4);
    while samples(t) + 2000 < n {
        let start = samples(t);
        let amp = peak * rng.range(0.5, 1.0);
        let mut click_band = Biquad::new(Shape::Bandpass, rng.range(2200.0, 5200.0), 1.6);
        let thock_hz = rng.range(140.0, 260.0);
        for (i, o) in out[start..].iter_mut().take(ms_samples(18.0)).enumerate() {
            let tt = i as f64 / RATE_F;
            let click = click_band.step(rng.signed()) * (-tt / 0.0022).exp() * 3.0;
            let thock = (TAU * thock_hz * tt).sin() * (-tt / 0.008).exp() * 0.6;
            *o += (click + thock) * amp;
        }
        let gap = if rng.uniform() < 0.2 {
            rng.range(0.25, 0.6)
        } else {
            rng.range(0.07, 0.2)
        };
        t += gap * (6.0 / keys_per_s);
    }
    out
}

/// A second talker heard from across a room: a Schroeder comb reverb dominated by the diffuse
/// tail (direct path 0.3, wet path 2.0 before the comb gains, roughly a -10 dB direct-to-
/// reverberant ratio), a 4.5 kHz low-pass, and a final normalization so `rms_dbfs` is the level
/// of the whole clip as heard. The voice runs through the clip, so there is no digital silence.
fn distant_voice(talker: &Talker, duration_s: f64) -> Vec<f64> {
    const DIRECT: f64 = 0.3;
    const WET: f64 = 2.0;
    /// (delay in ms, feedback): mutually non-harmonic delays give a dense, noise-like tail.
    const COMBS: [(f64, f64); 4] = [(29.7, 0.805), (37.1, 0.827), (41.1, 0.783), (43.7, 0.764)];
    let (dry, _) = talker.render(duration_s, &[(0.0, duration_s)]);
    let mut mixed: Vec<f64> = dry.iter().map(|v| v * DIRECT).collect();
    for (delay_ms, feedback) in COMBS {
        let delay = ms_samples(delay_ms);
        let mut comb = dry.clone();
        for i in delay..comb.len() {
            comb[i] += feedback * comb[i - delay];
        }
        let level = (1.0 - feedback) * WET;
        mixed
            .iter_mut()
            .zip(&comb)
            .for_each(|(m, c)| *m += c * level);
    }
    let mut low = Biquad::new(Shape::Lowpass, 4500.0, 0.7);
    let mut out: Vec<f64> = mixed.iter().map(|&v| low.step(v)).collect();
    let rms = (out.iter().map(|v| v * v).sum::<f64>() / out.len().max(1) as f64).sqrt();
    let gain = 10f64.powf(talker.rms_dbfs / 20.0) / rms.max(1e-9);
    out.iter_mut().for_each(|v| *v *= gain);
    out
}

/// Pink noise normalized to `rms_dbfs` over the whole clip.
fn scaled_pink_noise(seed: u64, n: usize, rms_dbfs: f64) -> Vec<f64> {
    let mut noise = pink_noise(seed, n);
    let rms = (noise.iter().map(|v| v * v).sum::<f64>() / n.max(1) as f64).sqrt();
    let gain = 10f64.powf(rms_dbfs / 20.0) / rms.max(1e-9);
    noise.iter_mut().for_each(|v| *v *= gain);
    noise
}

/// Keyboard clicks plus a distant background talker: the interference RNNoise passes through.
pub struct KeyboardVoiceRecipe {
    pub keyboard_seed: u64,
    pub keyboard_keys_per_s: f64,
    pub keyboard_peak: f64,
    pub background_seed: u64,
    pub background_f0_hz: f64,
    pub background_formant_shift: f64,
    pub background_rms_dbfs: f64,
    /// Room tone: steady pink noise that keeps every engine block above digital silence.
    pub room_seed: u64,
    pub room_rms_dbfs: f64,
}

impl KeyboardVoiceRecipe {
    pub fn committed() -> Self {
        Self {
            keyboard_seed: 5,
            keyboard_keys_per_s: 6.0,
            keyboard_peak: 0.3,
            background_seed: 99,
            background_f0_hz: 205.0,
            background_formant_shift: 1.15,
            background_rms_dbfs: -36.0,
            room_seed: 11,
            room_rms_dbfs: -40.0,
        }
    }

    pub fn render(&self, clean: &[f64], duration_s: f64) -> Vec<f64> {
        let keys = keyboard(
            self.keyboard_seed,
            clean.len(),
            self.keyboard_keys_per_s,
            self.keyboard_peak,
        );
        let background = distant_voice(
            &Talker::background(
                self.background_seed,
                self.background_f0_hz,
                self.background_formant_shift,
                self.background_rms_dbfs,
            ),
            duration_s,
        );
        let room = scaled_pink_noise(self.room_seed, clean.len(), self.room_rms_dbfs);
        clean
            .iter()
            .zip(&keys)
            .zip(&background)
            .zip(&room)
            .map(|(((c, k), b), r)| c + k + b + r)
            .collect()
    }
}

/// Rounds to 16-bit PCM, saturating (the fixtures stay far below full scale).
pub fn to_pcm16(signal: &[f64]) -> Vec<i16> {
    signal
        .iter()
        .map(|v| (v * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16)
        .collect()
}

/// Canonical 44-byte-header RIFF/WAVE, PCM, mono, 16-bit, 48 kHz.
pub fn wav_bytes(pcm: &[i16]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + pcm.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&(RATE as u32).to_le_bytes());
    bytes.extend_from_slice(&(RATE as u32 * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    bytes.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
    bytes
}
