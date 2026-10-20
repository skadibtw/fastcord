//! Microphone checks that never reach a call (issue #83).
//!
//! Two modes share one input [`AudioEngine`] opened with the selected device and processing
//! (noise suppression and gain; no echo cancellation, since nothing is played while it
//! listens):
//! - [`MicTestMode::Level`] only measures: a live level meter for as long as the handle lives
//!   (it also ends after [`LEVEL_LIMIT_MS`] so a forgotten check cannot keep the microphone);
//! - [`MicTestMode::RecordAndPlay`] also keeps the engine's Opus frames in memory (at most
//!   [`RECORD_MS`], about 20 KB) and plays them to the selected output afterwards.
//!
//! The frames never leave this module: there is no connection to the voice session, so a test
//! cannot transmit to a call, and recording first and playing second makes acoustic feedback
//! impossible. The engine, and with it the devices, exists only until the handle is dropped.
//! Failures are diagnosed to something the user can act on: which direction failed and why
//! (permission, another application, unplugged), or that the microphone is open but silent.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::codec::EncodedPacket;
use crate::denoise::NoiseSuppression;
use crate::device::{DeviceChoice, DeviceErrorKind, Direction};
use crate::engine::{AudioEngine, EngineChannels, EngineConfig, EngineControl, EngineEvent};
use crate::error::AudioError;
use crate::level::{FLOOR_TENTHS_DB, InputGain};

/// Length of the recording.
pub const RECORD_MS: u64 = 5_000;
/// A level check ends by itself after this long.
pub const LEVEL_LIMIT_MS: u64 = 10 * 60 * 1000;
const FRAME_MS: u64 = 20;
/// Peaks quieter than this mean the microphone delivers no signal.
const SILENT_TENTHS_DB: i16 = -700;
/// How long without a signal before the check calls the microphone silent.
const SILENT_AFTER: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(10);
const METER_EVERY: Duration = Duration::from_millis(50);
/// Time the output keeps playing after the last frame was handed over.
const DRAIN: Duration = Duration::from_millis(400);
/// Meter values are stored offset so that zero means silence.
const OFFSET: i32 = -(FLOOR_TENTHS_DB as i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicTestMode {
    /// A live level meter, nothing recorded and nothing played.
    Level,
    /// Record [`RECORD_MS`], then play them back.
    RecordAndPlay,
}

/// Why a microphone test could not run or had nothing to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicTestFailure {
    /// The microphone could not be opened or stopped.
    Input(DeviceErrorKind),
    /// The speakers or headset could not be opened or stopped.
    Output(DeviceErrorKind),
    /// The microphone is open but delivered no signal.
    Silent,
    /// The test thread or the encoder failed.
    Internal,
}

impl MicTestFailure {
    fn of(error: &AudioError) -> Self {
        match error {
            AudioError::Device(Direction::Input, kind) => Self::Input(*kind),
            AudioError::Device(Direction::Output, kind) => Self::Output(*kind),
            AudioError::DeviceNotFound(direction) | AudioError::NoDefaultDevice(direction) => {
                let kind = DeviceErrorKind::Disconnected;
                match direction {
                    Direction::Input => Self::Input(kind),
                    Direction::Output => Self::Output(kind),
                }
            }
            _ => Self::Internal,
        }
    }

    fn stream(direction: Direction, kind: DeviceErrorKind) -> Self {
        match direction {
            Direction::Input => Self::Input(kind),
            Direction::Output => Self::Output(kind),
        }
    }
}

impl std::fmt::Display for MicTestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input(kind) => write!(f, "Microphone: {}", kind.advice()),
            Self::Output(kind) => write!(f, "Playback: {}", kind.advice()),
            Self::Silent => f.write_str(
                "The microphone is open but no sound arrived. Check that it is not muted in the system sound settings or by a switch on the device, that it is plugged in, and that the right input is selected.",
            ),
            Self::Internal => f.write_str("The microphone test failed."),
        }
    }
}

impl std::error::Error for MicTestFailure {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicTestPhase {
    /// Listening (and, in [`MicTestMode::RecordAndPlay`], recording).
    Recording,
    Playing,
    Finished,
    Failed(MicTestFailure),
}

/// What a running test shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicTestSnapshot {
    pub phase: MicTestPhase,
    /// Recorded (or, while playing, played) milliseconds.
    pub elapsed_ms: u64,
    /// Loudest 20 ms RMS and highest peak since the previous snapshot, tenths of dBFS.
    pub rms_tenths_db: i16,
    pub peak_tenths_db: i16,
    /// Frames that reached full scale or the limiter ceiling.
    pub clipped_frames: u64,
    /// Nothing louder than -70 dBFS for the last three seconds.
    pub silent: bool,
}

const RECORDING: u32 = 0;
const PLAYING: u32 = 1;
const FINISHED: u32 = 2;
const FAILED: u32 = 3;

#[derive(Default)]
struct Shared {
    phase: AtomicU32,
    failure: AtomicU32,
    elapsed_ms: AtomicU64,
    rms: AtomicU32,
    peak: AtomicU32,
    clipped: AtomicU64,
    silent: AtomicBool,
    gain_bits: AtomicU32,
    cancel: AtomicBool,
}

fn failure_code(failure: MicTestFailure) -> u32 {
    match failure {
        MicTestFailure::Input(kind) => 0x100 | kind as u32,
        MicTestFailure::Output(kind) => 0x200 | kind as u32,
        MicTestFailure::Silent => 3,
        MicTestFailure::Internal => 4,
    }
}

fn failure_of_code(code: u32) -> MicTestFailure {
    let kind = DeviceErrorKind::from_code((code & 0xff) as u8);
    match code & 0xf00 {
        0x100 => MicTestFailure::Input(kind),
        0x200 => MicTestFailure::Output(kind),
        _ if code == 3 => MicTestFailure::Silent,
        _ => MicTestFailure::Internal,
    }
}

/// A microphone test in progress. Dropping it stops the test and releases the devices.
pub struct MicTest {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for MicTest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MicTest")
    }
}

impl MicTest {
    /// Opens the devices and starts. Blocks while they open: call it off the UI thread. A
    /// device that cannot be opened is reported here, already diagnosed. `output` is used by
    /// [`MicTestMode::RecordAndPlay`] only.
    pub fn start(
        mode: MicTestMode,
        input: DeviceChoice,
        output: DeviceChoice,
        noise_suppression: NoiseSuppression,
        gain: InputGain,
    ) -> Result<Self, MicTestFailure> {
        let started = AudioEngine::start(EngineConfig {
            input: Some(input),
            output: (mode == MicTestMode::RecordAndPlay).then_some(output),
            noise_suppression,
            echo_cancellation: false,
            capture_enabled: true,
            ..EngineConfig::default()
        });
        Self::spawn(started, mode, gain, RECORD_MS)
    }

    pub(crate) fn spawn(
        started: Result<(AudioEngine, EngineChannels), AudioError>,
        mode: MicTestMode,
        gain: InputGain,
        record_ms: u64,
    ) -> Result<Self, MicTestFailure> {
        let (engine, mut channels) = started.map_err(|error| MicTestFailure::of(&error))?;
        let mut captured = channels.captured.take().ok_or(MicTestFailure::Internal)?;
        let playback = channels.playback.take();
        if mode == MicTestMode::RecordAndPlay && playback.is_none() {
            return Err(MicTestFailure::Internal);
        }
        let _ = channels.controls.try_send(EngineControl::InputGain(gain));
        let _ = channels.controls.try_send(EngineControl::Diagnostics(true));
        let shared = Arc::new(Shared::default());
        shared.gain_bits.store(gain.to_bits(), Ordering::Relaxed);
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("fastcord-mic-test".to_owned())
            .spawn(move || {
                // Owning the engine here releases the devices when the thread ends.
                let engine = engine;
                let controls = channels.controls;
                let mut events = channels.events;
                let fail = |failure| {
                    thread_shared
                        .failure
                        .store(failure_code(failure), Ordering::Release);
                    thread_shared.phase.store(FAILED, Ordering::Release);
                };
                let max_frames = match mode {
                    MicTestMode::RecordAndPlay => (record_ms / FRAME_MS) as usize,
                    MicTestMode::Level => (LEVEL_LIMIT_MS / FRAME_MS) as usize,
                };
                let mut packets: Vec<EncodedPacket> = Vec::new();
                let mut frames = 0usize;
                let mut loudest = FLOOR_TENTHS_DB;
                let mut last_signal = Instant::now();
                let mut next_meter = Instant::now();
                let mut sent_gain = gain.to_bits();
                while frames < max_frames {
                    if thread_shared.cancel.load(Ordering::Acquire) {
                        return;
                    }
                    if let Ok(EngineEvent::StreamFailed { direction, kind }) = events.try_recv() {
                        fail(MicTestFailure::stream(direction, kind));
                        return;
                    }
                    if Instant::now() >= next_meter {
                        next_meter = Instant::now() + METER_EVERY;
                        let wanted = thread_shared.gain_bits.load(Ordering::Relaxed);
                        if wanted != sent_gain
                            && controls
                                .try_send(EngineControl::InputGain(InputGain::from_bits(wanted)))
                                .is_ok()
                        {
                            sent_gain = wanted;
                        }
                        let level = engine.input_diagnostics();
                        loudest = loudest.max(level.peak_tenths_db);
                        if level.peak_tenths_db >= SILENT_TENTHS_DB {
                            last_signal = Instant::now();
                        }
                        thread_shared
                            .silent
                            .store(last_signal.elapsed() >= SILENT_AFTER, Ordering::Relaxed);
                        thread_shared.rms.fetch_max(
                            (i32::from(level.rms_tenths_db) + OFFSET) as u32,
                            Ordering::Relaxed,
                        );
                        thread_shared.peak.fetch_max(
                            (i32::from(level.peak_tenths_db) + OFFSET) as u32,
                            Ordering::Relaxed,
                        );
                        thread_shared
                            .clipped
                            .store(level.clipped_frames, Ordering::Relaxed);
                    }
                    match captured.try_recv() {
                        Ok(frame) => {
                            frames += 1;
                            if mode == MicTestMode::RecordAndPlay {
                                packets.push(frame.packet);
                            }
                            thread_shared
                                .elapsed_ms
                                .store(frames as u64 * FRAME_MS, Ordering::Release);
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                            std::thread::sleep(POLL);
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            fail(MicTestFailure::Internal);
                            return;
                        }
                    }
                }
                let Some(playback) = playback.filter(|_| mode == MicTestMode::RecordAndPlay) else {
                    thread_shared.phase.store(FINISHED, Ordering::Release);
                    return;
                };
                if loudest < SILENT_TENTHS_DB {
                    fail(MicTestFailure::Silent);
                    return;
                }
                thread_shared.elapsed_ms.store(0, Ordering::Release);
                thread_shared.phase.store(PLAYING, Ordering::Release);
                for (index, mut packet) in packets.into_iter().enumerate() {
                    loop {
                        if thread_shared.cancel.load(Ordering::Acquire) {
                            return;
                        }
                        if let Ok(EngineEvent::StreamFailed { direction, kind }) = events.try_recv()
                        {
                            fail(MicTestFailure::stream(direction, kind));
                            return;
                        }
                        match playback.try_send(packet) {
                            Ok(()) => break,
                            Err(tokio::sync::mpsc::error::TrySendError::Full(back)) => {
                                packet = back;
                                std::thread::sleep(POLL);
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                fail(MicTestFailure::Output(DeviceErrorKind::Disconnected));
                                return;
                            }
                        }
                    }
                    thread_shared
                        .elapsed_ms
                        .store((index as u64 + 1) * FRAME_MS, Ordering::Release);
                }
                // Let the playback ring run dry before the output closes.
                let end = Instant::now() + DRAIN;
                while Instant::now() < end && !thread_shared.cancel.load(Ordering::Acquire) {
                    std::thread::sleep(POLL);
                }
                thread_shared.phase.store(FINISHED, Ordering::Release);
            })
            .map_err(|_| MicTestFailure::Internal)?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Changes the gain of the running test (the engine applies it within a few milliseconds).
    pub fn set_gain(&self, gain: InputGain) {
        self.shared
            .gain_bits
            .store(gain.to_bits(), Ordering::Relaxed);
    }

    /// The current state. The level maxima cover the time since the previous call, so poll
    /// from one reader.
    pub fn snapshot(&self) -> MicTestSnapshot {
        let tenths = |value: u32| (value as i32 - OFFSET) as i16;
        MicTestSnapshot {
            phase: match self.shared.phase.load(Ordering::Acquire) {
                RECORDING => MicTestPhase::Recording,
                PLAYING => MicTestPhase::Playing,
                FINISHED => MicTestPhase::Finished,
                _ => MicTestPhase::Failed(failure_of_code(
                    self.shared.failure.load(Ordering::Acquire),
                )),
            },
            elapsed_ms: self.shared.elapsed_ms.load(Ordering::Acquire),
            rms_tenths_db: tenths(self.shared.rms.swap(0, Ordering::Relaxed)),
            peak_tenths_db: tenths(self.shared.peak.swap(0, Ordering::Relaxed)),
            clipped_frames: self.shared.clipped.load(Ordering::Relaxed),
            silent: self.shared.silent.load(Ordering::Relaxed),
        }
    }
}

impl Drop for MicTest {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_codes_round_trip_and_texts_are_distinct() {
        use std::collections::HashSet;
        let mut all = vec![MicTestFailure::Silent, MicTestFailure::Internal];
        for code in 0..7 {
            let kind = DeviceErrorKind::from_code(code);
            all.push(MicTestFailure::Input(kind));
            all.push(MicTestFailure::Output(kind));
        }
        for failure in &all {
            assert_eq!(failure_of_code(failure_code(*failure)), *failure);
        }
        let texts: HashSet<_> = all.iter().map(ToString::to_string).collect();
        assert_eq!(texts.len(), all.len(), "every diagnosis reads differently");
    }

    #[test]
    fn device_errors_are_diagnosed_by_direction() {
        assert_eq!(
            MicTestFailure::of(&AudioError::Device(
                Direction::Input,
                DeviceErrorKind::PermissionDenied
            )),
            MicTestFailure::Input(DeviceErrorKind::PermissionDenied)
        );
        assert_eq!(
            MicTestFailure::of(&AudioError::NoDefaultDevice(Direction::Output)),
            MicTestFailure::Output(DeviceErrorKind::Disconnected)
        );
        assert_eq!(
            MicTestFailure::of(&AudioError::EngineThread),
            MicTestFailure::Internal
        );
        let text = MicTestFailure::Input(DeviceErrorKind::PermissionDenied).to_string();
        assert!(text.contains("privacy"), "{text}");
    }
}
