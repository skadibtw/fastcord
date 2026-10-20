//! Objective quality evidence for the noise-suppression engines on the committed fixtures.
//!
//! The fixtures in `fixtures/audio` are synthesized by [`super::synth`]; `manifest.json` records
//! their SHA-256, license and generator parameters. Metrics:
//! - SI-SDR against the clean reference, after searching the engine's delay (nominal latency
//!   +-48 samples) so a small estimate error cannot be mistaken for distortion;
//! - quiet-window attenuation, `10 log10(E_in / E_out)` over samples at least 50 ms away from
//!   every speech interval;
//! - speech gain, `10 log10(E_out / E_clean)` over the speech intervals.
//!
//! Engines are compared on the same mixture only: the synthetic talker is not natural speech,
//! so SI-SDR of an engine on the clean reference is far below its value on real speech.
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::synth::{self, RATE};
use super::{BLOCK_SAMPLES, NoiseSuppression, create};

const CLEAN_WAV: &[u8] = include_bytes!("../../../../fixtures/audio/speech-clean.wav");
const NOISY_SPEECH_WAV: &[u8] = include_bytes!("../../../../fixtures/audio/noisy-speech.wav");
const MANIFEST: &str = include_str!("../../../../fixtures/audio/manifest.json");

/// Samples ignored at the start while the engines settle (150 ms).
const SETTLE: usize = 7_200;
/// Quiet windows keep this distance from every speech interval (50 ms).
const QUIET_MARGIN: usize = 2_400;
/// Engine delay search radius around the nominal latency.
const LAG_SEARCH: usize = 48;
/// Gain of the hot copy used by the clipping checks.
const HOT_GAIN_DB: f64 = 9.0;
/// M23 floors (Light on `noisy-speech.wav`): SI-SDR improvement and speech-level change.
const LIGHT_MIN_SDR_GAIN_DB: f64 = 5.0;
const LIGHT_MIN_SPEECH_GAIN_DB: f64 = -3.0;
/// M23a floors (HighQuality minus Light on `keyboard-voice.wav`).
const HQ_MIN_SDR_LEAD_DB: f64 = 3.5;
const HQ_MIN_ATTENUATION_LEAD_DB: f64 = 10.0;
/// No sample of an engine output may reach this magnitude.
const CLIP_LEVEL: f32 = 0.999;

struct Measured {
    si_sdr_db: f64,
    lag: usize,
    quiet_attenuation_db: f64,
    speech_gain_db: f64,
    peak: f32,
}

fn manifest() -> Value {
    serde_json::from_str(MANIFEST).expect("manifest.json parses")
}

fn number(value: &Value, key: &str) -> f64 {
    value[key]
        .as_f64()
        .unwrap_or_else(|| panic!("manifest: `{key}` is not a number"))
}

fn seed(value: &Value, key: &str) -> u64 {
    value[key]
        .as_u64()
        .unwrap_or_else(|| panic!("manifest: `{key}` is not an unsigned integer"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Strict reader for the exact canonical layout [`synth::wav_bytes`] writes: 44-byte header,
/// PCM, mono, 16-bit, 48 kHz, `data` chunk last and exactly filling the file.
pub(crate) fn parse_wav(bytes: &[u8]) -> Result<Vec<i16>, String> {
    if bytes.len() < 44 || !bytes.len().is_multiple_of(2) {
        return Err(format!("unexpected length {}", bytes.len()));
    }
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let tag = |at: usize, want: &[u8; 4]| &bytes[at..at + 4] == want;
    if !tag(0, b"RIFF") || !tag(8, b"WAVE") || !tag(12, b"fmt ") || !tag(36, b"data") {
        return Err("not a canonical RIFF/WAVE file".into());
    }
    if u32_at(4) as usize != bytes.len() - 8 {
        return Err("RIFF size does not match the file".into());
    }
    if u32_at(16) != 16 || u16_at(20) != 1 {
        return Err("fmt chunk is not 16-byte PCM".into());
    }
    let (channels, rate, bits) = (u16_at(22), u32_at(24), u16_at(34));
    if channels != 1 || rate as usize != RATE || bits != 16 {
        return Err(format!(
            "{channels} ch, {rate} Hz, {bits} bit is not mono 48 kHz 16-bit"
        ));
    }
    if u32_at(28) != rate * 2 || u16_at(32) != 2 {
        return Err("byte rate or block align is inconsistent".into());
    }
    if u32_at(40) as usize != bytes.len() - 44 {
        return Err("data size does not fill the file".into());
    }
    Ok(bytes[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect())
}

fn to_f64(pcm: &[i16]) -> Vec<f64> {
    pcm.iter().map(|&s| f64::from(s) / 32_768.0).collect()
}

pub(crate) fn to_f32(pcm: &[i16]) -> Vec<f32> {
    pcm.iter().map(|&s| f32::from(s) / 32_768.0).collect()
}

fn peak(signal: &[f32]) -> f32 {
    signal.iter().fold(0.0, |m, v| m.max(v.abs()))
}

fn run_engine(mode: NoiseSuppression, input: &[f32]) -> (Vec<f32>, usize) {
    let mut engine = create(mode).expect("engine builds");
    let mut output = Vec::with_capacity(input.len());
    for block in input.as_chunks::<BLOCK_SAMPLES>().0 {
        let mut processed = [0.0; BLOCK_SAMPLES];
        engine
            .process(block, &mut processed)
            .expect("block processes");
        output.extend_from_slice(&processed);
    }
    (output, engine.latency_samples())
}

/// Scale-invariant SDR of `estimate` against `reference`, in dB.
fn si_sdr(estimate: &[f64], reference: &[f64]) -> f64 {
    let dot: f64 = estimate.iter().zip(reference).map(|(e, r)| e * r).sum();
    let energy: f64 = reference.iter().map(|r| r * r).sum();
    let alpha = dot / energy.max(1e-12);
    let (target, error) = estimate
        .iter()
        .zip(reference)
        .fold((0.0, 0.0), |(t, n), (e, r)| {
            let projected = alpha * r;
            (
                t + projected * projected,
                n + (e - projected) * (e - projected),
            )
        });
    10.0 * (target / error.max(1e-12)).log10()
}

/// Best SI-SDR over the delay search window, and the delay that achieves it.
fn best_alignment(output: &[f32], nominal: usize, clean: &[f64]) -> (f64, usize) {
    let mut best = (f64::NEG_INFINITY, nominal);
    for lag in nominal.saturating_sub(LAG_SEARCH)..=nominal + LAG_SEARCH {
        let estimate: Vec<f64> = output[lag + SETTLE..]
            .iter()
            .map(|&v| f64::from(v))
            .collect();
        let sdr = si_sdr(&estimate, &clean[SETTLE..]);
        if sdr > best.0 {
            best = (sdr, lag);
        }
    }
    best
}

/// True when sample `i` lies within `margin` samples of a speech interval.
fn near(i: usize, intervals: &[(usize, usize)], margin: usize) -> bool {
    intervals
        .iter()
        .any(|&(from, to)| i + margin >= from && i < to + margin)
}

/// Number of samples [`measure`] treats as quiet for an engine delay of `lag`.
fn quiet_samples(len: usize, lag: usize, intervals: &[(usize, usize)]) -> usize {
    (SETTLE..len - lag)
        .filter(|&i| !near(i, intervals, QUIET_MARGIN))
        .count()
}

fn measure(
    noisy: &[f32],
    output: &[f32],
    nominal_lag: usize,
    clean: &[f64],
    intervals: &[(usize, usize)],
) -> Measured {
    let (si_sdr_db, lag) = best_alignment(output, nominal_lag, clean);
    let (mut quiet_in, mut quiet_out, mut speech_clean, mut speech_out) = (0.0, 0.0, 0.0, 0.0);
    for (i, ((n, o), c)) in noisy
        .iter()
        .zip(&output[lag..])
        .zip(clean)
        .enumerate()
        .skip(SETTLE)
    {
        let out = f64::from(*o);
        if !near(i, intervals, QUIET_MARGIN) {
            quiet_in += f64::from(*n) * f64::from(*n);
            quiet_out += out * out;
        } else if near(i, intervals, 0) {
            speech_clean += c * c;
            speech_out += out * out;
        }
    }
    Measured {
        si_sdr_db,
        lag,
        quiet_attenuation_db: 10.0 * (quiet_in / quiet_out.max(1e-12)).log10(),
        speech_gain_db: 10.0 * (speech_out / speech_clean).log10(),
        peak: peak(output),
    }
}

fn print_measured(label: &str, baseline_db: f64, m: &Measured) {
    println!(
        "{label}: SI-SDR {:.2} dB ({:+.2} vs noisy {baseline_db:.2}), lag {}, quiet attenuation {:.1} dB, speech gain {:+.2} dB, peak {:.3}",
        m.si_sdr_db,
        m.si_sdr_db - baseline_db,
        m.lag,
        m.quiet_attenuation_db,
        m.speech_gain_db,
        m.peak
    );
}

fn interval_samples(manifest: &Value) -> Vec<(usize, usize)> {
    manifest["files"]["speech-clean.wav"]["speech_intervals_s"]
        .as_array()
        .expect("speech_intervals_s is an array")
        .iter()
        .map(|pair| {
            let at = |k: usize| {
                (pair[k].as_f64().expect("interval bound") * RATE as f64).round() as usize
            };
            (at(0), at(1))
        })
        .collect()
}

/// Fixture file entries in the manifest, keyed by file name.
fn entry<'a>(manifest: &'a Value, name: &str) -> &'a Value {
    &manifest["files"][name]
}

fn speech_recipe(manifest: &Value) -> synth::SpeechRecipe {
    let file = entry(manifest, "speech-clean.wav");
    let talker = &file["generator"]["talker"];
    synth::SpeechRecipe {
        duration_s: number(&manifest["format"], "duration_s"),
        intervals_s: file["speech_intervals_s"]
            .as_array()
            .expect("speech_intervals_s is an array")
            .iter()
            .map(|p| {
                (
                    p[0].as_f64().expect("interval start"),
                    p[1].as_f64().expect("interval end"),
                )
            })
            .collect(),
        seed: seed(talker, "seed"),
        f0_hz: number(talker, "f0_hz"),
        rms_dbfs: number(talker, "rms_dbfs"),
    }
}

/// Drift guard for one fixture: SHA-256 equals the manifest, the generator reproduces the
/// samples within 2 LSB, the peak is audible but at most 0.8 full scale, and (for mixtures)
/// every engine block carries energy, so DeepFilterNet never sees digital silence.
fn check_file(manifest: &Value, name: &str, bytes: &[u8], regenerated: &[f64], mixture: bool) {
    let recorded = entry(manifest, name)["sha256"]
        .as_str()
        .expect("sha256 recorded");
    assert_eq!(
        sha256_hex(bytes),
        recorded,
        "{name}: SHA-256 differs from the manifest"
    );
    let stored = parse_wav(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
    let regenerated = synth::to_pcm16(regenerated);
    assert_eq!(stored.len(), regenerated.len(), "{name}: sample count");
    let worst = stored
        .iter()
        .zip(&regenerated)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        worst <= 2,
        "{name}: generator drifted from the fixture by {worst} LSB"
    );
    let signal = to_f32(&stored);
    assert!(peak(&signal) <= 0.8, "{name}: peak above 0.8 full scale");
    assert!(peak(&signal) > 0.05, "{name}: inaudible");
    if mixture {
        for (index, block) in signal.as_chunks::<BLOCK_SAMPLES>().0.iter().enumerate() {
            let mean_square = block.iter().map(|v| v * v).sum::<f32>() / BLOCK_SAMPLES as f32;
            assert!(mean_square > 1e-6, "{name}: block {index} is (near-)silent");
        }
    }
}

#[test]
fn committed_fixtures_match_the_manifest_and_the_generator() {
    let manifest = manifest();
    assert_eq!(number(&manifest["format"], "sample_rate") as usize, RATE);
    assert_eq!(number(&manifest["format"], "channels") as usize, 1);
    assert_eq!(number(&manifest["format"], "bits_per_sample") as usize, 16);
    let license = manifest["license"].as_str().expect("license statement");
    assert!(
        license.contains("no third-party recordings"),
        "license: {license}"
    );

    let (clean, active) = speech_recipe(&manifest).render();
    check_file(&manifest, "speech-clean.wav", CLEAN_WAV, &clean, false);
    let noise = &entry(&manifest, "noisy-speech.wav")["generator"];
    let noisy = synth::NoisySpeechRecipe {
        noise_seed: seed(noise, "noise_seed"),
        snr_db: number(noise, "snr_db"),
    };
    let mixed = noisy.render(&clean, &active);
    check_file(
        &manifest,
        "noisy-speech.wav",
        NOISY_SPEECH_WAV,
        &mixed,
        true,
    );
    keyboard_voice::check_fixture(&manifest, &clean);
}

/// M23: the Light engine improves noisy speech without clipping.
#[test]
fn light_improves_noisy_speech_without_clipping() {
    let manifest = manifest();
    let clean = to_f64(&parse_wav(CLEAN_WAV).expect("speech-clean.wav parses"));
    let noisy = to_f32(&parse_wav(NOISY_SPEECH_WAV).expect("noisy-speech.wav parses"));
    let intervals = interval_samples(&manifest);
    let noisy_f64: Vec<f64> = noisy.iter().map(|&v| f64::from(v)).collect();
    let baseline = si_sdr(&noisy_f64[SETTLE..], &clean[SETTLE..]);

    let (output, latency) = run_engine(NoiseSuppression::Light, &noisy);
    let light = measure(&noisy, &output, latency, &clean, &intervals);
    print_measured("noisy-speech Light", baseline, &light);
    assert!(
        light.si_sdr_db - baseline >= LIGHT_MIN_SDR_GAIN_DB,
        "Light SI-SDR improvement {:+.2} dB is below +{LIGHT_MIN_SDR_GAIN_DB} dB",
        light.si_sdr_db - baseline
    );
    assert!(
        light.speech_gain_db >= LIGHT_MIN_SPEECH_GAIN_DB,
        "Light speech gain {:.2} dB",
        light.speech_gain_db
    );
    assert!(light.peak < CLIP_LEVEL);
    assert!(light.peak <= peak(&noisy) * 1.05);

    let hot_gain = 10f32.powf(HOT_GAIN_DB as f32 / 20.0);
    let hot: Vec<f32> = noisy.iter().map(|v| v * hot_gain).collect();
    let hot_peak = peak(&hot);
    assert!(
        hot_peak <= 0.95,
        "hot copy peak {hot_peak} would clip on its own"
    );
    let (hot_output, _) = run_engine(NoiseSuppression::Light, &hot);
    let out_peak = peak(&hot_output);
    println!(
        "noisy-speech Light hot copy (+{HOT_GAIN_DB} dB): input peak {hot_peak:.3}, output peak {out_peak:.3}"
    );
    assert!(out_peak < CLIP_LEVEL, "hot output peak {out_peak}");
    assert!(
        hot_output.iter().all(|v| v.abs() < CLIP_LEVEL),
        "a hot output sample reached {CLIP_LEVEL}"
    );
    assert!(
        out_peak <= hot_peak * 1.05,
        "hot output peak {out_peak} exceeds input peak {hot_peak}"
    );
}

/// Regenerates the committed fixtures from the generator.
///
/// ```text
/// cargo test -p fastcord-audio --locked regenerate_noise_fixtures -- --ignored --nocapture
/// ```
///
/// Prints the SHA-256 lines to paste into `fixtures/audio/manifest.json`; the generator
/// parameters there must stay equal to [`synth`]'s `committed()` recipes.
#[test]
#[ignore = "writes fixtures/audio; run to regenerate"]
fn regenerate_noise_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/audio");
    std::fs::create_dir_all(&dir).expect("create fixtures/audio");
    let write = |name: &str, signal: &[f64]| {
        let bytes = synth::wav_bytes(&synth::to_pcm16(signal));
        std::fs::write(dir.join(name), &bytes).expect("write fixture");
        println!("\"{name}\": sha256 {}", sha256_hex(&bytes));
    };
    let speech = synth::SpeechRecipe::committed();
    let (clean, active) = speech.render();
    write("speech-clean.wav", &clean);
    write(
        "noisy-speech.wav",
        &synth::NoisySpeechRecipe::committed().render(&clean, &active),
    );
    keyboard_voice::regenerate(&clean, speech.duration_s, &write);
}

/// M23a (HighQuality vs Light on keyboard clicks and a distant talker). Self-contained so the
/// whole module, its WAV and its manifest entry can be removed independently of M23.
mod keyboard_voice {
    use super::*;

    const WAV: &[u8] = include_bytes!("../../../../fixtures/audio/keyboard-voice.wav");
    pub(super) const FILE: (&str, &[u8]) = ("keyboard-voice.wav", WAV);

    pub(super) fn check_fixture(manifest: &Value, clean: &[f64]) {
        let generator = &entry(manifest, FILE.0)["generator"];
        let (keyboard, background, room) = (
            &generator["keyboard"],
            &generator["background_voice"],
            &generator["room_tone"],
        );
        let recipe = synth::KeyboardVoiceRecipe {
            keyboard_seed: seed(keyboard, "seed"),
            keyboard_keys_per_s: number(keyboard, "keys_per_s"),
            keyboard_peak: number(keyboard, "peak"),
            background_seed: seed(background, "seed"),
            background_f0_hz: number(background, "f0_hz"),
            background_formant_shift: number(background, "formant_shift"),
            background_rms_dbfs: number(background, "rms_dbfs"),
            room_seed: seed(room, "seed"),
            room_rms_dbfs: number(room, "rms_dbfs"),
        };
        let duration_s = number(&manifest["format"], "duration_s");
        check_file(
            manifest,
            FILE.0,
            WAV,
            &recipe.render(clean, duration_s),
            true,
        );
    }

    pub(super) fn regenerate(clean: &[f64], duration_s: f64, write: &dyn Fn(&str, &[f64])) {
        write(
            FILE.0,
            &synth::KeyboardVoiceRecipe::committed().render(clean, duration_s),
        );
    }

    #[test]
    fn high_quality_beats_light_on_keyboard_and_background_voice() {
        let manifest = manifest();
        let clean = to_f64(&parse_wav(CLEAN_WAV).expect("speech-clean.wav parses"));
        let noisy = to_f32(&parse_wav(WAV).expect("keyboard-voice.wav parses"));
        let intervals = interval_samples(&manifest);
        let noisy_f64: Vec<f64> = noisy.iter().map(|&v| f64::from(v)).collect();
        let baseline = si_sdr(&noisy_f64[SETTLE..], &clean[SETTLE..]);

        let (light_out, light_latency) = run_engine(NoiseSuppression::Light, &noisy);
        let (hq_out, hq_latency) = run_engine(NoiseSuppression::HighQuality, &noisy);
        let light = measure(&noisy, &light_out, light_latency, &clean, &intervals);
        let hq = measure(&noisy, &hq_out, hq_latency, &clean, &intervals);
        print_measured("keyboard-voice Light", baseline, &light);
        print_measured("keyboard-voice HighQuality", baseline, &hq);

        assert!(
            quiet_samples(clean.len(), hq_latency, &intervals) >= RATE,
            "less than one second of quiet windows measured"
        );

        let sdr_gap = hq.si_sdr_db - light.si_sdr_db;
        let attenuation_gap = hq.quiet_attenuation_db - light.quiet_attenuation_db;
        println!(
            "HighQuality - Light: SI-SDR {sdr_gap:+.2} dB, quiet attenuation {attenuation_gap:+.1} dB"
        );
        assert!(
            sdr_gap >= HQ_MIN_SDR_LEAD_DB,
            "HighQuality SI-SDR is only {sdr_gap:+.2} dB above Light"
        );
        assert!(
            attenuation_gap >= HQ_MIN_ATTENUATION_LEAD_DB,
            "HighQuality quiet-window attenuation is only {attenuation_gap:+.1} dB above Light"
        );
        for (name, m) in [("Light", &light), ("HighQuality", &hq)] {
            assert!(m.peak < CLIP_LEVEL, "{name} output peak {}", m.peak);
            assert!(
                m.peak <= peak(&noisy) * 1.05,
                "{name} output peak {} exceeds the input",
                m.peak
            );
        }
    }
}
