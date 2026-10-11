use fastcord_media::video::VideoOptions;
use fastcord_media::{StreamCredentials, VoiceSession, connect_with_video};

/// A stream voice session has its own websocket, UDP socket, nonce state, SSRC,
/// and DAVE identity. Watchers intentionally never construct an AudioEngine.
///
/// The session offers H.264 decode so the server forwards the stream; the
/// transport requests, repairs, reassembles and DAVE-opens its frames, and this
/// task discards them. Decoding and rendering are a later milestone (M28), and
/// sending (M29) will use `VideoOptions::share` for the owner's session.
pub(super) async fn run(credentials: StreamCredentials) {
    let Ok(session) = connect_with_video(credentials.voice_credentials(), VideoOptions::watch())
    else {
        return;
    };
    drain(session).await;
}

#[cfg(not(debug_assertions))]
async fn drain(mut session: VoiceSession) {
    while session.discard_next().await {}
    session.leave().await;
}

/// Debug builds also print the transport counters every ten seconds (and when the
/// session ends) so the live acceptance in docs/TESTING.md can read them. They
/// hold no keys, tokens, or media; the task may be cancelled at any await, which
/// is why they are not printed only at the end.
#[cfg(debug_assertions)]
async fn drain(mut session: VoiceSession) {
    let mut report = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        tokio::select! {
            more = session.discard_next() => {
                if !more {
                    break;
                }
            }
            _ = report.tick() => eprintln!("stream session: {:?}", session.stats()),
        }
    }
    eprintln!("stream session ended: {:?}", session.stats());
    session.leave().await;
}
