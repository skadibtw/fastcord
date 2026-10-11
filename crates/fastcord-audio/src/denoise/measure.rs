//! Manual measurements recorded in docs/TESTING.md: what each engine costs per 10 ms block,
//! how late High quality's answers arrive across the worker thread, and what memory and CPU the
//! process uses while the mode changes. They are ignored by default and only meaningful in a
//! release build on a quiet machine:
//!
//! ```text
//! cargo test -p fastcord-audio --release --locked measure_ -- --ignored --nocapture --test-threads=1
//! ```
#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::quality::{parse_wav, to_f32};
use super::worker::{Event, Worker};
use super::{BLOCK_SAMPLES, NoiseSuppression, create, high_quality_factory};
#[cfg(windows)]
use super::{
    DenoiseError, Denoiser, Factory, HighQuality, MicrophoneDenoiser, NoiseSuppressionStatus,
};

type Block = [f32; BLOCK_SAMPLES];

const NOISY_SPEECH: &[u8] = include_bytes!("../../../../fixtures/audio/noisy-speech.wav");
const KEYBOARD_VOICE: &[u8] = include_bytes!("../../../../fixtures/audio/keyboard-voice.wav");
const BLOCK: Duration = Duration::from_millis(10);

fn blocks(wav: &[u8]) -> Vec<Block> {
    let signal = to_f32(&parse_wav(wav).expect("fixture parses"));
    signal.as_chunks::<BLOCK_SAMPLES>().0.to_vec()
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * fraction).round() as usize]
}

fn summary(label: &str, mut milliseconds: Vec<f64>) {
    milliseconds.sort_by(f64::total_cmp);
    let mean = milliseconds.iter().sum::<f64>() / milliseconds.len() as f64;
    println!(
        "{label}: n={} mean {mean:.3} ms, p50 {:.3}, p95 {:.3}, p99 {:.3}, max {:.3}",
        milliseconds.len(),
        percentile(&milliseconds, 0.50),
        percentile(&milliseconds, 0.95),
        percentile(&milliseconds, 0.99),
        milliseconds.last().unwrap()
    );
}

/// Sleeps until the `n`th 10 ms tick after `start`.
fn tick(start: Instant, n: u32) {
    std::thread::sleep((start + BLOCK * n).saturating_duration_since(Instant::now()));
}

/// Time spent in `Denoiser::process` for one 10 ms block, on the calling thread. The mean
/// over audio time is the engine's share of one core (SPEC section 7.4 targets: p95 < 0.5 ms).
#[test]
#[ignore = "manual measurement; run in release"]
fn measure_engine_cost_per_block() {
    for (fixture, wav) in [
        ("noisy-speech", NOISY_SPEECH),
        ("keyboard-voice", KEYBOARD_VOICE),
    ] {
        let input = blocks(wav);
        for mode in [NoiseSuppression::Light, NoiseSuppression::HighQuality] {
            let built = Instant::now();
            let mut engine = create(mode).unwrap();
            println!(
                "{fixture} {mode:?}: built in {:.0} ms, latency {} samples",
                built.elapsed().as_secs_f64() * 1000.0,
                engine.latency_samples()
            );
            let mut output = [0.0; BLOCK_SAMPLES];
            let mut times = Vec::new();
            for _ in 0..5 {
                engine.reset().unwrap();
                for block in &input {
                    let started = Instant::now();
                    engine.process(block, &mut output).unwrap();
                    times.push(started.elapsed().as_secs_f64() * 1000.0);
                }
            }
            let busy: f64 = times.iter().sum();
            println!(
                "{fixture} {mode:?}: {:.2} % of one core at real time",
                100.0 * busy / (times.len() as f64 * 10.0)
            );
            summary(&format!("{fixture} {mode:?} process()"), times);
        }
    }
}

/// How long a block takes to come back from the real High quality worker at a real-time
/// 10 ms cadence: queueing plus inference, polled every 50 us. The audio owner collects it at
/// its next tick or pushed block, at most 5 ms later.
#[test]
#[ignore = "manual measurement; run in release"]
fn measure_high_quality_worker_round_trip() {
    let input = blocks(NOISY_SPEECH);
    let started = Instant::now();
    let mut worker = Worker::spawn(high_quality_factory()).unwrap();
    let latency = loop {
        match worker.next() {
            Some(Event::Ready { latency_samples }) => break latency_samples,
            Some(other) => panic!("{other:?}"),
            None => std::thread::sleep(Duration::from_millis(1)),
        }
    };
    println!(
        "worker ready after {:.0} ms, engine latency {latency} samples = {:.1} ms",
        started.elapsed().as_secs_f64() * 1000.0,
        latency as f64 / 48.0
    );
    let mut round_trips = Vec::new();
    let clock = Instant::now();
    for (n, block) in input.iter().cycle().take(1_500).enumerate() {
        tick(clock, n as u32);
        let sent = Instant::now();
        worker.submit(block).unwrap();
        loop {
            match worker.next() {
                Some(Event::Block(_)) => break,
                Some(other) => panic!("{other:?}"),
                None => std::thread::sleep(Duration::from_micros(50)),
            }
        }
        round_trips.push(sent.elapsed().as_secs_f64() * 1000.0);
    }
    summary("High quality worker round trip", round_trips);
}

/// The real High quality engine, counting the time spent in `process` on its own thread.
#[cfg(windows)]
struct Timed {
    inner: Box<dyn Denoiser>,
    busy_ns: Arc<AtomicU64>,
}

#[cfg(windows)]
impl Denoiser for Timed {
    fn mode(&self) -> NoiseSuppression {
        self.inner.mode()
    }
    fn latency_samples(&self) -> usize {
        self.inner.latency_samples()
    }
    fn process(&mut self, input: &Block, output: &mut Block) -> Result<(), DenoiseError> {
        let started = Instant::now();
        let result = self.inner.process(input, output);
        self.busy_ns
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        result
    }
    fn reset(&mut self) -> Result<(), DenoiseError> {
        self.inner.reset()
    }
}

#[cfg(windows)]
fn timed_factory(busy_ns: &Arc<AtomicU64>) -> Factory {
    let busy_ns = Arc::clone(busy_ns);
    Arc::new(move || {
        Ok(Box::new(Timed {
            inner: HighQuality::build()?,
            busy_ns: Arc::clone(&busy_ns),
        }))
    })
}

#[cfg(windows)]
mod process {
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Columns: Unix ms, working set, private bytes, CPU ms, peak working set, peak private.
    const SAMPLER: &str = r#"
$id = @PID@; $csv = '@CSV@'
Add-Type -Namespace Fc -Name Mem -MemberDefinition '[System.Runtime.InteropServices.DllImport("psapi.dll")] public static extern bool EmptyWorkingSet(System.IntPtr h);'
while ($true) {
  $p = Get-Process -Id $id -ErrorAction SilentlyContinue
  if (-not $p) { break }
  if (Test-Path "$csv.trim") { Remove-Item "$csv.trim"; [Fc.Mem]::EmptyWorkingSet($p.Handle) | Out-Null; $p.Refresh() }
  $line = '{0},{1},{2},{3},{4},{5}' -f [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds(), $p.WorkingSet64, $p.PrivateMemorySize64, [int64]$p.TotalProcessorTime.TotalMilliseconds, $p.PeakWorkingSet64, $p.PeakPagedMemorySize64
  Add-Content -Path $csv -Value $line
  Start-Sleep -Milliseconds 100
}"#;

    /// Samples this process's working set, private bytes and CPU time from PowerShell every
    /// 100 ms, like the SPEC section 12.3 protocol.
    pub(super) struct Sampler {
        child: Child,
        csv: PathBuf,
        stages: Vec<(String, u128, u128)>,
        open: Option<(String, u128)>,
    }

    fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
    }

    impl Sampler {
        pub(super) fn start() -> Self {
            let csv =
                std::env::temp_dir().join(format!("fastcord-denoise-{}.csv", std::process::id()));
            let _ = std::fs::remove_file(&csv);
            let script = SAMPLER
                .replace("@PID@", &std::process::id().to_string())
                .replace("@CSV@", &csv.display().to_string());
            let child = Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("PowerShell starts");
            std::thread::sleep(Duration::from_secs(3));
            Self {
                child,
                csv,
                stages: Vec::new(),
                open: None,
            }
        }

        pub(super) fn stage(&mut self, name: &str) {
            self.end();
            self.open = Some((name.to_owned(), now_ms()));
        }

        /// Asks the OS to trim this process's working set, as it would under memory pressure.
        pub(super) fn trim_working_set(&self) {
            std::fs::write(format!("{}.trim", self.csv.display()), b"").expect("trim request");
        }

        fn end(&mut self) {
            if let Some((name, from)) = self.open.take() {
                self.stages.push((name, from, now_ms()));
            }
        }

        /// Prints, per stage, the working set and private bytes (MB, min/mean/max) over the
        /// stage without its first and last second (short stages: all of it), and the peaks
        /// the process had reached when the stage ended.
        pub(super) fn report(mut self) {
            self.end();
            std::thread::sleep(Duration::from_millis(400));
            let _ = self.child.kill();
            let samples: Vec<[f64; 6]> = std::fs::read_to_string(&self.csv)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let mut fields = line.trim().split(',').map(|f| f.parse::<f64>().ok());
                    let mut row = [0.0; 6];
                    for slot in &mut row {
                        *slot = fields.next()??;
                    }
                    Some(row)
                })
                .collect();
            let _ = std::fs::remove_file(&self.csv);
            println!(
                "stage | working set MB min/mean/max | private MB min/mean/max | OS CPU % | peak WS / peak private MB at end"
            );
            for (name, from, to) in &self.stages {
                let margin = if to - from > 4_000 { 1_000.0 } else { 0.0 };
                let window: Vec<&[f64; 6]> = samples
                    .iter()
                    .filter(|s| s[0] >= *from as f64 + margin && s[0] <= *to as f64 - margin)
                    .collect();
                let (Some(first), Some(last)) = (window.first(), window.last()) else {
                    println!("{name}: no sample inside the stage");
                    continue;
                };
                let stats = |column: usize| {
                    let values: Vec<f64> = window.iter().map(|s| s[column] / 1e6).collect();
                    let mean = values.iter().sum::<f64>() / values.len() as f64;
                    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
                    let max = values.iter().copied().fold(0.0, f64::max);
                    format!("{min:.1}/{mean:.1}/{max:.1}")
                };
                let cpu = 100.0 * (last[3] - first[3]) / (last[0] - first[0]).max(1.0);
                let after = samples.iter().find(|s| s[0] >= *to as f64).unwrap_or(last);
                println!(
                    "{name} | {} | {} | {cpu:.2} | {:.1} / {:.1}",
                    stats(1),
                    stats(2),
                    after[4] / 1e6,
                    after[5] / 1e6
                );
            }
        }
    }
}

/// What one stage of pushed blocks cost the owner thread.
#[cfg(windows)]
fn owner_cost(label: &str, mut microseconds: Vec<f64>, worker_busy_ns: u64, seconds: f64) {
    microseconds.sort_by(f64::total_cmp);
    let n = microseconds.len().max(1);
    println!(
        "{label}: owner push+pop per block mean {:.1} us, p95 {:.1} us, max {:.1} us over {n} blocks; worker busy {:.2} % of one core",
        microseconds.iter().sum::<f64>() / n as f64,
        percentile(&microseconds, 0.95),
        microseconds.last().copied().unwrap_or(0.0),
        100.0 * worker_busy_ns as f64 / 1e9 / seconds
    );
}

/// Memory and CPU through the modes of a call, with the real controller and model at a
/// real-time cadence: Off, Light, then three High quality / Light cycles. Light after the
/// cycles must return to the first Light's memory (SPEC section 16, milestone 23a).
#[cfg(windows)]
#[test]
#[ignore = "manual measurement; run in release"]
fn measure_memory_and_cpu_across_mode_switches() {
    let input = blocks(NOISY_SPEECH);
    let busy_ns = Arc::new(AtomicU64::new(0));
    let mut sampler = process::Sampler::start();
    let mut denoiser = MicrophoneDenoiser::new(NoiseSuppression::Off, timed_factory(&busy_ns));
    let mut position = 0usize;
    let mut output = [0.0; BLOCK_SAMPLES];
    let mut run = |denoiser: &mut MicrophoneDenoiser,
                   label: &str,
                   seconds: u64,
                   until: &dyn Fn(&NoiseSuppressionStatus) -> bool| {
        let (busy_before, clock) = (busy_ns.load(Ordering::Relaxed), Instant::now());
        let (mut n, mut costs) = (0u32, Vec::new());
        while Duration::from_millis(10 * u64::from(n)) < Duration::from_secs(seconds)
            && !until(&denoiser.status())
        {
            tick(clock, n);
            let started = Instant::now();
            denoiser.push(&input[position % input.len()]);
            while denoiser.pop(&mut output) {}
            costs.push(started.elapsed().as_secs_f64() * 1e6);
            position += 1;
            n += 1;
        }
        let busy = busy_ns.load(Ordering::Relaxed) - busy_before;
        owner_cost(label, costs, busy, clock.elapsed().as_secs_f64());
        clock.elapsed()
    };
    let never = |_: &NoiseSuppressionStatus| false;

    sampler.stage("idle, fixtures loaded");
    std::thread::sleep(Duration::from_secs(4));
    sampler.stage("Off");
    run(&mut denoiser, "Off", 8, &never);
    denoiser.select(NoiseSuppression::Light);
    sampler.stage("Light");
    run(&mut denoiser, "Light", 10, &never);
    for cycle in 1..=3 {
        sampler.stage(&format!("High quality #{cycle} building and warming"));
        let selected = Instant::now();
        denoiser.select(NoiseSuppression::HighQuality);
        let select_call = selected.elapsed();
        let took = run(&mut denoiser, "  while building", 30, &|status| {
            status.active == NoiseSuppression::HighQuality
        });
        println!(
            "cycle {cycle}: select() returned in {select_call:?}; High quality took over after {:.0} ms of real-time audio; {:?}",
            took.as_secs_f64() * 1000.0,
            denoiser.status()
        );
        sampler.stage(&format!("High quality #{cycle} steady"));
        run(&mut denoiser, "High quality steady", 10, &never);
        sampler.stage(&format!("back to Light #{cycle}"));
        denoiser.select(NoiseSuppression::Light);
        run(&mut denoiser, "back to Light", 10, &never);
    }
    sampler.stage("muted, no blocks pushed");
    denoiser.reset();
    std::thread::sleep(Duration::from_secs(5));
    sampler.trim_working_set();
    sampler.stage("muted, working set trimmed by the OS");
    std::thread::sleep(Duration::from_secs(5));
    sampler.report();
}

/// Twenty High quality / Light cycles: the private memory after switching back must plateau
/// instead of growing with every cycle (SPEC section 12.2).
#[cfg(windows)]
#[test]
#[ignore = "manual measurement; run in release"]
fn measure_memory_plateaus_over_many_cycles() {
    let input = blocks(NOISY_SPEECH);
    let mut sampler = process::Sampler::start();
    let mut denoiser = MicrophoneDenoiser::new(NoiseSuppression::Light, high_quality_factory());
    let mut position = 0usize;
    let mut output = [0.0; BLOCK_SAMPLES];
    let mut run = |denoiser: &mut MicrophoneDenoiser, seconds: u64| {
        let clock = Instant::now();
        for n in 0..seconds as u32 * 100 {
            tick(clock, n);
            denoiser.push(&input[position % input.len()]);
            while denoiser.pop(&mut output) {}
            position += 1;
        }
    };
    sampler.stage("Light before any High quality");
    run(&mut denoiser, 5);
    for cycle in 1..=20 {
        sampler.stage(&format!("High quality #{cycle}"));
        denoiser.select(NoiseSuppression::HighQuality);
        run(&mut denoiser, 3);
        assert_eq!(denoiser.status().active, NoiseSuppression::HighQuality);
        sampler.stage(&format!("back to Light #{cycle}"));
        denoiser.select(NoiseSuppression::Light);
        run(&mut denoiser, 3);
    }
    sampler.report();
}
