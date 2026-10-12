use fastcord_media::video::VideoOptions;
use fastcord_media::{StreamCredentials, VoiceMessage, VoiceSession, connect_with_video};

use super::stream_video::StreamDecoder;

/// A stream voice session has its own websocket, UDP socket, nonce state, SSRC,
/// and DAVE identity. Watchers intentionally never construct an AudioEngine.
///
/// The session offers H.264 and AV1 decode so the server forwards the stream;
/// the transport requests, repairs, reassembles and DAVE-opens its frames. AV1
/// frames go to a dedicated, bounded, lazily started decoding thread
/// (`stream_video`, milestone 26) that decodes with dav1d and, until rendering
/// exists (M28), discards the pictures. H.264 frames are discarded here until the
/// platform decoders join them in M28. Sending (M29) will use
/// `VideoOptions::share` for the owner's session.
pub(super) async fn run(credentials: StreamCredentials) {
    let Ok(session) = connect_with_video(credentials.voice_credentials(), VideoOptions::watch())
    else {
        return;
    };
    pump(session).await;
}

#[cfg(not(debug_assertions))]
async fn pump(mut session: VoiceSession) {
    let mut decoder = StreamDecoder::new(session.media());
    while let Some(message) = session.next_message().await {
        if let VoiceMessage::Video(frame) = message {
            decoder.push(frame);
        }
    }
    session.leave().await;
}

/// Debug builds also print the transport and decoding counters every ten seconds
/// (and when the session ends) so the live acceptance in docs/TESTING.md can read
/// them. They hold no keys, tokens, or media; the task may be cancelled at any
/// await, which is why they are not printed only at the end.
#[cfg(debug_assertions)]
async fn pump(mut session: VoiceSession) {
    let mut decoder = StreamDecoder::new(session.media());
    let mut report = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        tokio::select! {
            message = session.next_message() => match message {
                Some(VoiceMessage::Video(frame)) => decoder.push(frame),
                Some(_) => {}
                None => break,
            },
            _ = report.tick() => {
                eprintln!("stream session: {:?}", session.stats());
                eprintln!("stream decoding: {:?}", decoder.stats());
            }
        }
    }
    eprintln!("stream session ended: {:?}", session.stats());
    eprintln!("stream decoding ended: {:?}", decoder.stats());
    session.leave().await;
}
