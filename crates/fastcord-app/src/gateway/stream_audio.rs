use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use fastcord_media::video::VideoOptions;
use fastcord_media::{
    CloseReason, StreamCredentials, VoiceEvent, VoiceMessage, VoiceSession, VoiceStatus,
    connect_with_video,
};
use tokio::sync::watch;

use super::VoiceControls;
use super::stream_speaker::Playback;
use super::stream_video::{Sink, StatusSink, StreamDecoder, StreamStatus};

/// A watch connection owns its transport and DAVE identity, but never a
/// microphone. The session task routes borrowed-frame and status callbacks to
/// the lazily allocated decoding worker, and the stream's Opus packets to an
/// output-only speaker (`stream_speaker`) that follows the call's output
/// device and deafen state, started by the first packet.
///
/// `visible` gates submissions and in-flight output immediately; `visibility`
/// wakes the session even when no packets arrive so native references are
/// released while hidden. The gate owner updates the atomic before publishing
/// actual changes to the watch; coalesced hide/show pulses also reset decoding.
pub(super) async fn run(
    connection: (fastcord_net::NetworkContext, StreamCredentials),
    visible: Arc<AtomicBool>,
    visibility: watch::Receiver<bool>,
    sink: Sink,
    status: StatusSink,
    controls: watch::Receiver<VoiceControls>,
    presentation: Arc<crate::playback::Session>,
) {
    let (network, credentials) = connection;
    (status)(StreamStatus::Connecting);
    let session =
        match connect_with_video(network, credentials.voice_credentials(), watch_options()) {
            Ok(session) => session,
            Err(error) => {
                (status)(StreamStatus::Failed(format!(
                    "Could not start stream session: {error}"
                )));
                return;
            }
        };
    pump(
        session,
        visible,
        visibility,
        sink,
        status,
        controls,
        presentation,
    )
    .await;
}

fn watch_options() -> VideoOptions {
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    {
        VideoOptions::watch()
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        // All watch codecs need a real decoder backend; this platform has none
        // yet, so it offers AV1 receive only (which still depacketizes).
        VideoOptions {
            codecs: vec![fastcord_media::video::CodecOffer::av1()],
            send: None,
        }
    }
}

/// Returns true for a terminal session status, preserving concrete failures.
fn publish_session_end(voice: VoiceStatus, status: &StatusSink) -> bool {
    match voice {
        VoiceStatus::Closed(CloseReason::Left) => {
            (status)(StreamStatus::Ended);
            true
        }
        VoiceStatus::Closed(reason) => {
            (status)(StreamStatus::Failed(format!(
                "Stream session failed: {reason}"
            )));
            true
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn pump(
    mut session: VoiceSession,
    visible: Arc<AtomicBool>,
    mut visibility: watch::Receiver<bool>,
    sink: Sink,
    status: StatusSink,
    mut controls: watch::Receiver<VoiceControls>,
    presentation: Arc<crate::playback::Session>,
) {
    let mut playback = Playback::default();
    let mut audio = presentation.watch_audio();
    let mut controls_open = true;
    let mut decoder = StreamDecoder::new(session.media(), visible, Some(sink), Arc::clone(&status));
    let mut connection = session.watch_status();
    let mut observed_visible = *visibility.borrow_and_update();
    let mut last_video = Instant::now();
    let mut refreshes = 0;
    // Session-health checks only while video is visible, never a redraw or
    // playback clock. A silent freeze becomes actionable after bounded repair.
    let mut health = tokio::time::interval(Duration::from_secs(5));
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // A hidden stream costs nothing: the server forwards its video only while
    // it is drawn (and again, from a keyframe, once it is).
    session.media().set_video_wanted(observed_visible);
    decoder.visibility_changed();
    #[cfg(debug_assertions)]
    let mut report = tokio::time::interval(std::time::Duration::from_secs(10));
    let mut terminal = false;
    loop {
        #[cfg(debug_assertions)]
        let debug_tick = report.tick();
        #[cfg(not(debug_assertions))]
        let debug_tick = std::future::pending::<()>();
        tokio::select! {
            // Gate/source/session controls precede ready media, preventing a
            // continuous media stream from starving a suspension notification.
            biased;
            changed = visibility.changed() => {
                if changed.is_err() {
                    (status)(StreamStatus::Ended);
                    terminal = true;
                    break;
                }
                let announced = *visibility.borrow_and_update();
                session.media().set_video_wanted(announced);
                decoder.visibility_notified(observed_visible, announced);
                observed_visible = announced;
                last_video = Instant::now();
                refreshes = 0;
            }
            changed = controls.changed(), if controls_open => {
                if changed.is_err() {
                    controls_open = false;
                } else {
                    playback.apply(&controls.borrow_and_update(), *audio.borrow());
                }
            }
            changed = audio.changed() => {
                if changed.is_ok() {
                    playback.apply(&controls.borrow(), *audio.borrow_and_update());
                }
            }
            changed = connection.changed() => {
                if changed.is_err() {
                    break;
                }
                let voice = *connection.borrow_and_update();
                if publish_session_end(voice, &status) {
                    terminal = true;
                    break;
                }
            }
            _ = health.tick(), if observed_visible => {
                if last_video.elapsed() >= Duration::from_secs(5) && refreshes < 3 {
                    refreshes += 1;
                    decoder.refresh();
                    if refreshes == 3 {
                        (status)(StreamStatus::Failed(
                            "No video is arriving. Retry watching; your call and screen share are still connected.".into(),
                        ));
                    }
                }
            }
            _ = debug_tick => {
                #[cfg(debug_assertions)]
                {
                    eprintln!("stream session: {:?}", session.stats());
                    eprintln!("stream decoding: {:?}", decoder.stats());
                }
            }
            message = session.next_message() => match message {
                Some(VoiceMessage::Video(frame)) => {
                    last_video = Instant::now();
                    refreshes = 0;
                    decoder.push(frame);
                },
                Some(VoiceMessage::Audio(packet)) => {
                    let report = playback.push(packet, &controls.borrow(), *audio.borrow());
                    presentation.audio_status(report);
                },
                Some(VoiceMessage::Event(VoiceEvent::VideoSource { ssrc, active, .. })) => {
                    decoder.source_changed(if active { ssrc } else { 0 });
                }
                Some(_) => {}
                None => break,
            },
        }
    }
    #[cfg(debug_assertions)]
    {
        eprintln!("stream session ended: {:?}", session.stats());
        eprintln!("stream decoding ended: {:?}", decoder.stats());
    }
    // Schedule native teardown before the transport's close-handshake await.
    drop(decoder);
    drop(playback);
    if !terminal && !publish_session_end(session.status(), &status) {
        (status)(StreamStatus::Failed(
            "Stream session stopped unexpectedly".into(),
        ));
    }
    session.leave().await;
}

#[cfg(test)]
mod tests {
    use fastcord_media::video::VideoCodec;
    use parking_lot::Mutex;

    use super::*;

    #[test]
    fn a_watcher_offers_only_real_receive_paths_and_never_sends() {
        let options = watch_options();
        assert!(options.send.is_none());
        assert!(
            options
                .codecs
                .iter()
                .all(|offer| !offer.encode && offer.decode)
        );
        assert!(
            options
                .codecs
                .iter()
                .any(|offer| offer.codec == VideoCodec::Av1)
        );
        assert_eq!(
            options
                .codecs
                .iter()
                .any(|offer| offer.codec == VideoCodec::H264),
            cfg!(any(windows, target_os = "macos", target_os = "linux"))
        );
        options.validate().unwrap();
    }

    #[test]
    fn session_failures_and_normal_leave_have_distinct_ui_statuses() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&observed);
        let status: StatusSink = Arc::new(move |status| capture.lock().push(status));
        assert!(!publish_session_end(VoiceStatus::Connecting, &status));
        assert!(observed.lock().is_empty());
        assert!(publish_session_end(
            VoiceStatus::Closed(CloseReason::DaveUnavailable),
            &status
        ));
        assert_eq!(
            observed.lock().last(),
            Some(&StreamStatus::Failed(
                "Stream session failed: voice could not establish DAVE encryption".into()
            ))
        );
        assert!(publish_session_end(
            VoiceStatus::Closed(CloseReason::Left),
            &status
        ));
        assert_eq!(observed.lock().last(), Some(&StreamStatus::Ended));
    }
}
