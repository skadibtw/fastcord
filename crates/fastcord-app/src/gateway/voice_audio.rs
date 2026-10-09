use super::{VoiceControls, VoiceParticipant};
use fastcord_audio::{
    AudioEngine, AudioError, EncodedPacket, EngineChannels, EngineConfig, EngineControl,
    RemoteFrame,
};
use fastcord_media::{VoiceCredentials, VoiceEvent, VoiceMessage, VoiceStatus, connect};
use std::collections::HashMap;
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
    credentials: VoiceCredentials,
    cancel_rx: oneshot::Receiver<()>,
    state: watch::Sender<VoiceAudioState>,
    controls: watch::Receiver<VoiceControls>,
    controls_tx: watch::Sender<VoiceControls>,
) {
    run_with(credentials, cancel_rx, state, controls, controls_tx, || {
        AudioEngine::start(EngineConfig::default())
    })
    .await;
}

/// At most this many participants are tracked, published, and sent gains per call.
const MAX_PARTICIPANTS: usize = 128;
/// Remote speakers whose gain was sent to the engine; bounds the dedupe map.
const MAX_SENT_GAINS: usize = 256;
/// How long after the last voice activity a participant stops being shown as speaking.
const SPEAKING_RELEASE: Duration = Duration::from_millis(250);
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
}

impl EngineSync {
    fn new(controls: mpsc::Sender<EngineControl>) -> Self {
        Self {
            controls,
            capture: None,
            output: None,
            gains: HashMap::new(),
            retry: false,
        }
    }

    /// Sends whatever differs from what the engine already has.
    fn apply(&mut self, controls: &VoiceControls) {
        self.retry = false;
        let capture = controls.capture_enabled();
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

async fn run_with(
    credentials: VoiceCredentials,
    mut cancel_rx: oneshot::Receiver<()>,
    state: watch::Sender<VoiceAudioState>,
    mut controls: watch::Receiver<VoiceControls>,
    controls_tx: watch::Sender<VoiceControls>,
    start_engine: impl FnOnce() -> EngineStart + Send + 'static,
) {
    let mut session = connect(credentials);
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
    let mut captured = channels.captured;
    let remote = channels.remote_audio;
    let mut sync = EngineSync::new(channels.controls);
    let mut events = channels.events;
    let mut status = session.watch_status();
    let mut capture_enabled = {
        let initial = controls.borrow_and_update();
        sync.apply(&initial);
        initial.capture_enabled()
    };
    let mut retry_at = sync.retry_deadline();
    let mut engine_failed = false;
    let mut participants = HashMap::<Snowflake, VoiceParticipant>::new();
    let mut release_at = HashMap::<Snowflake, Instant>::new();
    loop {
        let release_deadline = release_at.values().copied().min();
        tokio::select! {
            frame = receive_capture(&mut captured) => {
                let Some(frame) = frame else {
                    captured = None;
                    continue;
                };
                if capture_enabled {
                    let _ = media.send_opus(frame.packet.as_bytes().to_vec());
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
                        let Some(output) = remote.as_ref() else { break };
                        let _ = output.try_send(RemoteFrame {
                            ssrc: packet.ssrc,
                            sequence: packet.sequence,
                            timestamp: packet.timestamp,
                            packet: packet_bytes,
                        });
                    }
                    Some(VoiceMessage::Event(event)) => {
                        apply_voice_event(event, &mut participants, &mut release_at);
                        publish_participants(&controls_tx, &participants);
                    }
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
                sync.apply(&updated);
                retry_at = sync.retry_deadline();
                let enabled = updated.capture_enabled();
                drop(updated);
                if capture_enabled && !enabled {
                    let _ = media.end_speech();
                }
                capture_enabled = enabled;
            }
            _ = wait_for_release(retry_at) => {
                sync.apply(&controls.borrow());
                retry_at = sync.retry_deadline();
            }
            event = events.recv() => {
                if event.is_none()
                    || matches!(event, Some(fastcord_audio::EngineEvent::StreamFailed {
                        direction: fastcord_audio::Direction::Output,
                        ..
                    }))
                {
                    engine_failed = true;
                    break;
                }
            }
            changed = status.changed() => {
                if changed.is_err() || matches!(*status.borrow_and_update(), VoiceStatus::Closed(_)) { break; }
            }
            _ = &mut cancel_rx => break,
        }
    }
    participants.clear();
    release_at.clear();
    publish_participants(&controls_tx, &participants);

    // Release the audio devices before the next call can open them, then say goodbye.
    drop(engine);
    let _ = media.end_speech();
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
        VoiceEvent::UdpLatency(_) | VoiceEvent::ReceptionReport { .. } => {}
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
                credentials(),
                cancel_rx,
                state,
                controls,
                controls_tx,
                || Err(AudioError::NothingToRun),
            ),
        )
        .await
        .expect("a failed engine start must end the task promptly");
        assert_eq!(*observer.borrow(), VoiceAudioState::EngineFailed);
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
        sync.apply(&controls);
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
        sync.apply(&controls);
        assert!(!sync.retry);
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineControl::RemoteGain { ssrc: 40, gain }) if gain == 1.0
        ));
        // Nothing changed: nothing is sent, however many times the controls are applied
        // or packets of that SSRC arrive.
        sync.apply(&controls);
        sync.gain(40, 100);
        assert!(rx.try_recv().is_err());
        // A deafen toggle that finds the queue full is retried instead of lost.
        sync.gain(41, 100);
        sync.gain(42, 100);
        controls.deafened = true;
        sync.apply(&controls);
        assert!(sync.retry);
        while rx.try_recv().is_ok() {}
        sync.apply(&controls);
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
}
