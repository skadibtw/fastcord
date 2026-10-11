//! The call-owned microphone denoise stage: engine selection, hand-over and mute handling.
//!
//! The audio owner pushes one 480-sample block at a time and pops whatever processed blocks
//! are ready; neither call ever waits. Off and Light run inline and answer at once. High
//! quality runs on a [`Worker`] thread, so its blocks come back later and the previous engine
//! keeps producing audio until the worker has built its runtime and consumed its lookahead.
//!
//! Changing engine never leaves a gap. A slower engine takes over by repeating at most its
//! extra delay of already-sent speech; a faster one resumes exactly where the slower one
//! stopped, from retained input, so no speech is skipped. A one-block offset ramp joins the
//! two waveforms without a click.
use std::collections::VecDeque;
use std::thread::JoinHandle;

use super::worker::{Block, Event, Worker};
use super::{
    BLOCK_SAMPLES, DenoiseError, Denoiser, Factory, NoiseSuppression, NoiseSuppressionStatus,
    light, off,
};

/// Input blocks kept for resuming a faster engine: enough for the worker queue plus its
/// lookahead (8 + 3 blocks) and the priming block.
const HISTORY_BLOCKS: usize = 16;
/// Processed blocks waiting for the pipeline, which drains them after every pushed block.
const READY_BLOCKS: usize = 32;
/// Samples over which the join between two engines is smoothed (2 ms at 48 kHz).
const SPLICE_SAMPLES: usize = 96;

fn blocks(samples: usize) -> i64 {
    samples.div_ceil(BLOCK_SAMPLES) as i64
}

enum Stage {
    /// Off or Light, built and run on the owner thread; one output per input.
    Inline(Box<dyn Denoiser>),
    /// High quality: a worker thread owns the engine and answers asynchronously.
    Remote(Remote),
}

struct Remote {
    worker: Worker,
    latency_samples: usize,
    /// Which input block the last emitted output was made from.
    emitted_through: i64,
}

/// A High quality worker that has not produced audio yet.
struct Warming {
    worker: Worker,
    state: Warm,
}

enum Warm {
    /// The thread is still decompressing the model and building its graph.
    Building,
    /// Blocks are being sent; the first `skip` answers are the engine's delayed start.
    Priming {
        latency_samples: usize,
        skip: i64,
        /// Index of the first input block sent to this worker.
        origin: i64,
    },
}

pub(crate) struct MicrophoneDenoiser {
    factory: Factory,
    requested: NoiseSuppression,
    stage: Stage,
    warming: Option<Warming>,
    /// The previous worker; it may still be finishing a build that cannot be interrupted.
    /// A new worker waits for it, so at most one runtime exists at any time.
    retired: Option<JoinHandle<()>>,
    spawn_pending: bool,
    /// The most recent input blocks; `history[0]` is input block `history_base`.
    history: VecDeque<Block>,
    history_base: i64,
    /// Input blocks pushed since the last reset.
    pushed: i64,
    ready: VecDeque<Block>,
    /// Last sample handed to the pipeline, for joining the next engine's first block.
    tail: f32,
    splice: bool,
    error: Option<DenoiseError>,
    changed: bool,
    inference_blocks: u64,
}

impl MicrophoneDenoiser {
    pub(crate) fn new(mode: NoiseSuppression, factory: Factory) -> Self {
        let mut denoiser = Self {
            factory,
            requested: mode,
            stage: Stage::Inline(if mode == NoiseSuppression::Off {
                off()
            } else {
                light()
            }),
            warming: None,
            retired: None,
            spawn_pending: false,
            history: VecDeque::with_capacity(HISTORY_BLOCKS),
            history_base: 0,
            pushed: 0,
            ready: VecDeque::with_capacity(READY_BLOCKS),
            tail: 0.0,
            splice: false,
            error: None,
            changed: true,
            inference_blocks: 0,
        };
        if mode == NoiseSuppression::HighQuality {
            denoiser.begin_high_quality();
        }
        denoiser
    }

    /// Selects a mode without interrupting the stream. Selecting the current mode again is
    /// a no-op unless High quality failed, in which case it retries.
    pub(crate) fn select(&mut self, mode: NoiseSuppression) {
        if mode == self.requested && self.error.is_none() {
            return;
        }
        self.requested = mode;
        self.error = None;
        self.changed = true;
        if mode == NoiseSuppression::HighQuality {
            self.begin_high_quality();
            return;
        }
        self.cancel_warming();
        let inline = match &self.stage {
            Stage::Inline(engine) => Some(engine.mode()),
            Stage::Remote(_) => None,
        };
        if inline != Some(mode) {
            self.switch_inline(if mode == NoiseSuppression::Off {
                off()
            } else {
                light()
            });
        }
    }

    /// Submits one microphone block. Never blocks and never runs a High quality engine.
    pub(crate) fn push(&mut self, input: &Block) {
        if self.history.len() == HISTORY_BLOCKS {
            self.history.pop_front();
            self.history_base += 1;
        }
        self.history.push_back(*input);
        self.pushed += 1;
        match &mut self.stage {
            Stage::Inline(engine) => {
                let mut output = [0.0; BLOCK_SAMPLES];
                let result = engine.process(input, &mut output);
                self.inference_blocks += u64::from(engine.mode() != NoiseSuppression::Off);
                match result {
                    Ok(()) => self.emit(output),
                    Err(error) => {
                        self.error = Some(error);
                        self.changed = true;
                        self.emit([0.0; BLOCK_SAMPLES]);
                    }
                }
            }
            Stage::Remote(remote) => match remote.worker.submit(input) {
                Ok(()) => self.inference_blocks += 1,
                Err(error) => self.demote(error),
            },
        }
        if let Some(warming) = &mut self.warming
            && matches!(warming.state, Warm::Priming { .. })
        {
            match warming.worker.submit(input) {
                Ok(()) => self.inference_blocks += 1,
                Err(error) => self.abandon_warming(error),
            }
        }
    }

    /// The next processed block, if one is ready.
    pub(crate) fn pop(&mut self, output: &mut Block) -> bool {
        self.service();
        self.ready.pop_front().is_some_and(|block| {
            *output = block;
            true
        })
    }

    /// Advances worker start-up, hand-over and cleanup. The audio owner calls this every
    /// tick, also while muted, so a finished build is noticed without microphone input.
    pub(crate) fn poll(&mut self) {
        self.service();
    }

    /// Forgets delayed microphone audio (mute, device replacement) without running any
    /// engine; the next blocks start a fresh stream.
    pub(crate) fn reset(&mut self) {
        self.history.clear();
        self.history_base = 0;
        self.pushed = 0;
        self.ready.clear();
        self.tail = 0.0;
        self.splice = false;
        match &mut self.stage {
            Stage::Inline(engine) => {
                if let Err(error) = engine.reset() {
                    self.error = Some(error);
                    self.changed = true;
                }
            }
            Stage::Remote(remote) => {
                remote.worker.reset();
                remote.emitted_through = -1 - blocks(remote.latency_samples);
            }
        }
        if let Some(warming) = &mut self.warming
            && let Warm::Priming {
                latency_samples,
                skip,
                origin,
            } = &mut warming.state
        {
            warming.worker.reset();
            *skip = blocks(*latency_samples);
            *origin = 0;
        }
    }

    pub(crate) fn status(&self) -> NoiseSuppressionStatus {
        let (active, latency_samples) = match &self.stage {
            Stage::Inline(engine) => (engine.mode(), engine.latency_samples()),
            Stage::Remote(remote) => (NoiseSuppression::HighQuality, remote.latency_samples),
        };
        NoiseSuppressionStatus {
            requested: self.requested,
            active,
            loading: self.requested == NoiseSuppression::HighQuality
                && active != NoiseSuppression::HighQuality
                && self.error.is_none(),
            latency_samples,
            error: self.error,
        }
    }

    /// The status, once, after it changed.
    pub(crate) fn take_status(&mut self) -> Option<NoiseSuppressionStatus> {
        std::mem::take(&mut self.changed).then(|| self.status())
    }

    /// Blocks handed to a non-Off engine, including the priming and warm-up blocks of a
    /// mode change. Unchanged while muted or Off.
    pub(crate) fn inference_blocks(&self) -> u64 {
        self.inference_blocks
    }

    fn service(&mut self) {
        if self.retired.as_ref().is_some_and(JoinHandle::is_finished) {
            self.retired = None;
        }
        if self.spawn_pending && self.retired.is_none() {
            self.begin_high_quality();
        }
        while let Stage::Remote(remote) = &mut self.stage {
            match remote.worker.next() {
                Some(Event::Block(block)) => {
                    remote.emitted_through += 1;
                    self.emit(block);
                }
                Some(Event::Failed(error)) => self.demote(error),
                Some(Event::Ready { .. }) => {}
                None => break,
            }
        }
        while let Some(warming) = &mut self.warming {
            match warming.worker.next() {
                Some(Event::Ready { latency_samples }) => {
                    warming.state = Warm::Priming {
                        latency_samples,
                        skip: blocks(latency_samples),
                        origin: self.pushed,
                    };
                }
                Some(Event::Block(block)) => {
                    let Warm::Priming {
                        latency_samples,
                        skip,
                        origin,
                    } = &mut warming.state
                    else {
                        continue;
                    };
                    if *skip > 0 {
                        *skip -= 1;
                    } else {
                        let (latency_samples, origin) = (*latency_samples, *origin);
                        self.hand_over(latency_samples, origin, block);
                    }
                }
                Some(Event::Failed(error)) => self.abandon_warming(error),
                None => break,
            }
        }
    }

    fn begin_high_quality(&mut self) {
        if self.warming.is_some() || matches!(self.stage, Stage::Remote(_)) {
            return;
        }
        if self
            .retired
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
        {
            self.spawn_pending = true;
            return;
        }
        self.retired = None;
        self.spawn_pending = false;
        match Worker::spawn(self.factory.clone()) {
            Ok(worker) => {
                self.warming = Some(Warming {
                    worker,
                    state: Warm::Building,
                });
            }
            Err(error) => {
                self.error = Some(error);
                self.changed = true;
            }
        }
    }

    fn cancel_warming(&mut self) {
        self.spawn_pending = false;
        if let Some(warming) = self.warming.take() {
            self.retired = Some(warming.worker.retire());
        }
    }

    fn abandon_warming(&mut self, error: DenoiseError) {
        self.cancel_warming();
        self.error = Some(error);
        self.changed = true;
    }

    /// The warming worker's first real output replaces the inline engine's.
    fn hand_over(&mut self, latency_samples: usize, origin: i64, block: Block) {
        let Some(warming) = self.warming.take() else {
            return;
        };
        self.stage = Stage::Remote(Remote {
            worker: warming.worker,
            latency_samples,
            emitted_through: origin,
        });
        self.splice = true;
        self.emit(block);
        self.changed = true;
    }

    /// High quality failed while active: Light carries on from where it stopped.
    fn demote(&mut self, error: DenoiseError) {
        self.error = Some(error);
        self.changed = true;
        self.switch_inline(light());
    }

    /// Replaces the engine with an inline one, resuming from retained input so that speech
    /// the previous engine had not emitted yet is not lost.
    fn switch_inline(&mut self, mut engine: Box<dyn Denoiser>) {
        let next = match &self.stage {
            Stage::Inline(old) => self.pushed - blocks(old.latency_samples()),
            Stage::Remote(remote) => remote.emitted_through + 1,
        };
        if let Stage::Remote(remote) = std::mem::replace(&mut self.stage, Stage::Inline(off())) {
            self.retired = Some(remote.worker.retire());
        }
        self.splice = self.pushed > 0;
        let delay = blocks(engine.latency_samples());
        let counted = engine.mode() != NoiseSuppression::Off;
        // One block of priming absorbs the engine's artifact-laden first output.
        for index in (next - delay).max(self.history_base)..self.pushed {
            let input = self.history[(index - self.history_base) as usize];
            let mut output = [0.0; BLOCK_SAMPLES];
            if engine.process(&input, &mut output).is_err() {
                break;
            }
            self.inference_blocks += u64::from(counted);
            if index - delay >= next {
                self.emit(output);
            }
        }
        self.stage = Stage::Inline(engine);
    }

    fn emit(&mut self, mut block: Block) {
        if self.splice {
            self.splice = false;
            let step = self.tail - block[0];
            for (i, sample) in block.iter_mut().take(SPLICE_SAMPLES).enumerate() {
                *sample += step * (1.0 - i as f32 / SPLICE_SAMPLES as f32);
            }
        }
        self.tail = block[BLOCK_SAMPLES - 1];
        if self.ready.len() == READY_BLOCKS {
            self.ready.pop_front();
        }
        self.ready.push_back(block);
    }
}
