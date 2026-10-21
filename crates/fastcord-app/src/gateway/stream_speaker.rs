//! Stream audio on the viewer's side (SPEC §8.7, milestone 35): one
//! output-only audio engine per watched stream, fed with the stream's
//! DAVE-opened Opus packets.
//!
//! The engine is the same speaker path the call uses (per-SSRC adaptive jitter
//! buffer, loss concealment, stereo decoding, mixing into the output device
//! ring, nothing on the audio callback but ring reads). It has no microphone,
//! and it follows the call's output settings: the chosen output device and
//! the deafen state (own or server). The audio clock is therefore the only
//! clock; video is presented as it is decoded and is not delayed to match.
//! Dropping a [`Speaker`] closes the output stream.

use std::time::{Duration, Instant};

use fastcord_audio::{
    AudioEngine, DeviceChoice, Direction, EncodedPacket, EngineConfig, EngineControl, EngineEvent,
    NoiseSuppression, RemoteFrame,
};
use fastcord_media::ReceivedAudio;
use tokio::sync::mpsc;

use super::VoiceControls;
use crate::playback::AudioSettings;

/// A lost output device is retried no more often than this.
const RETRY: Duration = Duration::from_secs(5);

pub(super) struct Speaker {
    // Held so the output stream lives exactly as long as the speaker.
    _engine: AudioEngine,
    remote: mpsc::Sender<RemoteFrame>,
    controls: mpsc::Sender<EngineControl>,
    events: mpsc::Receiver<EngineEvent>,
    device: DeviceChoice,
    enabled: bool,
    gain: Option<(u32, u16)>,
}

/// Starts playing stream audio on the call's chosen output device.
pub(super) fn start(controls: &VoiceControls, audio: AudioSettings) -> Result<Speaker, String> {
    let (engine, channels) = AudioEngine::start(EngineConfig {
        input: None,
        output: Some(controls.output_device.clone()),
        noise_suppression: NoiseSuppression::Off,
        echo_cancellation: false,
        capture_enabled: false,
        ..EngineConfig::default()
    })
    .map_err(|error| format!("Stream audio output failed: {error}"))?;
    let mut speaker = Speaker {
        _engine: engine,
        remote: channels
            .remote_audio
            .ok_or("Stream audio output did not open")?,
        controls: channels.controls,
        events: channels.events,
        device: controls.output_device.clone(),
        enabled: true,
        gain: None,
    };
    speaker.apply(controls, audio);
    Ok(speaker)
}

impl Speaker {
    /// Offers one packet; a full queue drops it (live audio is never queued).
    pub(super) fn push(&mut self, audio: ReceivedAudio, settings: AudioSettings) {
        // Apply gain before the first packet (also after an SSRC replacement).
        if self.gain != Some((audio.ssrc, settings.percent)) {
            if self
                .controls
                .try_send(EngineControl::RemoteGain {
                    ssrc: audio.ssrc,
                    gain: f32::from(settings.percent) / 100.0,
                })
                .is_err()
            {
                return;
            }
            self.gain = Some((audio.ssrc, settings.percent));
        }
        let Ok(packet) = EncodedPacket::from_slice(&audio.payload) else {
            return;
        };
        let _ = self.remote.try_send(RemoteFrame {
            ssrc: audio.ssrc,
            sequence: audio.sequence,
            timestamp: audio.timestamp,
            packet,
        });
    }

    /// Follows the call's output device and deafen state.
    pub(super) fn apply(&mut self, controls: &VoiceControls, audio: AudioSettings) {
        let enabled = controls.output_enabled() && !audio.muted && audio.percent > 0;
        if enabled != self.enabled
            && self
                .controls
                .try_send(EngineControl::OutputEnabled(enabled))
                .is_ok()
        {
            self.enabled = enabled;
        }
        if controls.output_device != self.device
            && self
                .controls
                .try_send(EngineControl::SelectDevice {
                    direction: Direction::Output,
                    choice: controls.output_device.clone(),
                })
                .is_ok()
        {
            self.device = controls.output_device.clone();
        }
        if let Some((ssrc, percent)) = self.gain
            && percent != audio.percent
            && self
                .controls
                .try_send(EngineControl::RemoteGain {
                    ssrc,
                    gain: f32::from(audio.percent) / 100.0,
                })
                .is_ok()
        {
            self.gain = Some((ssrc, audio.percent));
        }
    }

    /// Whether the output stream failed (device unplugged); drains the events.
    pub(super) fn failed(&mut self) -> bool {
        let mut failed = false;
        while let Ok(event) = self.events.try_recv() {
            failed |= matches!(
                event,
                EngineEvent::StreamFailed {
                    direction: Direction::Output,
                    ..
                }
            );
        }
        failed
    }
}

/// The viewer's speaker: started on the first packet, restarted (rarely)
/// after a device loss, and absent while the stream has no audio.
#[derive(Default)]
pub(super) struct Playback {
    speaker: Option<Speaker>,
    retry_at: Option<Instant>,
    error: Option<String>,
}

impl Playback {
    pub(super) fn push(
        &mut self,
        audio: ReceivedAudio,
        controls: &VoiceControls,
        settings: AudioSettings,
    ) -> &str {
        if self.speaker.as_mut().is_some_and(Speaker::failed) {
            self.speaker = None;
            self.retry_at = Some(Instant::now() + RETRY);
            self.error = Some("Stream audio output was lost; retrying in 5 seconds".into());
        }
        if self.speaker.is_none() && self.retry_at.is_none_or(|at| Instant::now() >= at) {
            match start(controls, settings) {
                Ok(speaker) => {
                    self.speaker = Some(speaker);
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
            self.retry_at = self.speaker.is_none().then(|| Instant::now() + RETRY);
        }
        if let Some(speaker) = self.speaker.as_mut() {
            // Retry controls if the bounded engine queue was full.
            speaker.apply(controls, settings);
            speaker.push(audio, settings);
        }
        self.error.as_deref().unwrap_or_else(|| {
            if !controls.output_enabled() {
                "Stream audio muted by deafen"
            } else if settings.muted || settings.percent == 0 {
                "Stream audio muted"
            } else {
                "Receiving stream audio"
            }
        })
    }

    pub(super) fn apply(&mut self, controls: &VoiceControls, audio: AudioSettings) {
        if let Some(speaker) = self.speaker.as_mut() {
            speaker.apply(controls, audio);
        }
    }

    #[cfg(test)]
    pub(super) fn is_playing(&self) -> bool {
        self.speaker.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_playback_opens_no_device() {
        let mut playback = Playback::default();
        playback.apply(&VoiceControls::default(), AudioSettings::default());
        assert!(!playback.is_playing());
    }

    /// Real receive output -> WASAPI application loopback -> stream Opus
    /// encoder, without Discord or a microphone. No captured audio is played
    /// back, so this cannot feed itself.
    #[test]
    #[cfg(windows)]
    #[ignore = "plays a quiet test tone; requires Windows process loopback and an output device"]
    fn hardware_stream_audio_volume_mute_and_capture_round_trip() {
        use fastcord_platform::loopback::{self, LoopbackScope};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
        let energy = Arc::new(AtomicU64::new(0));
        let samples = Arc::new(AtomicU64::new(0));
        let (encoder, mut input) = fastcord_audio::soundshare::start(|_| {}).unwrap();
        let capture = loopback::start(
            LoopbackScope::Application {
                process_id: std::process::id(),
            },
            {
                let energy = energy.clone();
                let samples = samples.clone();
                Box::new(move |block| {
                    energy.fetch_add(
                        (block.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() * 1e9) as u64,
                        Ordering::Relaxed,
                    );
                    samples.fetch_add(block.len() as u64, Ordering::Relaxed);
                    input.push(block);
                })
            },
        )
        .unwrap();
        let controls = VoiceControls::default();
        let mut speaker = start(&controls, AudioSettings::default()).unwrap();
        let mut opus = fastcord_audio::MusicEncoder::new(128_000).unwrap();
        let mut sequence = 0;
        let mut measure = |audio: AudioSettings| {
            speaker.apply(&controls, audio);
            for index in 0..100 {
                if index == 50 {
                    energy.store(0, Ordering::Relaxed);
                    samples.store(0, Ordering::Relaxed);
                }
                let mut pcm = [0.0; 1920];
                for (n, stereo) in pcm.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let value =
                        ((sequence * 960 + n as u64) as f32 * std::f32::consts::TAU * 440.0
                            / 48_000.0)
                            .sin()
                            * 0.08;
                    stereo.fill(value);
                }
                let mut packet = EncodedPacket::from_slice(&[]).unwrap();
                opus.encode_stereo(&pcm, &mut packet).unwrap();
                speaker.push(
                    ReceivedAudio {
                        user_id: fastcord_model::Snowflake(1),
                        ssrc: 77,
                        sequence,
                        timestamp: (sequence * 960) as u32,
                        payload: packet.as_bytes().to_vec(),
                    },
                    audio,
                );
                sequence += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            let count = samples.load(Ordering::Relaxed);
            assert!(count > 4_800, "WASAPI delivered no audio samples");
            (energy.load(Ordering::Relaxed) as f64 / 1e9 / count as f64).sqrt()
        };
        let full = measure(AudioSettings::default());
        let quiet = measure(AudioSettings {
            percent: 25,
            muted: false,
        });
        let muted = measure(AudioSettings {
            percent: 100,
            muted: true,
        });
        assert!(full > 0.005, "stream output is silent: {full}");
        assert!(
            (0.12..0.40).contains(&(quiet / full)),
            "volume did not reach the mixer: {full}, {quiet}"
        );
        assert!(
            muted < full * 0.02,
            "stream mute did not silence output: {muted}"
        );
        assert!(
            encoder.stats().frames_sent > 20,
            "captured application audio was not encoded"
        );
        assert_eq!(speaker._engine.stats().decode_errors, 0);
        drop(capture);
        drop(encoder);
    }
}
