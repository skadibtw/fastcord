//! The audio engine: one owner thread per active call.
//!
//! The owner thread opens the cpal streams, runs the capture/playback
//! pipelines, and closes the streams again; device callbacks only touch the
//! preallocated rings. The engine exists only while audio is needed, so idle
//! has no audio thread or callback stream (SPEC §2.3 invariant 8).
//! Dropping [`AudioEngine`] stops the streams, waits for the application's data
//! and error callback resources to retire, and joins the owner thread. Native
//! backend disposal can finish after releasing those callback resources.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::callback::{InputCallback, OutputCallback, StreamCounters};
use tokio::sync::mpsc;

use crate::codec::{DEFAULT_BITRATE, EncodedPacket};
use crate::denoise::{
    Factory, MicrophoneDenoiser, NoiseSuppression, NoiseSuppressionStatus, high_quality_factory,
};
use crate::device::{Backend, CpalBackend, DeviceChoice, DeviceErrorKind, Direction, StreamFormat};
use crate::echo::EchoCanceller;
use crate::error::AudioError;
use crate::pipeline::{
    CapturePipeline, CapturedFrame, PipelineCounters, PlaybackPipeline, RING_LIMIT_MS, RemoteFrame,
    frames_for,
};
use crate::ring::sample_ring;

/// How often the owner thread services the rings. Rings hold 100 ms and
/// playback targets 40 ms, so a coarse OS timer (15.6 ms on Windows) still
/// keeps both comfortably supplied.
const TICK: Duration = Duration::from_millis(5);
/// Encoded frames buffered toward the media session (320 ms, ≤ 24 KiB).
const CAPTURE_QUEUE: usize = 16;
/// Local and remote encoded packets buffered toward playback.
const PLAYBACK_QUEUE: usize = 64;
const EVENT_QUEUE: usize = 8;

/// What the engine should open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    /// Microphone; `None` runs without capture.
    pub input: Option<DeviceChoice>,
    /// Speakers/headset; `None` runs without playback.
    pub output: Option<DeviceChoice>,
    /// Opus bitrate in bits/s.
    pub bitrate: u32,
    /// Microphone-only suppression at call start. High quality is built on its own thread:
    /// the call starts with Light and switches when the model is ready.
    pub noise_suppression: NoiseSuppression,
    /// Remove the speaker echo from the microphone using the played mix as reference. Needs
    /// both an input and an output.
    pub echo_cancellation: bool,
    /// Initial mute/deafen gate, applied before the first input callback.
    pub capture_enabled: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            input: Some(DeviceChoice::SystemDefault),
            output: Some(DeviceChoice::SystemDefault),
            bitrate: DEFAULT_BITRATE,
            noise_suppression: NoiseSuppression::Light,
            echo_cancellation: true,
            capture_enabled: true,
        }
    }
}

/// The format a device stream runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

fn store_format(slot: &std::sync::atomic::AtomicU64, format: Option<DeviceFormat>) {
    slot.store(
        format.map_or(0, |f| {
            (u64::from(f.sample_rate) << 16) | u64::from(f.channels)
        }),
        Ordering::Release,
    );
}

fn load_format(slot: &std::sync::atomic::AtomicU64) -> Option<DeviceFormat> {
    let value = slot.load(Ordering::Acquire);
    (value != 0).then_some(DeviceFormat {
        sample_rate: (value >> 16) as u32,
        channels: value as u16,
    })
}

impl From<StreamFormat> for DeviceFormat {
    fn from(format: StreamFormat) -> Self {
        Self {
            sample_rate: format.rate,
            channels: format.channels as u16,
        }
    }
}

/// Something the UI/media session must react to.
#[derive(Debug)]
pub enum EngineEvent {
    /// The stream stopped and was closed. The engine does not switch to
    /// another device by itself.
    StreamFailed {
        direction: Direction,
        kind: DeviceErrorKind,
    },
    /// A replacement is running. Old endpoints are closed before these are published.
    StreamReady {
        direction: Direction,
        format: DeviceFormat,
        captured: Option<mpsc::Receiver<CapturedFrame>>,
        playback: Option<mpsc::Sender<EncodedPacket>>,
        remote_audio: Option<mpsc::Sender<RemoteFrame>>,
    },
    /// Actual processing mode, initialization state, latency and bounded failure notice.
    NoiseSuppressionChanged(NoiseSuppressionStatus),
}
/// Runtime changes to an active call. Commands are bounded and serviced by the
/// engine owner; audio callbacks never touch control state.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineControl {
    /// Stop processing microphone samples while retaining the input stream.
    CaptureEnabled(bool),
    /// Suppress all remote playback locally.
    OutputEnabled(bool),
    /// Linear gain for one remote RTP SSRC. Values are clamped to 0–2.
    RemoteGain { ssrc: u32, gain: f32 },
    /// Select a microphone denoiser without reopening either device.
    NoiseSuppression(NoiseSuppression),
    /// Turn acoustic echo cancellation on or off without reopening either device.
    EchoCancellation(bool),
    /// Replace only this direction, on the owner thread. IDs are bounded to 512 bytes.
    SelectDevice {
        direction: Direction,
        choice: DeviceChoice,
    },
}

/// Channels connecting the engine to the media session.
#[derive(Debug)]
pub struct EngineChannels {
    /// Encoded 20 ms microphone frames (present when an input was opened).
    pub captured: Option<mpsc::Receiver<CapturedFrame>>,
    /// Opus packets to play (present when an output was opened).
    pub playback: Option<mpsc::Sender<EncodedPacket>>,
    /// Decrypted, DAVE-authenticated packets tagged by their RTP SSRC.
    pub remote_audio: Option<mpsc::Sender<RemoteFrame>>,
    /// Runtime controls, applied by the owner thread.
    pub controls: mpsc::Sender<EngineControl>,
    pub events: mpsc::Receiver<EngineEvent>,
}

/// Counters for diagnostics and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineStats {
    pub input_callbacks: u64,
    /// Captured samples discarded because processing fell behind.
    pub input_overrun_samples: u64,
    pub output_callbacks: u64,
    /// Times playback ran dry while audio was flowing (silence was played).
    pub output_underruns: u64,
    pub output_underrun_samples: u64,
    /// Device-reported xruns on either stream.
    pub device_xruns: u64,
    pub frames_encoded: u64,
    /// Encoded frames dropped because the consumer did not keep up.
    pub frames_dropped: u64,
    pub encode_errors: u64,
    /// Blocks given to a non-Off denoiser; unchanged while muted or Off.
    pub denoise_blocks: u64,
    pub packets_decoded: u64,
    pub decode_errors: u64,
    /// Remote 20 ms frames recovered from FEC or concealed because a packet was lost or late.
    pub plc_frames: u64,
}

#[derive(Default)]
struct Shared {
    input: Arc<StreamCounters>,
    output: Arc<StreamCounters>,
    capture_enabled: Arc<AtomicBool>,
    pipeline: PipelineCounters,
    input_format: std::sync::atomic::AtomicU64,
    output_format: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    owner_service: OwnerService,
    #[cfg(test)]
    probe_defaults: AtomicBool,
}
#[cfg(test)]
struct OwnerService {
    lock: parking_lot::Mutex<u64>,
    changed: parking_lot::Condvar,
}
#[cfg(test)]
impl Default for OwnerService {
    fn default() -> Self {
        Self {
            lock: parking_lot::Mutex::new(0),
            changed: parking_lot::Condvar::new(),
        }
    }
}
#[cfg(test)]
impl OwnerService {
    fn notify(&self) {
        let mut generation = self.lock.lock();
        *generation += 1;
        self.changed.notify_all();
    }

    fn generation(&self) -> u64 {
        *self.lock.lock()
    }

    fn wait_after(&self, previous: u64) -> u64 {
        let mut generation = self.lock.lock();
        while *generation <= previous {
            self.changed.wait(&mut generation);
        }
        *generation
    }

    fn wait_for(&self, mut ready: impl FnMut() -> bool) {
        let mut generation = self.lock.lock();
        while !ready() {
            self.changed.wait(&mut generation);
        }
    }
}

/// A running audio engine. Drop it to stop all audio work.
pub struct AudioEngine {
    stop: Option<std_mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

type Ready = Result<(Option<DeviceFormat>, Option<DeviceFormat>), AudioError>;

impl AudioEngine {
    /// Opens the configured devices on a new owner thread and starts
    /// processing. Blocks until the streams are running or have failed, so
    /// call it off the UI thread.
    pub fn start(config: EngineConfig) -> Result<(Self, EngineChannels), AudioError> {
        Self::start_with(config, CpalBackend::new)
    }

    pub(crate) fn start_with<B, F>(
        config: EngineConfig,
        make_backend: F,
    ) -> Result<(Self, EngineChannels), AudioError>
    where
        B: Backend,
        F: FnOnce() -> B + Send + 'static,
    {
        Self::start_with_denoise(config, make_backend, high_quality_factory())
    }

    /// As [`AudioEngine::start_with`], with the High quality engine built by `high_quality`
    /// on the denoise worker thread.
    pub(crate) fn start_with_denoise<B, F>(
        config: EngineConfig,
        make_backend: F,
        high_quality: Factory,
    ) -> Result<(Self, EngineChannels), AudioError>
    where
        B: Backend,
        F: FnOnce() -> B + Send + 'static,
    {
        if config.input.is_none() && config.output.is_none() {
            return Err(AudioError::NothingToRun);
        }
        let (captured_tx, captured_rx) = mpsc::channel(CAPTURE_QUEUE);
        let (playback_tx, playback_rx) = mpsc::channel(PLAYBACK_QUEUE);
        let (remote_tx, remote_rx) = mpsc::channel(PLAYBACK_QUEUE);
        let (controls_tx, controls_rx) = mpsc::channel(32);
        let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
        let (stop_tx, stop_rx) = std_mpsc::channel();
        let (ready_tx, ready_rx) = std_mpsc::sync_channel::<Ready>(1);
        let shared = Arc::new(Shared::default());
        shared
            .capture_enabled
            .store(config.capture_enabled, Ordering::Release);
        let has_input = config.input.is_some();
        let has_output = config.output.is_some();
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("fastcord-audio".to_owned())
            .spawn(move || {
                let io = OwnerIo {
                    stop: stop_rx,
                    captured: Some(captured_tx),
                    playback: Some(playback_rx),
                    remote_audio: Some(remote_rx),
                    controls: controls_rx,
                    events: events_tx,
                };
                run_owner(
                    make_backend(),
                    config,
                    &thread_shared,
                    io,
                    ready_tx,
                    high_quality,
                );
            })
            .map_err(|_| AudioError::EngineThread)?;
        let ready = ready_rx.recv();
        match ready {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(AudioError::EngineThread);
            }
        };
        let engine = Self {
            stop: Some(stop_tx),
            thread: Some(thread),
            shared,
        };
        let channels = EngineChannels {
            captured: has_input.then_some(captured_rx),
            playback: has_output.then_some(playback_tx),
            remote_audio: has_output.then_some(remote_tx),
            controls: controls_tx,
            events: events_rx,
        };
        Ok((engine, channels))
    }

    /// Format of the open input stream.
    pub fn input_format(&self) -> Option<DeviceFormat> {
        load_format(&self.shared.input_format)
    }

    /// Format of the open output stream.
    pub fn output_format(&self) -> Option<DeviceFormat> {
        load_format(&self.shared.output_format)
    }

    pub fn stats(&self) -> EngineStats {
        let input = &self.shared.input;
        let output = &self.shared.output;
        let pipeline = &self.shared.pipeline;
        let load = |counter: &std::sync::atomic::AtomicU64| counter.load(Ordering::Relaxed);
        EngineStats {
            input_callbacks: load(&input.callbacks),
            input_overrun_samples: load(&input.overrun_samples),
            output_callbacks: load(&output.callbacks),
            output_underruns: load(&output.underruns),
            output_underrun_samples: load(&output.underrun_samples),
            device_xruns: load(&input.xruns) + load(&output.xruns),
            frames_encoded: load(&pipeline.frames_encoded),
            frames_dropped: load(&pipeline.frames_dropped),
            encode_errors: load(&pipeline.encode_errors),
            denoise_blocks: load(&pipeline.denoise_blocks),
            packets_decoded: load(&pipeline.packets_decoded),
            decode_errors: load(&pipeline.decode_errors),
            plc_frames: load(&pipeline.plc_frames),
        }
    }

    /// Weak handles to counters held by the application's data and error
    /// closures: once both are dead, those callback resources have retired.
    /// This does not assert completion of every backend thread or native
    /// disposal operation that can follow destruction of our closures.
    #[cfg(test)]
    fn callback_probes(
        &self,
    ) -> (
        std::sync::Weak<StreamCounters>,
        std::sync::Weak<StreamCounters>,
    ) {
        (
            Arc::downgrade(&self.shared.input),
            Arc::downgrade(&self.shared.output),
        )
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        // Disconnecting the stop channel wakes the owner thread at once.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl std::fmt::Debug for AudioEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEngine")
            .field("input", &self.input_format())
            .field("output", &self.output_format())
            .finish_non_exhaustive()
    }
}

struct OwnerIo {
    stop: std_mpsc::Receiver<()>,
    captured: Option<mpsc::Sender<CapturedFrame>>,
    playback: Option<mpsc::Receiver<EncodedPacket>>,
    remote_audio: Option<mpsc::Receiver<RemoteFrame>>,
    controls: mpsc::Receiver<EngineControl>,
    events: mpsc::Sender<EngineEvent>,
}

fn open_capture<B: Backend>(
    backend: &mut B,
    choice: &DeviceChoice,
    counters: &Arc<StreamCounters>,
    enabled: &Arc<AtomicBool>,
    bitrate: u32,
) -> Result<(B::Stream, CapturePipeline, StreamFormat), AudioError> {
    let mut consumer = None;
    let (stream, format) = backend.open_input(choice, counters, &mut |format| {
        let (producer, ring) = sample_ring(format.channels, frames_for(format.rate, RING_LIMIT_MS));
        consumer = Some(ring);
        InputCallback::new(producer, Arc::clone(counters), Arc::clone(enabled))
    })?;
    let ring = consumer.ok_or(AudioError::EngineThread)?;
    let pipeline = CapturePipeline::new(format, ring, bitrate)?;
    Ok((stream, pipeline, format))
}

fn open_playback<B: Backend>(
    backend: &mut B,
    choice: &DeviceChoice,
    counters: &Arc<StreamCounters>,
) -> Result<(B::Stream, PlaybackPipeline, StreamFormat), AudioError> {
    let mut producer = None;
    let (stream, format) = backend.open_output(choice, counters, &mut |format| {
        let (ring, consumer) = sample_ring(format.channels, frames_for(format.rate, RING_LIMIT_MS));
        producer = Some(ring);
        OutputCallback::new(consumer, Arc::clone(counters))
    })?;
    let ring = producer.ok_or(AudioError::EngineThread)?;
    let pipeline = PlaybackPipeline::new(format, ring)?;
    Ok((stream, pipeline, format))
}

fn run_owner<B: Backend>(
    mut backend: B,
    mut config: EngineConfig,
    shared: &Shared,
    mut io: OwnerIo,
    ready: std_mpsc::SyncSender<Ready>,
    high_quality: Factory,
) {
    let mut capture = None;
    let mut playback = None;
    let opened = (|| {
        let mut formats = (None, None);
        if let Some(choice) = &config.input {
            let (stream, pipeline, format) = open_capture(
                &mut backend,
                choice,
                &shared.input,
                &shared.capture_enabled,
                config.bitrate,
            )?;
            capture = Some((stream, pipeline));
            formats.0 = Some(format.into());
        }
        if let Some(choice) = &config.output {
            let (stream, pipeline, format) = open_playback(&mut backend, choice, &shared.output)?;
            playback = Some((stream, pipeline));
            formats.1 = Some(format.into());
        }
        Ok(formats)
    })();
    if let Ok((input, output)) = &opened {
        store_format(&shared.input_format, *input);
        store_format(&shared.output_format, *output);
    }
    let mut defaults = [
        backend.default_key(Direction::Input),
        backend.default_key(Direction::Output),
    ];
    let failed = opened.is_err();
    if ready.send(opened).is_err() || failed {
        // Streams opened before the failure are dropped (stopped) here.
        return;
    }

    // Dropping the engine handle disconnects the stop channel, ending the loop.
    let mut capture_enabled = config.capture_enabled;
    let mut denoiser = config
        .input
        .as_ref()
        .map(|_| MicrophoneDenoiser::new(config.noise_suppression, high_quality.clone()));
    let mut output_enabled = true;
    let mut echo_enabled = config.echo_cancellation;
    let mut echo: Option<Box<EchoCanceller>> = None;
    let mut gains = std::collections::HashMap::<u32, f32>::new();
    let mut probe_at = std::time::Instant::now() + Duration::from_secs(1);
    // Coalesced input/output/suppression notifications bound control backpressure.
    let mut pending: [Option<EngineEvent>; 3] = [None, None, None];
    while let Err(std_mpsc::RecvTimeoutError::Timeout) = io.stop.recv_timeout(TICK) {
        for _ in 0..32 {
            let Ok(control) = io.controls.try_recv() else {
                break;
            };
            match control {
                EngineControl::CaptureEnabled(enabled) => {
                    if capture_enabled
                        && !enabled
                        && let Some(denoiser) = &mut denoiser
                    {
                        denoiser.reset();
                    }
                    capture_enabled = enabled;
                    shared.capture_enabled.store(enabled, Ordering::Release);
                }
                EngineControl::OutputEnabled(enabled) => {
                    output_enabled = enabled;
                    if let Some((_, pipeline)) = playback.as_mut() {
                        pipeline.set_enabled(enabled);
                    }
                }
                EngineControl::RemoteGain { ssrc, gain } => {
                    if gains.len() >= 256 && !gains.contains_key(&ssrc) {
                        gains.clear();
                    }
                    gains.insert(ssrc, gain);
                    if let Some((_, pipeline)) = playback.as_mut() {
                        pipeline.set_gain(ssrc, gain);
                    }
                }
                EngineControl::NoiseSuppression(mode) => {
                    config.noise_suppression = mode;
                    if let Some(denoiser) = &mut denoiser {
                        denoiser.select(mode);
                    }
                }
                EngineControl::EchoCancellation(enabled) => echo_enabled = enabled,
                EngineControl::SelectDevice { direction, choice } => {
                    if direction == Direction::Input {
                        if let Some(denoiser) = &mut denoiser {
                            denoiser.reset();
                        } else {
                            denoiser = Some(MicrophoneDenoiser::new(
                                config.noise_suppression,
                                high_quality.clone(),
                            ));
                        }
                    }
                    replace_device(
                        &mut backend,
                        direction,
                        &choice,
                        config.bitrate,
                        shared,
                        &mut capture,
                        &mut playback,
                        &mut io,
                        output_enabled,
                        &gains,
                        &mut pending,
                    );
                    echo = None;
                    match direction {
                        Direction::Input => config.input = Some(choice),
                        Direction::Output => config.output = Some(choice),
                    }
                }
            }
        }
        let probe = std::time::Instant::now() >= probe_at;
        #[cfg(test)]
        let probe = probe || shared.probe_defaults.swap(false, Ordering::AcqRel);
        if probe {
            probe_at = std::time::Instant::now() + Duration::from_secs(1);
            for (index, direction, choice) in [
                (0, Direction::Input, &config.input),
                (1, Direction::Output, &config.output),
            ] {
                if choice.as_ref() != Some(&DeviceChoice::SystemDefault) {
                    continue;
                }
                let current = backend.default_key(direction);
                if current != defaults[index] {
                    defaults[index] = current;
                    if direction == Direction::Input
                        && let Some(denoiser) = &mut denoiser
                    {
                        denoiser.reset();
                    }
                    replace_device(
                        &mut backend,
                        direction,
                        &DeviceChoice::SystemDefault,
                        config.bitrate,
                        shared,
                        &mut capture,
                        &mut playback,
                        &mut io,
                        output_enabled,
                        &gains,
                        &mut pending,
                    );
                    echo = None;
                }
            }
        }
        if capture.is_some()
            && let Some(kind) = shared.input.fault()
        {
            capture = None;
            drop(io.captured.take());
            store_format(&shared.input_format, None);
            pending[0] = Some(EngineEvent::StreamFailed {
                direction: Direction::Input,
                kind,
            });
        }
        if playback.is_some()
            && let Some(kind) = shared.output.fault()
        {
            // Dropping the receiver closes senders and discards queued packets.
            drop(io.playback.take());
            drop(io.remote_audio.take());
            playback = None;
            store_format(&shared.output_format, None);
            pending[1] = Some(EngineEvent::StreamFailed {
                direction: Direction::Output,
                kind,
            });
        }
        if let Some(denoiser) = &mut denoiser {
            denoiser.poll();
        }
        // Echo cancellation needs both streams; it follows the preference and is rebuilt
        // from scratch after either device changes.
        match (&mut playback, echo_enabled && capture.is_some()) {
            (Some((_, pipeline)), true) => {
                let canceller = echo.get_or_insert_with(|| Box::new(EchoCanceller::new()));
                pipeline.set_echo_reference(true);
                pipeline.feed_echo(canceller, std::time::Instant::now());
            }
            (playback, _) => {
                echo = None;
                if let Some((_, pipeline)) = playback {
                    pipeline.set_echo_reference(false);
                }
            }
        }
        if let Some((_, pipeline)) = capture.as_mut() {
            if capture_enabled {
                if let (Some(captured), Some(denoiser)) = (io.captured.as_ref(), denoiser.as_mut())
                {
                    pipeline.run(
                        &shared.pipeline,
                        denoiser,
                        echo.as_deref_mut(),
                        &mut |frame| {
                            if captured.try_send(frame).is_err() {
                                shared
                                    .pipeline
                                    .frames_dropped
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        },
                    );
                }
            } else {
                pipeline.discard();
                if let Some(echo) = &mut echo {
                    echo.clear_far();
                }
            }
        }
        if let Some(denoiser) = &mut denoiser {
            shared
                .pipeline
                .denoise_blocks
                .store(denoiser.inference_blocks(), Ordering::Relaxed);
            if let Some(status) = denoiser.take_status() {
                pending[2] = Some(EngineEvent::NoiseSuppressionChanged(status));
            }
        }
        if let Some((_, pipeline)) = playback.as_mut() {
            if output_enabled {
                if let Some(remote) = io.remote_audio.as_mut() {
                    for _ in 0..PLAYBACK_QUEUE {
                        let Ok(packet) = remote.try_recv() else { break };
                        pipeline.push_remote(packet);
                    }
                }
                if let Some(playback_receiver) = io.playback.as_mut() {
                    pipeline.run(&shared.pipeline, &mut || playback_receiver.try_recv().ok());
                }
            } else {
                for _ in 0..PLAYBACK_QUEUE {
                    if !io
                        .remote_audio
                        .as_mut()
                        .is_some_and(|rx| rx.try_recv().is_ok())
                    {
                        break;
                    }
                }
                for _ in 0..PLAYBACK_QUEUE {
                    if !io.playback.as_mut().is_some_and(|rx| rx.try_recv().is_ok()) {
                        break;
                    }
                }
            }
        }
        for slot in &mut pending {
            if let Some(event) = slot.take()
                && let Err(mpsc::error::TrySendError::Full(event)) = io.events.try_send(event)
            {
                *slot = Some(event);
            }
        }
        #[cfg(test)]
        shared.owner_service.notify();
    }
    // Stop the device streams (joining their callback threads) before the
    // owner thread exits.
    drop(capture);
    drop(playback);
}

type CaptureStream<S> = Option<(S, CapturePipeline)>;
type PlaybackStream<S> = Option<(S, PlaybackPipeline)>;

#[allow(clippy::too_many_arguments)]
fn replace_device<B: Backend>(
    backend: &mut B,
    direction: Direction,
    choice: &DeviceChoice,
    bitrate: u32,
    shared: &Shared,
    capture: &mut CaptureStream<B::Stream>,
    playback: &mut PlaybackStream<B::Stream>,
    io: &mut OwnerIo,
    output_enabled: bool,
    gains: &std::collections::HashMap<u32, f32>,
    pending: &mut [Option<EngineEvent>; 3],
) {
    let result = match direction {
        Direction::Input => {
            // Close the old microphone and endpoint before starting any new callbacks.
            *capture = None;
            io.captured = None;
            shared.input.clear_fault();
            store_format(&shared.input_format, None);
            open_capture(
                backend,
                choice,
                &shared.input,
                &shared.capture_enabled,
                bitrate,
            )
            .map(|(stream, pipeline, format)| {
                let (tx, rx) = mpsc::channel(CAPTURE_QUEUE);
                io.captured = Some(tx);
                *capture = Some((stream, pipeline));
                store_format(&shared.input_format, Some(format.into()));
                EngineEvent::StreamReady {
                    direction,
                    format: format.into(),
                    captured: Some(rx),
                    playback: None,
                    remote_audio: None,
                }
            })
        }
        Direction::Output => {
            *playback = None;
            io.playback = None;
            io.remote_audio = None;
            shared.output.clear_fault();
            store_format(&shared.output_format, None);
            open_playback(backend, choice, &shared.output).map(|(stream, mut pipeline, format)| {
                pipeline.set_enabled(output_enabled);
                for (&ssrc, &gain) in gains {
                    pipeline.set_gain(ssrc, gain);
                }
                let (tx, rx) = mpsc::channel(PLAYBACK_QUEUE);
                let (remote_tx, remote_rx) = mpsc::channel(PLAYBACK_QUEUE);
                io.playback = Some(rx);
                io.remote_audio = Some(remote_rx);
                *playback = Some((stream, pipeline));
                store_format(&shared.output_format, Some(format.into()));
                EngineEvent::StreamReady {
                    direction,
                    format: format.into(),
                    captured: None,
                    playback: Some(tx),
                    remote_audio: Some(remote_tx),
                }
            })
        }
    };
    let index = usize::from(direction == Direction::Output);
    pending[index] = Some(result.unwrap_or_else(|error| EngineEvent::StreamFailed {
        direction,
        kind: match error {
            AudioError::Device(_, kind) => kind,
            AudioError::DeviceNotFound(_) | AudioError::NoDefaultDevice(_) => {
                DeviceErrorKind::Disconnected
            }
            AudioError::Rate(_) => DeviceErrorKind::Unsupported,
            _ => DeviceErrorKind::Other,
        },
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{FRAME_SAMPLES, VoiceEncoder};
    use crate::device::DeviceKey;
    use crate::resample::tests::tone_level;
    use fastcord_media::TransportMode;
    use fastcord_media::crypto::{TransportCipher, TransportKey};
    use fastcord_media::rtp::RtpHeader;
    use parking_lot::Mutex;
    use std::sync::Weak;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Instant;

    /// Fake device host that can pace callbacks in real time or advance them
    /// explicitly behind a test handshake.
    #[derive(Clone, Default)]
    struct FakeHost {
        live_threads: Arc<AtomicUsize>,
        played: Arc<Mutex<Vec<f32>>>,
        input_rate: u32,
        output_rate: u32,
        fail_input: Option<DeviceErrorKind>,
        fail_output: Option<DeviceErrorKind>,
        manual_callbacks: bool,
        defaults: Arc<Mutex<[Option<DeviceKey>; 2]>>,
        opened: Arc<Mutex<Vec<(Direction, DeviceChoice)>>>,
        driver: Arc<FakeDriver>,
    }

    struct FakeStream {
        stop: Arc<AtomicBool>,
        manual: Option<std_mpsc::SyncSender<FakeCommand>>,
        thread: Option<JoinHandle<()>>,
    }

    #[derive(Debug)]
    enum FakeCommand {
        Step(std_mpsc::SyncSender<()>),
        Stop,
    }

    #[derive(Default)]
    struct FakeDriver {
        input: Mutex<Option<std_mpsc::SyncSender<FakeCommand>>>,
        output: Mutex<Option<std_mpsc::SyncSender<FakeCommand>>>,
    }

    impl FakeDriver {
        fn step(&self, direction: Direction) {
            let sender = match direction {
                Direction::Input => self.input.lock().as_ref().unwrap().clone(),
                Direction::Output => self.output.lock().as_ref().unwrap().clone(),
            };
            let (done_tx, done_rx) = std_mpsc::sync_channel(0);
            sender.send(FakeCommand::Step(done_tx)).unwrap();
            done_rx.recv().unwrap();
        }
    }

    impl Drop for FakeStream {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(manual) = self.manual.take() {
                let _ = manual.send(FakeCommand::Stop);
            }
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    impl FakeHost {
        fn spawn(
            &self,
            direction: Direction,
            mut body: impl FnMut() + Send + 'static,
        ) -> FakeStream {
            let stop = Arc::new(AtomicBool::new(false));
            let live = Arc::clone(&self.live_threads);
            live.fetch_add(1, Ordering::SeqCst);
            let (thread, manual) = if self.manual_callbacks {
                let (sender, receiver) = std_mpsc::sync_channel(0);
                match direction {
                    Direction::Input => *self.driver.input.lock() = Some(sender.clone()),
                    Direction::Output => *self.driver.output.lock() = Some(sender.clone()),
                }
                let thread = std::thread::spawn(move || {
                    while let Ok(command) = receiver.recv() {
                        match command {
                            FakeCommand::Step(done) => {
                                body();
                                let _ = done.send(());
                            }
                            FakeCommand::Stop => break,
                        }
                    }
                    live.fetch_sub(1, Ordering::SeqCst);
                });
                (thread, Some(sender))
            } else {
                let flag = Arc::clone(&stop);
                let thread = std::thread::spawn(move || {
                    let start = Instant::now();
                    let mut period = 0u32;
                    while !flag.load(Ordering::Relaxed) {
                        body();
                        period += 1;
                        let due = start + Duration::from_millis(10) * period;
                        std::thread::sleep(due.saturating_duration_since(Instant::now()));
                    }
                    live.fetch_sub(1, Ordering::SeqCst);
                });
                (thread, None)
            };
            FakeStream {
                stop,
                manual,
                thread: Some(thread),
            }
        }
    }

    fn check_choice(choice: &DeviceChoice, direction: Direction) -> Result<(), AudioError> {
        match choice {
            DeviceChoice::Device(key) if key.as_str() == "missing" => {
                Err(AudioError::DeviceNotFound(direction))
            }
            _ => Ok(()),
        }
    }

    fn fake_stream_error(kind: DeviceErrorKind) -> cpal::ErrorKind {
        match kind {
            DeviceErrorKind::Disconnected => cpal::ErrorKind::DeviceNotAvailable,
            _ => cpal::ErrorKind::Other,
        }
    }

    impl Backend for FakeHost {
        type Stream = FakeStream;

        fn default_key(&mut self, direction: Direction) -> Option<DeviceKey> {
            self.defaults.lock()[usize::from(direction == Direction::Output)].clone()
        }

        fn open_input(
            &mut self,
            choice: &DeviceChoice,
            counters: &Arc<StreamCounters>,
            make: &mut dyn FnMut(StreamFormat) -> InputCallback,
        ) -> Result<(FakeStream, StreamFormat), AudioError> {
            assert_eq!(std::thread::current().name(), Some("fastcord-audio"));
            self.opened.lock().push((Direction::Input, choice.clone()));
            let choice = match choice {
                DeviceChoice::SystemDefault => DeviceChoice::Device(
                    self.default_key(Direction::Input)
                        .ok_or(AudioError::NoDefaultDevice(Direction::Input))?,
                ),
                explicit => explicit.clone(),
            };
            check_choice(&choice, Direction::Input)?;
            let rate = if matches!(&choice, DeviceChoice::Device(key) if key.as_str() == "44k") {
                44_100
            } else {
                self.input_rate
            };
            let format = StreamFormat {
                rate,
                channels: 2,
                period_frames: Some(rate / 100),
            };
            let mut callback = make(format);
            let fail = self.fail_input;
            let counters = Arc::clone(counters);
            let mut phase = 0u64;
            let mut buffer = vec![0i16; (rate / 100) as usize * 2];
            let mut calls = 0;
            let stream = self.spawn(Direction::Input, move || {
                for frame in buffer.as_chunks_mut::<2>().0 {
                    let t = phase as f32 / rate as f32;
                    let sample = (0.5
                        * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                        * f32::from(i16::MAX)) as i16;
                    frame.fill(sample);
                    phase += 1;
                }
                callback.process(&buffer);
                calls += 1;
                if calls == 20
                    && let Some(kind) = fail
                {
                    counters.record_error(fake_stream_error(kind));
                }
            });
            Ok((stream, format))
        }

        fn open_output(
            &mut self,
            choice: &DeviceChoice,
            counters: &Arc<StreamCounters>,
            make: &mut dyn FnMut(StreamFormat) -> OutputCallback,
        ) -> Result<(FakeStream, StreamFormat), AudioError> {
            assert_eq!(std::thread::current().name(), Some("fastcord-audio"));
            self.opened.lock().push((Direction::Output, choice.clone()));
            let choice = match choice {
                DeviceChoice::SystemDefault => DeviceChoice::Device(
                    self.default_key(Direction::Output)
                        .ok_or(AudioError::NoDefaultDevice(Direction::Output))?,
                ),
                explicit => explicit.clone(),
            };
            check_choice(&choice, Direction::Output)?;
            let rate = if matches!(&choice, DeviceChoice::Device(key) if key.as_str() == "44k") {
                44_100
            } else {
                self.output_rate
            };
            let format = StreamFormat {
                rate,
                channels: 2,
                period_frames: Some(rate / 100),
            };
            let mut callback = make(format);
            let played = Arc::clone(&self.played);
            let mut calls = 0;
            let fail = self.fail_output;
            let counters = Arc::clone(counters);
            let mut buffer = vec![0.0f32; (rate / 100) as usize * 2];
            let stream = self.spawn(Direction::Output, move || {
                callback.process(&mut buffer);
                played.lock().extend(buffer.iter().step_by(2));
                calls += 1;
                if calls == 20
                    && let Some(kind) = fail
                {
                    counters.record_error(fake_stream_error(kind));
                }
            });
            Ok((stream, format))
        }
    }

    fn host(input_rate: u32, output_rate: u32) -> FakeHost {
        FakeHost {
            defaults: Arc::new(Mutex::new([
                DeviceKey::from_persisted("fake-input"),
                DeviceKey::from_persisted("fake-output"),
            ])),
            input_rate,
            output_rate,
            ..FakeHost::default()
        }
    }

    fn assert_torn_down(host: &FakeHost, probes: (Weak<StreamCounters>, Weak<StreamCounters>)) {
        assert_eq!(
            host.live_threads.load(Ordering::SeqCst),
            0,
            "callback threads joined"
        );
        assert!(probes.0.upgrade().is_none(), "input callback dropped");
        assert!(probes.1.upgrade().is_none(), "output callback dropped");
    }

    fn explicit(id: &str) -> DeviceChoice {
        DeviceChoice::Device(DeviceKey::from_persisted(id).unwrap())
    }

    fn expect_initial_noise_suppression(events: &mut mpsc::Receiver<EngineEvent>) {
        let Some(EngineEvent::NoiseSuppressionChanged(status)) = events.blocking_recv() else {
            panic!("initial noise suppression status missing");
        };
        assert_eq!(status, NoiseSuppressionStatus::default());
    }

    #[test]
    fn switching_directions_closes_old_endpoints_preserves_controls_and_converts_formats() {
        let mut fake = host(48_000, 48_000);
        fake.manual_callbacks = true;
        let backend = fake.clone();
        let (engine, mut channels) =
            AudioEngine::start_with(EngineConfig::default(), move || backend).unwrap();
        expect_initial_noise_suppression(&mut channels.events);
        let mut old_input = channels.captured.take().unwrap();
        let old_output = channels.remote_audio.take().unwrap();
        let owner = &engine.shared.owner_service;
        channels
            .controls
            .blocking_send(EngineControl::CaptureEnabled(false))
            .unwrap();
        channels
            .controls
            .blocking_send(EngineControl::OutputEnabled(false))
            .unwrap();
        channels
            .controls
            .blocking_send(EngineControl::RemoteGain { ssrc: 9, gain: 0.0 })
            .unwrap();
        channels
            .controls
            .blocking_send(EngineControl::SelectDevice {
                direction: Direction::Input,
                choice: explicit("44k"),
            })
            .unwrap();
        let Some(EngineEvent::StreamReady {
            direction: Direction::Input,
            format,
            captured: Some(mut captured),
            ..
        }) = channels.events.blocking_recv()
        else {
            panic!("input not replaced")
        };
        assert_eq!(format.sample_rate, 44_100);
        assert_eq!(engine.input_format(), Some(format));
        assert!(matches!(
            old_input.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        assert!(
            !old_output.is_closed(),
            "healthy output must not close for an input switch"
        );
        let before = engine.stats().frames_encoded;
        for _ in 0..4 {
            let generation = owner.generation();
            fake.driver.step(Direction::Input);
            owner.wait_after(generation);
        }
        assert_eq!(
            engine.stats().frames_encoded,
            before,
            "mute survived replacement"
        );
        channels
            .controls
            .blocking_send(EngineControl::SelectDevice {
                direction: Direction::Output,
                choice: explicit("44k"),
            })
            .unwrap();
        let Some(EngineEvent::StreamReady {
            direction: Direction::Output,
            format,
            remote_audio: Some(remote),
            ..
        }) = channels.events.blocking_recv()
        else {
            panic!("output not replaced")
        };
        assert_eq!(format.sample_rate, 44_100);
        assert!(old_output.is_closed());
        assert_eq!(engine.output_format(), Some(format));
        channels
            .controls
            .blocking_send(EngineControl::CaptureEnabled(true))
            .unwrap();
        let mut generation = owner.generation();
        owner.wait_after(generation);
        for _ in 0..8 {
            fake.driver.step(Direction::Input);
            generation = owner.wait_after(generation);
        }
        let frame = captured.try_recv().unwrap();
        remote
            .blocking_send(RemoteFrame {
                ssrc: 9,
                sequence: 1,
                timestamp: 960,
                packet: frame.packet.clone(),
            })
            .unwrap();
        generation = owner.wait_after(generation);
        fake.driver.step(Direction::Output);
        owner.wait_after(generation);
        assert!(
            fake.played.lock().iter().all(|sample| *sample == 0.0),
            "deafen survived replacement"
        );
        channels
            .controls
            .blocking_send(EngineControl::OutputEnabled(true))
            .unwrap();
        generation = owner.generation();
        generation = owner.wait_after(generation);
        for sequence in 2..18 {
            remote
                .blocking_send(RemoteFrame {
                    ssrc: 9,
                    sequence,
                    timestamp: sequence as u32 * 960,
                    packet: frame.packet.clone(),
                })
                .unwrap();
            generation = owner.wait_after(generation);
            fake.driver.step(Direction::Output);
            generation = owner.wait_after(generation);
        }
        assert!(
            fake.played
                .lock()
                .iter()
                .all(|sample| sample.abs() < 0.00001),
            "per-SSRC zero gain survived replacement"
        );
        drop(engine);
        assert_eq!(fake.live_threads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn missing_and_unplugged_explicit_output_never_open_default_and_can_be_reselected() {
        let mut fake = host(48_000, 48_000);
        fake.manual_callbacks = true;
        let backend = fake.clone();
        let config = EngineConfig {
            output: Some(explicit("private-headset")),
            ..EngineConfig::default()
        };
        let (engine, mut channels) = AudioEngine::start_with(config, move || backend).unwrap();
        expect_initial_noise_suppression(&mut channels.events);
        let old_output = channels.remote_audio.take().unwrap();
        engine
            .shared
            .output
            .record_error(cpal::ErrorKind::DeviceNotAvailable);
        assert!(matches!(
            channels.events.blocking_recv(),
            Some(EngineEvent::StreamFailed {
                direction: Direction::Output,
                kind: DeviceErrorKind::Disconnected
            })
        ));
        assert!(old_output.is_closed());
        assert_eq!(fake.live_threads.load(Ordering::SeqCst), 1);
        assert_eq!(engine.output_format(), None);
        channels
            .controls
            .blocking_send(EngineControl::SelectDevice {
                direction: Direction::Output,
                choice: explicit("missing"),
            })
            .unwrap();
        assert!(matches!(
            channels.events.blocking_recv(),
            Some(EngineEvent::StreamFailed {
                direction: Direction::Output,
                kind: DeviceErrorKind::Disconnected
            })
        ));
        assert!(
            !fake
                .opened
                .lock()
                .iter()
                .any(|(direction, choice)| *direction == Direction::Output
                    && *choice == DeviceChoice::SystemDefault)
        );
        channels
            .controls
            .blocking_send(EngineControl::SelectDevice {
                direction: Direction::Output,
                choice: explicit("private-headset"),
            })
            .unwrap();
        assert!(matches!(
            channels.events.blocking_recv(),
            Some(EngineEvent::StreamReady {
                direction: Direction::Output,
                ..
            })
        ));
        assert_eq!(fake.live_threads.load(Ordering::SeqCst), 2);
        assert_eq!(engine.output_format().unwrap().sample_rate, 48_000);
        drop(engine);
    }

    #[test]
    fn only_chosen_system_default_follows_endpoint_changes() {
        let mut fake = host(48_000, 48_000);
        fake.manual_callbacks = true;
        let backend = fake.clone();
        let config = EngineConfig {
            output: Some(explicit("private-headset")),
            ..EngineConfig::default()
        };
        let (engine, mut channels) = AudioEngine::start_with(config, move || backend).unwrap();
        expect_initial_noise_suppression(&mut channels.events);
        *fake.defaults.lock() = [
            DeviceKey::from_persisted("44k"),
            DeviceKey::from_persisted("new-speakers"),
        ];
        engine.shared.probe_defaults.store(true, Ordering::Release);
        assert!(matches!(
            channels.events.blocking_recv(),
            Some(EngineEvent::StreamReady {
                direction: Direction::Input,
                format: DeviceFormat {
                    sample_rate: 44_100,
                    ..
                },
                ..
            })
        ));
        assert_eq!(
            fake.opened
                .lock()
                .iter()
                .filter(|(direction, _)| *direction == Direction::Output)
                .count(),
            1
        );
        channels
            .controls
            .blocking_send(EngineControl::SelectDevice {
                direction: Direction::Output,
                choice: DeviceChoice::SystemDefault,
            })
            .unwrap();
        assert!(matches!(
            channels.events.blocking_recv(),
            Some(EngineEvent::StreamReady {
                direction: Direction::Output,
                ..
            })
        ));
        fake.defaults.lock()[1] = None;
        engine.shared.probe_defaults.store(true, Ordering::Release);
        assert!(matches!(
            channels.events.blocking_recv(),
            Some(EngineEvent::StreamFailed {
                direction: Direction::Output,
                ..
            })
        ));
        assert_eq!(fake.live_threads.load(Ordering::SeqCst), 1);
        fake.defaults.lock()[1] = DeviceKey::from_persisted("44k");
        engine.shared.probe_defaults.store(true, Ordering::Release);
        assert!(matches!(
            channels.events.blocking_recv(),
            Some(EngineEvent::StreamReady {
                direction: Direction::Output,
                format: DeviceFormat {
                    sample_rate: 44_100,
                    ..
                },
                ..
            })
        ));
        drop(engine);
    }

    /// Microphone → Opus → playback through the real engine thread, callbacks,
    /// rings, resamplers, and codec, with barrier-stepped fake device threads.
    #[test]
    fn loopback_round_trip_plays_the_captured_tone_and_tears_down() {
        let mut fake = host(44_100, 48_000);
        fake.manual_callbacks = true;
        let heap_before = crate::alloc_counter::callback_heap_ops();
        let backend = fake.clone();
        // A codec/device round-trip checks a pure tone, not speech enhancement; the tone
        // would also be the echo canceller's reference, which it rightly removes.
        let config = EngineConfig {
            noise_suppression: NoiseSuppression::Off,
            echo_cancellation: false,
            ..EngineConfig::default()
        };
        let (engine, channels) = AudioEngine::start_with(config, move || backend).unwrap();
        assert_eq!(
            engine.input_format(),
            Some(DeviceFormat {
                sample_rate: 44_100,
                channels: 2
            })
        );
        let mut captured = channels.captured.unwrap();
        let playback = channels.playback.unwrap();
        let mut sequences = Vec::with_capacity(60);
        let owner_service = &engine.shared.owner_service;
        let mut service_generation = owner_service.generation();
        for sequence in 0..60_u64 {
            let frame = loop {
                fake.driver.step(Direction::Input);
                service_generation = owner_service.wait_after(service_generation);
                fake.driver.step(Direction::Output);
                match captured.try_recv() {
                    Ok(frame) => break frame,
                    Err(mpsc::error::TryRecvError::Empty) => {}
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        panic!("capture endpoint closed");
                    }
                }
            };
            assert_eq!(frame.sequence, sequence);
            sequences.push(frame.sequence);
            if sequence < 59 {
                playback.blocking_send(frame.packet).unwrap();
                let expected = sequence + 1;
                owner_service.wait_for(|| engine.stats().packets_decoded >= expected);
            }
        }
        assert_eq!(sequences, (0..60).collect::<Vec<u64>>());
        drop(playback);
        drop(captured);
        let stats = engine.stats();
        let probes = engine.callback_probes();
        drop(engine);
        assert_torn_down(&fake, probes);

        assert!(stats.input_callbacks >= 100, "{stats:?}");
        assert!(stats.frames_encoded >= 60, "{stats:?}");
        assert_eq!(stats.packets_decoded, 59, "{stats:?}");
        assert_eq!(stats.decode_errors + stats.encode_errors, 0);
        assert_eq!(stats.input_overrun_samples, 0, "{stats:?}");
        assert_eq!(
            crate::alloc_counter::callback_heap_ops(),
            heap_before,
            "a device callback touched the heap"
        );
        let played = fake.played.lock();
        let start = played
            .iter()
            .position(|s| s.abs() > 0.05)
            .expect("captured tone was played");
        let audible = &played[start + 4_800..start + 4_800 + 30 * FRAME_SAMPLES];
        let level = tone_level(audible, 48_000, 440.0);
        assert!((level - 0.5).abs() < 0.08, "played level {level}");
        // Local latency: capture to playback start well inside 80 ms plus the
        // fake devices' 10 ms periods and the 20 ms forwarding granularity.
        assert!(start < 48_000 / 4, "audio started after {start} samples");
    }

    #[test]
    fn capture_control_stops_processing_and_resumes_without_stale_samples() {
        let mut fake = host(48_000, 48_000);
        fake.manual_callbacks = true;
        let backend = fake.clone();
        let (engine, mut channels) =
            AudioEngine::start_with(EngineConfig::default(), move || backend).unwrap();
        let controls = channels.controls;
        let mut captured = channels.captured.take().unwrap();
        let owner = &engine.shared.owner_service;
        let mut generation = owner.generation();
        controls
            .blocking_send(EngineControl::CaptureEnabled(false))
            .unwrap();
        generation = owner.wait_after(generation);
        for _ in 0..4 {
            fake.driver.step(Direction::Input);
            generation = owner.wait_after(generation);
        }
        assert_eq!(engine.stats().frames_encoded, 0);
        assert_eq!(engine.stats().denoise_blocks, 0);
        assert!(captured.try_recv().is_err());

        controls
            .blocking_send(EngineControl::CaptureEnabled(true))
            .unwrap();
        generation = owner.wait_after(generation);
        for _ in 0..4 {
            fake.driver.step(Direction::Input);
            generation = owner.wait_after(generation);
            if captured.try_recv().is_ok() {
                break;
            }
        }
        assert_eq!(engine.stats().frames_encoded, 1);
        drop(captured);
        let probes = engine.callback_probes();
        drop(engine);
        assert_torn_down(&fake, probes);
        let _ = generation;
    }

    fn next_noise_status(events: &mut mpsc::Receiver<EngineEvent>) -> NoiseSuppressionStatus {
        loop {
            match events.blocking_recv() {
                Some(EngineEvent::NoiseSuppressionChanged(status)) => return status,
                Some(_) => {}
                None => panic!("engine events closed"),
            }
        }
    }

    /// Steps both fake devices once and returns the encoded frames that became available.
    fn step_call(
        fake: &FakeHost,
        owner: &OwnerService,
        generation: &mut u64,
        captured: &mut mpsc::Receiver<CapturedFrame>,
    ) -> Vec<u64> {
        fake.driver.step(Direction::Input);
        *generation = owner.wait_after(*generation);
        fake.driver.step(Direction::Output);
        std::iter::from_fn(|| captured.try_recv().ok())
            .map(|frame| frame.sequence)
            .collect()
    }

    #[test]
    fn selecting_high_quality_mid_call_never_stalls_the_owner_and_leaves_no_gap() {
        use crate::denoise::fake::{Probe, Spec, factory, wait_until};
        let mut fake = host(48_000, 48_000);
        fake.manual_callbacks = true;
        let backend = fake.clone();
        let probe = Arc::new(Probe::default());
        let spec = Spec::default().gated();
        let gate = Arc::clone(&spec.gate);
        let config = EngineConfig {
            noise_suppression: NoiseSuppression::Off,
            ..EngineConfig::default()
        };
        let (engine, mut channels) =
            AudioEngine::start_with_denoise(config, move || backend, factory(spec, &probe))
                .unwrap();
        assert_eq!(
            next_noise_status(&mut channels.events).active,
            NoiseSuppression::Off
        );
        let mut captured = channels.captured.take().unwrap();
        let playback = channels.playback.take().unwrap();
        let owner = &engine.shared.owner_service;
        let mut generation = owner.generation();
        let mut sequences = Vec::new();

        // The model's build is shut behind a gate, so it cannot finish. The owner must
        // keep capturing and playing while it is "building".
        channels
            .controls
            .blocking_send(EngineControl::NoiseSuppression(
                NoiseSuppression::HighQuality,
            ))
            .unwrap();
        let loading = next_noise_status(&mut channels.events);
        assert!(loading.loading, "{loading:?}");
        assert_eq!(loading.active, NoiseSuppression::Off);
        let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();
        for _ in 0..10 {
            let mut packet = EncodedPacket::empty();
            encoder
                .encode_mono(&[0.1; FRAME_SAMPLES], &mut packet)
                .unwrap();
            playback.blocking_send(packet).unwrap();
        }
        for _ in 0..40 {
            sequences.extend(step_call(&fake, owner, &mut generation, &mut captured));
        }
        assert_eq!(sequences, (0..20).collect::<Vec<u64>>(), "capture stalled");
        assert_eq!(engine.stats().packets_decoded, 10, "playback stalled");
        assert_eq!(engine.stats().denoise_blocks, 0, "Off ran inference");

        // Open the gate: the worker is built, primed and takes over, still without a gap.
        gate.store(true, Ordering::Release);
        let mut hand_over = None;
        for _ in 0..400 {
            sequences.extend(step_call(&fake, owner, &mut generation, &mut captured));
            while let Ok(event) = channels.events.try_recv() {
                if let EngineEvent::NoiseSuppressionChanged(status) = event
                    && status.active == NoiseSuppression::HighQuality
                {
                    hand_over = Some(status);
                }
            }
            if hand_over.is_some() {
                break;
            }
        }
        let hand_over = hand_over.expect("High quality never took over");
        assert!(!hand_over.loading);
        assert_eq!(hand_over.latency_samples, 3 * crate::denoise::BLOCK_SAMPLES);
        for _ in 0..40 {
            sequences.extend(step_call(&fake, owner, &mut generation, &mut captured));
        }
        // Frames came one per two blocks all along: nothing was dropped or reordered, and
        // the stream never fell silent while the tone played.
        assert!(
            sequences.windows(2).all(|w| w[1] == w[0] + 1),
            "{sequences:?}"
        );
        assert_eq!(engine.stats().frames_dropped, 0);
        assert!(engine.stats().denoise_blocks > 0);

        // Switching away drops the runtime on the worker thread; audio keeps flowing.
        channels
            .controls
            .blocking_send(EngineControl::NoiseSuppression(NoiseSuppression::Light))
            .unwrap();
        let before = sequences.len();
        for _ in 0..40 {
            sequences.extend(step_call(&fake, owner, &mut generation, &mut captured));
        }
        assert!(
            sequences.len() > before + 15,
            "capture stalled after switching away"
        );
        assert!(
            sequences.windows(2).all(|w| w[1] == w[0] + 1),
            "{sequences:?}"
        );
        wait_until(|| probe.live.load(Ordering::SeqCst) == 0);
        assert_eq!(probe.dropped_on_worker.load(Ordering::SeqCst), 1);

        drop(captured);
        drop(playback);
        let probes = engine.callback_probes();
        drop(engine);
        assert_torn_down(&fake, probes);
    }

    #[test]
    fn muting_with_high_quality_active_runs_no_inference_and_leaves_the_runtime_idle() {
        use crate::denoise::fake::{Probe, Spec, factory};
        let mut fake = host(48_000, 48_000);
        fake.manual_callbacks = true;
        let backend = fake.clone();
        let probe = Arc::new(Probe::default());
        let config = EngineConfig {
            noise_suppression: NoiseSuppression::HighQuality,
            ..EngineConfig::default()
        };
        let (engine, mut channels) = AudioEngine::start_with_denoise(
            config,
            move || backend,
            factory(Spec::default(), &probe),
        )
        .unwrap();
        let mut captured = channels.captured.take().unwrap();
        let owner = &engine.shared.owner_service;
        let mut generation = owner.generation();
        let mut sequences = Vec::new();
        for _ in 0..400 {
            sequences.extend(step_call(&fake, owner, &mut generation, &mut captured));
            if next_noise_active(&mut channels.events) == Some(NoiseSuppression::HighQuality) {
                break;
            }
        }
        assert_eq!(probe.live.load(Ordering::SeqCst), 1, "no engine was built");
        channels
            .controls
            .blocking_send(EngineControl::CaptureEnabled(false))
            .unwrap();
        generation = owner.wait_after(generation);
        let (encoded, inferred) = (engine.stats().frames_encoded, engine.stats().denoise_blocks);
        for _ in 0..20 {
            sequences.extend(step_call(&fake, owner, &mut generation, &mut captured));
        }
        assert_eq!(
            engine.stats().frames_encoded,
            encoded,
            "muted audio was encoded"
        );
        assert_eq!(
            engine.stats().denoise_blocks,
            inferred,
            "muted audio was denoised"
        );

        channels
            .controls
            .blocking_send(EngineControl::CaptureEnabled(true))
            .unwrap();
        generation = owner.wait_after(generation);
        for _ in 0..40 {
            sequences.extend(step_call(&fake, owner, &mut generation, &mut captured));
        }
        assert!(
            engine.stats().frames_encoded > encoded,
            "capture did not resume"
        );
        assert!(
            engine.stats().denoise_blocks > inferred,
            "inference did not resume"
        );
        assert_eq!(
            probe.live.load(Ordering::SeqCst),
            1,
            "the runtime stayed loaded"
        );
        drop(captured);
        let probes = engine.callback_probes();
        drop(engine);
        assert_torn_down(&fake, probes);
    }

    fn next_noise_active(events: &mut mpsc::Receiver<EngineEvent>) -> Option<NoiseSuppression> {
        let mut active = None;
        while let Ok(event) = events.try_recv() {
            if let EngineEvent::NoiseSuppressionChanged(status) = event {
                active = Some(status.active);
            }
        }
        active
    }
    #[test]
    fn encrypted_multi_peer_rtp_reorders_recovers_loss_and_reaches_output() {
        let mut fake = host(48_000, 48_000);
        fake.manual_callbacks = true;
        let backend = fake.clone();
        let config = EngineConfig {
            input: None,
            output: Some(DeviceChoice::SystemDefault),
            bitrate: DEFAULT_BITRATE,
            ..EngineConfig::default()
        };
        let (engine, channels) = AudioEngine::start_with(config, move || backend).unwrap();
        let remote = channels.remote_audio.unwrap();

        let key = TransportKey::from_slice(&[7; 32]).unwrap();
        let mut sender = TransportCipher::new(TransportMode::Aes256GcmRtpSize, &key);
        let receiver = TransportCipher::new(TransportMode::Aes256GcmRtpSize, &key);
        let mut speakers = Vec::new();
        for ssrc in 1..=4 {
            let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();
            let mut frames = Vec::new();
            for sequence in 0..3 {
                let pcm = std::array::from_fn(|sample| {
                    0.4 * (std::f32::consts::TAU
                        * 440.0
                        * (sample + sequence * FRAME_SAMPLES) as f32
                        / 48_000.0)
                        .sin()
                });
                let mut packet = EncodedPacket::empty();
                encoder.encode_mono(&pcm, &mut packet).unwrap();
                frames.push(packet);
            }
            speakers.push((ssrc, frames));
        }

        let mut decrypted = Vec::new();
        for (ssrc, frames) in &speakers {
            // seq 2 arrives before seq 0; seq 1 is lost. The packet at seq 2
            // carries in-band FEC for seq 1.
            for sequence in [2_usize, 0] {
                let header = RtpHeader::new(
                    fastcord_media::rtp::OPUS_PAYLOAD_TYPE,
                    sequence as u16,
                    sequence as u32 * FRAME_SAMPLES as u32,
                    *ssrc,
                );
                let mut clear = Vec::new();
                header.write(&mut clear);
                let mut encrypted = Vec::new();
                sender
                    .seal(&clear, frames[sequence].as_bytes(), &mut encrypted)
                    .unwrap();
                let parsed = RtpHeader::parse(&encrypted).unwrap();
                let mut payload = Vec::new();
                receiver
                    .open(&encrypted, parsed.clear_len(), &mut payload)
                    .unwrap();
                decrypted.push(RemoteFrame {
                    ssrc: *ssrc,
                    sequence: sequence as u64,
                    timestamp: header.timestamp,
                    packet: EncodedPacket::from_slice(&payload).unwrap(),
                });
            }
        }
        for frame in decrypted {
            remote.try_send(frame).unwrap();
        }
        let heap_before = crate::alloc_counter::callback_heap_ops();
        let owner = &engine.shared.owner_service;
        let mut generation = owner.generation();
        for _ in 0..36 {
            fake.driver.step(Direction::Output);
            generation = owner.wait_after(generation);
        }
        assert_eq!(crate::alloc_counter::callback_heap_ops(), heap_before);
        assert!(
            fake.played.lock().iter().any(|sample| sample.abs() > 0.02),
            "decrypted multi-peer audio reached the fake output device"
        );
        let probes = engine.callback_probes();
        drop(engine);
        assert_torn_down(&fake, probes);
    }

    #[test]
    fn a_missing_selected_device_fails_start_without_leaving_threads() {
        let fake = host(48_000, 48_000);
        let backend = fake.clone();
        let config = EngineConfig {
            output: Some(DeviceChoice::Device(
                DeviceKey::from_persisted("missing").unwrap(),
            )),
            ..EngineConfig::default()
        };
        let error = AudioEngine::start_with(config, move || backend).unwrap_err();
        assert_eq!(error, AudioError::DeviceNotFound(Direction::Output));
        // The already-opened input stream was stopped again.
        assert_eq!(fake.live_threads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn nothing_to_run_is_rejected_before_spawning() {
        let config = EngineConfig {
            input: None,
            output: None,
            bitrate: DEFAULT_BITRATE,
            ..EngineConfig::default()
        };
        let error = AudioEngine::start_with(config, || host(48_000, 48_000)).unwrap_err();
        assert_eq!(error, AudioError::NothingToRun);
    }

    #[test]
    fn a_failed_device_is_closed_and_reported_while_the_other_keeps_running() {
        let mut fake = host(48_000, 48_000);
        fake.fail_input = Some(DeviceErrorKind::Disconnected);
        let backend = fake.clone();
        let (engine, mut channels) =
            AudioEngine::start_with(EngineConfig::default(), move || backend).unwrap();
        expect_initial_noise_suppression(&mut channels.events);
        let deadline = Instant::now() + Duration::from_secs(5);
        let event = loop {
            if let Ok(event) = channels.events.try_recv() {
                break event;
            }
            assert!(Instant::now() < deadline, "no failure event");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(
            event,
            EngineEvent::StreamFailed {
                direction: Direction::Input,
                kind: DeviceErrorKind::Disconnected
            }
        ));
        let mut captured = channels.captured.take().unwrap();
        let capture_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match captured.try_recv() {
                Ok(_) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => break,
                Err(mpsc::error::TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < capture_deadline,
                        "failed capture endpoint remained open"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        // Only the failed input's device thread was stopped.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(fake.live_threads.load(Ordering::SeqCst), 1);
        let before = engine.stats().output_callbacks;
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            engine.stats().output_callbacks > before,
            "output still running"
        );
        let probes = engine.callback_probes();
        drop(engine);
        assert_torn_down(&fake, probes);
    }

    #[test]
    fn output_failure_closes_playback_and_keeps_capture_running() {
        let mut fake = host(48_000, 48_000);
        fake.fail_output = Some(DeviceErrorKind::Disconnected);
        let backend = fake.clone();
        let (engine, mut channels) =
            AudioEngine::start_with(EngineConfig::default(), move || backend).unwrap();
        expect_initial_noise_suppression(&mut channels.events);
        let deadline = Instant::now() + Duration::from_secs(5);
        let event = loop {
            if let Ok(event) = channels.events.try_recv() {
                break event;
            }
            assert!(Instant::now() < deadline, "no failure event");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(
            event,
            EngineEvent::StreamFailed {
                direction: Direction::Output,
                kind: DeviceErrorKind::Disconnected
            }
        ));
        let playback = channels.playback.take().unwrap();
        assert!(playback.is_closed());
        assert!(matches!(
            playback.try_send(EncodedPacket::empty()),
            Err(mpsc::error::TrySendError::Closed(_))
        ));
        for _ in 0..=PLAYBACK_QUEUE {
            assert!(playback.blocking_send(EncodedPacket::empty()).is_err());
        }
        assert_eq!(fake.live_threads.load(Ordering::SeqCst), 1);
        let before = engine.stats().input_callbacks;
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            engine.stats().input_callbacks > before,
            "input still running"
        );
        let probes = engine.callback_probes();
        drop(engine);
        assert_torn_down(&fake, probes);
    }

    #[test]
    fn capture_only_engine_has_no_playback_channel() {
        let fake = host(48_000, 48_000);
        let backend = fake.clone();
        let config = EngineConfig {
            output: None,
            ..EngineConfig::default()
        };
        let (engine, channels) = AudioEngine::start_with(config, move || backend).unwrap();
        assert!(channels.playback.is_none());
        assert!(channels.captured.is_some());
        assert_eq!(engine.output_format(), None);
        assert_eq!(fake.live_threads.load(Ordering::SeqCst), 1);
        drop(engine);
        assert_eq!(fake.live_threads.load(Ordering::SeqCst), 0);
    }

    /// Real WASAPI/CoreAudio/ALSA switching on the running owner, without Discord.
    #[test]
    #[ignore = "needs real audio devices"]
    fn live_switches_available_devices_without_fallback() {
        let heap_before = crate::alloc_counter::callback_heap_ops();
        let inputs = crate::list_devices(Direction::Input).unwrap();
        let outputs = crate::list_devices(Direction::Output).unwrap();
        println!("available inputs: {inputs:?}; outputs: {outputs:?}");
        let (engine, mut channels) = AudioEngine::start(EngineConfig::default()).unwrap();
        expect_initial_noise_suppression(&mut channels.events);
        let controls = channels.controls.clone();
        for (direction, devices) in [(Direction::Input, &inputs), (Direction::Output, &outputs)] {
            for device in devices {
                controls
                    .blocking_send(EngineControl::SelectDevice {
                        direction,
                        choice: DeviceChoice::Device(device.key.clone()),
                    })
                    .unwrap();
                let event = channels.events.blocking_recv().unwrap();
                let EngineEvent::StreamReady {
                    format,
                    captured,
                    playback,
                    remote_audio,
                    ..
                } = event
                else {
                    panic!("device did not open: {event:?}")
                };
                println!("switched {direction:?}: {} {format:?}", device.label);
                match direction {
                    Direction::Input => channels.captured = captured,
                    Direction::Output => {
                        channels.playback = playback;
                        channels.remote_audio = remote_audio;
                    }
                }
                let before = engine.stats();
                std::thread::sleep(Duration::from_millis(200));
                let after = engine.stats();
                assert!(
                    after.input_callbacks > before.input_callbacks
                        && after.output_callbacks > before.output_callbacks,
                    "both directions must keep running"
                );
                if let Some(rx) = channels.captured.as_mut()
                    && let Some(tx) = channels.playback.as_ref()
                {
                    while let Ok(frame) = rx.try_recv() {
                        let _ = tx.try_send(frame.packet);
                    }
                }
            }
        }
        for direction in [Direction::Input, Direction::Output] {
            controls
                .blocking_send(EngineControl::SelectDevice {
                    direction,
                    choice: DeviceChoice::SystemDefault,
                })
                .unwrap();
            assert!(matches!(
                channels.events.blocking_recv(),
                Some(EngineEvent::StreamReady { .. })
            ));
        }
        controls
            .blocking_send(EngineControl::SelectDevice {
                direction: Direction::Output,
                choice: explicit("wasapi:fastcord-absent-device"),
            })
            .unwrap();
        assert!(matches!(
            channels.events.blocking_recv(),
            Some(EngineEvent::StreamFailed {
                direction: Direction::Output,
                kind: DeviceErrorKind::Disconnected
            })
        ));
        assert_eq!(engine.output_format(), None);
        let before = engine.stats().output_callbacks;
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            engine.stats().output_callbacks,
            before,
            "no speaker fallback"
        );
        controls
            .blocking_send(EngineControl::SelectDevice {
                direction: Direction::Output,
                choice: DeviceChoice::Device(outputs[0].key.clone()),
            })
            .unwrap();
        assert!(matches!(
            channels.events.blocking_recv(),
            Some(EngineEvent::StreamReady { .. })
        ));
        let stats = engine.stats();
        let probes = engine.callback_probes();
        drop(engine);
        assert!(probes.0.upgrade().is_none() && probes.1.upgrade().is_none());
        assert_eq!(crate::alloc_counter::callback_heap_ops(), heap_before);
        println!(
            "owner switching stats: {stats:?}; zero callback heap operations; streams released"
        );
    }

    /// Real devices: default microphone → Opus → default output, with the
    /// counting allocator watching the real cpal callback threads. Plays the
    /// microphone through the speakers (use headphones) for three seconds, or
    /// `FASTCORD_AUDIO_LOOPBACK_SECS` seconds for a listening check; run:
    /// `cargo test -p fastcord-audio live_ -- --ignored --nocapture --test-threads=1`.
    #[test]
    #[ignore = "needs real audio devices"]
    fn live_default_devices_loopback_round_trip() {
        let heap_before = crate::alloc_counter::callback_heap_ops();
        let (engine, channels) = AudioEngine::start(EngineConfig::default()).unwrap();
        println!(
            "input {:?}, output {:?}",
            engine.input_format(),
            engine.output_format()
        );
        let mut captured = channels.captured.unwrap();
        let playback = channels.playback.unwrap();
        // Decode a copy of the microphone stream to report its level.
        let mut monitor = crate::VoiceDecoder::new().unwrap();
        let mut pcm = vec![0.0; crate::codec::MAX_FRAME_SAMPLES * 2];
        let (mut energy, mut samples, mut peak) = (0.0f64, 0usize, 0.0f32);
        let seconds = std::env::var("FASTCORD_AUDIO_LOOPBACK_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(3);
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            while let Ok(frame) = captured.try_recv() {
                let frames = monitor.decode(frame.packet.as_bytes(), &mut pcm).unwrap();
                for &s in &pcm[..frames * 2] {
                    energy += f64::from(s * s);
                    peak = peak.max(s.abs());
                }
                samples += frames * 2;
                let _ = playback.try_send(frame.packet);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let stats = engine.stats();
        println!("{stats:#?}");
        let rms = (energy / samples.max(1) as f64).sqrt();
        println!(
            "microphone level: RMS {:.1} dBFS, peak {:.1} dBFS",
            20.0 * rms.max(1e-9).log10(),
            20.0 * f64::from(peak).max(1e-9).log10()
        );
        let probes = engine.callback_probes();
        drop(engine);
        assert!(probes.0.upgrade().is_none() && probes.1.upgrade().is_none());
        assert!(stats.input_callbacks > 0 && stats.output_callbacks > 0);
        // 50 frames per second, minus startup.
        let expected = (seconds * 50).saturating_sub(10);
        assert!(stats.frames_encoded >= expected, "{stats:?}");
        assert!(stats.packets_decoded >= expected, "{stats:?}");
        assert_eq!(crate::alloc_counter::callback_heap_ops(), heap_before);
        println!("no callback heap operations; callbacks dropped after teardown");
    }

    /// Real devices: muting stops capture processing and deafening stops
    /// playback decoding, and both resume, while real callbacks keep running.
    #[test]
    #[ignore = "needs real audio devices"]
    fn live_mute_and_deafen_controls_gate_real_devices() {
        let (engine, mut channels) = AudioEngine::start(EngineConfig::default()).unwrap();
        let mut captured = channels.captured.take().unwrap();
        let playback = channels.playback.take().unwrap();
        let controls = channels.controls;
        let settle = || std::thread::sleep(Duration::from_millis(400));
        let drain = |rx: &mut mpsc::Receiver<CapturedFrame>| while rx.try_recv().is_ok() {};

        settle();
        assert!(engine.stats().frames_encoded > 0, "capture never started");

        controls
            .blocking_send(EngineControl::CaptureEnabled(false))
            .unwrap();
        settle();
        drain(&mut captured);
        let muted = engine.stats();
        settle();
        let still_muted = engine.stats();
        assert_eq!(muted.frames_encoded, still_muted.frames_encoded);
        assert!(still_muted.input_callbacks > muted.input_callbacks);
        controls
            .blocking_send(EngineControl::CaptureEnabled(true))
            .unwrap();
        settle();
        assert!(engine.stats().frames_encoded > still_muted.frames_encoded);

        controls
            .blocking_send(EngineControl::OutputEnabled(false))
            .unwrap();
        settle();
        let decoded = engine.stats().packets_decoded;
        let mut encoder = crate::VoiceEncoder::new(DEFAULT_BITRATE).unwrap();
        for _ in 0..8 {
            let mut packet = EncodedPacket::empty();
            encoder
                .encode_mono(&[0.0; FRAME_SAMPLES], &mut packet)
                .unwrap();
            playback.blocking_send(packet).unwrap();
        }
        settle();
        assert_eq!(engine.stats().packets_decoded, decoded);
        controls
            .blocking_send(EngineControl::OutputEnabled(true))
            .unwrap();
        for _ in 0..4 {
            let mut packet = EncodedPacket::empty();
            encoder
                .encode_mono(&[0.0; FRAME_SAMPLES], &mut packet)
                .unwrap();
            playback.blocking_send(packet).unwrap();
        }
        settle();
        assert!(engine.stats().packets_decoded > decoded);
        println!("{:#?}", engine.stats());
    }

    /// Real microphone: High quality builds on its own thread while capture keeps running,
    /// takes over, and switching back to Light releases it; no capture overrun, dropped
    /// frame or callback heap operation happens along the way.
    #[test]
    #[ignore = "needs a real microphone; records and discards a few seconds"]
    fn live_high_quality_takes_over_on_the_real_microphone() {
        /// Consumes frames and collects status events for `millis`.
        fn pump(
            captured: &mut mpsc::Receiver<CapturedFrame>,
            events: &mut mpsc::Receiver<EngineEvent>,
            statuses: &mut Vec<NoiseSuppressionStatus>,
            millis: u64,
        ) {
            let end = Instant::now() + Duration::from_millis(millis);
            while Instant::now() < end {
                while captured.try_recv().is_ok() {}
                while let Ok(event) = events.try_recv() {
                    if let EngineEvent::NoiseSuppressionChanged(status) = event {
                        statuses.push(status);
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let heap_before = crate::alloc_counter::callback_heap_ops();
        let config = EngineConfig {
            output: None,
            ..EngineConfig::default()
        };
        let (engine, mut channels) = AudioEngine::start(config).unwrap();
        let mut captured = channels.captured.take().unwrap();
        let mut statuses = Vec::new();
        pump(&mut captured, &mut channels.events, &mut statuses, 500);
        let frames_before = engine.stats().frames_encoded;
        channels
            .controls
            .blocking_send(EngineControl::NoiseSuppression(
                NoiseSuppression::HighQuality,
            ))
            .unwrap();
        let started = Instant::now();
        while !statuses
            .iter()
            .any(|s| s.active == NoiseSuppression::HighQuality)
        {
            assert!(started.elapsed() < Duration::from_secs(10), "{statuses:?}");
            pump(&mut captured, &mut channels.events, &mut statuses, 50);
        }
        println!("High quality took over after {:?}", started.elapsed());
        let took_over = engine.stats();
        assert!(took_over.frames_encoded > frames_before, "capture stalled");
        pump(&mut captured, &mut channels.events, &mut statuses, 2_000);
        let busy = engine.stats();
        assert!(busy.denoise_blocks > took_over.denoise_blocks);
        assert!(busy.frames_encoded >= took_over.frames_encoded + 90);
        channels
            .controls
            .blocking_send(EngineControl::NoiseSuppression(NoiseSuppression::Light))
            .unwrap();
        pump(&mut captured, &mut channels.events, &mut statuses, 1_000);
        let last = statuses.last().expect("status events");
        assert_eq!(last.active, NoiseSuppression::Light, "{statuses:?}");
        assert_eq!(last.error, None);
        let stats = engine.stats();
        println!("{stats:#?}\n{statuses:#?}");
        assert_eq!(stats.input_overrun_samples, 0);
        assert_eq!(stats.frames_dropped, 0);
        drop(captured);
        drop(engine);
        assert_eq!(
            crate::alloc_counter::callback_heap_ops(),
            heap_before,
            "a device callback touched the heap"
        );
    }

    /// Real output device: an Opus-encoded 440 Hz tone played through the
    /// engine is recorded back with WASAPI loopback capture of the same
    /// device and must be the dominant component of what the device played.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs a real output device; plays a quiet tone"]
    fn live_output_tone_is_recorded_by_wasapi_loopback() {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host.default_output_device().expect("default output");
        let config = device.default_output_config().unwrap();
        assert_eq!(config.sample_format(), cpal::SampleFormat::F32);
        let rate = config.sample_rate();
        let channels = usize::from(config.channels());
        let recorded = Arc::new(Mutex::new(Vec::<f32>::new()));
        let sink = Arc::clone(&recorded);
        let recorder = device
            .build_input_stream::<f32, _, _>(
                config.config(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    sink.lock().extend(data.iter().step_by(channels));
                },
                |error| println!("loopback recorder: {error}"),
                None,
            )
            .expect("WASAPI loopback capture");
        recorder.play().unwrap();

        let config = EngineConfig {
            input: None,
            ..EngineConfig::default()
        };
        let (engine, channels_io) = AudioEngine::start(config).unwrap();
        let playback = channels_io.playback.unwrap();
        let mut encoder = crate::VoiceEncoder::new(DEFAULT_BITRATE).unwrap();
        let tone = crate::resample::tests::sine(48_000, 440.0, 0.1, 48_000 * 2);
        for chunk in tone.as_chunks::<FRAME_SAMPLES>().0 {
            let mut packet = EncodedPacket::empty();
            encoder.encode_mono(chunk, &mut packet).unwrap();
            playback.blocking_send(packet).unwrap();
        }
        // Let the 16-packet queue (320 ms) and the playback ring drain.
        std::thread::sleep(Duration::from_millis(600));
        let stats = engine.stats();
        drop(engine);
        drop(recorder);
        let recorded = recorded.lock();
        println!(
            "{stats:#?}\nrecorded {} samples at {rate} Hz",
            recorded.len()
        );
        let loudest = recorded
            .windows(rate as usize / 2)
            .step_by(rate as usize / 20)
            .max_by(|a, b| tone_level(a, rate, 440.0).total_cmp(&tone_level(b, rate, 440.0)))
            .expect("recorded at least half a second");
        let wanted = tone_level(loudest, rate, 440.0);
        let off = [300.0, 620.0, 1_000.0, 2_000.0]
            .map(|f| tone_level(loudest, rate, f))
            .into_iter()
            .fold(0.0f32, f32::max);
        println!("440 Hz level {wanted:.4}, strongest off-tone {off:.4}");
        // The device paces playback in real time; packets still queued at teardown are not decoded.
        assert!(
            stats.packets_decoded >= 50,
            "{} packets decoded",
            stats.packets_decoded
        );
        assert_eq!(stats.decode_errors, 0);
        assert!(wanted > 0.01, "tone not heard on the device");
        assert!(wanted > 10.0 * off, "tone not dominant");
    }
}
