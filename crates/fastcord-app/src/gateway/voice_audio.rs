use fastcord_audio::{
    AudioEngine, AudioError, EncodedPacket, EngineChannels, EngineConfig, RemoteFrame,
};
use fastcord_media::{VoiceCredentials, VoiceStatus, connect};
use tokio::sync::{mpsc, oneshot, watch};

/// What the audio side of the current call is doing, for the voice controls to show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VoiceAudioState {
    /// No call, or the call ended normally.
    #[default]
    Idle,
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
) {
    run_with(credentials, cancel_rx, state, || {
        AudioEngine::start(EngineConfig::default())
    })
    .await;
}

async fn run_with(
    credentials: VoiceCredentials,
    mut cancel_rx: oneshot::Receiver<()>,
    state: watch::Sender<VoiceAudioState>,
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
    let mut events = channels.events;
    let mut status = session.watch_status();

    loop {
        tokio::select! {
            frame = receive_capture(&mut captured) => {
                let Some(frame) = frame else {
                    captured = None;
                    continue;
                };
                let _ = media.send_opus(frame.packet.as_bytes().to_vec());
            }
            packet = session.next_audio() => {
                let Some(packet) = packet else { break };
                let Ok(packet_bytes) = EncodedPacket::from_slice(&packet.payload) else { continue };
                let Some(output) = remote.as_ref() else { break };
                let _ = output.try_send(RemoteFrame {
                    ssrc: packet.ssrc,
                    sequence: packet.sequence,
                    timestamp: packet.timestamp,
                    packet: packet_bytes,
                });
            }
            event = events.recv() => {
                if event.is_none()
                    || matches!(event, Some(fastcord_audio::EngineEvent::StreamFailed {
                        direction: fastcord_audio::Direction::Output,
                        ..
                    }))
                {
                    break;
                }
            }
            changed = status.changed() => {
                if changed.is_err() || matches!(*status.borrow_and_update(), VoiceStatus::Closed(_)) { break; }
            }
            _ = &mut cancel_rx => break,
        }
    }

    // Release the audio devices before the next call can open them, then say goodbye.
    drop(engine);
    let _ = media.end_speech();
    session.leave().await;
    state.send_replace(VoiceAudioState::Idle);
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
        }
    }

    /// An engine that cannot start must leave the call and say so, not linger half-joined.
    #[tokio::test]
    async fn an_engine_that_cannot_start_ends_the_call_and_reports_it() {
        let (state, observer) = watch::channel(VoiceAudioState::Idle);
        let (_cancel, cancel_rx) = oneshot::channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_with(credentials(), cancel_rx, state, || {
                Err(AudioError::NothingToRun)
            }),
        )
        .await
        .expect("a failed engine start must end the task promptly");
        assert_eq!(*observer.borrow(), VoiceAudioState::EngineFailed);
    }
}
