//! Microphone gain staging and level diagnostics (issue #83).
//!
//! Processing order of the capture path (SPEC §7.5):
//! device -> mono -> 48 kHz -> echo cancellation -> noise suppression -> **gain** (manual and
//! automatic, with a peak limiter) -> **level meter** -> input-sensitivity gate -> Opus.
//! Gain comes after suppression so that it never amplifies what the suppressor removes, and
//! the meter measures exactly the signal the sensitivity gate compares with its threshold.
//!
//! [`GainStage`] is pure arithmetic on the owner thread: no allocation, no lock. With 0 dB and
//! automatic gain off it is bypassed and the samples are untouched. The meter is computed only
//! while a diagnostics consumer asked for it ([`crate::EngineControl::Diagnostics`]).

use crate::jitter::LimiterEnvelope;

/// Lowest and highest manual microphone gain.
pub const MIN_GAIN_DB: i8 = -20;
pub const MAX_GAIN_DB: i8 = 30;

/// Speech level automatic gain steers toward, as block RMS.
const AGC_TARGET_DBFS: f32 = -21.0;
/// Blocks quieter than this (before gain) are noise or silence: the automatic gain holds.
const AGC_FLOOR_DBFS: f32 = -55.0;
const AGC_MIN_DB: f32 = -12.0;
const AGC_MAX_DB: f32 = 24.0;
/// Fraction of the error removed per 10 ms block when the signal is too loud (fast) ...
const AGC_ATTACK: f32 = 0.3;
/// ... and the dB added per 10 ms block when it is too quiet (5 dB/s).
const AGC_RELEASE_DB: f32 = 0.05;
/// Reported level of silence.
pub const FLOOR_TENTHS_DB: i16 = -1000;

/// Manual and automatic microphone gain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputGain {
    /// Fixed gain in dB, clamped to [`MIN_GAIN_DB`]..=[`MAX_GAIN_DB`].
    pub manual_db: i8,
    /// Steer speech toward a steady level on top of the manual gain.
    pub automatic: bool,
}

impl InputGain {
    pub fn clamped(self) -> Self {
        Self {
            manual_db: self.manual_db.clamp(MIN_GAIN_DB, MAX_GAIN_DB),
            automatic: self.automatic,
        }
    }

    pub(crate) fn to_bits(self) -> u32 {
        let this = self.clamped();
        u32::from(this.manual_db as u8) | (u32::from(this.automatic) << 8)
    }

    pub(crate) fn from_bits(bits: u32) -> Self {
        Self {
            manual_db: bits as u8 as i8,
            automatic: bits & 0x100 != 0,
        }
        .clamped()
    }

    fn is_unity(self) -> bool {
        self.manual_db == 0 && !self.automatic
    }
}

fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn mean_square_db(samples: &[f32]) -> f32 {
    let mean_square = samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32;
    10.0 * mean_square.max(1e-12).log10()
}

/// Gain, automatic gain and peak limiting for 10 ms microphone blocks.
pub(crate) struct GainStage {
    /// Linear gain at the end of the previous block.
    applied: f32,
    /// Automatic component, dB.
    auto_db: f32,
    limiter: LimiterEnvelope,
}

impl GainStage {
    pub(crate) fn new() -> Self {
        Self {
            applied: 1.0,
            auto_db: 0.0,
            limiter: LimiterEnvelope::per_second(48_000.0),
        }
    }

    /// Applies `gain` to `block` in place and returns the total gain in dB. The gain moves
    /// linearly across the block, so a change is never a step.
    pub(crate) fn process(&mut self, block: &mut [f32], gain: InputGain) -> f32 {
        let gain = gain.clamped();
        if gain.is_unity() && self.applied == 1.0 && self.auto_db == 0.0 {
            // Nothing to limit; forget an old limiting episode instead of freezing it.
            self.limiter = LimiterEnvelope::per_second(48_000.0);
            return 0.0;
        }
        if gain.automatic {
            let level = mean_square_db(block);
            if level > AGC_FLOOR_DBFS {
                let manual = f32::from(gain.manual_db);
                let wanted = (AGC_TARGET_DBFS - level - manual).clamp(AGC_MIN_DB, AGC_MAX_DB);
                self.auto_db = if wanted < self.auto_db {
                    self.auto_db + (wanted - self.auto_db) * AGC_ATTACK
                } else {
                    (self.auto_db + AGC_RELEASE_DB).min(wanted)
                };
            }
        } else {
            // Leave the automatic component the same way it was entered: smoothly.
            self.auto_db = 0.0;
        }
        let total_db = f32::from(gain.manual_db) + self.auto_db;
        let target = db_to_linear(total_db);
        let step = (target - self.applied) / block.len().max(1) as f32;
        let mut current = self.applied;
        for sample in block.iter_mut() {
            current += step;
            *sample *= current;
        }
        self.applied = target;
        // Bit-exact unity once the ramp has arrived, so the bypass above can take over.
        if (self.applied - 1.0).abs() < 1e-6 {
            self.applied = 1.0;
        }
        self.limiter.process(block);
        total_db
    }
}

/// Echo-canceller state worth showing next to the microphone meter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EchoDiagnostics {
    /// How much quieter the microphone got while the far end was speaking, in tenths of a dB
    /// (smoothed over about a second; includes any near-end speech that stayed).
    pub reduction_tenths_db: i16,
    /// Echo path delay the filter found, in ms, including the standing reference lead.
    pub delay_ms: u16,
    /// The filter has locked onto an echo path since the last reset.
    pub converged: bool,
}

/// Microphone level and processing state, read while a diagnostics consumer is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputDiagnostics {
    /// 20 ms frames measured since diagnostics were switched on; 0 = nothing to show yet
    /// (muted, or the microphone is not delivering).
    pub frames: u64,
    /// Loudest frame RMS since the previous read, in tenths of dBFS (floor -100.0).
    pub rms_tenths_db: i16,
    /// Highest sample magnitude since the previous read, in tenths of dBFS.
    pub peak_tenths_db: i16,
    /// Frames that reached full scale after gain.
    pub clipped_frames: u64,
    /// Total gain in force (manual plus automatic), tenths of dB.
    pub gain_tenths_db: i16,
    /// Present while echo cancellation is running.
    pub echo: Option<EchoDiagnostics>,
}

impl InputDiagnostics {
    pub fn rms_db(&self) -> f32 {
        f32::from(self.rms_tenths_db) / 10.0
    }

    pub fn peak_db(&self) -> f32 {
        f32::from(self.peak_tenths_db) / 10.0
    }
}

/// `(rms dB, peak dB)` of one frame in tenths, floored at [`FLOOR_TENTHS_DB`].
pub(crate) fn measure(frame: &[f32]) -> (i16, i16) {
    let rms = mean_square_db(frame);
    let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let peak_db = 20.0 * peak.max(1e-6).log10();
    let tenths = |db: f32| (db * 10.0).round().clamp(f32::from(FLOOR_TENTHS_DB), 0.0) as i16;
    (tenths(rms), tenths(peak_db))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(amplitude: f32, blocks: usize) -> Vec<f32> {
        (0..blocks * 480)
            .map(|i| amplitude * (std::f32::consts::TAU * 440.0 * i as f32 / 48_000.0).sin())
            .collect()
    }

    fn run(stage: &mut GainStage, signal: &mut [f32], gain: InputGain) {
        for block in signal.chunks_mut(480) {
            stage.process(block, gain);
        }
    }

    fn rms(signal: &[f32]) -> f32 {
        (signal.iter().map(|s| s * s).sum::<f32>() / signal.len() as f32).sqrt()
    }

    #[test]
    fn unity_is_bit_exact_and_bypassed() {
        let original = tone(0.5, 4);
        let mut signal = original.clone();
        run(&mut GainStage::new(), &mut signal, InputGain::default());
        assert_eq!(signal, original);
    }

    #[test]
    fn manual_gain_scales_without_a_step() {
        let mut stage = GainStage::new();
        let mut signal = tone(0.1, 30);
        let gain = InputGain {
            manual_db: 6,
            automatic: false,
        };
        run(&mut stage, &mut signal, gain);
        let steady = rms(&signal[480 * 20..]) / rms(&tone(0.1, 10));
        assert!((steady - 1.995).abs() < 0.02, "gain {steady}");
        // The ramp across the first block is smooth: no jump larger than the signal's own slope.
        let slope = 0.1 * 1.99 * std::f32::consts::TAU * 440.0 / 48_000.0;
        let jump = signal
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(jump < slope * 1.2, "step {jump} vs slope {slope}");
    }

    #[test]
    fn the_limiter_keeps_a_hot_gain_from_clipping() {
        let mut stage = GainStage::new();
        let mut signal = tone(0.6, 60);
        run(
            &mut stage,
            &mut signal,
            InputGain {
                manual_db: MAX_GAIN_DB,
                automatic: false,
            },
        );
        assert!(signal.iter().all(|s| s.abs() <= 0.97 + 1e-6));
        assert!(signal.iter().any(|s| s.abs() > 0.9), "still loud");
    }

    #[test]
    fn automatic_gain_lifts_quiet_speech_and_holds_through_silence() {
        let gain = InputGain {
            manual_db: 0,
            automatic: true,
        };
        let mut stage = GainStage::new();
        // 4 s of quiet speech-level tone at -40 dBFS RMS.
        let mut quiet = tone(0.014, 400);
        run(&mut stage, &mut quiet, gain);
        let before = stage.auto_db;
        assert!(before > 10.0, "auto gain only {before:.1} dB");
        let last = rms(&quiet[quiet.len() - 4_800..]);
        assert!(last > 0.014 / 2f32.sqrt() * 3.0, "output rms {last}");
        // Silence must not wind the gain up.
        let mut silence = vec![0.0001; 480 * 300];
        run(&mut stage, &mut silence, gain);
        assert!((stage.auto_db - before).abs() < 1e-3, "{}", stage.auto_db);
    }

    #[test]
    fn automatic_gain_attenuates_a_hot_signal_toward_the_target() {
        let gain = InputGain {
            manual_db: 0,
            automatic: true,
        };
        let mut stage = GainStage::new();
        // 0.4 amplitude is -11 dBFS RMS: within the -12 dB attenuation range.
        let mut hot = tone(0.4, 100);
        run(&mut stage, &mut hot, gain);
        let level = 20.0 * rms(&hot[hot.len() - 4_800..]).log10();
        assert!(
            (level - AGC_TARGET_DBFS).abs() < 2.0,
            "level {level:.1} dBFS"
        );
    }

    #[test]
    fn turning_gain_off_returns_to_exact_unity() {
        let mut stage = GainStage::new();
        let mut signal = tone(0.2, 40);
        run(
            &mut stage,
            &mut signal,
            InputGain {
                manual_db: 10,
                automatic: true,
            },
        );
        let mut after = tone(0.2, 40);
        let original = after.clone();
        run(&mut stage, &mut after, InputGain::default());
        // One block of ramp, then bit-exact (the limiter has long released).
        assert_eq!(after[480 * 30..], original[480 * 30..]);
        assert_eq!(stage.applied, 1.0);
    }

    #[test]
    fn bits_round_trip_and_clamp() {
        for gain in [
            InputGain::default(),
            InputGain {
                manual_db: -20,
                automatic: true,
            },
            InputGain {
                manual_db: 30,
                automatic: false,
            },
        ] {
            assert_eq!(InputGain::from_bits(gain.to_bits()), gain);
        }
        assert_eq!(
            InputGain {
                manual_db: 100,
                automatic: false
            }
            .clamped()
            .manual_db,
            MAX_GAIN_DB
        );
    }

    #[test]
    fn measure_reports_rms_peak_and_the_floor() {
        assert_eq!(measure(&[0.0; 960]), (FLOOR_TENTHS_DB, FLOOR_TENTHS_DB));
        let (rms, peak) = measure(&[0.5; 960]);
        assert_eq!(rms, -60);
        assert_eq!(peak, -60);
        let (_, peak) = measure(&[1.0; 8]);
        assert_eq!(peak, 0);
    }
}
