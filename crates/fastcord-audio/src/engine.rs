//! The audio engine: one owner thread per active call.
//!
//! The owner thread opens the cpal streams, runs the capture/playback
//! pipelines, and closes the streams again; device callbacks only touch the
//! preallocated rings. The engine exists only while audio is needed, so idle
//! has no audio thread or callback stream (SPEC §2.3 invariant 8).
//! Dropping [`AudioEngine`] stops the streams (which joins their callback
//! threads) and then joins the owner thread.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::callback::{InputCallback, OutputCallback, StreamCounters};
use crate::codec::{DEFAULT_BITRATE, EncodedPacket};
use crate::device::{Backend, CpalBackend, DeviceChoice, DeviceErrorKind, Direction, StreamFormat};
use crate::error::AudioError;
use crate::pipeline::{
    CapturePipeline, CapturedFrame, PipelineCounters, PlaybackPipeline, RING_LIMIT_MS, frames_for,
};
use crate::ring::sample_ring;

/// How often the owner thread services the rings. Rings hold 100 ms and
/// playback targets 40 ms, so a coarse OS timer (15.6 ms on Windows) still
/// keeps both comfortably supplied.
const TICK: Duration = Duration::from_millis(5);
/// Encoded frames buffered toward the media session (320 ms, ≤ 24 KiB).
const CAPTURE_QUEUE: usize = 16;
/// Encoded packets buffered toward playback (320 ms, ≤ 24 KiB).
const PLAYBACK_QUEUE: usize = 16;
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
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            input: Some(DeviceChoice::SystemDefault),
            output: Some(DeviceChoice::SystemDefault),
            bitrate: DEFAULT_BITRATE,
        }
    }
}

/// The format a device stream runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceFormat {
    pub sample_rate: u32,
    pub channels: u16,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineEvent {
    /// The stream stopped and was closed. The engine does not switch to
    /// another device by itself.
    StreamFailed {
        direction: Direction,
        kind: DeviceErrorKind,
    },
}

/// Channels connecting the engine to the media session.
#[derive(Debug)]
pub struct EngineChannels {
    /// Encoded 20 ms microphone frames (present when an input was opened).
    pub captured: Option<mpsc::Receiver<CapturedFrame>>,
    /// Opus packets to play (present when an output was opened).
    pub playback: Option<mpsc::Sender<EncodedPacket>>,
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
    pub packets_decoded: u64,
    pub decode_errors: u64,
}

#[derive(Default)]
struct Shared {
    input: Arc<StreamCounters>,
    output: Arc<StreamCounters>,
    pipeline: PipelineCounters,
    #[cfg(test)]
    owner_service: OwnerService,
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
    input: Option<DeviceFormat>,
    output: Option<DeviceFormat>,
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
        if config.input.is_none() && config.output.is_none() {
            return Err(AudioError::NothingToRun);
        }
        let (captured_tx, captured_rx) = mpsc::channel(CAPTURE_QUEUE);
        let (playback_tx, playback_rx) = mpsc::channel(PLAYBACK_QUEUE);
        let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
        let (stop_tx, stop_rx) = std_mpsc::channel();
        let (ready_tx, ready_rx) = std_mpsc::sync_channel::<Ready>(1);
        let shared = Arc::new(Shared::default());
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
                    events: events_tx,
                };
                run_owner(make_backend(), config, &thread_shared, io, ready_tx);
            })
            .map_err(|_| AudioError::EngineThread)?;
        let ready = ready_rx.recv();
        let (input, output) = match ready {
            Ok(Ok(formats)) => formats,
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
            input,
            output,
        };
        let channels = EngineChannels {
            captured: has_input.then_some(captured_rx),
            playback: has_output.then_some(playback_tx),
            events: events_rx,
        };
        Ok((engine, channels))
    }

    /// Format of the open input stream.
    pub fn input_format(&self) -> Option<DeviceFormat> {
        self.input
    }

    /// Format of the open output stream.
    pub fn output_format(&self) -> Option<DeviceFormat> {
        self.output
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
            packets_decoded: load(&pipeline.packets_decoded),
            decode_errors: load(&pipeline.decode_errors),
        }
    }

    /// Weak handles to the per-stream counters, which the callbacks keep
    /// alive: once both are dead, no callback closure exists anymore.
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
            .field("input", &self.input)
            .field("output", &self.output)
            .finish_non_exhaustive()
    }
}

struct OwnerIo {
    stop: std_mpsc::Receiver<()>,
    captured: Option<mpsc::Sender<CapturedFrame>>,
    playback: Option<mpsc::Receiver<EncodedPacket>>,
    events: mpsc::Sender<EngineEvent>,
}

fn open_capture<B: Backend>(
    backend: &mut B,
    choice: &DeviceChoice,
    counters: &Arc<StreamCounters>,
    bitrate: u32,
) -> Result<(B::Stream, CapturePipeline, StreamFormat), AudioError> {
    let mut consumer = None;
    let (stream, format) = backend.open_input(choice, counters, &mut |format| {
        let (producer, ring) = sample_ring(format.channels, frames_for(format.rate, RING_LIMIT_MS));
        consumer = Some(ring);
        InputCallback::new(producer, Arc::clone(counters))
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
    config: EngineConfig,
    shared: &Shared,
    mut io: OwnerIo,
    ready: std_mpsc::SyncSender<Ready>,
) {
    let mut capture = None;
    let mut playback = None;
    let opened = (|| {
        let mut formats = (None, None);
        if let Some(choice) = &config.input {
            let (stream, pipeline, format) =
                open_capture(&mut backend, choice, &shared.input, config.bitrate)?;
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
    let failed = opened.is_err();
    if ready.send(opened).is_err() || failed {
        // Streams opened before the failure are dropped (stopped) here.
        return;
    }

    // Dropping the engine handle disconnects the stop channel, ending the loop.
    while let Err(std_mpsc::RecvTimeoutError::Timeout) = io.stop.recv_timeout(TICK) {
        if capture.is_some()
            && let Some(kind) = shared.input.fault()
        {
            capture = None;
            drop(io.captured.take());
            let _ = io.events.try_send(EngineEvent::StreamFailed {
                direction: Direction::Input,
                kind,
            });
        }
        if playback.is_some()
            && let Some(kind) = shared.output.fault()
        {
            // Dropping the receiver closes senders and discards queued packets.
            drop(io.playback.take());
            playback = None;
            let _ = io.events.try_send(EngineEvent::StreamFailed {
                direction: Direction::Output,
                kind,
            });
        }
        if let (Some((_, pipeline)), Some(captured)) = (capture.as_mut(), io.captured.as_ref()) {
            pipeline.run(&shared.pipeline, &mut |frame| {
                if captured.try_send(frame).is_err() {
                    shared
                        .pipeline
                        .frames_dropped
                        .fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        if let (Some((_, pipeline)), Some(playback_receiver)) =
            (playback.as_mut(), io.playback.as_mut())
        {
            pipeline.run(&shared.pipeline, &mut || playback_receiver.try_recv().ok());
        }
        #[cfg(test)]
        shared.owner_service.notify();
    }
    // Stop the device streams (joining their callback threads) before the
    // owner thread exits.
    drop(capture);
    drop(playback);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::FRAME_SAMPLES;
    use crate::device::DeviceKey;
    use crate::resample::tests::tone_level;
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

        fn open_input(
            &mut self,
            choice: &DeviceChoice,
            counters: &Arc<StreamCounters>,
            make: &mut dyn FnMut(StreamFormat) -> InputCallback,
        ) -> Result<(FakeStream, StreamFormat), AudioError> {
            check_choice(choice, Direction::Input)?;
            let format = StreamFormat {
                rate: self.input_rate,
                channels: 2,
                period_frames: Some(self.input_rate / 100),
            };
            let mut callback = make(format);
            let rate = self.input_rate;
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
            check_choice(choice, Direction::Output)?;
            let format = StreamFormat {
                rate: self.output_rate,
                channels: 2,
                period_frames: Some(self.output_rate / 100),
            };
            let mut callback = make(format);
            let played = Arc::clone(&self.played);
            let mut calls = 0;
            let fail = self.fail_output;
            let counters = Arc::clone(counters);
            let mut buffer = vec![0.0f32; (self.output_rate / 100) as usize * 2];
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

    /// Microphone → Opus → playback through the real engine thread, callbacks,
    /// rings, resamplers, and codec, with barrier-stepped fake device threads.
    #[test]
    fn loopback_round_trip_plays_the_captured_tone_and_tears_down() {
        let mut fake = host(44_100, 48_000);
        fake.manual_callbacks = true;
        let heap_before = crate::alloc_counter::callback_heap_ops();
        let backend = fake.clone();
        let (engine, channels) =
            AudioEngine::start_with(EngineConfig::default(), move || backend).unwrap();
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
    fn a_missing_selected_device_fails_start_without_leaving_threads() {
        let fake = host(48_000, 48_000);
        let backend = fake.clone();
        let config = EngineConfig {
            output: Some(DeviceChoice::Device(DeviceKey::from_persisted("missing"))),
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
        let deadline = Instant::now() + Duration::from_secs(5);
        let event = loop {
            if let Ok(event) = channels.events.try_recv() {
                break event;
            }
            assert!(Instant::now() < deadline, "no failure event");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            event,
            EngineEvent::StreamFailed {
                direction: Direction::Input,
                kind: DeviceErrorKind::Disconnected
            }
        );
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
        let deadline = Instant::now() + Duration::from_secs(5);
        let event = loop {
            if let Ok(event) = channels.events.try_recv() {
                break event;
            }
            assert!(Instant::now() < deadline, "no failure event");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            event,
            EngineEvent::StreamFailed {
                direction: Direction::Output,
                kind: DeviceErrorKind::Disconnected
            }
        );
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
        assert_eq!(stats.packets_decoded, 100);
        assert!(wanted > 0.01, "tone not heard on the device");
        assert!(wanted > 10.0 * off, "tone not dominant");
    }
}
