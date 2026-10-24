use super::camera::{self, CallVideo, CameraTiles};
use super::{VoiceControls, VoiceParticipant};
use fastcord_audio::{
    AudioEngine, AudioError, EncodedPacket, EngineChannels, EngineConfig, EngineControl,
    NoiseSuppression, NoiseSuppressionStatus, RemoteFrame,
};
use fastcord_media::{
    VoiceCredentials, VoiceEvent, VoiceMessage, VoiceStatus, connect, connect_with_video,
};
use std::collections::HashMap;
use std::mem::replace;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

use fastcord_model::Snowflake;
use tokio::time::Instant;

/// What the audio side of the current call is doing, for the voice controls to show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VoiceAudioState {
    /// No call, or the call ended normally.
    #[default]
    Idle,
    /// A join or move has been requested and is waiting for voice credentials.
    Connecting,
    /// The audio engine is running and carrying the call.
    Running,
    /// The engine could not open the selected audio devices; the call was left.
    EngineFailed,
}

type EngineStart = Result<(AudioEngine, EngineChannels), AudioError>;

/// Runs one voice call's audio until it ends, is cancelled, or fails.
///
/// Cancellation (and every other exit) leaves gracefully: speech is ended and the voice
/// session is closed with a normal close instead of being aborted.
pub(super) async fn run(
    network: fastcord_net::NetworkContext,
    credentials: VoiceCredentials,
    cancel_rx: oneshot::Receiver<()>,
    state: watch::Sender<VoiceAudioState>,
    controls: watch::Receiver<VoiceControls>,
    controls_tx: watch::Sender<VoiceControls>,
    tiles: Arc<CameraTiles>,
) {
    let initial = controls.borrow().clone();
    let capture_enabled = initial.capture_enabled();
    let bitrate = initial.send_bitrate();
    run_with(
        (network, credentials),
        cancel_rx,
        state,
        controls,
        controls_tx,
        tiles,
        move || {
            AudioEngine::start(EngineConfig {
                input: Some(initial.input_device),
                output: Some(initial.output_device),
                noise_suppression: initial.noise_suppression,
                echo_cancellation: !initial.echo_cancellation_off,
                capture_enabled,
                bitrate,
            })
        },
    )
    .await;
}

/// At most this many participants are tracked, published, and sent gains per call.
const MAX_PARTICIPANTS: usize = 128;
/// Remote speakers whose gain was sent to the engine; bounds the dedupe map.
const MAX_SENT_GAINS: usize = 256;
/// How long after the last voice activity a participant stops being shown as speaking.
const SPEAKING_RELEASE: Duration = Duration::from_millis(250);
/// How often the microphone level is read while the audio dialog is open.
const METER_INTERVAL: Duration = Duration::from_millis(100);
/// When the engine's control queue was full, the unsent controls are retried after this.
const CONTROL_RETRY: Duration = Duration::from_millis(20);

/// What the engine has been told, so the bounded control queue carries changes only
/// (never one message per received packet) and a refused control is retried, not lost.
struct EngineSync {
    controls: mpsc::Sender<EngineControl>,
    capture: Option<bool>,
    output: Option<bool>,
    gains: HashMap<u32, f32>,
    retry: bool,
    input_device: (fastcord_audio::DeviceChoice, u64),
    output_device: (fastcord_audio::DeviceChoice, u64),
    noise_suppression: (NoiseSuppression, u64),
    echo_cancellation: bool,
    sensitivity: Option<u8>,
    input_gain: fastcord_audio::InputGain,
    diagnostics: bool,
    bitrate: u32,
}

impl EngineSync {
    fn new(controls: mpsc::Sender<EngineControl>) -> Self {
        Self {
            controls,
            capture: None,
            output: None,
            gains: HashMap::new(),
            retry: false,
            input_device: (fastcord_audio::DeviceChoice::SystemDefault, 0),
            output_device: (fastcord_audio::DeviceChoice::SystemDefault, 0),
            noise_suppression: (NoiseSuppression::Light, 0),
            // The engine starts with the preference it was configured with.
            echo_cancellation: true,
            // The engine starts with no level gate, no gain and no measurement.
            sensitivity: None,
            input_gain: fastcord_audio::InputGain::default(),
            diagnostics: false,
            bitrate: fastcord_audio::DEFAULT_BITRATE,
        }
    }

    /// Sends whatever differs from what the engine already has. `ptt_active` is the
    /// push-to-talk hold including its release delay.
    fn apply(&mut self, controls: &VoiceControls, ptt_active: bool) {
        self.retry = false;
        let capture = controls.capture_enabled_with(ptt_active);
        if self.capture != Some(capture) {
            if self
                .controls
                .try_send(EngineControl::CaptureEnabled(capture))
                .is_ok()
            {
                self.capture = Some(capture);
            } else {
                self.retry = true;
            }
        }
        let output = controls.output_enabled();
        if self.output != Some(output) {
            if self
                .controls
                .try_send(EngineControl::OutputEnabled(output))
                .is_ok()
            {
                self.output = Some(output);
            } else {
                self.retry = true;
            }
        }
        for (direction, desired, generation, sent) in [
            (
                fastcord_audio::Direction::Input,
                &controls.input_device,
                controls.input_generation,
                &mut self.input_device,
            ),
            (
                fastcord_audio::Direction::Output,
                &controls.output_device,
                controls.output_generation,
                &mut self.output_device,
            ),
        ] {
            if &sent.0 != desired || sent.1 != generation {
                if self
                    .controls
                    .try_send(EngineControl::SelectDevice {
                        direction,
                        choice: desired.clone(),
                    })
                    .is_ok()
                {
                    *sent = (desired.clone(), generation);
                } else {
                    self.retry = true;
                }
            }
        }
        let desired_noise = (controls.noise_suppression, controls.noise_generation);
        if self.noise_suppression != desired_noise {
            if self
                .controls
                .try_send(EngineControl::NoiseSuppression(desired_noise.0))
                .is_ok()
            {
                self.noise_suppression = desired_noise;
            } else {
                self.retry = true;
            }
        }
        let sensitivity = controls.effective_sensitivity();
        if self.sensitivity != sensitivity {
            if self
                .controls
                .try_send(EngineControl::InputSensitivity(sensitivity))
                .is_ok()
            {
                self.sensitivity = sensitivity;
            } else {
                self.retry = true;
            }
        }
        let echo = !controls.echo_cancellation_off;
        if self.echo_cancellation != echo {
            if self
                .controls
                .try_send(EngineControl::EchoCancellation(echo))
                .is_ok()
            {
                self.echo_cancellation = echo;
            } else {
                self.retry = true;
            }
        }
        if self.input_gain != controls.input_gain {
            if self
                .controls
                .try_send(EngineControl::InputGain(controls.input_gain))
                .is_ok()
            {
                self.input_gain = controls.input_gain;
            } else {
                self.retry = true;
            }
        }
        if self.diagnostics != controls.diagnostics_open {
            if self
                .controls
                .try_send(EngineControl::Diagnostics(controls.diagnostics_open))
                .is_ok()
            {
                self.diagnostics = controls.diagnostics_open;
            } else {
                self.retry = true;
            }
        }
        let bitrate = controls.send_bitrate();
        if self.bitrate != bitrate {
            if self
                .controls
                .try_send(EngineControl::Bitrate(bitrate))
                .is_ok()
            {
                self.bitrate = bitrate;
            } else {
                self.retry = true;
            }
        }
        for participant in &controls.participants {
            if let Some(ssrc) = participant.ssrc {
                self.gain(ssrc, controls.volume_percent(participant.user_id));
            }
        }
    }

    fn gain(&mut self, ssrc: u32, percent: u16) {
        let gain = percent as f32 / 100.0;
        if self.gains.get(&ssrc) == Some(&gain) {
            return;
        }
        if self
            .controls
            .try_send(EngineControl::RemoteGain { ssrc, gain })
            .is_ok()
        {
            if self.gains.len() >= MAX_SENT_GAINS {
                self.gains.clear();
            }
            self.gains.insert(ssrc, gain);
        } else {
            self.retry = true;
        }
    }

    fn retry_deadline(&self) -> Option<Instant> {
        self.retry.then(|| Instant::now() + CONTROL_RETRY)
    }
}

/// How long after the last sent frame the speech burst is ended (five 20 ms frames),
/// so a gate that stops frames also sends "speaking off".
const SPEECH_IDLE: Duration = Duration::from_millis(100);

/// Push-to-talk as the call worker sees it: the key, extended by the release delay, and
/// dropped at once on a cancel (focus, listener, device or session loss).
#[derive(Debug, Default)]
struct PttHold {
    active: bool,
    release_at: Option<Instant>,
    cancel_seen: u64,
    /// A cancel arrived while the key was still held; it must be released and pressed
    /// again before it transmits, so a lost device never resumes by itself.
    blocked: bool,
}

impl PttHold {
    /// Follows the shared key state. Returns whether the active state changed.
    fn update(&mut self, controls: &VoiceControls, now: Instant) -> bool {
        let before = self.active;
        if controls.ptt_cancel != self.cancel_seen {
            self.cancel_seen = controls.ptt_cancel;
            self.cancel(controls.ptt_down);
        } else if controls.ptt_down {
            if !self.blocked {
                self.active = true;
                self.release_at = None;
            }
        } else {
            self.blocked = false;
            if self.active && self.release_at.is_none() {
                let delay = Duration::from_millis(u64::from(controls.ptt_release_ms));
                if delay.is_zero() {
                    self.active = false;
                } else {
                    self.release_at = Some(now + delay);
                }
            }
        }
        self.active != before
    }

    /// Drops the hold now. `still_down` keeps it off until the key is released.
    fn cancel(&mut self, still_down: bool) {
        self.active = false;
        self.release_at = None;
        self.blocked = still_down;
    }

    /// Ends the release delay once it has run out. Returns whether it did.
    fn expire(&mut self, now: Instant) -> bool {
        if self.release_at.is_some_and(|at| at <= now) {
            self.active = false;
            self.release_at = None;
            return true;
        }
        false
    }
}

/// Publishes the running engine's noise-suppression state for the settings dialog and
/// withdraws it when the engine stops, however the call ends.
struct NoiseStatus<'a>(&'a watch::Sender<VoiceControls>);

impl NoiseStatus<'_> {
    fn publish(&self, status: NoiseSuppressionStatus) {
        self.0
            .send_modify(|controls| controls.noise_status = Some(status));
    }

    /// Publishes the microphone level, only when it changed.
    fn publish_level(&self, diagnostics: Option<fastcord_audio::InputDiagnostics>) {
        self.0.send_if_modified(|controls| {
            super::replace_if_changed(&mut controls.input_diagnostics, diagnostics)
        });
    }

    /// Publishes the engine counters the dialog shows, only when one of them changed.
    /// `transport` is `Some` only while the audio dialog is open; the
    /// microphone counters then join the change key.
    fn publish_stats(
        &self,
        stats: fastcord_audio::EngineStats,
        transport: Option<fastcord_media::gateway::TransportStats>,
    ) {
        self.0.send_if_modified(|controls| {
            let shown = transport.is_some();
            let key = |s: &fastcord_audio::EngineStats| {
                (
                    s.output_underruns,
                    s.plc_frames,
                    s.device_xruns,
                    s.input_overrun_samples,
                    s.late_ticks,
                    s.callback_realtime_denied,
                    shown.then_some((s.frames_encoded, s.frames_gated)),
                )
            };
            if controls.audio_stats.as_ref().map(key) == Some(key(&stats))
                && controls.transport_stats == transport
            {
                return false;
            }
            controls.audio_stats = Some(stats);
            controls.transport_stats = transport;
            true
        });
    }
}

impl Drop for NoiseStatus<'_> {
    fn drop(&mut self) {
        self.0.send_modify(|controls| {
            controls.noise_status = None;
            controls.audio_stats = None;
            controls.transport_stats = None;
            controls.input_diagnostics = None;
        });
    }
}
async fn run_with(
    connection: (fastcord_net::NetworkContext, VoiceCredentials),
    mut cancel_rx: oneshot::Receiver<()>,
    state: watch::Sender<VoiceAudioState>,
    mut controls: watch::Receiver<VoiceControls>,
    controls_tx: watch::Sender<VoiceControls>,
    tiles: Arc<CameraTiles>,
    start_engine: impl FnOnce() -> EngineStart + Send + 'static,
) {
    let (network, credentials) = connection;
    // A call connection offers video (cameras) next to audio; if the offer is
    // ever inconsistent the call is still an audio call.
    let mut session =
        connect_with_video(network.clone(), credentials.clone(), camera::call_options())
            .unwrap_or_else(|_| connect(network, credentials));
    let media = session.media();
    let started = tokio::task::spawn_blocking(start_engine).await;
    let (engine, channels) = match started {
        Ok(Ok(running)) => running,
        Ok(Err(_)) | Err(_) => {
            state.send_replace(VoiceAudioState::EngineFailed);
            let _ = media.end_speech();
            session.leave().await;
            return;
        }
    };
    state.send_replace(VoiceAudioState::Running);
    crate::voice_log::note("call: audio engine running");
    let mut anomalies = Anomalies::default();
    // Every call starts with the camera off; only the user's click turns it on.
    let mut video = CallVideo::new(media.clone(), tiles, controls_tx.clone());
    let mut tile_changes = video.changes();
    let mut camera_progress = video.progress();
    let noise_status = NoiseStatus(&controls_tx);
    let mut captured = channels.captured;
    let mut remote = channels.remote_audio;
    let mut sync = EngineSync::new(channels.controls);
    let mut events = channels.events;
    let mut status = session.watch_status();
    let mut hold = PttHold::default();
    let mut speech_end_at: Option<Instant> = None;
    let mut capture_enabled = {
        let initial = controls.borrow_and_update();
        hold.cancel_seen = initial.ptt_cancel;
        hold.update(&initial, Instant::now());
        sync.apply(&initial, hold.active);
        initial.capture_enabled_with(hold.active)
    };
    let mut retry_at = sync.retry_deadline();
    // Level polling runs only while the audio dialog is open.
    let mut meter_at = controls.borrow().diagnostics_open.then(Instant::now);
    let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
    let mut engine_failed = false;
    let mut participants = HashMap::<Snowflake, VoiceParticipant>::new();
    let mut release_at = HashMap::<Snowflake, Instant>::new();
    loop {
        let speaking_now = speech_end_at.is_some();
        controls_tx.send_if_modified(|controls| {
            replace(&mut controls.self_speaking, speaking_now) != speaking_now
        });
        let release_deadline = release_at.values().copied().min();
        tokio::select! {
            frame = receive_capture(&mut captured) => {
                let Some(frame) = frame else {
                    captured = None;
                    continue;
                };
                if capture_enabled {
                    let _ = media.send_opus(frame.packet.as_bytes().to_vec());
                    speech_end_at = Some(Instant::now() + SPEECH_IDLE);
                }
            }
            message = session.next_message() => {
                match message {
                    Some(VoiceMessage::Audio(packet)) => {
                        let percent = controls.borrow().volume_percent(packet.user_id);
                        sync.gain(packet.ssrc, percent);
                        if sync.retry && retry_at.is_none() {
                            retry_at = sync.retry_deadline();
                        }
                        // Received audio is the speaking signal; op-5 only attributes SSRCs.
                        if note_audio(
                            packet.user_id,
                            packet.ssrc,
                            &mut participants,
                            &mut release_at,
                            Instant::now(),
                        ) {
                            publish_participants(&controls_tx, &participants);
                        }
                        let Ok(packet_bytes) = EncodedPacket::from_slice(&packet.payload) else {
                            continue;
                        };
                        let Some(output) = remote.as_ref() else { continue };
                        let _ = output.try_send(RemoteFrame {
                            ssrc: packet.ssrc,
                            sequence: packet.sequence,
                            timestamp: packet.timestamp,
                            packet: packet_bytes,
                        });
                    }
                    Some(VoiceMessage::Event(event)) => {
                        video.on_event(&event);
                        apply_voice_event(event, &mut participants, &mut release_at);
                        publish_participants(&controls_tx, &participants);
                    }
                    // Cameras ride on this connection; screen shares have their own.
                    // Video never touches the audio engine's capture path.
                    Some(VoiceMessage::Video(frame)) => video.on_frame(frame),
                    None => break,
                }
            }
            _ = wait_for_release(release_deadline) => {
                let now = Instant::now();
                let expired: Vec<_> = release_at.iter()
                    .filter_map(|(user, deadline)| (*deadline <= now).then_some(*user))
                    .collect();
                for user in &expired {
                    release_at.remove(user);
                    if let Some(participant) = participants.get_mut(user) {
                        participant.speaking = false;
                    }
                }
                if !expired.is_empty() {
                    publish_participants(&controls_tx, &participants);
                }
            }
            changed = controls.changed() => {
                if changed.is_err() { break; }
                let updated = controls.borrow_and_update();
                hold.update(&updated, Instant::now());
                sync.apply(&updated, hold.active);
                retry_at = sync.retry_deadline();
                let enabled = updated.capture_enabled_with(hold.active);
                let dialog_open = updated.diagnostics_open;
                let camera_wanted = updated.camera.clone();
                drop(updated);
                video.sync(&camera_wanted);
                if capture_enabled && !enabled {
                    let _ = media.end_speech();
                    speech_end_at = None;
                }
                capture_enabled = enabled;
                if !dialog_open {
                    meter_at = None;
                    noise_status.publish_level(None);
                } else if meter_at.is_none() {
                    meter_at = Some(Instant::now());
                }
            }
            changed = tile_changes.changed() => {
                if changed.is_err() { break; }
                tile_changes.borrow_and_update();
                video.on_layout();
            }
            changed = camera_progress.changed() => {
                if changed.is_err() { break; }
                camera_progress.borrow_and_update();
                video.on_progress();
            }
            _ = wait_for_release(meter_at) => {
                meter_at = Some(Instant::now() + METER_INTERVAL);
                noise_status.publish_level(Some(engine.input_diagnostics()));
            }
            _ = wait_for_release(hold.release_at) => {
                if hold.expire(Instant::now()) {
                    let updated = controls.borrow();
                    sync.apply(&updated, hold.active);
                    retry_at = sync.retry_deadline();
                    let enabled = updated.capture_enabled_with(hold.active);
                    drop(updated);
                    if capture_enabled && !enabled {
                        let _ = media.end_speech();
                        speech_end_at = None;
                    }
                    capture_enabled = enabled;
                }
            }
            _ = wait_for_release(speech_end_at) => {
                speech_end_at = None;
                let _ = media.end_speech();
            }
            _ = stats_tick.tick() => {
                let (engine_stats, transport_stats) = (engine.stats(), session.stats());
                anomalies.check(&engine_stats, &transport_stats);
                let transport = controls.borrow().diagnostics_open.then_some(transport_stats);
                noise_status.publish_stats(engine_stats, transport);
            }
            _ = wait_for_release(retry_at) => {
                sync.apply(&controls.borrow(), hold.active);
                retry_at = sync.retry_deadline();
            }
            event = events.recv() => {
                match event {
                    Some(fastcord_audio::EngineEvent::StreamFailed { direction, kind }) => {
                        controls_tx.send_modify(|controls| match direction {
                            fastcord_audio::Direction::Input => controls.input_error = Some(kind),
                            fastcord_audio::Direction::Output => controls.output_error = Some(kind),
                        });
                        crate::voice_log::note(&format!("device: {direction:?} stream failed ({kind:?})"));
                        match direction {
                            fastcord_audio::Direction::Input => {
                                captured = None;
                                let _ = media.end_speech();
                                release_push_to_talk(&controls_tx);
                            }
                            fastcord_audio::Direction::Output => remote = None,
                        }
                    }
                    Some(fastcord_audio::EngineEvent::StreamReady { direction, captured: new_capture, remote_audio, .. }) => {
                        controls_tx.send_modify(|controls| match direction {
                            fastcord_audio::Direction::Input => controls.input_error = None,
                            fastcord_audio::Direction::Output => controls.output_error = None,
                        });
                        crate::voice_log::note(&format!("device: {direction:?} stream ready"));
                        match direction {
                            fastcord_audio::Direction::Input => {
                                let _ = media.end_speech();
                                release_push_to_talk(&controls_tx);
                                captured = new_capture;
                            }
                            fastcord_audio::Direction::Output => remote = remote_audio,
                        }
                    }
                    Some(fastcord_audio::EngineEvent::NoiseSuppressionChanged(noise)) => {
                        noise_status.publish(noise);
                    }
                    None => {
                        crate::voice_log::note("call: audio engine stopped unexpectedly");
                        engine_failed = true;
                        break;
                    }
                }
            }
            changed = status.changed() => {
                if changed.is_err() { break; }
                let now = *status.borrow_and_update();
                crate::voice_log::note(&format!("connection: {}", describe_status(&now)));
                if matches!(now, VoiceStatus::Closed(_)) { break; }
            }
            _ = &mut cancel_rx => break,
        }
    }
    participants.clear();
    release_at.clear();
    publish_participants(&controls_tx, &participants);
    // No key state outlives the call: the next one starts with nothing held.
    release_push_to_talk(&controls_tx);
    controls_tx.send_modify(|controls| controls.push_to_mute_down = false);

    // Release the camera and the decoders, then the audio devices, before the next call
    // can open them, then say goodbye.
    video.finish().await;
    drop(engine);
    drop(noise_status);
    let _ = media.end_speech();
    controls_tx.send_if_modified(|controls| replace(&mut controls.self_speaking, false));
    crate::voice_log::note("call: left");
    session.leave().await;
    // A failed engine stays visible (the worker then leaves the Gateway voice state too).
    state.send_replace(if engine_failed {
        VoiceAudioState::EngineFailed
    } else {
        VoiceAudioState::Idle
    });
}

/// Treats a received audio packet as speaking activity. Returns whether the published
/// participant changed (it newly speaks, was newly attributed, or is new). A packet on an
/// SSRC other than the participant's current one is stale and ignored.
fn note_audio(
    user_id: Snowflake,
    ssrc: u32,
    participants: &mut HashMap<Snowflake, VoiceParticipant>,
    release_at: &mut HashMap<Snowflake, Instant>,
    now: Instant,
) -> bool {
    if participants.len() >= MAX_PARTICIPANTS && !participants.contains_key(&user_id) {
        return false;
    }
    let before = participants.get(&user_id).copied();
    let participant = participants.entry(user_id).or_insert(VoiceParticipant {
        user_id,
        ssrc: Some(ssrc),
        speaking: false,
    });
    if participant.ssrc.is_none() {
        participant.ssrc = Some(ssrc);
    } else if participant.ssrc != Some(ssrc) {
        return false;
    }
    participant.speaking = true;
    release_at.insert(user_id, now + SPEAKING_RELEASE);
    before != Some(*participant)
}

fn apply_voice_event(
    event: VoiceEvent,
    participants: &mut HashMap<Snowflake, VoiceParticipant>,
    release_at: &mut HashMap<Snowflake, Instant>,
) {
    match event {
        VoiceEvent::ClientsConnected(users) => {
            for user_id in users.into_iter().take(MAX_PARTICIPANTS) {
                if participants.len() >= MAX_PARTICIPANTS && !participants.contains_key(&user_id) {
                    break;
                }
                participants.entry(user_id).or_insert(VoiceParticipant {
                    user_id,
                    ssrc: None,
                    speaking: false,
                });
            }
        }
        VoiceEvent::ClientDisconnected(user_id) => {
            participants.remove(&user_id);
            release_at.remove(&user_id);
        }
        VoiceEvent::Speaking {
            user_id,
            ssrc,
            flags,
        } => {
            if participants.len() < MAX_PARTICIPANTS || participants.contains_key(&user_id) {
                let participant = participants.entry(user_id).or_insert(VoiceParticipant {
                    user_id,
                    ssrc: None,
                    speaking: false,
                });
                if flags & fastcord_media::gateway::speaking::VOICE != 0 {
                    participant.ssrc = (ssrc != 0).then_some(ssrc);
                    participant.speaking = true;
                    release_at.insert(user_id, Instant::now() + SPEAKING_RELEASE);
                } else if participant.ssrc == Some(ssrc) && participant.speaking {
                    release_at.insert(user_id, Instant::now() + SPEAKING_RELEASE);
                }
            }
        }
        // Video negotiation and encoder feedback belong to stream sessions; this
        // parent call offers no video.
        VoiceEvent::UdpLatency(_)
        | VoiceEvent::ReceptionReport { .. }
        | VoiceEvent::VideoNegotiated { .. }
        | VoiceEvent::VideoSource { .. }
        | VoiceEvent::KeyframeRequested
        | VoiceEvent::VideoBitrate(_)
        | VoiceEvent::VideoActive(_) => {}
    }
}

/// Publishes the participant list. Only the participants field is written, in place, so a
/// concurrent mute, deafen, volume, or server-restriction change is never overwritten.
pub(super) fn publish_participants(
    controls: &watch::Sender<VoiceControls>,
    participants: &HashMap<Snowflake, VoiceParticipant>,
) {
    let mut next: Vec<_> = participants.values().copied().collect();
    next.sort_by_key(|participant| participant.user_id.0);
    if controls.borrow().participants != next {
        controls.send_modify(|controls| controls.participants = next);
    }
}

/// Drops a held push-to-talk at once (device lost or replaced, call over). The key has
/// to be released and pressed again before it transmits.
fn release_push_to_talk(controls: &watch::Sender<VoiceControls>) {
    controls.send_modify(|controls| {
        controls.ptt_down = false;
        controls.ptt_cancel = controls.ptt_cancel.wrapping_add(1);
    });
}

async fn wait_for_release(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
async fn receive_capture(
    receiver: &mut Option<mpsc::Receiver<fastcord_audio::CapturedFrame>>,
) -> Option<fastcord_audio::CapturedFrame> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use fastcord_media::{Generation, VoiceCredentials};
    use fastcord_model::{Snowflake, VoiceToken};

    use super::*;

    fn credentials() -> VoiceCredentials {
        VoiceCredentials {
            generation: Generation(1),
            server_id: Snowflake(1),
            channel_id: Snowflake(2),
            user_id: Snowflake(3),
            session_id: "test-session".into(),
            token: VoiceToken::new("test-token".into()),
            endpoint: "127.0.0.1:9".into(),
            dave_group_id: None,
        }
    }

    /// An engine that cannot start must leave the call and say so, not linger half-joined.
    #[tokio::test]
    async fn an_engine_that_cannot_start_ends_the_call_and_reports_it() {
        let (state, observer) = watch::channel(VoiceAudioState::Idle);
        let (_cancel, cancel_rx) = oneshot::channel();
        let (controls_tx, controls) = watch::channel(VoiceControls::default());
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_with(
                (fastcord_net::NetworkContext::off(), credentials()),
                cancel_rx,
                state,
                controls,
                controls_tx,
                Arc::default(),
                || Err(AudioError::NothingToRun),
            ),
        )
        .await
        .expect("a failed engine start must end the task promptly");
        assert_eq!(*observer.borrow(), VoiceAudioState::EngineFailed);
    }

    #[test]
    fn the_noise_status_is_withdrawn_when_the_engine_stops() {
        let (controls_tx, controls) = watch::channel(VoiceControls::default());
        assert_eq!(controls.borrow().noise_status, None);
        let status = NoiseSuppressionStatus {
            active: NoiseSuppression::HighQuality,
            ..NoiseSuppressionStatus::default()
        };
        {
            let published = NoiseStatus(&controls_tx);
            published.publish(status);
            assert_eq!(controls.borrow().noise_status, Some(status));
        }
        assert_eq!(controls.borrow().noise_status, None);
    }
    #[test]
    fn speaking_identity_survives_ssrc_replacement_and_has_release_delay() {
        let mut participants = HashMap::new();
        let mut release_at = HashMap::new();
        let user = Snowflake(7);
        apply_voice_event(
            VoiceEvent::Speaking {
                user_id: user,
                ssrc: 100,
                flags: fastcord_media::gateway::speaking::VOICE,
            },
            &mut participants,
            &mut release_at,
        );
        apply_voice_event(
            VoiceEvent::Speaking {
                user_id: user,
                ssrc: 101,
                flags: fastcord_media::gateway::speaking::VOICE,
            },
            &mut participants,
            &mut release_at,
        );
        assert_eq!(participants.len(), 1);
        assert_eq!(participants[&user].ssrc, Some(101));
        assert!(participants[&user].speaking);
        let release = release_at[&user];
        apply_voice_event(
            VoiceEvent::Speaking {
                user_id: user,
                ssrc: 100,
                flags: 0,
            },
            &mut participants,
            &mut release_at,
        );
        assert_eq!(participants[&user].ssrc, Some(101));
        assert_eq!(release_at[&user], release);
        apply_voice_event(
            VoiceEvent::Speaking {
                user_id: user,
                ssrc: 101,
                flags: 0,
            },
            &mut participants,
            &mut release_at,
        );
        assert!(participants[&user].speaking);
        assert!(release_at[&user] > Instant::now());
    }

    #[test]
    fn received_audio_drives_speaking_and_rearms_the_release() {
        let mut participants = HashMap::new();
        let mut release_at = HashMap::new();
        let user = Snowflake(9);
        let start = Instant::now();
        // A user whose op-5 arrived before this client joined is still shown speaking.
        assert!(note_audio(
            user,
            500,
            &mut participants,
            &mut release_at,
            start
        ));
        assert!(participants[&user].speaking);
        assert_eq!(participants[&user].ssrc, Some(500));
        let first = release_at[&user];
        // More packets re-arm the release but change nothing published.
        let later = start + Duration::from_millis(100);
        assert!(!note_audio(
            user,
            500,
            &mut participants,
            &mut release_at,
            later
        ));
        assert!(release_at[&user] > first);
        // A stale packet from a replaced SSRC is not activity.
        participants.get_mut(&user).unwrap().ssrc = Some(501);
        assert!(!note_audio(
            user,
            500,
            &mut participants,
            &mut release_at,
            later
        ));
        assert_eq!(participants[&user].ssrc, Some(501));
        // After the release a new talkspurt flips the indicator again.
        participants.get_mut(&user).unwrap().speaking = false;
        assert!(note_audio(
            user,
            501,
            &mut participants,
            &mut release_at,
            later
        ));
    }

    #[test]
    fn connect_and_audio_events_cannot_grow_the_participant_map_without_bound() {
        let mut participants = HashMap::new();
        let mut release_at = HashMap::new();
        for batch in 0..10u64 {
            let users = (0..100)
                .map(|user| Snowflake(batch * 100 + user + 1))
                .collect();
            apply_voice_event(
                VoiceEvent::ClientsConnected(users),
                &mut participants,
                &mut release_at,
            );
        }
        assert_eq!(participants.len(), MAX_PARTICIPANTS);
        assert!(!note_audio(
            Snowflake(99_999),
            1,
            &mut participants,
            &mut release_at,
            Instant::now()
        ));
        assert_eq!(participants.len(), MAX_PARTICIPANTS);
    }

    fn controls_with_participant(user: u64, ssrc: u32) -> VoiceControls {
        VoiceControls {
            participants: vec![VoiceParticipant {
                user_id: Snowflake(user),
                ssrc: Some(ssrc),
                speaking: false,
            }],
            ..VoiceControls::default()
        }
    }

    #[test]
    fn engine_controls_are_sent_once_per_change_and_retried_when_the_queue_is_full() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut sync = EngineSync::new(tx);
        let mut controls = controls_with_participant(4, 40);
        sync.apply(&controls, controls.ptt_down);
        // Capture and output fit; the gain does not and must be retried, not dropped.
        assert!(sync.retry);
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineControl::CaptureEnabled(true))
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineControl::OutputEnabled(true))
        ));
        sync.apply(&controls, controls.ptt_down);
        assert!(!sync.retry);
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineControl::RemoteGain { ssrc: 40, gain }) if gain == 1.0
        ));
        // Nothing changed: nothing is sent, however many times the controls are applied
        // or packets of that SSRC arrive.
        sync.apply(&controls, controls.ptt_down);
        sync.gain(40, 100);
        assert!(rx.try_recv().is_err());
        // A deafen toggle that finds the queue full is retried instead of lost.
        sync.gain(41, 100);
        sync.gain(42, 100);
        controls.deafened = true;
        sync.apply(&controls, controls.ptt_down);
        assert!(sync.retry);
        while rx.try_recv().is_ok() {}
        sync.apply(&controls, controls.ptt_down);
        assert!(!sync.retry);
        let mut sent = Vec::new();
        while let Ok(control) = rx.try_recv() {
            sent.push(control);
        }
        assert!(
            sent.iter()
                .any(|control| matches!(control, EngineControl::CaptureEnabled(false)))
        );
        assert!(
            sent.iter()
                .any(|control| matches!(control, EngineControl::OutputEnabled(false)))
        );
    }

    #[test]
    fn device_switches_are_independent_coalesced_and_retry_same_missing_choice() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut sync = EngineSync::new(tx);
        let mut controls = VoiceControls::default();
        sync.apply(&controls, controls.ptt_down);
        controls.input_device = fastcord_audio::DeviceChoice::Device(
            fastcord_audio::DeviceKey::from_persisted("mic").unwrap(),
        );
        controls.output_device = fastcord_audio::DeviceChoice::Device(
            fastcord_audio::DeviceKey::from_persisted("headset").unwrap(),
        );
        sync.apply(&controls, controls.ptt_down);
        assert!(sync.retry);
        while rx.try_recv().is_ok() {}
        sync.apply(&controls, controls.ptt_down);
        assert!(!sync.retry);
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineControl::SelectDevice {
                direction: fastcord_audio::Direction::Input,
                ..
            })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineControl::SelectDevice {
                direction: fastcord_audio::Direction::Output,
                ..
            })
        ));
        sync.apply(&controls, controls.ptt_down);
        assert!(rx.try_recv().is_err());
        controls.output_generation += 1;
        sync.apply(&controls, controls.ptt_down);
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineControl::SelectDevice {
                direction: fastcord_audio::Direction::Output,
                ..
            })
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn noise_change_retries_a_full_queue_and_same_selection_retry() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut sync = EngineSync::new(tx);
        let mut controls = VoiceControls::default();
        sync.apply(&controls, controls.ptt_down);
        controls.noise_suppression = NoiseSuppression::HighQuality;
        controls.noise_generation = 1;
        sync.apply(&controls, controls.ptt_down);
        assert!(sync.retry);
        while rx.try_recv().is_ok() {}
        sync.apply(&controls, controls.ptt_down);
        assert!(!sync.retry);
        assert_eq!(
            rx.try_recv().unwrap(),
            EngineControl::NoiseSuppression(NoiseSuppression::HighQuality)
        );
        sync.apply(&controls, controls.ptt_down);
        assert!(rx.try_recv().is_err());
        controls.noise_generation += 1;
        sync.apply(&controls, controls.ptt_down);
        assert_eq!(
            rx.try_recv().unwrap(),
            EngineControl::NoiseSuppression(NoiseSuppression::HighQuality)
        );
    }

    fn ptt_controls(down: bool, release_ms: u16, cancel: u64) -> VoiceControls {
        VoiceControls {
            input_mode: crate::shortcuts::InputMode::PushToTalk,
            ptt_down: down,
            ptt_release_ms: release_ms,
            ptt_cancel: cancel,
            ..VoiceControls::default()
        }
    }

    #[test]
    fn push_to_talk_holds_through_the_release_delay_then_stops() {
        let now = Instant::now();
        let mut hold = PttHold::default();
        assert!(hold.update(&ptt_controls(true, 300, 0), now));
        assert!(!hold.update(&ptt_controls(false, 300, 0), now));
        assert!(hold.active, "still inside the release delay");
        assert!(!hold.expire(now + Duration::from_millis(299)));
        assert!(hold.expire(now + Duration::from_millis(300)));
        assert!(!hold.active);
        // Pressing again inside the delay cancels the pending release.
        hold.update(&ptt_controls(true, 300, 0), now);
        hold.update(&ptt_controls(false, 300, 0), now);
        hold.update(&ptt_controls(true, 300, 0), now);
        assert!(!hold.expire(now + Duration::from_secs(5)));
        assert!(hold.active);
        // No delay releases immediately.
        assert!(hold.update(&ptt_controls(false, 0, 0), now));
        assert!(!hold.active);
    }

    #[test]
    fn a_cancel_drops_push_to_talk_at_once_and_needs_a_new_press() {
        let now = Instant::now();
        let mut hold = PttHold::default();
        hold.update(&ptt_controls(true, 500, 0), now);
        assert!(
            hold.update(&ptt_controls(false, 500, 1), now),
            "no delay on cancel"
        );
        assert!(!hold.active && hold.release_at.is_none());
        // A cancel while the key is still reported down stays off until it is released.
        hold.update(&ptt_controls(true, 500, 1), now);
        assert!(hold.active);
        hold.update(&ptt_controls(true, 500, 2), now);
        assert!(!hold.active);
        hold.update(&ptt_controls(true, 500, 2), now);
        assert!(!hold.active, "blocked until released");
        hold.update(&ptt_controls(false, 500, 2), now);
        hold.update(&ptt_controls(true, 500, 2), now);
        assert!(hold.active);
    }

    #[test]
    fn the_engine_follows_the_one_gate_and_the_sensitivity() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut sync = EngineSync::new(tx);
        let mut controls = ptt_controls(false, 0, 0);
        sync.apply(&controls, false);
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineControl::CaptureEnabled(false))
        ));
        while rx.try_recv().is_ok() {}
        sync.apply(&controls, true);
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineControl::CaptureEnabled(true))
        ));
        // The level gate is for voice activity only.
        controls.sensitivity = Some(40);
        sync.apply(&controls, true);
        assert!(rx.try_recv().is_err());
        controls.input_mode = crate::shortcuts::InputMode::VoiceActivity;
        sync.apply(&controls, false);
        let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(sent.contains(&EngineControl::InputSensitivity(Some(40))));
    }

    #[test]
    fn gain_and_the_level_meter_reach_the_engine_once_and_only_when_changed() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut sync = EngineSync::new(tx);
        let mut controls = ptt_controls(true, 0, 0);
        sync.apply(&controls, false);
        let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !sent.iter().any(|c| matches!(
                c,
                EngineControl::InputGain(_) | EngineControl::Diagnostics(_)
            )),
            "defaults are what the engine starts with: {sent:?}"
        );
        let gain = fastcord_audio::InputGain {
            manual_db: 6,
            automatic: true,
        };
        controls.input_gain = gain;
        controls.diagnostics_open = true;
        sync.apply(&controls, false);
        let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(sent.contains(&EngineControl::InputGain(gain)));
        assert!(sent.contains(&EngineControl::Diagnostics(true)));
        sync.apply(&controls, false);
        assert!(rx.try_recv().is_err(), "nothing repeats");
        controls.diagnostics_open = false;
        sync.apply(&controls, false);
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineControl::Diagnostics(false))
        ));
    }

    #[test]
    fn a_channel_bitrate_change_reaches_the_engine_once() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut sync = EngineSync::new(tx);
        let mut controls = ptt_controls(true, 0, 0);
        sync.apply(&controls, false);
        let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !sent.iter().any(|c| matches!(c, EngineControl::Bitrate(_))),
            "a DM call keeps the 64 kbps the engine starts with: {sent:?}"
        );
        controls.channel_bitrate = 96_000;
        sync.apply(&controls, false);
        sync.apply(&controls, false);
        let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(sent, vec![EngineControl::Bitrate(96_000)]);
        controls.channel_bitrate = 1;
        sync.apply(&controls, false);
        assert_eq!(rx.try_recv().ok(), Some(EngineControl::Bitrate(8_000)));
    }
}

/// A short, key-free name for a connection state.
fn describe_status(status: &VoiceStatus) -> String {
    match status {
        VoiceStatus::Connecting => "connecting".into(),
        VoiceStatus::Identifying => "identifying".into(),
        VoiceStatus::Discovering => "discovering address".into(),
        VoiceStatus::Negotiating => "negotiating transport".into(),
        VoiceStatus::Rekeying { transition_id } => match transition_id {
            Some(id) => format!(
                "end-to-end keys changing (transition {id}); audio pauses until it executes"
            ),
            None => "waiting for end-to-end keys; audio paused".into(),
        },
        VoiceStatus::Connected { .. } => "connected".into(),
        VoiceStatus::Resuming { attempt } => {
            format!("connection dropped, resuming (attempt {attempt})")
        }
        VoiceStatus::Closed(reason) => format!("closed: {reason:?}"),
    }
}

/// Once-a-second counters that only grow when something goes wrong; a line
/// is logged for each second in which one of them grew.
#[derive(Default)]
struct Anomalies {
    engine: Option<fastcord_audio::EngineStats>,
    transport: Option<fastcord_media::gateway::TransportStats>,
}

impl Anomalies {
    fn check(
        &mut self,
        engine: &fastcord_audio::EngineStats,
        transport: &fastcord_media::gateway::TransportStats,
    ) {
        for line in anomaly_lines(
            self.engine.as_ref(),
            engine,
            self.transport.as_ref(),
            transport,
        ) {
            crate::voice_log::note(&line);
        }
        self.engine = Some(*engine);
        self.transport = Some(*transport);
    }
}

fn anomaly_lines(
    old_engine: Option<&fastcord_audio::EngineStats>,
    engine: &fastcord_audio::EngineStats,
    old_transport: Option<&fastcord_media::gateway::TransportStats>,
    transport: &fastcord_media::gateway::TransportStats,
) -> Vec<String> {
    let mut lines = Vec::new();
    let (Some(e), Some(t)) = (old_engine, old_transport) else {
        return lines;
    };
    let mut grew = |now: u64, before: u64, what: &str| {
        if now > before {
            lines.push(format!("{what}: {}", now - before));
        }
    };
    grew(
        transport.rejected_authentication,
        t.rejected_authentication,
        "inbound: packets that failed decryption (end-to-end keys out of step) in the last second",
    );
    grew(
        transport.dropped_unknown_ssrc,
        t.dropped_unknown_ssrc,
        "inbound: packets from an unannounced speaker dropped",
    );
    grew(
        transport.dropped_queue_full,
        t.dropped_queue_full,
        "inbound: packets dropped because playback fell behind",
    );
    grew(
        engine.decode_errors,
        e.decode_errors,
        "playback: Opus decode errors",
    );
    grew(
        engine.output_underruns,
        e.output_underruns,
        "playback: underruns (silence played)",
    );
    grew(engine.device_xruns, e.device_xruns, "device: xruns");
    grew(
        engine.late_ticks,
        e.late_ticks,
        "engine: late wake-ups (CPU starved)",
    );
    lines
}

#[cfg(test)]
mod anomaly_tests {
    use super::*;

    #[test]
    fn only_counters_that_grew_are_logged() {
        let engine = fastcord_audio::EngineStats::default();
        let transport = fastcord_media::gateway::TransportStats::default();
        assert!(anomaly_lines(None, &engine, None, &transport).is_empty());
        assert!(anomaly_lines(Some(&engine), &engine, Some(&transport), &transport).is_empty());
        let failing = fastcord_media::gateway::TransportStats {
            rejected_authentication: 40,
            ..transport
        };
        let lines = anomaly_lines(Some(&engine), &engine, Some(&transport), &failing);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("failed decryption") && lines[0].ends_with(": 40"));
    }
}
