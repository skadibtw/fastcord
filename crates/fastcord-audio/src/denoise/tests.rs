use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::fake::{PATIENCE, Probe, Spec, factory, wait_until};
use super::worker::QUEUE_BLOCKS;
use super::*;

type Block = [f32; BLOCK_SAMPLES];

/// Lets the worker answer between two pushed blocks, as a real 10 ms cadence does.
const PACE: Duration = Duration::from_millis(3);

#[test]
fn light_is_default_and_modes_round_trip() {
    assert_eq!(NoiseSuppression::default(), NoiseSuppression::Light);
    for mode in NoiseSuppression::ALL {
        assert_eq!(
            NoiseSuppression::from_persisted(mode.persisted()),
            Some(mode)
        );
    }
    assert_eq!(NoiseSuppression::from_persisted("krisp"), None);
}

#[test]
fn light_matches_upstream_scaling_and_replaces_initial_artifacts_with_silence() {
    let input: Block = std::array::from_fn(|i| (i as f32 * 0.07).sin() * 0.3);
    let scaled = input.map(|sample| (sample * PCM_SCALE).clamp(-32_768.0, 32_767.0));
    let mut raw = DenoiseState::new();
    let mut light = create(NoiseSuppression::Light).unwrap();
    let mut actual = [0.0; BLOCK_SAMPLES];
    let mut expected = [0.0; BLOCK_SAMPLES];
    raw.process_frame(&mut expected, &scaled);
    light.process(&input, &mut actual).unwrap();
    assert_eq!(actual, [0.0; BLOCK_SAMPLES]);
    raw.process_frame(&mut expected, &scaled);
    light.process(&input, &mut actual).unwrap();
    assert_eq!(actual, expected.map(|sample| sample / PCM_SCALE));
    assert_eq!(light.latency_samples(), BLOCK_SAMPLES);
    light.reset().unwrap();
    light.process(&input, &mut actual).unwrap();
    assert_eq!(actual, [0.0; BLOCK_SAMPLES]);
}

#[test]
fn off_passes_audio_through_unchanged_and_runs_no_inference() {
    let probe = Arc::new(Probe::default());
    let mut rig = Rig::new(NoiseSuppression::Off, factory(Spec::default(), &probe));
    for _ in 0..10 {
        rig.push(Duration::ZERO);
    }
    assert_eq!(rig.contents, (0..10).collect::<Vec<i64>>());
    assert_eq!(rig.denoiser.inference_blocks(), 0);
    assert_eq!(
        probe.attempts.load(Ordering::SeqCst),
        0,
        "no engine was built"
    );
    assert_eq!(rig.denoiser.status().latency_samples, 0);
}

#[test]
fn selecting_high_quality_never_blocks_and_the_current_engine_keeps_running() {
    let probe = Arc::new(Probe::default());
    let spec = Spec::default().gated();
    let gate = Arc::clone(&spec.gate);
    let mut rig = Rig::new(NoiseSuppression::Off, factory(spec, &probe));
    let started = Instant::now();
    rig.denoiser.select(NoiseSuppression::HighQuality);
    for _ in 0..200 {
        rig.push(Duration::ZERO);
        rig.denoiser.poll();
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the caller waited for the model to build"
    );
    // The build has not finished (its gate is shut), yet every block was answered.
    assert_eq!(rig.contents, (0..200).collect::<Vec<i64>>());
    let status = rig.denoiser.status();
    assert_eq!(status.active, NoiseSuppression::Off);
    assert!(status.loading, "{status:?}");
    gate.store(true, Ordering::Release);
}

#[test]
fn high_quality_takes_over_repeating_only_its_extra_delay_and_leaving_no_gap() {
    let probe = Arc::new(Probe::default());
    let spec = Spec::default().gated();
    let gate = Arc::clone(&spec.gate);
    let mut rig = Rig::new(NoiseSuppression::Off, factory(spec, &probe));
    rig.denoiser.select(NoiseSuppression::HighQuality);
    for _ in 0..20 {
        rig.push(PACE);
    }
    gate.store(true, Ordering::Release);
    rig.push_until(|status| status.active == NoiseSuppression::HighQuality);
    for _ in 0..40 {
        rig.push(PACE);
    }
    rig.settle();
    let status = rig.denoiser.status();
    assert!(!status.loading, "{status:?}");
    assert_eq!(status.latency_samples, 3 * BLOCK_SAMPLES);
    assert_eq!(status.error, None);
    // Speech is never skipped. The only repetition is one step back by the engine's extra
    // delay (three blocks) plus the block or two its first answer was in flight.
    let steps: Vec<i64> = rig.contents.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        steps.iter().all(|&step| step <= 1),
        "skipped audio: {steps:?}"
    );
    let back: Vec<i64> = steps.iter().copied().filter(|&step| step < 1).collect();
    assert_eq!(back.len(), 1, "one hand-over, one repeat: {steps:?}");
    let repeated_blocks = 1 - back[0];
    assert!(
        (4..=4 + QUEUE_BLOCKS as i64).contains(&repeated_blocks),
        "repeated {repeated_blocks} blocks"
    );
    // A late answer may leave a few pushes without output, but never more than the bounded
    // queue holds: beyond that the controller hands the stream back to Light (see the
    // slow-worker tests). Exact timing is left to the engine tests, which step the devices.
    assert!(
        rig.max_empty_run <= QUEUE_BLOCKS,
        "gap of {} blocks",
        rig.max_empty_run
    );
    // Once the pipeline is empty, everything pushed came out, delayed by the engine.
    assert_eq!(
        rig.contents.last().copied(),
        Some(rig.pushed as i64 - 1 - 3)
    );
}

#[test]
fn switching_back_resumes_exactly_where_the_slower_engine_stopped() {
    let probe = Arc::new(Probe::default());
    let mut rig = Rig::new(NoiseSuppression::Off, factory(Spec::default(), &probe));
    rig.denoiser.select(NoiseSuppression::HighQuality);
    rig.push_until(|status| status.active == NoiseSuppression::HighQuality);
    for _ in 0..20 {
        rig.push(PACE);
    }
    let switched_at = rig.contents.len();
    rig.denoiser.select(NoiseSuppression::Off);
    for _ in 0..20 {
        rig.push(PACE);
    }
    rig.settle();
    let status = rig.denoiser.status();
    assert_eq!(status.active, NoiseSuppression::Off);
    assert_eq!(status.latency_samples, 0);
    // The blocks still inside the engine are emitted from the retained input: from the
    // switch on the content is strictly consecutive, nothing skipped and nothing repeated,
    // however many blocks were in flight.
    assert!(
        rig.contents[switched_at - 1..]
            .windows(2)
            .all(|w| w[1] - w[0] == 1),
        "{:?}",
        &rig.contents[switched_at - 1..]
    );
    assert_eq!(rig.contents.last().copied(), Some(rig.pushed as i64 - 1));
}

#[test]
fn light_and_off_switch_without_silence_or_loss() {
    let probe = Arc::new(Probe::default());
    let mut rig = Rig::with_signal(
        NoiseSuppression::Off,
        factory(Spec::default(), &probe),
        |index| noise(index, 0.2),
    );
    for _ in 0..10 {
        rig.push(Duration::ZERO);
    }
    rig.denoiser.select(NoiseSuppression::Light);
    for _ in 0..10 {
        rig.push(Duration::ZERO);
    }
    rig.denoiser.select(NoiseSuppression::Off);
    for _ in 0..10 {
        rig.push(Duration::ZERO);
    }
    // One block out per block in, never an all-zero block while the input is not silent,
    // and the faster engine catches up on the block the slower one still held.
    assert_eq!(rig.blocks.len(), 31, "Light to Off resumes one block");
    assert!(
        rig.blocks
            .iter()
            .all(|block| block.iter().any(|&s| s != 0.0)),
        "a silent block was sent"
    );
    assert_eq!(probe.attempts.load(Ordering::SeqCst), 0);
}

#[test]
fn switching_away_drops_the_runtime_on_its_own_thread() {
    let probe = Arc::new(Probe::default());
    let mut rig = Rig::new(
        NoiseSuppression::HighQuality,
        factory(Spec::default(), &probe),
    );
    rig.push_until(|status| status.active == NoiseSuppression::HighQuality);
    assert_eq!(probe.live.load(Ordering::SeqCst), 1);
    rig.denoiser.select(NoiseSuppression::Light);
    wait_until(|| probe.live.load(Ordering::SeqCst) == 0);
    assert_eq!(probe.dropped_on_worker.load(Ordering::SeqCst), 1);
    assert_eq!(rig.denoiser.status().active, NoiseSuppression::Light);

    // The same goes for leaving the call while High quality is running.
    rig.denoiser.select(NoiseSuppression::HighQuality);
    rig.push_until(|status| status.active == NoiseSuppression::HighQuality);
    assert_eq!(probe.live.load(Ordering::SeqCst), 1);
    drop(rig);
    wait_until(|| probe.live.load(Ordering::SeqCst) == 0);
}

#[test]
fn a_failed_build_keeps_the_current_engine_and_reselecting_retries() {
    let probe = Arc::new(Probe::default());
    let spec = Spec {
        fail: Some(DenoiseError::ModelLoad),
        ..Spec::default()
    };
    let mut rig = Rig::new(NoiseSuppression::Light, factory(spec, &probe));
    rig.denoiser.select(NoiseSuppression::HighQuality);
    wait_until(|| {
        rig.denoiser.poll();
        rig.denoiser.status().error.is_some()
    });
    let status = rig.denoiser.status();
    assert_eq!(status.error, Some(DenoiseError::ModelLoad));
    assert_eq!(status.active, NoiseSuppression::Light);
    assert_eq!(status.requested, NoiseSuppression::HighQuality);
    assert!(!status.loading);
    assert_eq!(probe.attempts.load(Ordering::SeqCst), 1);
    rig.denoiser.select(NoiseSuppression::HighQuality);
    assert!(rig.denoiser.status().loading, "reselecting retries");
    wait_until(|| {
        rig.denoiser.poll();
        probe.attempts.load(Ordering::SeqCst) == 2 && rig.denoiser.status().error.is_some()
    });
    assert_eq!(probe.live.load(Ordering::SeqCst), 0);
}

#[test]
fn a_worker_that_cannot_keep_up_is_replaced_by_light_without_stalling_the_caller() {
    let probe = Arc::new(Probe::default());
    let spec = Spec {
        work: Duration::from_millis(40),
        ..Spec::default()
    };
    let mut rig = Rig::new(NoiseSuppression::Light, factory(spec, &probe));
    rig.denoiser.select(NoiseSuppression::HighQuality);
    let mut slowest = Duration::ZERO;
    let deadline = Instant::now() + PATIENCE;
    while rig.denoiser.status().error.is_none() {
        assert!(Instant::now() < deadline, "no verdict on a hopeless worker");
        let started = Instant::now();
        rig.push(Duration::ZERO);
        slowest = slowest.max(started.elapsed());
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(rig.denoiser.status().error, Some(DenoiseError::TooSlow));
    assert!(
        slowest < Duration::from_millis(100),
        "one block took {slowest:?} to hand over"
    );
    assert_eq!(rig.denoiser.status().active, NoiseSuppression::Light);
    wait_until(|| probe.live.load(Ordering::SeqCst) == 0);
}

#[test]
fn an_active_worker_that_slows_down_hands_the_stream_back_to_light() {
    let probe = Arc::new(Probe::default());
    let spec = Spec {
        slow_after: Some(30),
        work: Duration::from_millis(50),
        ..Spec::default()
    };
    let mut rig = Rig::with_signal(
        NoiseSuppression::HighQuality,
        factory(spec, &probe),
        |index| noise(index, 0.2),
    );
    rig.push_until(|status| status.active == NoiseSuppression::HighQuality);
    let deadline = Instant::now() + PATIENCE;
    while rig.denoiser.status().error.is_none() {
        assert!(
            Instant::now() < deadline,
            "the slow worker was never noticed"
        );
        rig.push(Duration::from_millis(1));
    }
    let status = rig.denoiser.status();
    assert_eq!(status.error, Some(DenoiseError::TooSlow));
    assert_eq!(status.active, NoiseSuppression::Light);
    assert_eq!(status.latency_samples, BLOCK_SAMPLES);
    let before = rig.blocks.len();
    for _ in 0..10 {
        rig.push(Duration::ZERO);
    }
    assert_eq!(rig.blocks.len(), before + 10, "Light answers every block");
    // Only the very first block of the stream, Light's documented artifact, is silent.
    assert!(
        rig.blocks[1..]
            .iter()
            .all(|block| block.iter().any(|&s| s != 0.0))
    );
    wait_until(|| probe.live.load(Ordering::SeqCst) == 0);
}

#[test]
fn reset_discards_audio_in_flight_and_runs_no_inference() {
    let probe = Arc::new(Probe::default());
    let spec = Spec {
        delay_blocks: 1,
        ..Spec::default()
    };
    let mut rig = Rig::with_signal(NoiseSuppression::HighQuality, factory(spec, &probe), |_| {
        [1.0; BLOCK_SAMPLES]
    });
    rig.push_until(|status| status.active == NoiseSuppression::HighQuality);
    // Several blocks are answered but not collected when the microphone is muted.
    for _ in 0..4 {
        rig.denoiser.push(&[1.0; BLOCK_SAMPLES]);
    }
    let blocks_before = rig.denoiser.inference_blocks();
    std::thread::sleep(PACE * 5);
    rig.denoiser.reset();
    let mut output = [0.0; BLOCK_SAMPLES];
    std::thread::sleep(PACE * 5);
    assert!(
        !rig.denoiser.pop(&mut output),
        "audio from before the mute came out"
    );
    assert_eq!(rig.denoiser.inference_blocks(), blocks_before);
    // The first microphone audio after the mute is what comes out, delayed by the engine.
    rig.signal = Box::new(|_| [2.0; BLOCK_SAMPLES]);
    let before = rig.blocks.len();
    for _ in 0..10 {
        rig.push(PACE);
    }
    rig.settle();
    let after = &rig.blocks[before..];
    assert!(
        after.iter().all(|block| block.iter().all(|&s| s != 1.0)),
        "pre-mute audio leaked"
    );
    assert!(after.iter().any(|block| block[BLOCK_SAMPLES / 2] == 2.0));
}

#[test]
fn toggling_modes_quickly_never_builds_two_runtimes_at_once() {
    let probe = Arc::new(Probe::default());
    let spec = Spec {
        build_time: Duration::from_millis(40),
        ..Spec::default()
    };
    let mut rig = Rig::new(NoiseSuppression::Light, factory(spec, &probe));
    for round in 0..30 {
        rig.denoiser.select(NoiseSuppression::HighQuality);
        std::thread::sleep(Duration::from_millis(round % 7));
        rig.denoiser.poll();
        rig.denoiser.select(NoiseSuppression::Light);
        rig.push(Duration::ZERO);
    }
    rig.denoiser.select(NoiseSuppression::Light);
    wait_until(|| {
        rig.denoiser.poll();
        probe.live.load(Ordering::SeqCst) == 0
    });
    assert!(probe.attempts.load(Ordering::SeqCst) >= 1);
    assert_eq!(probe.peak.load(Ordering::SeqCst), 1);
    assert_eq!(rig.denoiser.status().active, NoiseSuppression::Light);
    assert!(!rig.denoiser.status().loading);
}

#[test]
fn a_worker_that_panics_is_reported_and_light_continues() {
    let factory: Factory = Arc::new(|| panic!("simulated tract failure"));
    let mut rig = Rig::new(NoiseSuppression::Light, factory);
    rig.denoiser.select(NoiseSuppression::HighQuality);
    wait_until(|| {
        rig.denoiser.poll();
        rig.denoiser.status().error.is_some()
    });
    let status = rig.denoiser.status();
    assert_eq!(status.error, Some(DenoiseError::WorkerStopped));
    assert_eq!(status.active, NoiseSuppression::Light);
    let before = rig.blocks.len();
    rig.push(Duration::ZERO);
    assert_eq!(rig.blocks.len(), before + 1);
}

#[test]
fn status_is_reported_once_per_change() {
    let probe = Arc::new(Probe::default());
    let mut rig = Rig::new(NoiseSuppression::Light, factory(Spec::default(), &probe));
    assert_eq!(
        rig.denoiser.take_status(),
        Some(NoiseSuppressionStatus::default())
    );
    assert_eq!(rig.denoiser.take_status(), None);
    rig.denoiser.select(NoiseSuppression::Light);
    assert_eq!(
        rig.denoiser.take_status(),
        None,
        "reselecting the active mode is silent"
    );
    rig.denoiser.select(NoiseSuppression::Off);
    let status = rig.denoiser.take_status().expect("a change is reported");
    assert_eq!(status.requested, NoiseSuppression::Off);
    assert_eq!(status.active, NoiseSuppression::Off);
}

/// The shipped model: hand-over on the real worker, without blocking the caller.
#[test]
fn the_real_model_builds_off_thread_and_takes_over_at_its_published_latency() {
    let mut rig = Rig::with_signal(NoiseSuppression::Light, high_quality_factory(), |index| {
        noise(index, 0.1)
    });
    rig.denoiser.select(NoiseSuppression::HighQuality);
    let mut slowest = Duration::ZERO;
    // Its own budget: a debug-build tract optimisation of the real graph takes
    // over two minutes on hosted macOS runners. The test grades the hand-over
    // and that the caller never blocks, not how fast debug code loads a model.
    let deadline = Instant::now() + Duration::from_secs(300);
    while rig.denoiser.status().active != NoiseSuppression::HighQuality {
        assert!(Instant::now() < deadline, "{:?}", rig.denoiser.status());
        assert_eq!(rig.denoiser.status().error, None);
        let started = Instant::now();
        rig.push(Duration::ZERO);
        slowest = slowest.max(started.elapsed());
        // Far slower than real time on purpose: debug builds on shared CI machines are far
        // from release speed, and the queue only overflows if a block takes longer than this.
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(slowest < Duration::from_secs(1), "{slowest:?}");
    let status = rig.denoiser.status();
    assert_eq!(status.latency_samples, 3 * BLOCK_SAMPLES);
    assert!(
        rig.blocks
            .iter()
            .all(|block| block.iter().all(|s| s.is_finite()))
    );
    rig.denoiser.select(NoiseSuppression::Off);
    assert_eq!(rig.denoiser.status().active, NoiseSuppression::Off);
}

/// A block of deterministic noise, so no block is ever digital silence.
fn noise(index: usize, amplitude: f32) -> Block {
    let mut state = (index as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    std::array::from_fn(|_| {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let unit = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / 8_388_608.0;
        (unit - 1.0) * amplitude
    })
}

/// Block `index` of a counter signal: every sample is its own stream position, so the
/// content of any emitted block can be read back from it.
fn ramp(index: usize) -> Block {
    std::array::from_fn(|i| (index * BLOCK_SAMPLES + i) as f32)
}

/// Which input block an emitted ramp block was made from. The middle sample is read
/// because the join between two engines is smoothed over the first 96.
fn content(block: &Block) -> i64 {
    block[BLOCK_SAMPLES / 2] as i64 / BLOCK_SAMPLES as i64
}

/// Plays the audio pipeline's part: push a block, then collect what is ready.
struct Rig {
    denoiser: MicrophoneDenoiser,
    signal: Box<dyn Fn(usize) -> Block>,
    ramp: bool,
    pushed: usize,
    blocks: Vec<Block>,
    contents: Vec<i64>,
    empty_run: usize,
    max_empty_run: usize,
}

impl Rig {
    fn new(mode: NoiseSuppression, factory: Factory) -> Self {
        let mut rig = Self::with_signal(mode, factory, ramp);
        rig.ramp = true;
        rig
    }

    fn with_signal(
        mode: NoiseSuppression,
        factory: Factory,
        signal: impl Fn(usize) -> Block + 'static,
    ) -> Self {
        Self {
            denoiser: MicrophoneDenoiser::new(mode, factory),
            signal: Box::new(signal),
            ramp: false,
            pushed: 0,
            blocks: Vec::new(),
            contents: Vec::new(),
            empty_run: 0,
            max_empty_run: 0,
        }
    }

    fn push(&mut self, pause: Duration) {
        self.denoiser.push(&(self.signal)(self.pushed));
        self.pushed += 1;
        std::thread::sleep(pause);
        if self.collect() == 0 {
            self.empty_run += 1;
            self.max_empty_run = self.max_empty_run.max(self.empty_run);
        } else {
            self.empty_run = 0;
        }
    }

    fn collect(&mut self) -> usize {
        let mut output = [0.0; BLOCK_SAMPLES];
        let mut collected = 0;
        while self.denoiser.pop(&mut output) {
            self.blocks.push(output);
            if self.ramp {
                self.contents.push(content(&output));
            }
            collected += 1;
        }
        collected
    }

    /// Keeps the microphone running until the condition holds.
    fn push_until(&mut self, done: impl Fn(&NoiseSuppressionStatus) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while !done(&self.denoiser.status()) {
            assert!(Instant::now() < deadline, "{:?}", self.denoiser.status());
            self.push(PACE);
        }
    }

    /// Collects the answers still in flight once no more blocks arrive.
    fn settle(&mut self) {
        for _ in 0..50 {
            std::thread::sleep(PACE);
            self.collect();
        }
    }
}
