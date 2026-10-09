use fastcord_media::{StreamCredentials, VoiceSession, connect};

/// A stream voice session has its own websocket, UDP socket, nonce state, SSRC,
/// and DAVE identity. Watchers intentionally never construct an AudioEngine.
pub(super) async fn run(credentials: StreamCredentials) {
    let session = connect(credentials.voice_credentials());
    drain(session).await;
}

async fn drain(mut session: VoiceSession) {
    while session.discard_next().await {}
    session.leave().await;
}
