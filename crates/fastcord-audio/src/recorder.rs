//! Explicit microphone recording for a voice message (issue #75).
//!
//! A recorder exists only between "start" and "finish"/cancel: it opens an input-only
//! [`AudioEngine`] (the selected device, the selected noise suppression, no echo canceller),
//! keeps the engine's 20 ms Opus frames in memory up to [`MAX_RECORDING_MS`] plus a per-frame
//! loudness for the waveform, and drops the engine, and with it the microphone, on `finish`,
//! on cancel (drop) and on a stream failure. Nothing runs while no recorder exists. The device
//! callbacks are the engine's (lock- and allocation-free); this thread only drains the bounded
//! capture channel and decodes each packet to measure its level.
//!
//! The result is a standard mono Ogg Opus file ([`crate::ogg::write_ogg_opus`]) plus the
//! length and the 256-sample loudness envelope Discord's voice-message attachments carry.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use crate::codec::{FRAME_SAMPLES, MAX_FRAME_SAMPLES, VoiceDecoder};
use crate::denoise::NoiseSuppression;
use crate::device::{DeviceChoice, DeviceErrorKind};
use crate::engine::{AudioEngine, EngineChannels, EngineConfig, EngineEvent};
use crate::error::AudioError;
use crate::ogg::write_ogg_opus;

/// Longest recording; reaching it ends the recording (the audio so far is kept).
pub const MAX_RECORDING_MS: u64 = 5 * 60 * 1000;
/// Shortest recording that may be kept.
pub const MIN_RECORDING_MS: u64 = 500;
/// Samples in the loudness envelope of a voice message.
pub const WAVEFORM_SAMPLES: usize = 256;
/// Opus bitrate of a recording: speech quality at about 4 KB per second.
const BITRATE: u32 = 32_000;
const FRAME_MS: u64 = 20;
const MAX_FRAMES: usize = (MAX_RECORDING_MS / FRAME_MS) as usize;
const POLL: Duration = Duration::from_millis(10);

const RUNNING: u32 = 0;
const LIMIT: u32 = 1;
const DEVICE_LOST: u32 = 2;
const INTERNAL: u32 = 3;

/// Why a recording cannot be started or kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingFailure {
    /// The OS denied microphone access.
    PermissionDenied,
    /// The microphone is missing, in use by another application, or stopped.
    Device,
    /// Shorter than [`MIN_RECORDING_MS`].
    TooShort,
    /// The recorder thread or encoder failed.
    Internal,
}

impl std::fmt::Display for RecordingFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PermissionDenied => {
                "Microphone access was denied. Allow it in the system privacy settings."
            }
            Self::Device => "The microphone is unavailable or stopped. Check the input device.",
            Self::TooShort => "The recording is too short.",
            Self::Internal => "The recording failed.",
        })
    }
}

impl std::error::Error for RecordingFailure {}

fn failure_of(error: &AudioError) -> RecordingFailure {
    match error {
        AudioError::Device(_, DeviceErrorKind::PermissionDenied) => {
            RecordingFailure::PermissionDenied
        }
        AudioError::EngineThread | AudioError::Codec(_) | AudioError::Rate(_) => {
            RecordingFailure::Internal
        }
        _ => RecordingFailure::Device,
    }
}

/// A finished recording, ready to preview and upload.
#[derive(Clone, PartialEq, Eq)]
pub struct Recording {
    /// The complete Ogg Opus file.
    pub ogg: Vec<u8>,
    pub duration_ms: u64,
    /// [`WAVEFORM_SAMPLES`] loudness values, 0-255.
    pub waveform: Vec<u8>,
}

impl std::fmt::Debug for Recording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recording")
            .field("bytes", &self.ogg.len())
            .field("duration_ms", &self.duration_ms)
            .finish()
    }
}

/// What a running recorder shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecorderSnapshot {
    pub elapsed_ms: u64,
    /// Loudness of the latest frame, 0-255.
    pub level: u8,
    /// The recorder stopped by itself: the length limit was reached (`Ok`) or the microphone
    /// failed.
    pub stopped: Option<Result<(), RecordingFailure>>,
}

#[derive(Default)]
struct Shared {
    frames: AtomicU64,
    level: AtomicU32,
    ended: AtomicU32,
    cancel: AtomicBool,
}

type Frames = (Vec<Vec<u8>>, Vec<u8>);

/// A recording in progress. Dropping it cancels and releases the microphone.
pub struct VoiceRecorder {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<Frames>>,
}

impl std::fmt::Debug for VoiceRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VoiceRecorder")
    }
}

/// Level (0-255) of one decoded frame: its RMS on a fourth-root scale, so quiet speech is
/// visible and loud speech does not saturate.
fn level_of(samples: &[f32]) -> u8 {
    if samples.is_empty() {
        return 0;
    }
    let mean = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    (mean.sqrt().sqrt() * 255.0).clamp(0.0, 255.0) as u8
}

/// The loudest frame in each of [`WAVEFORM_SAMPLES`] equal spans.
fn envelope(levels: &[u8]) -> Vec<u8> {
    (0..WAVEFORM_SAMPLES)
        .map(|bucket| {
            let from = bucket * levels.len() / WAVEFORM_SAMPLES;
            let to = ((bucket + 1) * levels.len() / WAVEFORM_SAMPLES).max(from + 1);
            levels
                .get(from..to.min(levels.len()))
                .and_then(|span| span.iter().copied().max())
                .unwrap_or(0)
        })
        .collect()
}

impl VoiceRecorder {
    /// Opens the microphone and starts recording. Blocks while the device opens: call it off
    /// the UI thread.
    pub fn start(
        input: DeviceChoice,
        noise_suppression: NoiseSuppression,
    ) -> Result<Self, RecordingFailure> {
        Self::spawn(AudioEngine::start(EngineConfig {
            input: Some(input),
            output: None,
            bitrate: BITRATE,
            noise_suppression,
            echo_cancellation: false,
            capture_enabled: true,
        }))
    }

    pub(crate) fn spawn(
        started: Result<(AudioEngine, EngineChannels), AudioError>,
    ) -> Result<Self, RecordingFailure> {
        let (engine, mut channels) = started.map_err(|error| failure_of(&error))?;
        let mut captured = channels.captured.take().ok_or(RecordingFailure::Device)?;
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("fastcord-recorder".to_owned())
            .spawn(move || {
                // Owning the engine here releases the microphone when the thread ends.
                let _engine = engine;
                let mut events = channels.events;
                let mut decoder = VoiceDecoder::new().ok();
                let mut pcm = vec![0.0f32; MAX_FRAME_SAMPLES * 2];
                let mut packets: Vec<Vec<u8>> = Vec::new();
                let mut levels: Vec<u8> = Vec::new();
                while !thread_shared.cancel.load(Ordering::Acquire) {
                    if packets.len() >= MAX_FRAMES {
                        thread_shared.ended.store(LIMIT, Ordering::Release);
                        break;
                    }
                    if let Ok(EngineEvent::StreamFailed { .. }) = events.try_recv() {
                        thread_shared.ended.store(DEVICE_LOST, Ordering::Release);
                        break;
                    }
                    match captured.try_recv() {
                        Ok(frame) => {
                            let bytes = frame.packet.as_bytes();
                            let level = decoder
                                .as_mut()
                                .and_then(|decoder| decoder.decode(bytes, &mut pcm).ok())
                                .map_or(0, |frames| {
                                    level_of(&pcm[..frames.min(FRAME_SAMPLES) * 2])
                                });
                            packets.push(bytes.to_vec());
                            levels.push(level);
                            thread_shared
                                .level
                                .store(u32::from(level), Ordering::Release);
                            thread_shared
                                .frames
                                .store(packets.len() as u64, Ordering::Release);
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                            std::thread::sleep(POLL);
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            thread_shared.ended.store(INTERNAL, Ordering::Release);
                            break;
                        }
                    }
                }
                (packets, levels)
            })
            .map_err(|_| RecordingFailure::Internal)?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    pub fn snapshot(&self) -> RecorderSnapshot {
        RecorderSnapshot {
            elapsed_ms: self.shared.frames.load(Ordering::Acquire) * FRAME_MS,
            level: self.shared.level.load(Ordering::Acquire) as u8,
            stopped: match self.shared.ended.load(Ordering::Acquire) {
                RUNNING => None,
                LIMIT => Some(Ok(())),
                DEVICE_LOST => Some(Err(RecordingFailure::Device)),
                _ => Some(Err(RecordingFailure::Internal)),
            },
        }
    }

    /// Stops, releases the microphone and returns what was recorded. A recorder that failed
    /// on its own returns that failure and discards the audio.
    pub fn finish(mut self) -> Result<Recording, RecordingFailure> {
        let (packets, levels) = self.join();
        match self.shared.ended.load(Ordering::Acquire) {
            DEVICE_LOST => return Err(RecordingFailure::Device),
            INTERNAL => return Err(RecordingFailure::Internal),
            _ => {}
        }
        let duration_ms = packets.len() as u64 * FRAME_MS;
        if duration_ms < MIN_RECORDING_MS {
            return Err(RecordingFailure::TooShort);
        }
        let ogg = write_ogg_opus(&packets).ok_or(RecordingFailure::Internal)?;
        Ok(Recording {
            ogg,
            duration_ms,
            waveform: envelope(&levels),
        })
    }

    fn join(&mut self) -> Frames {
        self.shared.cancel.store(true, Ordering::Release);
        self.thread
            .take()
            .and_then(|thread| thread.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for VoiceRecorder {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_loudness_and_the_envelope_has_a_fixed_size() {
        assert_eq!(level_of(&[0.0; 960]), 0);
        assert!(level_of(&[0.01; 960]) < level_of(&[0.1; 960]));
        assert_eq!(level_of(&[1.0; 960]), 255);
        let levels: Vec<u8> = (0..1000u32).map(|i| (i % 256) as u8).collect();
        assert_eq!(envelope(&levels).len(), WAVEFORM_SAMPLES);
        assert_eq!(envelope(&[]).len(), WAVEFORM_SAMPLES);
        assert_eq!(envelope(&[7]).len(), WAVEFORM_SAMPLES);
        assert!(envelope(&[9, 3]).iter().all(|&v| v == 9 || v == 3));
    }

    #[test]
    fn failures_have_distinct_texts_and_permission_is_recognised() {
        use std::collections::HashSet;
        let all = [
            RecordingFailure::PermissionDenied,
            RecordingFailure::Device,
            RecordingFailure::TooShort,
            RecordingFailure::Internal,
        ];
        let texts: HashSet<_> = all.iter().map(ToString::to_string).collect();
        assert_eq!(texts.len(), all.len());
        assert_eq!(
            failure_of(&AudioError::Device(
                crate::device::Direction::Input,
                DeviceErrorKind::PermissionDenied
            )),
            RecordingFailure::PermissionDenied
        );
        assert_eq!(
            failure_of(&AudioError::NoDefaultDevice(
                crate::device::Direction::Input
            )),
            RecordingFailure::Device
        );
    }
}
