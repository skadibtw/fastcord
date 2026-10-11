//! A dedicated thread that builds, runs and drops one denoiser that cannot leave it.
//!
//! DeepFilterNet's tract runtime is not `Send`, and building its graph takes far longer than
//! an audio tick. The thread therefore constructs the engine from a factory, owns it for its
//! whole life and drops it itself; the audio owner only exchanges 480-sample blocks over
//! bounded channels and never waits for either side. Dropping the [`Worker`] closes the job
//! queue, which ends the thread and releases the runtime.
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::JoinHandle;

use super::{BLOCK_SAMPLES, DenoiseError, Factory};

/// Blocks that may be queued toward the worker before it counts as unable to keep real time:
/// 8 x 10 ms = 80 ms, inside the SPEC's 100 ms hard buffering cap.
pub(super) const QUEUE_BLOCKS: usize = 8;

pub(super) type Block = [f32; BLOCK_SAMPLES];

/// What the worker reports back.
// Blocks are the hot path; boxing each would allocate once per 10 ms block.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(super) enum Event {
    /// The engine is built; its output lags its input by this many samples.
    Ready { latency_samples: usize },
    /// One processed block, in submission order.
    Block(Block),
    /// The engine failed or the thread ended; nothing more will arrive.
    Failed(DenoiseError),
}

struct Job {
    epoch: u32,
    block: Block,
}

#[allow(clippy::large_enum_variant)]
enum Message {
    Ready { latency_samples: usize },
    Block { epoch: u32, block: Block },
    Failed(DenoiseError),
}

pub(super) struct Worker {
    jobs: SyncSender<Job>,
    messages: Receiver<Message>,
    thread: JoinHandle<()>,
    epoch: u32,
}

impl Worker {
    /// Starts the thread; the factory runs there, so this returns at once.
    pub(super) fn spawn(factory: Factory) -> Result<Self, DenoiseError> {
        let (jobs, job_queue) = mpsc::sync_channel(QUEUE_BLOCKS);
        let (sender, messages) = mpsc::sync_channel(QUEUE_BLOCKS + 2);
        let thread = std::thread::Builder::new()
            .name("fastcord-denoise".to_owned())
            .spawn(move || run(&factory, &job_queue, &sender))
            .map_err(|_| DenoiseError::WorkerStopped)?;
        Ok(Self {
            jobs,
            messages,
            thread,
            epoch: 0,
        })
    }

    /// Queues one block without blocking. A full queue means the worker is more than 80 ms
    /// behind real time.
    pub(super) fn submit(&mut self, block: &Block) -> Result<(), DenoiseError> {
        self.jobs
            .try_send(Job {
                epoch: self.epoch,
                block: *block,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => DenoiseError::TooSlow,
                TrySendError::Disconnected(_) => DenoiseError::WorkerStopped,
            })
    }

    /// The next report, if any. Blocks processed before the last [`Worker::reset`] are
    /// dropped here, so microphone audio from before a mute never reaches the stream.
    pub(super) fn next(&mut self) -> Option<Event> {
        loop {
            match self.messages.try_recv() {
                Ok(Message::Block { epoch, .. }) if epoch != self.epoch => {}
                Ok(Message::Block { block, .. }) => break Some(Event::Block(block)),
                Ok(Message::Ready { latency_samples }) => {
                    break Some(Event::Ready { latency_samples });
                }
                Ok(Message::Failed(error)) => break Some(Event::Failed(error)),
                Err(TryRecvError::Empty) => break None,
                Err(TryRecvError::Disconnected) => {
                    break Some(Event::Failed(DenoiseError::WorkerStopped));
                }
            }
        }
    }

    /// Makes the engine forget delayed audio before the next block it is given.
    pub(super) fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Closes both queues and returns the thread handle, so the caller can tell when the
    /// engine has really been dropped. A build already under way cannot be interrupted; the
    /// thread drops its result as soon as it finishes.
    pub(super) fn retire(self) -> JoinHandle<()> {
        self.thread
    }
}

fn run(factory: &Factory, jobs: &Receiver<Job>, messages: &SyncSender<Message>) {
    let mut engine = match factory() {
        Ok(engine) => engine,
        Err(error) => {
            let _ = messages.send(Message::Failed(error));
            return;
        }
    };
    let ready = Message::Ready {
        latency_samples: engine.latency_samples(),
    };
    if messages.send(ready).is_err() {
        return;
    }
    let mut epoch = 0;
    let mut output = [0.0; BLOCK_SAMPLES];
    while let Ok(job) = jobs.recv() {
        if job.epoch != epoch {
            epoch = job.epoch;
            if let Err(error) = engine.reset() {
                let _ = messages.send(Message::Failed(error));
                return;
            }
        }
        if let Err(error) = engine.process(&job.block, &mut output) {
            let _ = messages.send(Message::Failed(error));
            return;
        }
        let block = Message::Block {
            epoch,
            block: output,
        };
        if messages.send(block).is_err() {
            return;
        }
    }
}
