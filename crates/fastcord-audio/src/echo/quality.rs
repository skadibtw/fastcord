//! Objective echo-cancellation evidence on the committed speakerphone fixtures
//! (`fixtures/audio/speakerphone-*.wav`, issue #83).
//!
//! The fixtures are synthesized here from [`SpeakerphoneRecipe`] with the same deterministic
//! talker as the noise-suppression fixtures: no third-party recordings. `speakerphone-far.wav`
//! is what the call plays; `speakerphone-mic.wav` is what a laptop microphone hears: the far
//! end through a small speaker (soft compression), a 40 ms bulk delay and a 100 ms decaying
//! room response, plus a near-end talker between 5.5 s and 6.9 s and a faint room tone.
//!
//! Metrics, all on 48 kHz mono:
//! - echo return loss enhancement (ERLE), `10 log10(E_mic / E_out)`, on far-end-only speech
//!   (after the double talk, and the time to the first 15 dB over half a second);
//! - near-end preservation during double talk, `10 log10(E_near / E_(out - near))`;
//! - no feedback: no 100 ms window of the output is louder than the microphone;
//! - no clipping: the output never exceeds the microphone's own peak.
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{EchoCanceller, FRAME, LEAD};
use crate::denoise::quality::{parse_wav, to_f32};
use crate::denoise::synth::{self, RATE, Rng, Talker};

const FAR_WAV: &[u8] = include_bytes!("../../../../fixtures/audio/speakerphone-far.wav");
const MIC_WAV: &[u8] = include_bytes!("../../../../fixtures/audio/speakerphone-mic.wav");
const MANIFEST: &str = include_str!("../../../../fixtures/audio/manifest.json");

/// Floors, set well under what the committed fixtures measure (see docs/TESTING.md).
const MIN_ERLE_DB: f64 = 12.0;
const MIN_NEAR_KEPT_DB: f64 = 10.0;
const MAX_CONVERGENCE_S: f64 = 3.0;
/// Output may not exceed the microphone by more than this in any 100 ms window (1 dB).
const MAX_WINDOW_GAIN: f64 = 1.26;

/// Peak amplitude of the room response's tail relative to the direct path.
const TAIL_AMPLITUDE: f64 = 0.08;

struct SpeakerphoneRecipe {
    duration_s: f64,
    far_spans: Vec<(f64, f64)>,
    far: (u64, f64, f64),
    near_span: (f64, f64),
    near: (u64, f64, f64),
    ir_seed: u64,
    bulk_delay_ms: f64,
    tail_ms: f64,
    tail_decay_ms: f64,
    /// Echo level at the microphone relative to the far end, dB.
    echo_gain_db: f64,
    /// `tanh(drive x) / drive`: loudspeaker compression.
    speaker_drive: f64,
    room_seed: u64,
    room_rms_dbfs: f64,
}

struct Rendered {
    far: Vec<f64>,
    mic: Vec<f64>,
    near: Vec<f64>,
}

impl SpeakerphoneRecipe {
    fn committed() -> Self {
        Self {
            duration_s: 10.0,
            far_spans: vec![(0.3, 5.0), (5.3, 7.2), (7.5, 9.7)],
            far: (21, 118.0, -22.0),
            near_span: (5.5, 6.9),
            near: (22, 190.0, -26.0),
            ir_seed: 5,
            bulk_delay_ms: 40.0,
            tail_ms: 100.0,
            tail_decay_ms: 20.0,
            echo_gain_db: -6.0,
            speaker_drive: 2.0,
            room_seed: 7,
            room_rms_dbfs: -60.0,
        }
    }

    fn impulse_response(&self) -> Vec<f64> {
        let ms = |ms: f64| (ms / 1000.0 * RATE as f64).round() as usize;
        let (bulk, tail) = (ms(self.bulk_delay_ms), ms(self.tail_ms));
        let tau = self.tail_decay_ms / 1000.0 * RATE as f64;
        let mut rng = Rng::new(self.ir_seed);
        let mut taps = vec![0.0; bulk + 1 + tail];
        taps[bulk] = 1.0;
        for (i, tap) in taps[bulk + 1..].iter_mut().enumerate() {
            // Reverberant tail at about the energy of the direct path (a microphone a few
            // centimetres from the speaker).
            *tap = TAIL_AMPLITUDE * rng.signed() * (-(i as f64) / tau).exp();
        }
        // Total energy gain of the path is `echo_gain_db`.
        let energy: f64 = taps.iter().map(|t| t * t).sum();
        let scale = 10f64.powf(self.echo_gain_db / 20.0) / energy.sqrt();
        taps.iter_mut().for_each(|t| *t *= scale);
        taps
    }

    fn render(&self) -> Rendered {
        let (seed, f0, rms) = self.far;
        let (far, _) = Talker::foreground(seed, f0, rms).render(self.duration_s, &self.far_spans);
        let (seed, f0, rms) = self.near;
        let (near, _) =
            Talker::foreground(seed, f0, rms).render(self.duration_s, &[self.near_span]);
        let drive = self.speaker_drive;
        let played: Vec<f64> = far.iter().map(|x| (drive * x).tanh() / drive).collect();
        let taps = self.impulse_response();
        let mut room = Rng::new(self.room_seed);
        let room_scale = 10f64.powf(self.room_rms_dbfs / 20.0) * 3f64.sqrt();
        let mic = (0..far.len())
            .map(|n| {
                let reach = taps.len().min(n + 1);
                let echo: f64 = (0..reach).map(|m| taps[m] * played[n - m]).sum();
                echo + near[n] + room.signed() * room_scale
            })
            .collect();
        Rendered { far, mic, near }
    }
}

fn seconds(range: (f64, f64)) -> std::ops::Range<usize> {
    (range.0 * RATE as f64) as usize..(range.1 * RATE as f64) as usize
}

fn energy(signal: &[f32]) -> f64 {
    signal.iter().map(|&s| f64::from(s).powi(2)).sum()
}

fn db(numerator: f64, denominator: f64) -> f64 {
    10.0 * (numerator.max(1e-20) / denominator.max(1e-20)).log10()
}

fn peak(signal: &[f32]) -> f32 {
    signal.iter().fold(0.0, |m, v| m.max(v.abs()))
}

/// Runs the canceller the way the engine does: 10 ms blocks, the reference ahead of the
/// microphone by the standing lead.
fn cancel(far: &[f32], mic: &[f32]) -> Vec<f32> {
    let mut aec = EchoCanceller::new();
    aec.push_far(&[0.0; LEAD]);
    let mut out = Vec::with_capacity(mic.len());
    let mut block = [0.0; FRAME];
    for (far, mic) in far
        .as_chunks::<FRAME>()
        .0
        .iter()
        .zip(mic.as_chunks::<FRAME>().0)
    {
        aec.push_far(far);
        aec.process(mic, &mut block);
        out.extend_from_slice(&block);
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn manifest() -> Value {
    serde_json::from_str(MANIFEST).expect("manifest.json parses")
}

fn f32s(signal: &[f64]) -> Vec<f32> {
    signal.iter().map(|&v| v as f32).collect()
}

#[test]
fn committed_speakerphone_fixtures_match_the_manifest_and_the_generator() {
    let manifest = manifest();
    let rendered = SpeakerphoneRecipe::committed().render();
    for (name, bytes, regenerated) in [
        ("speakerphone-far.wav", FAR_WAV, &rendered.far),
        ("speakerphone-mic.wav", MIC_WAV, &rendered.mic),
    ] {
        let entry = &manifest["files"][name];
        assert_eq!(
            sha256_hex(bytes),
            entry["sha256"].as_str().expect("sha256 recorded"),
            "{name}: SHA-256 differs from the manifest"
        );
        let stored = parse_wav(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let expected = synth::to_pcm16(regenerated);
        assert_eq!(stored.len(), expected.len(), "{name}: sample count");
        let worst = stored
            .iter()
            .zip(&expected)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        assert!(worst <= 2, "{name}: generator drifted by {worst} LSB");
        let signal = to_f32(&stored);
        assert!(
            peak(&signal) <= 0.8 && peak(&signal) > 0.05,
            "{name}: level"
        );
    }
    // The generator parameters recorded for readers stay equal to the recipe.
    let recipe = SpeakerphoneRecipe::committed();
    let generator = &manifest["files"]["speakerphone-mic.wav"]["generator"];
    assert_eq!(
        manifest["files"]["speakerphone-mic.wav"]["generator"]["echo_path"]["tail_amplitude"]
            .as_f64(),
        Some(TAIL_AMPLITUDE)
    );
    assert_eq!(
        generator["bulk_delay_ms"].as_f64(),
        Some(recipe.bulk_delay_ms)
    );
    assert_eq!(
        generator["echo_gain_db"].as_f64(),
        Some(recipe.echo_gain_db)
    );
    assert_eq!(
        generator["speaker_drive"].as_f64(),
        Some(recipe.speaker_drive)
    );
    assert_eq!(
        generator["near_span_s"][0].as_f64(),
        Some(recipe.near_span.0)
    );
}

#[test]
fn speakerphone_echo_is_cancelled_without_feedback_or_clipping() {
    let recipe = SpeakerphoneRecipe::committed();
    let far = to_f32(&parse_wav(FAR_WAV).expect("far parses"));
    let mic = to_f32(&parse_wav(MIC_WAV).expect("mic parses"));
    let near = f32s(&recipe.render().near);
    let out = cancel(&far, &mic);
    assert_eq!(out.len(), mic.len() / FRAME * FRAME);
    let at = |range: (f64, f64)| seconds(range);

    // Far-end-only speech after the double talk (settled filter).
    let tail = at((8.0, 9.7));
    let erle = db(energy(&mic[tail.clone()]), energy(&out[tail]));

    // Time to 15 dB over half a second while the far end speaks.
    let window = RATE / 2;
    let mut convergence = None;
    for start in (at((0.3, 0.3)).start..at((5.0, 5.0)).start - window).step_by(RATE / 10) {
        let range = start..start + window;
        if db(energy(&mic[range.clone()]), energy(&out[range])) >= 15.0 {
            convergence = Some(start as f64 / RATE as f64 - 0.3);
            break;
        }
    }

    // Double talk: how much of the near talker is left in the output, and the echo that
    // stayed beside it.
    let talk = at((5.6, 6.8));
    let removed: Vec<f32> = out[talk.clone()]
        .iter()
        .zip(&near[talk.clone()])
        .map(|(o, n)| o - n)
        .collect();
    let kept = db(energy(&near[talk.clone()]), energy(&removed));
    let echo_only: Vec<f32> = mic[talk.clone()]
        .iter()
        .zip(&near[talk.clone()])
        .map(|(m, n)| m - n)
        .collect();
    let double_talk_reduction = db(energy(&echo_only), energy(&removed));

    // Feedback: the output is never louder than the microphone over 100 ms.
    let worst_window = out
        .as_chunks::<{ RATE / 10 }>()
        .0
        .iter()
        .zip(mic.as_chunks::<{ RATE / 10 }>().0)
        .skip(10)
        .map(|(o, m)| energy(o) / energy(m).max(1e-12))
        .fold(0.0, f64::max);

    let per_second: Vec<String> = (0..10)
        .map(|s| {
            let range = s * RATE..(s + 1) * RATE;
            format!(
                "{:.1}",
                db(energy(&mic[range.clone()]), energy(&out[range]))
            )
        })
        .collect();
    println!("speakerphone ERLE per second: {per_second:?}");
    let (mic_peak, out_peak) = (peak(&mic), peak(&out));
    println!(
        "speakerphone: ERLE {erle:.1} dB (8.0-9.7 s); 15 dB after {} ; near kept {kept:.1} dB, \
         echo reduction in double talk {double_talk_reduction:.1} dB; worst 100 ms output/mic \
         {worst_window:.2}; peak mic {mic_peak:.3} -> out {out_peak:.3}",
        convergence.map_or_else(|| "never".to_owned(), |s| format!("{s:.1} s")),
    );
    assert!(erle >= MIN_ERLE_DB, "ERLE {erle:.1} dB");
    assert!(
        convergence.is_some_and(|s| s <= MAX_CONVERGENCE_S),
        "convergence {convergence:?}"
    );
    assert!(kept >= MIN_NEAR_KEPT_DB, "near talker kept {kept:.1} dB");
    assert!(
        double_talk_reduction > 0.0,
        "echo grew in double talk: {double_talk_reduction:.1} dB"
    );
    assert!(
        worst_window <= MAX_WINDOW_GAIN,
        "feedback: {worst_window:.2}"
    );
    assert!(out_peak <= mic_peak * 1.05 && out_peak < 0.999, "clipping");
}

/// Added CPU of the canceller per 10 ms microphone block, for docs/TESTING.md:
/// `cargo test -p fastcord-audio --locked --release echo_cost -- --ignored --nocapture`.
/// Debug builds measure nothing useful, so this is not part of the normal run.
#[test]
#[ignore = "timing; run in release"]
fn echo_cost() {
    let far = to_f32(&parse_wav(FAR_WAV).expect("far parses"));
    let mic = to_f32(&parse_wav(MIC_WAV).expect("mic parses"));
    let blocks = (far.len() / FRAME) as f64;
    let mut best = f64::MAX;
    for _ in 0..5 {
        let start = std::time::Instant::now();
        let out = cancel(&far, &mic);
        best = best.min(start.elapsed().as_secs_f64());
        assert_eq!(out.len(), mic.len() / FRAME * FRAME);
    }
    let per_block_us = best / blocks * 1e6;
    println!(
        "echo cancellation: {per_block_us:.0} us per 10 ms block = {:.2} % of one core; state {} bytes + {} KiB spectra",
        per_block_us / 100.0,
        std::mem::size_of::<EchoCanceller>(),
        4 * super::PARTITIONS * super::BINS * 4 / 1024
    );
}

/// ```text
/// cargo test -p fastcord-audio --locked regenerate_speakerphone_fixtures -- --ignored --nocapture
/// ```
/// Prints the SHA-256 lines for `fixtures/audio/manifest.json`.
#[test]
#[ignore = "writes fixtures/audio; run to regenerate"]
fn regenerate_speakerphone_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/audio");
    let rendered = SpeakerphoneRecipe::committed().render();
    for (name, signal) in [
        ("speakerphone-far.wav", &rendered.far),
        ("speakerphone-mic.wav", &rendered.mic),
    ] {
        let bytes = synth::wav_bytes(&synth::to_pcm16(signal));
        std::fs::write(dir.join(name), &bytes).expect("write fixture");
        println!("\"{name}\": sha256 {}", sha256_hex(&bytes));
    }
}
