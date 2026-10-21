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
}

/// Starts playing stream audio on the call's chosen output device.
pub(super) fn start(controls: &VoiceControls) -> Option<Speaker> {
    let (engine, channels) = AudioEngine::start(EngineConfig {
        input: None,
        output: Some(controls.output_device.clone()),
        noise_suppression: NoiseSuppression::Off,
        echo_cancellation: false,
        capture_enabled: false,
        ..EngineConfig::default()
    })
    .ok()?;
    let mut speaker = Speaker {
        _engine: engine,
        remote: channels.remote_audio?,
        controls: channels.controls,
        events: channels.events,
        device: controls.output_device.clone(),
        enabled: true,
    };
    speaker.apply(controls);
    Some(speaker)
}

impl Speaker {
    /// Offers one packet; a full queue drops it (live audio is never queued).
    pub(super) fn push(&mut self, audio: ReceivedAudio) {
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
    pub(super) fn apply(&mut self, controls: &VoiceControls) {
        let enabled = controls.output_enabled();
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
}

impl Playback {
    pub(super) fn push(&mut self, audio: ReceivedAudio, controls: &VoiceControls) {
        if self.speaker.as_mut().is_some_and(Speaker::failed) {
            self.speaker = None;
            self.retry_at = Some(Instant::now() + RETRY);
        }
        if self.speaker.is_none() && self.retry_at.is_none_or(|at| Instant::now() >= at) {
            self.speaker = start(controls);
            self.retry_at = self.speaker.is_none().then(|| Instant::now() + RETRY);
        }
        if let Some(speaker) = self.speaker.as_mut() {
            speaker.push(audio);
        }
    }

    pub(super) fn apply(&mut self, controls: &VoiceControls) {
        if let Some(speaker) = self.speaker.as_mut() {
            speaker.apply(controls);
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
        playback.apply(&VoiceControls::default());
        assert!(!playback.is_playing());
    }
}
