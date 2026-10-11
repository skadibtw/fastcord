//! Stand-ins for the High quality engine: transparent delay lines that are built, slow,
//! fail or stop on request, so the controller and the audio engine can be tested without
//! depending on the model's speed.
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::{BLOCK_SAMPLES, DenoiseError, Denoiser, Factory, NoiseSuppression};

type Block = [f32; BLOCK_SAMPLES];

/// How long a test waits for another thread before failing.
pub(crate) const PATIENCE: Duration = Duration::from_secs(30);

pub(crate) fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// What the stand-in engines observed.
#[derive(Default)]
pub(crate) struct Probe {
    /// Factory runs started.
    pub(crate) attempts: AtomicUsize,
    /// Engines being built or alive at this moment.
    pub(crate) live: AtomicUsize,
    pub(crate) peak: AtomicUsize,
    /// Engines dropped by the thread that built them.
    pub(crate) dropped_on_worker: AtomicUsize,
    owner: Mutex<Option<std::thread::ThreadId>>,
}

#[derive(Clone)]
pub(crate) struct Spec {
    /// Output lags input by this many blocks, like DeepFilterNet's lookahead.
    pub(crate) delay_blocks: usize,
    /// The build waits until this opens.
    pub(crate) gate: Arc<AtomicBool>,
    pub(crate) build_time: Duration,
    /// Time spent on every block.
    pub(crate) work: Duration,
    /// Blocks processed at full speed before `work` applies; all of them if `None`.
    pub(crate) slow_after: Option<usize>,
    pub(crate) fail: Option<DenoiseError>,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            delay_blocks: 3,
            gate: Arc::new(AtomicBool::new(true)),
            build_time: Duration::ZERO,
            work: Duration::ZERO,
            slow_after: None,
            fail: None,
        }
    }
}

impl Spec {
    /// The build does not finish until `gate` is opened.
    pub(crate) fn gated(self) -> Self {
        self.gate.store(false, Ordering::Release);
        self
    }
}

/// A transparent engine with a delay line. Not `Send`, like the real runtime.
struct Delayed {
    line: VecDeque<Block>,
    spec: Spec,
    probe: Arc<Probe>,
    processed: usize,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Denoiser for Delayed {
    fn mode(&self) -> NoiseSuppression {
        NoiseSuppression::HighQuality
    }
    fn latency_samples(&self) -> usize {
        self.spec.delay_blocks * BLOCK_SAMPLES
    }
    fn process(&mut self, input: &Block, output: &mut Block) -> Result<(), DenoiseError> {
        self.processed += 1;
        if self.spec.slow_after.is_none_or(|n| self.processed > n) {
            std::thread::sleep(self.spec.work);
        }
        self.line.push_back(*input);
        *output = self.line.pop_front().unwrap_or([0.0; BLOCK_SAMPLES]);
        Ok(())
    }
    fn reset(&mut self) -> Result<(), DenoiseError> {
        self.line = VecDeque::from(vec![[0.0; BLOCK_SAMPLES]; self.spec.delay_blocks]);
        Ok(())
    }
}

impl Drop for Delayed {
    fn drop(&mut self) {
        if *self.probe.owner.lock() == Some(std::thread::current().id()) {
            self.probe.dropped_on_worker.fetch_add(1, Ordering::SeqCst);
        }
        self.probe.live.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) fn factory(spec: Spec, probe: &Arc<Probe>) -> Factory {
    let probe = Arc::clone(probe);
    Arc::new(move || {
        probe.attempts.fetch_add(1, Ordering::SeqCst);
        let live = probe.live.fetch_add(1, Ordering::SeqCst) + 1;
        probe.peak.fetch_max(live, Ordering::SeqCst);
        let deadline = Instant::now() + PATIENCE;
        while !spec.gate.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(spec.build_time);
        if let Some(error) = spec.fail {
            probe.live.fetch_sub(1, Ordering::SeqCst);
            return Err(error);
        }
        *probe.owner.lock() = Some(std::thread::current().id());
        Ok(Box::new(Delayed {
            line: VecDeque::from(vec![[0.0; BLOCK_SAMPLES]; spec.delay_blocks]),
            spec: spec.clone(),
            probe: Arc::clone(&probe),
            processed: 0,
            _not_send: std::marker::PhantomData,
        }))
    })
}
