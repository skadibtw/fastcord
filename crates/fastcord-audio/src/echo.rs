//! Acoustic echo cancellation for the microphone path (pure Rust, no native dependency).
//!
//! The reference is what the output device actually played: [`ReferenceTap`] copies every
//! sample the playback pipeline writes into the output ring, and releases it to the canceller
//! only when the ring has been consumed, so the reference follows the output device clock
//! and is silent while nothing plays (wall clock fills idle time).
//!
//! [`EchoCanceller`] is a partitioned-block frequency-domain adaptive filter (overlap-save,
//! 240-sample hop, 512-point FFT, 64 partitions = 320 ms of echo path), the same structure as
//! the MDF/PBFDAF family. Delay is not estimated separately: partitions that cover the actual
//! round-trip delay converge, the rest stay near zero. A fixed 20 ms lead on the reference
//! absorbs the jitter between the two device callback clocks; real round trips (device
//! buffers plus propagation) are always longer than that.
//!
//! Double-talk protection: adaptation is frozen when the microphone is not explained by the
//! filter output (low correlation) after the filter has converged, and the output is never
//! allowed to be louder than the raw microphone. Everything is allocated up front (about
//! 0.3 MiB); processing never allocates and never runs in a device callback.
use std::collections::VecDeque;
use std::time::Instant;

use realfft::num_complex::Complex;

#[cfg(test)]
mod quality;
/// Partitions whose gradient constraint is applied per step (more cost no gain: measured).
const CONSTRAINED_PER_STEP: usize = 1;
/// The residual-echo estimate is this multiple of <error, estimate> (Speex MDF's factor 3:
/// the correlation under-reads echo spread over taps the estimate has not learned yet).
const SHARE_GAIN: f32 = 3.0;
/// Weight of the estimate in the microphone (<microphone, estimate> / |estimate|^2) below
/// which the filter is judged wrong, rising to [`MISMATCH_SHARE`] of the adaptation range at
/// [`MISMATCH_FULL`].
const MISMATCH_START: f32 = 0.85;
const MISMATCH_FULL: f32 = 0.35;
const MISMATCH_SHARE: f32 = 0.6;

use crate::codec::SAMPLE_RATE;
use crate::resample::RateConverter;

/// Samples per adaptation step (5 ms at 48 kHz); two per 480-sample microphone block.
const HOP: usize = 240;
const FFT: usize = 512;
const BINS: usize = FFT / 2 + 1;
const PARTITIONS: usize = 64;
/// Microphone block size accepted by [`EchoCanceller::process`].
pub(crate) const FRAME: usize = 480;
/// Standing reference lead (20 ms). The reference is only consumed after this much is queued.
const LEAD: usize = 960;
/// Reference queue limit: lead plus 100 ms; the oldest samples are dropped beyond it.
const FAR_LIMIT: usize = LEAD + 4_800;
/// Mean-square level (about -60 dBFS) below which the reference carries no information.
const FAR_ACTIVE: f32 = 1e-6;
const REGULARIZATION: f32 = 1e-2;
/// Adaptation step range: the floor keeps tracking the room, the ceiling is the NLMS limit.
const MU_MIN: f32 = 0.03;
const MU_MAX: f32 = 0.9;
/// Hops of exactly silent reference after which the window and every
/// partition's spectrum are zero, so the estimate is exactly zero and a step
/// is the identity: the window spans `FFT` samples, the history `PARTITIONS`
/// windows.
const SILENT_SKIP: usize = PARTITIONS + FFT.div_ceil(HOP);

/// 512-point real transforms (SIMD through realfft/rustfft). Plans and
/// scratch are allocated in `new`; transforms never allocate.
struct Fft {
    forward: std::sync::Arc<dyn realfft::RealToComplex<f32>>,
    inverse: std::sync::Arc<dyn realfft::ComplexToReal<f32>>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
}

impl Fft {
    fn new() -> Self {
        let mut planner = realfft::RealFftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(FFT);
        let inverse = planner.plan_fft_inverse(FFT);
        let scratch = forward.get_scratch_len().max(inverse.get_scratch_len());
        Self {
            forward,
            inverse,
            spectrum: vec![Complex::default(); BINS],
            scratch: vec![Complex::default(); scratch],
        }
    }

    /// Spectrum of `time` (which is used as scratch) into `re`/`im`.
    fn forward(&mut self, time: &mut [f32; FFT], re: &mut [f32], im: &mut [f32]) {
        // Lengths are fixed at construction, so this cannot fail.
        let _ = self
            .forward
            .process_with_scratch(time, &mut self.spectrum, &mut self.scratch);
        for (k, bin) in self.spectrum.iter().enumerate() {
            re[k] = bin.re;
            im[k] = bin.im;
        }
    }

    /// Real signal of the half spectrum `re`/`im`, scaled by 1/N.
    fn inverse(&mut self, re: &[f32], im: &[f32], time: &mut [f32; FFT]) {
        for (k, bin) in self.spectrum.iter_mut().enumerate() {
            *bin = Complex::new(re[k], im[k]);
        }
        // DC and Nyquist of a real signal have no imaginary part.
        self.spectrum[0].im = 0.0;
        self.spectrum[BINS - 1].im = 0.0;
        let _ = self
            .inverse
            .process_with_scratch(&mut self.spectrum, time, &mut self.scratch);
        let scale = 1.0 / FFT as f32;
        time.iter_mut().for_each(|v| *v *= scale);
    }
}

pub(crate) struct EchoCanceller {
    fft: Fft,
    /// Last `FFT` reference samples.
    window: [f32; FFT],
    /// Reference spectra history, newest at `head`, and the filter, indexed by partition.
    xr: Box<[f32]>,
    xi: Box<[f32]>,
    wr: Box<[f32]>,
    wi: Box<[f32]>,
    head: usize,
    next_constrained: usize,
    time: Box<[f32; FFT]>,
    yr: [f32; BINS],
    yi: [f32; BINS],
    /// Error spectrum of the current step.
    er: [f32; BINS],
    ei: [f32; BINS],
    /// Running sum over partitions of |X|^2 per bin.
    power: [f32; BINS],
    /// Per-bin step size `mu / (power + REGULARIZATION)`.
    gain: [f32; BINS],
    /// Inserts since `power` was last summed from scratch (bounds f32 drift).
    power_age: usize,
    far: VecDeque<f32>,
    // Smoothed energies for the double-talk decision.
    s_near: f32,
    s_echo: f32,
    /// Smoothed <error, estimate>: the echo the filter missed, which the estimate predicts.
    s_residual: f32,
    /// Smoothed <microphone, estimate>: equals the estimate's energy when it is right.
    s_cross: f32,
    s_error: f32,
    /// Microphone and residual energy, smoothed over about a second of far-end speech only,
    /// for the reduction shown in diagnostics.
    d_near: f32,
    d_error: f32,
    /// The filter explains the microphone (diagnostics only; it no longer gates adaptation).
    converged: bool,
    /// Steps of faster adaptation left after [`EchoCanceller::path_changed`].
    boost_steps: u32,
    /// Consecutive hops whose reference was exactly zero.
    silent_hops: usize,
}

/// Fast adaptation after the reference and microphone streams were re-aligned by chance:
/// 400 steps of 5 ms.
const BOOST_STEPS: u32 = 400;

impl EchoCanceller {
    pub(crate) fn new() -> Self {
        let spectra = || vec![0.0; PARTITIONS * BINS].into_boxed_slice();
        Self {
            fft: Fft::new(),
            window: [0.0; FFT],
            xr: spectra(),
            xi: spectra(),
            wr: spectra(),
            wi: spectra(),
            head: 0,
            next_constrained: 0,
            time: Box::new([0.0; FFT]),
            yr: [0.0; BINS],
            yi: [0.0; BINS],
            er: [0.0; BINS],
            ei: [0.0; BINS],
            power: [0.0; BINS],
            gain: [0.0; BINS],
            power_age: 0,
            far: VecDeque::with_capacity(FAR_LIMIT + FRAME),
            s_near: 0.0,
            s_echo: 0.0,
            s_residual: 0.0,
            s_cross: 0.0,
            s_error: 0.0,
            d_near: 0.0,
            d_error: 0.0,
            converged: false,
            boost_steps: 0,
            silent_hops: 0,
        }
    }

    /// Drops queued reference without forgetting the filter (microphone muted).
    pub(crate) fn clear_far(&mut self) {
        self.far.clear();
    }

    /// The microphone stream restarted (unmute, push-to-talk), so the reference pairing moved
    /// by up to a device period relative to what the filter learned. Forget the "converged"
    /// verdict (which would hold adaptation for two seconds) and adapt fast for a second.
    pub(crate) fn path_changed(&mut self) {
        self.converged = false;
        self.boost_steps = BOOST_STEPS;
    }

    /// What the filter has found, for the diagnostics view. Scans the filter once; call it
    /// at a few hertz at most.
    pub(crate) fn diagnostics(&self) -> crate::level::EchoDiagnostics {
        let mut strongest = (0usize, 0.0f32);
        for partition in 0..PARTITIONS {
            let base = partition * BINS;
            let energy: f32 = (base..base + BINS)
                .map(|k| self.wr[k] * self.wr[k] + self.wi[k] * self.wi[k])
                .sum();
            if energy > strongest.1 {
                strongest = (partition, energy);
            }
        }
        let found = strongest.1 > 1e-4;
        let lag = (strongest.0 * HOP + HOP / 2 + LEAD) * 1000 / SAMPLE_RATE as usize;
        let reduction = if self.d_near > 1e-9 && self.d_error > 0.0 {
            (10.0 * (self.d_near / self.d_error.max(1e-12)).log10()).clamp(0.0, 60.0)
        } else {
            0.0
        };
        crate::level::EchoDiagnostics {
            reduction_tenths_db: (reduction * 10.0).round() as i16,
            delay_ms: if found { lag as u16 } else { 0 },
            converged: self.converged,
        }
    }

    pub(crate) fn push_far(&mut self, samples: &[f32]) {
        let samples = &samples[samples.len().saturating_sub(FAR_LIMIT)..];
        let excess = (self.far.len() + samples.len()).saturating_sub(FAR_LIMIT);
        self.far.drain(..excess);
        self.far.extend(samples.iter().copied());
    }

    pub(crate) fn push_far_silence(&mut self, mut count: usize) {
        const ZEROS: [f32; FRAME] = [0.0; FRAME];
        while count > 0 {
            let take = count.min(FRAME);
            self.push_far(&ZEROS[..take]);
            count -= take;
        }
    }

    /// Removes the echo of the queued reference from one 10 ms microphone block.
    pub(crate) fn process(&mut self, near: &[f32; FRAME], out: &mut [f32; FRAME]) {
        if self.far.len() < FRAME {
            // Prime (or re-prime after a stall) with the standing lead.
            let missing = FRAME + LEAD - self.far.len();
            for _ in 0..missing {
                self.far.push_front(0.0);
            }
        }
        let mut far = [0.0; FRAME];
        for (slot, sample) in far.iter_mut().zip(self.far.drain(..FRAME)) {
            *slot = sample;
        }
        for half in 0..FRAME / HOP {
            let range = half * HOP..(half + 1) * HOP;
            self.step(&near[range.clone()], &far[range.clone()], &mut out[range]);
        }
    }

    fn step(&mut self, near: &[f32], far: &[f32], out: &mut [f32]) {
        // Nobody else is talking for most of a call. Once the reference has
        // been exactly silent for the whole history, every spectrum is zero,
        // the estimate is zero and the step returns the microphone unchanged
        // without adapting: skip the FFTs, the result is bit-identical.
        if far.iter().all(|sample| *sample == 0.0) {
            self.silent_hops = self.silent_hops.saturating_add(1);
            if self.silent_hops > SILENT_SKIP {
                self.head = (self.head + PARTITIONS - 1) % PARTITIONS;
                out.copy_from_slice(near);
                return;
            }
        } else {
            self.silent_hops = 0;
        }
        self.window.copy_within(HOP.., 0);
        self.window[FFT - HOP..].copy_from_slice(far);
        self.time.copy_from_slice(&self.window);
        self.head = (self.head + PARTITIONS - 1) % PARTITIONS;
        let newest = self.head * BINS..(self.head + 1) * BINS;
        // The slot being replaced is the partition that drops off the sum.
        for k in 0..BINS {
            let at = newest.start + k;
            self.power[k] -= self.xr[at].powi(2) + self.xi[at].powi(2);
        }
        self.fft.forward(
            &mut self.time,
            &mut self.xr[newest.clone()],
            &mut self.xi[newest.clone()],
        );
        self.update_power(newest.start);

        // Echo estimate: sum over partitions of W_p * X(t - p).
        self.yr.fill(0.0);
        self.yi.fill(0.0);
        for p in 0..PARTITIONS {
            let x = ((self.head + p) % PARTITIONS) * BINS;
            let w = p * BINS;
            let (xr, xi) = (&self.xr[x..x + BINS], &self.xi[x..x + BINS]);
            let (wr, wi) = (&self.wr[w..w + BINS], &self.wi[w..w + BINS]);
            for k in 0..BINS {
                self.yr[k] += xr[k] * wr[k] - xi[k] * wi[k];
                self.yi[k] += xr[k] * wi[k] + xi[k] * wr[k];
            }
        }
        self.fft.inverse(&self.yr, &self.yi, &mut self.time);

        let (mut p_near, mut p_echo, mut p_residual, mut p_cross, mut p_error) =
            (0.0, 0.0, 0.0, 0.0, 0.0);
        let mut error = [0.0; HOP];
        for i in 0..HOP {
            let echo = self.time[FFT - HOP + i];
            error[i] = near[i] - echo;
            p_near += near[i] * near[i];
            p_echo += echo * echo;
            p_residual += error[i] * echo;
            p_cross += near[i] * echo;
            p_error += error[i] * error[i];
        }
        // Never louder than the microphone (diverged filter, double talk).
        // ponytail: hard per-step switch, a cross-fade if it is ever audible.
        let diverged = p_error > 2.0 * p_near + 1e-9;
        out.copy_from_slice(if diverged { near } else { &error });

        let far_active = self.window.iter().map(|s| s * s).sum::<f32>() / FFT as f32 > FAR_ACTIVE;
        // The statistics follow far-end speech only, so a pause keeps what was learned (they
        // would otherwise decay to "nothing known" and the next onset would adapt wildly).
        let mut mu = 0.0;
        if far_active {
            const SMOOTH: f32 = 0.9;
            self.s_near = SMOOTH * self.s_near + (1.0 - SMOOTH) * p_near;
            self.s_echo = SMOOTH * self.s_echo + (1.0 - SMOOTH) * p_echo;
            self.s_residual = SMOOTH * self.s_residual + (1.0 - SMOOTH) * p_residual;
            self.s_cross = SMOOTH * self.s_cross + (1.0 - SMOOTH) * p_cross;
            self.s_error = SMOOTH * self.s_error + (1.0 - SMOOTH) * p_error;
            const DIAGNOSTIC_SMOOTH: f32 = 0.995;
            self.d_near = DIAGNOSTIC_SMOOTH * self.d_near + (1.0 - DIAGNOSTIC_SMOOTH) * p_near;
            self.d_error = DIAGNOSTIC_SMOOTH * self.d_error + (1.0 - DIAGNOSTIC_SMOOTH) * p_error;

            // The error is the echo the filter missed plus whatever the near end adds. The
            // missed echo is correlated with the estimate (estimate = a x echo leaves
            // (1 - a) x echo, so <error, estimate>^2 / |estimate|^2 is its energy); the near
            // end is not. Adapt in proportion to the missed echo's share of the error: fast
            // while the filter is wrong (start, path change), slow while a talker is heard.
            let mut missed_share = if self.s_echo < 1e-3 * self.s_near {
                1.0 // nothing learned yet
            } else {
                (SHARE_GAIN * self.s_residual.max(0.0) / (self.s_error + 1e-12)).clamp(0.0, 1.0)
            };
            // A filter that is simply wrong (the reference moved: an underrun, an unmute, a
            // device change) leaves an estimate that does not even appear in the microphone,
            // and the correlation above then reads "no echo". The estimate of a working
            // filter appears in the microphone with weight 1 even beside a talker.
            if self.s_echo > 0.02 * self.s_near {
                let weight = self.s_cross / self.s_echo;
                let mismatch =
                    ((MISMATCH_START - weight) / (MISMATCH_START - MISMATCH_FULL)).clamp(0.0, 1.0);
                missed_share = missed_share.max(MISMATCH_SHARE * mismatch);
            }
            mu = MU_MIN + (MU_MAX - MU_MIN) * missed_share;
            if self.s_error < 0.25 * self.s_near {
                self.converged = true;
            } else if self.s_error > 0.7 * self.s_near {
                self.converged = false;
            }
        }
        if self.boost_steps > 0 && far_active {
            self.boost_steps -= 1;
            mu = mu.max(0.5);
        }
        if mu > 0.0 {
            self.adapt(&error, mu);
        }
    }

    /// Adds the spectrum just written at `newest` to `power` (the dropped
    /// partition was subtracted before it was overwritten). Every `PARTITIONS`
    /// inserts the sum is redone from scratch, which bounds rounding drift.
    fn update_power(&mut self, newest: usize) {
        self.power_age += 1;
        if self.power_age >= PARTITIONS {
            self.power_age = 0;
            self.power.fill(0.0);
            for slot in 0..PARTITIONS {
                let x = slot * BINS;
                for k in 0..BINS {
                    self.power[k] += self.xr[x + k].powi(2) + self.xi[x + k].powi(2);
                }
            }
        } else {
            for k in 0..BINS {
                let added = self.xr[newest + k].powi(2) + self.xi[newest + k].powi(2);
                self.power[k] = (self.power[k] + added).max(0.0);
            }
        }
    }

    fn adapt(&mut self, error: &[f32; HOP], mu: f32) {
        self.time[..FFT - HOP].fill(0.0);
        self.time[FFT - HOP..].copy_from_slice(error);
        self.fft.forward(&mut self.time, &mut self.er, &mut self.ei);
        // One division per bin, not per (partition, bin).
        for (gain, power) in self.gain.iter_mut().zip(&self.power) {
            *gain = mu / (*power + REGULARIZATION);
        }
        for p in 0..PARTITIONS {
            let x = ((self.head + p) % PARTITIONS) * BINS;
            let w = p * BINS;
            for k in 0..BINS {
                let gain = self.gain[k];
                let (xr, xi) = (self.xr[x + k], self.xi[x + k]);
                self.wr[w + k] += gain * (xr * self.er[k] + xi * self.ei[k]);
                self.wi[w + k] += gain * (xr * self.ei[k] - xi * self.er[k]);
            }
        }
        // Gradient constraint (keep each partition `HOP` taps long), a few partitions per step.
        for _ in 0..CONSTRAINED_PER_STEP {
            let w = self.next_constrained * BINS..(self.next_constrained + 1) * BINS;
            self.next_constrained = (self.next_constrained + 1) % PARTITIONS;
            self.fft
                .inverse(&self.wr[w.clone()], &self.wi[w.clone()], &mut self.time);
            self.time[HOP..].fill(0.0);
            self.fft
                .forward(&mut self.time, &mut self.wr[w.clone()], &mut self.wi[w]);
        }
    }
}

/// Copies what the playback pipeline writes to the output ring and releases it, resampled to
/// 48 kHz mono, as the device consumes it. Owned by the playback pipeline.
pub(crate) struct ReferenceTap {
    /// Device-rate mono samples written to the ring and not yet consumed by the device.
    queued: VecDeque<f32>,
    converter: RateConverter,
    chunk: Box<[f32]>,
    last: Instant,
    owed_silence: f64,
}

impl ReferenceTap {
    pub(crate) fn new(rate: u32, ring_frames: usize) -> Result<Self, crate::UnsupportedRate> {
        let chunk = crate::pipeline::frames_for(rate, 10);
        Ok(Self {
            queued: VecDeque::with_capacity(ring_frames),
            converter: RateConverter::new(rate, SAMPLE_RATE, 1, chunk)?,
            chunk: vec![0.0; chunk].into_boxed_slice(),
            last: Instant::now(),
            owed_silence: 0.0,
        })
    }

    pub(crate) fn push(&mut self, mono: f32) {
        self.queued.push_back(mono);
    }

    /// Forgets queued samples when the ring itself was cleared.
    pub(crate) fn clear(&mut self) {
        self.queued.clear();
    }

    /// Releases the frames the device consumed since the last call (`unread` frames remain in
    /// the ring), or, while the device is idle, wall-clock silence.
    pub(crate) fn advance(&mut self, unread: usize, now: Instant, echo: &mut EchoCanceller) {
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        let mut consumed = self.queued.len().saturating_sub(unread);
        if consumed == 0 && unread == 0 {
            self.owed_silence = (self.owed_silence + elapsed * f64::from(SAMPLE_RATE)).min(4_800.0);
            let whole = self.owed_silence as usize;
            self.owed_silence -= whole as f64;
            echo.push_far_silence(whole);
            return;
        }
        self.owed_silence = 0.0;
        while consumed > 0 {
            let take = consumed.min(self.chunk.len());
            for (slot, sample) in self.chunk.iter_mut().zip(self.queued.drain(..take)) {
                *slot = sample;
            }
            consumed -= take;
            self.converter
                .process(&self.chunk[..take], &mut |block: &[f32]| {
                    echo.push_far(block)
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// After the reference has been silent for the whole history the step
    /// is skipped and the microphone passes through untouched; the filter
    /// learned before the pause is kept for when the far end speaks again.
    #[test]
    fn a_silent_reference_skips_the_work_and_passes_the_microphone() {
        let mut canceller = EchoCanceller::new();
        let mut noise = Noise(7);
        let mut out = [0.0; FRAME];
        for _ in 0..100 {
            let block: Vec<f32> = noise.speech(FRAME, 0.3);
            canceller.push_far(&block);
            let near: [f32; FRAME] = std::array::from_fn(|i| block[i] * 0.5);
            canceller.process(&near, &mut out);
        }
        let mut learned = None;
        let mut skipped = 0;
        for _ in 0..200 {
            canceller.push_far_silence(FRAME);
            let near: [f32; FRAME] = std::array::from_fn(|_| noise.next() * 0.1);
            canceller.process(&near, &mut out);
            if canceller.silent_hops > SILENT_SKIP {
                skipped += 1;
                assert_eq!(out, near, "a skipped step is the identity");
                learned.get_or_insert_with(|| canceller.wr.clone());
            }
        }
        assert!(skipped > 100, "{skipped}");
        assert_eq!(
            Some(canceller.wr),
            learned,
            "skipped steps do not adapt the filter"
        );
    }

    struct Noise(u64);
    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 40) as f32 / (1u64 << 23) as f32) - 1.0
        }
        /// Speech-like: noise with a slowly varying envelope, pauses included.
        fn speech(&mut self, len: usize, level: f32) -> Vec<f32> {
            let mut lowpass = 0.0;
            (0..len)
                .map(|i| {
                    lowpass += 0.6 * (self.next() - lowpass);
                    let envelope = (0.5
                        + 0.5 * (i as f32 * 2.0 * std::f32::consts::PI / 9_600.0).sin())
                    .powi(2);
                    lowpass * envelope * level
                })
                .collect()
        }
    }

    /// 100 ms decaying room response with a 40 ms bulk delay.
    fn echo_path(seed: u64) -> Vec<f32> {
        let mut noise = Noise(seed);
        let mut taps = vec![0.0; 1_920 + 4_800];
        for (i, tap) in taps[1_920..].iter_mut().enumerate() {
            *tap = noise.next() * 0.5 * (-(i as f32) / 900.0).exp();
        }
        taps[1_920] = 0.5;
        taps
    }

    fn convolve(signal: &[f32], taps: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0; signal.len()];
        for (n, slot) in out.iter_mut().enumerate() {
            let reach = taps.len().min(n + 1);
            *slot = (0..reach).map(|m| taps[m] * signal[n - m]).sum();
        }
        out
    }

    fn energy(signal: &[f32]) -> f64 {
        signal.iter().map(|&s| f64::from(s).powi(2)).sum()
    }

    fn db(numerator: f64, denominator: f64) -> f64 {
        10.0 * (numerator / denominator.max(1e-20)).log10()
    }

    /// Runs `near` through the canceller one block at a time, `far` fed ahead of it.
    fn run(aec: &mut EchoCanceller, far: &[f32], near: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(near.len());
        let mut block = [0.0; FRAME];
        for (far, near) in far
            .as_chunks::<FRAME>()
            .0
            .iter()
            .zip(near.as_chunks::<FRAME>().0)
        {
            aec.push_far(far);
            aec.process(near, &mut block);
            out.extend_from_slice(&block);
        }
        out
    }

    #[test]
    fn fft_round_trips() {
        let mut fft = Fft::new();
        let mut noise = Noise(1);
        let mut time = Box::new([0.0; FFT]);
        time.iter_mut().for_each(|v| *v = noise.next());
        let original = *time;
        let (mut re, mut im) = ([0.0; BINS], [0.0; BINS]);
        fft.forward(&mut time, &mut re, &mut im);
        fft.inverse(&re, &im, &mut time);
        assert!(time.iter().zip(original).all(|(a, b)| (a - b).abs() < 1e-4));
    }

    #[test]
    fn the_running_reference_power_matches_a_full_sum() {
        let mut aec = EchoCanceller::new();
        let far = Noise(3).speech(48_000, 0.5);
        // Not a multiple of PARTITIONS hops, so the last full resum is behind.
        run(&mut aec, &far[..FRAME * 90], &far[..FRAME * 90]);
        for k in 0..BINS {
            let full: f32 = (0..PARTITIONS)
                .map(|p| aec.xr[p * BINS + k].powi(2) + aec.xi[p * BINS + k].powi(2))
                .sum();
            assert!(
                (aec.power[k] - full).abs() <= 1e-3 * full.max(1.0),
                "bin {k}: running {} vs full {full}",
                aec.power[k]
            );
        }
    }

    #[test]
    fn synthetic_echo_is_cancelled_by_at_least_25_db() {
        let len = 48_000 * 8;
        let far = Noise(7).speech(len, 0.5);
        let mut near = convolve(&far, &echo_path(3));
        let mut floor = Noise(11);
        near.iter_mut().for_each(|s| *s += floor.next() * 1e-4);
        let mut aec = EchoCanceller::new();
        let out = run(&mut aec, &far, &near);
        let tail = len - 48_000 * 2..;
        let erle = db(energy(&near[tail.clone()]), energy(&out[tail]));
        println!("synthetic path: ERLE {erle:.1} dB over the last 2 s of 8 s");
        assert!(erle >= 25.0, "ERLE {erle:.1} dB");
    }

    #[test]
    fn near_end_speech_survives_double_talk_and_the_filter_does_not_diverge() {
        let seconds = 12;
        let len = 48_000 * seconds;
        let far = Noise(21).speech(len, 0.5);
        let echo = convolve(&far, &echo_path(5));
        let mut talker = Noise(99).speech(len, 0.5);
        // Near-end talker only between 7 s and 9 s.
        for (i, sample) in talker.iter_mut().enumerate() {
            if !(48_000 * 7..48_000 * 9).contains(&i) {
                *sample = 0.0;
            }
        }
        let near: Vec<f32> = echo.iter().zip(&talker).map(|(e, t)| e + t).collect();
        let mut aec = EchoCanceller::new();
        let out = run(&mut aec, &far, &near);

        let talk = 48_000 * 7 + 24_000..48_000 * 9;
        let removed: Vec<f32> = out[talk.clone()]
            .iter()
            .zip(&talker[talk.clone()])
            .map(|(o, t)| o - t)
            .collect();
        let kept = db(energy(&talker[talk.clone()]), energy(&removed));
        let echo_gain = db(energy(&echo[talk.clone()]), energy(&removed));
        println!("double talk: near kept {kept:.1} dB, echo reduced {echo_gain:.1} dB");
        assert!(kept >= 8.0, "near speech distorted: {kept:.1} dB");
        assert!(
            echo_gain >= 10.0,
            "echo not reduced in double talk: {echo_gain:.1} dB"
        );

        let after = len - 48_000 * 2..;
        let erle = db(energy(&near[after.clone()]), energy(&out[after]));
        println!("after double talk: ERLE {erle:.1} dB");
        assert!(erle >= 20.0, "no recovery after double talk: {erle:.1} dB");
    }

    /// ERLE per second of `out` against `near`.
    fn erle_per_second(near: &[f32], out: &[f32]) -> Vec<f64> {
        near.chunks(48_000)
            .zip(out.chunks(48_000))
            .map(|(n, o)| db(energy(n), energy(o)))
            .collect()
    }

    /// After a mute or push-to-talk pause the microphone stream restarts and its pairing with
    /// the played reference moves by up to a device period. `path_changed` brings the echo
    /// back down quickly instead of holding adaptation for two seconds.
    #[test]
    fn echo_recovers_quickly_after_the_streams_are_realigned() {
        let seconds = 12;
        let len = 48_000 * seconds;
        let far = Noise(31).speech(len, 0.5);
        let near = convolve(&far, &echo_path(8));
        let converge = 48_000 * 6;
        let shift = 384; // 8 ms: the pairing moved by 1.6 partitions
        let mut recovered = [0.0; 2];
        for (slot, notify) in recovered.iter_mut().zip([false, true]) {
            let mut aec = EchoCanceller::new();
            let mut out = run(&mut aec, &far[..converge], &near[..converge]);
            // Muted gap: the queued reference is dropped; the stream resumes 8 ms later.
            aec.clear_far();
            if notify {
                aec.path_changed();
            }
            let mut late_far = vec![0.0; shift];
            late_far.extend_from_slice(&far[converge..len - shift]);
            out.extend(run(&mut aec, &late_far, &near[converge..len - shift]));
            let erle = erle_per_second(&near[converge..len - shift], &out[converge..]);
            println!("path_changed {notify}: ERLE per second after the gap {erle:.1?}");
            // Mean ERLE of the fourth and fifth second after the gap.
            *slot = (erle[3] + erle[4]) / 2.0;
        }
        assert!(
            recovered[1] >= 15.0,
            "no recovery with path_changed: {:.1} dB",
            recovered[1]
        );
        assert!(
            recovered[1] > recovered[0] + 10.0,
            "path_changed {:.1} dB vs without {:.1} dB",
            recovered[1],
            recovered[0]
        );
    }

    #[test]
    fn diagnostics_report_the_echo_delay_and_reduction() {
        let len = 48_000 * 8;
        let far = Noise(41).speech(len, 0.5);
        let near = convolve(&far, &echo_path(3));
        let mut aec = EchoCanceller::new();
        assert_eq!(aec.diagnostics().delay_ms, 0, "nothing learned yet");
        run(&mut aec, &far, &near);
        let found = aec.diagnostics();
        // 40 ms bulk delay in the path, plus the 20 ms standing lead the engine always keeps
        // on the reference (this test feeds the canceller without it, so the report is
        // 60 ms), to within a few partitions.
        assert!(
            (50..=70).contains(&found.delay_ms),
            "delay {} ms",
            found.delay_ms
        );
        assert!(found.converged);
        assert!(
            found.reduction_tenths_db >= 200,
            "reduction {} dB",
            f32::from(found.reduction_tenths_db) / 10.0
        );
    }

    #[test]
    fn silent_reference_passes_the_microphone_through() {
        let mut aec = EchoCanceller::new();
        let mut noise = Noise(4);
        let near = noise.speech(48_000, 0.3);
        let silence = vec![0.0; 48_000];
        let out = run(&mut aec, &silence, &near);
        assert!(out.iter().zip(&near).all(|(a, b)| (a - b).abs() < 1e-6));
    }

    #[test]
    fn memory_is_bounded() {
        assert!(std::mem::size_of::<EchoCanceller>() < 64 * 1024);
        let mut aec = EchoCanceller::new();
        aec.push_far(&vec![0.1; 100_000]);
        assert_eq!(aec.far.len(), FAR_LIMIT);
        assert!(aec.far.capacity() < 4 * FAR_LIMIT);
        // 4 spectra of 64 x 257 bins as f32 = about 0.25 MiB.
        const { assert!(4 * PARTITIONS * BINS * 4 < 300 * 1024) };
    }

    #[test]
    fn tap_releases_consumed_audio_and_pads_idle_time_with_silence() {
        let mut tap = ReferenceTap::new(48_000, 4_800).unwrap();
        let mut aec = EchoCanceller::new();
        let start = tap.last;
        for _ in 0..960 {
            tap.push(0.5);
        }
        // Device consumed 480 of 960 frames.
        tap.advance(480, start + std::time::Duration::from_millis(10), &mut aec);
        assert_eq!(aec.far.len(), 480);
        assert!(aec.far.iter().all(|&s| s == 0.5));
        tap.advance(0, start + std::time::Duration::from_millis(20), &mut aec);
        assert_eq!(aec.far.len(), 960);
        // Idle: 100 ms of wall clock becomes 4800 silent samples.
        tap.advance(0, start + std::time::Duration::from_millis(120), &mut aec);
        assert_eq!(aec.far.len(), 960 + 4_800);
        assert!(aec.far.iter().skip(960).all(|&s| s == 0.0));
    }
}
