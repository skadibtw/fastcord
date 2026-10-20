//! Owner-side Go Live (SPEC §8.1–8.3, milestone 29): the share session the
//! worker drives, the stream RTC connection task that turns the voice
//! connection's feedback into capture/encoder control, and the user-facing
//! text.
//!
//! The order is capture first: choosing a source starts the OS capture (so a
//! refused permission or a vanished window fails before anything exists on
//! Discord), only then is the stream created (opcode 18), and the stream's own
//! RTC connection is opened when Discord answers with STREAM_CREATE and
//! STREAM_SERVER_UPDATE. The parent voice connection is never touched: the
//! share has its own worker thread, WebSocket, UDP socket, SSRCs and DAVE
//! group.

use std::sync::Arc;

use fastcord_media::video::{SendOptions, VideoCodec, VideoFrame, VideoOptions};
use fastcord_media::{
    CloseReason, MediaSendError, MediaSender, StreamCredentials, VoiceEvent, VoiceMessage,
    VoiceStatus, connect_with_video,
};
use fastcord_model::{Snowflake, StreamCreateRequest, StreamDeleteReason, StreamKey, StreamType};
use fastcord_platform::capture::{CaptureConfig, CaptureError, CaptureSource, ConversionPath};
use fastcord_video::share::{
    EncoderReport, FrameSink, ShareConfig, ShareControl, ShareEnd, ShareEvent, ShareWorker,
    SinkOutcome,
};
use tokio::sync::watch;

/// What the owner offers the server: the SPEC §8.2 baseline layer.
const SEND: SendOptions = SendOptions::SCREEN_720P30;

/// The capture/encoder settings that match the layer announced in Identify.
pub(super) fn share_config() -> ShareConfig {
    ShareConfig {
        capture: CaptureConfig {
            width: u32::from(SEND.width),
            height: u32::from(SEND.height),
            frame_rate: u32::from(SEND.framerate),
        },
        bitrate: SEND.bitrate,
        max_bitrate: SEND.bitrate,
        ..ShareConfig::SCREEN_SHARE_720P30
    }
}

/// What the screen shows about a share.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareView {
    /// A share exists (starting or live): Stop sharing is offered.
    pub active: bool,
    /// Progress, or why the last share ended.
    pub text: String,
}

impl ShareView {
    pub fn idle() -> Self {
        Self::default()
    }

    pub(super) fn active(text: String) -> Self {
        Self { active: true, text }
    }

    pub(super) fn failed(text: String) -> Self {
        Self {
            active: false,
            text,
        }
    }
}

/// The source picker's state; it holds the list only while it is open.
#[derive(Clone, Debug, Default)]
pub enum SharePicker {
    #[default]
    Closed,
    Loading,
    Listed(Vec<CaptureSource>),
    Failed(String),
}

impl SharePicker {
    pub fn is_open(&self) -> bool {
        !matches!(self, Self::Closed)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Connection {
    #[default]
    Idle,
    Connecting,
    Live,
}

/// Everything the capture thread and the stream task report about one share,
/// as a newest-wins snapshot the worker reads when it changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ShareProgress {
    pub(super) capturing: bool,
    pub(super) source: Option<(u32, u32)>,
    pub(super) converter: Option<ConversionPath>,
    pub(super) encoder: Option<EncoderReport>,
    pub(super) connection: Connection,
    /// The first reason the share ended, as text for the screen.
    pub(super) ended: Option<String>,
}

impl ShareProgress {
    pub(super) fn apply(&mut self, event: ShareEvent) {
        match event {
            ShareEvent::Capturing => self.capturing = true,
            ShareEvent::Converting(path) => self.converter = Some(path),
            ShareEvent::SourceSize { width, height } => self.source = Some((width, height)),
            ShareEvent::Encoder(report) => self.encoder = Some(report),
            ShareEvent::Ended(end) => self.fail(end_text(&end)),
        }
    }

    pub(super) fn fail(&mut self, text: String) {
        self.ended.get_or_insert(text);
    }
}

fn end_text(end: &ShareEnd) -> String {
    match end {
        ShareEnd::Stopped => "Sharing stopped.".to_owned(),
        ShareEnd::Capture(CaptureError::SourceClosed) => {
            "The shared window or screen was closed, so sharing stopped.".to_owned()
        }
        ShareEnd::Capture(CaptureError::PermissionDenied) => {
            "Windows refused to capture this source (a privacy setting, a policy, or a protected window). Choose another window or screen.".to_owned()
        }
        ShareEnd::Capture(CaptureError::Unsupported(detail)) => (*detail).to_owned(),
        ShareEnd::Capture(error) => format!("Screen capture stopped: {error}."),
        ShareEnd::Encoder(detail) => format!("Video encoding failed: {detail}."),
        ShareEnd::SinkClosed => "The stream connection closed.".to_owned(),
        // Only a camera has an effect stage.
        ShareEnd::Effect(detail) => format!("The picture effect failed: {detail}."),
    }
}

/// Why Discord ended the stream, for the screen.
pub(super) fn delete_text(reason: StreamDeleteReason, unavailable: bool) -> String {
    if unavailable {
        return "Discord's stream server is unavailable; sharing stopped.".to_owned();
    }
    match reason {
        StreamDeleteReason::UserRequested => "Sharing stopped.".to_owned(),
        StreamDeleteReason::StreamEnded => "Discord ended the stream.".to_owned(),
        StreamDeleteReason::StreamFull => "Discord says the stream is full.".to_owned(),
        StreamDeleteReason::Unauthorized => {
            "Discord refused the stream: you may not stream in this channel.".to_owned()
        }
        StreamDeleteReason::SafetyGuildRateLimited => {
            "Discord rate limited streaming in this server. Try again later.".to_owned()
        }
        StreamDeleteReason::ParseFailed | StreamDeleteReason::InvalidChannel => {
            "Discord rejected the stream request.".to_owned()
        }
    }
}

pub(super) fn describe(title: &str, progress: &ShareProgress) -> String {
    if let Some(text) = &progress.ended {
        return text.clone();
    }
    if !progress.capturing {
        return format!("Starting capture of “{title}”…");
    }
    match progress.connection {
        Connection::Live => {
            let mut text = format!("Sharing “{title}”");
            if let Some(encoder) = &progress.encoder {
                text.push_str(&format!(
                    " · {}x{} · {}",
                    encoder.width, encoder.height, encoder.backend
                ));
            }
            text
        }
        Connection::Idle | Connection::Connecting => {
            format!("Starting your stream of “{title}”…")
        }
    }
}

/// The stream key Discord will give this account's stream in `request`'s
/// location.
pub(super) fn owner_key(request: &StreamCreateRequest, owner: Snowflake) -> Option<StreamKey> {
    match (request.stream_type, request.guild_id) {
        (StreamType::Guild, Some(guild_id)) => Some(StreamKey::Guild {
            guild_id,
            channel_id: request.channel_id,
            owner_id: owner,
        }),
        (StreamType::Call, None) => Some(StreamKey::Call {
            channel_id: request.channel_id,
            owner_id: owner,
        }),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// The OS capture is starting; no stream exists yet.
    StartingCapture,
    /// Opcode 18 was sent; waiting for the stream's RTC connection.
    CreatingStream,
    Streaming,
}

/// What the worker must do after a progress change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// The capture is running: create the stream.
    CreateStream,
    /// The share is over for this reason; stop everything.
    End(String),
    Refresh(String),
}

/// One share being started, run and ended by the worker.
pub(super) struct ShareSession {
    /// `None` only in tests, which have no capture thread.
    pub(super) worker: Option<ShareWorker>,
    pub(super) progress: watch::Receiver<ShareProgress>,
    pub(super) sender: Arc<watch::Sender<ShareProgress>>,
    pub(super) title: String,
    pub(super) request: StreamCreateRequest,
    pub(super) key: StreamKey,
    pub(super) phase: Phase,
}

impl ShareSession {
    pub(super) fn on_progress(&mut self, progress: &ShareProgress) -> Step {
        if let Some(text) = &progress.ended {
            return Step::End(text.clone());
        }
        if progress.capturing && self.phase == Phase::StartingCapture {
            self.phase = Phase::CreatingStream;
            return Step::CreateStream;
        }
        if progress.connection == Connection::Live && self.phase == Phase::CreatingStream {
            self.phase = Phase::Streaming;
        }
        Step::Refresh(describe(&self.title, progress))
    }

    /// Whether opcode 18 was sent, so ending needs opcode 19.
    pub(super) fn stream_created(&self) -> bool {
        self.phase != Phase::StartingCapture
    }
}

/// A started capture, and the channel its progress is reported through.
type Started = (
    ShareWorker,
    Arc<watch::Sender<ShareProgress>>,
    watch::Receiver<ShareProgress>,
);

/// Starts the capture of `source`, reporting into a new progress channel.
pub(super) fn start_worker(source: &CaptureSource) -> Result<Started, String> {
    let (sender, receiver) = watch::channel(ShareProgress::default());
    let sender = Arc::new(sender);
    let observer = {
        let sender = Arc::clone(&sender);
        Arc::new(move |event| sender.send_modify(|progress| progress.apply(event)))
    };
    ShareWorker::start(source.id, share_config(), observer)
        .map(|worker| (worker, sender, receiver))
        .map_err(|error| match error {
            fastcord_video::share::ShareError::Capture(error) => {
                end_text(&ShareEnd::Capture(error))
            }
            other => format!("Could not start sharing: {other}."),
        })
}

/// What the voice connection's events ask of the share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    Negotiated(bool),
    Keyframe,
    Bitrate(u32),
    Active(bool),
}

pub(super) fn route(event: &VoiceEvent) -> Option<Route> {
    match event {
        VoiceEvent::VideoNegotiated { codec } => {
            Some(Route::Negotiated(*codec == Some(VideoCodec::H264)))
        }
        VoiceEvent::KeyframeRequested => Some(Route::Keyframe),
        VoiceEvent::VideoBitrate(bitrate) => Some(Route::Bitrate(*bitrate)),
        VoiceEvent::VideoActive(active) => Some(Route::Active(*active)),
        _ => None,
    }
}

pub(super) fn outcome(result: Result<(), MediaSendError>) -> SinkOutcome {
    match result {
        Ok(()) => SinkOutcome::Sent,
        Err(MediaSendError::Closed) => SinkOutcome::Closed,
        // A full queue or an unusable unit: the unit is gone either way and
        // the stream must resume at a keyframe.
        Err(MediaSendError::Full | MediaSendError::InvalidFrame) => SinkOutcome::Dropped,
    }
}

pub(super) fn sink(media: MediaSender) -> FrameSink {
    Box::new(move |unit| {
        outcome(media.send_video(VideoFrame {
            data: unit.data,
            timestamp: unit.timestamp,
        }))
    })
}

fn session_end_text(status: VoiceStatus) -> Option<String> {
    match status {
        VoiceStatus::Closed(CloseReason::Left) => Some("The stream session ended.".to_owned()),
        VoiceStatus::Closed(reason) => Some(format!("The stream connection failed: {reason}.")),
        _ => None,
    }
}

/// The stream RTC connection of a share this account owns. It offers video
/// with one encoder layer, so the server assigns an SSRC and negotiates H.264;
/// the share worker's pictures flow out through the connection's frame queue,
/// and its feedback (keyframe requests, bitrate, pause) flows back in.
pub(super) async fn run(
    credentials: StreamCredentials,
    control: ShareControl,
    progress: Arc<watch::Sender<ShareProgress>>,
) {
    control.set_negotiated(false);
    progress.send_modify(|progress| progress.connection = Connection::Connecting);
    let mut session =
        match connect_with_video(credentials.voice_credentials(), VideoOptions::share(SEND)) {
            Ok(session) => session,
            Err(error) => {
                progress.send_modify(|progress| {
                    progress.fail(format!("Could not start the stream session: {error}."));
                });
                return;
            }
        };
    // Until H.264 is negotiated the share only captures; attaching resets the
    // stream to a keyframe.
    control.attach(sink(session.media()));
    let mut connection = session.watch_status();
    loop {
        tokio::select! {
            biased;
            changed = connection.changed() => {
                if changed.is_err() {
                    break;
                }
                let status = *connection.borrow_and_update();
                if let Some(text) = session_end_text(status) {
                    progress.send_modify(|progress| progress.fail(text));
                    break;
                }
            }
            message = session.next_message() => match message {
                Some(VoiceMessage::Event(event)) => match route(&event) {
                    Some(Route::Negotiated(true)) => {
                        control.set_negotiated(true);
                        progress.send_modify(|progress| progress.connection = Connection::Live);
                    }
                    Some(Route::Negotiated(false)) => {
                        progress.send_modify(|progress| {
                            progress.fail(
                                "Discord did not accept H.264 video from fastcord for this stream."
                                    .to_owned(),
                            );
                        });
                        break;
                    }
                    Some(Route::Keyframe) => control.request_keyframe(),
                    Some(Route::Bitrate(bitrate)) => control.set_bitrate(bitrate),
                    Some(Route::Active(active)) => control.set_active(active),
                    None => {}
                },
                // The owner asked for no video from anyone.
                Some(_) => {}
                None => break,
            },
        }
    }
    #[cfg(debug_assertions)]
    {
        eprintln!("share session ended: {:?}", session.stats());
        eprintln!("share encoding ended: {:?}", control.stats());
    }
    if let Some(text) = session_end_text(session.status()) {
        progress.send_modify(|progress| progress.fail(text));
    } else {
        progress.send_modify(|progress| {
            progress.fail("The stream session stopped unexpectedly.".to_owned());
        });
    }
    session.leave().await;
}

use fastcord_discord::gateway::Gateway;

impl super::Worker {
    /// Starts a share: the capture first, then (once it runs) the stream.
    pub(super) fn start_share(&mut self, source: CaptureSource, request: StreamCreateRequest) {
        if self.share.is_some() {
            // The UI offers no second share; keep the running one.
            return;
        }
        let Some(user) = self.stream_intents.user_id else {
            self.publish_share(ShareView::failed(
                "Still connecting to Discord; try again in a moment.".to_owned(),
            ));
            return;
        };
        let Some(key) = owner_key(&request, user) else {
            self.publish_share(ShareView::failed(
                "This voice channel cannot be shared to.".to_owned(),
            ));
            return;
        };
        match start_worker(&source) {
            Ok((worker, sender, progress)) => {
                self.share = Some(ShareSession {
                    worker: Some(worker),
                    progress,
                    sender,
                    title: source.title.clone(),
                    request,
                    key,
                    phase: Phase::StartingCapture,
                });
                self.publish_share(ShareView::active(format!(
                    "Starting capture of “{}”…",
                    source.title
                )));
            }
            Err(text) => self.publish_share(ShareView::failed(text)),
        }
    }

    /// Applies a change of the share's progress: create the stream once the
    /// capture runs, end the share when it ended, otherwise refresh the text.
    pub(super) fn share_progress(&mut self, gateway: &Gateway) {
        let Some(share) = self.share.as_mut() else {
            return;
        };
        let progress = share.progress.borrow_and_update().clone();
        match share.on_progress(&progress) {
            Step::CreateStream => {
                let request = share.request.clone();
                let text = describe(&share.title, &progress);
                self.stream_command(gateway, super::StreamIntent::Create(request));
                self.publish_share(ShareView::active(text));
            }
            Step::End(text) => self.end_share(Some(gateway), ShareView::failed(text), true),
            Step::Refresh(text) => self.publish_share(ShareView::active(text)),
        }
    }

    /// Ends the share: stops the capture, retires the stream's RTC task, and
    /// deletes the stream (opcode 19) when it exists and a Gateway is at hand.
    pub(super) fn end_share(&mut self, gateway: Option<&Gateway>, view: ShareView, delete: bool) {
        let Some(share) = self.share.take() else {
            return;
        };
        match gateway {
            Some(gateway) if delete && share.stream_created() => {
                self.stream_command(gateway, super::StreamIntent::Unwatch(share.key.to_wire()));
            }
            _ => {
                self.stream_intents.retire(&share.key);
                self.stop_stream_key(&share.key.to_wire());
            }
        }
        if let Some(worker) = share.worker {
            // Dropping stops the capture thread and waits for it to release the
            // OS objects: not on the async worker.
            tokio::task::spawn_blocking(move || drop(worker));
        }
        self.publish_share(view);
    }

    /// Discord deleted the stream with this key.
    pub(super) fn share_stream_deleted(
        &mut self,
        key: &StreamKey,
        reason: StreamDeleteReason,
        unavailable: bool,
    ) {
        if self.share.as_ref().is_some_and(|share| &share.key == key) {
            let view = if reason == StreamDeleteReason::UserRequested && !unavailable {
                ShareView::idle()
            } else {
                ShareView::failed(delete_text(reason, unavailable))
            };
            self.end_share(None, view, false);
        }
    }

    fn publish_share(&mut self, view: ShareView) {
        self.bridge.share_view.send_replace(view);
        self.share_dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use fastcord_model::StreamType;
    use fastcord_platform::capture::{SourceId, SourceKind};

    use super::*;

    fn request() -> StreamCreateRequest {
        StreamCreateRequest::guild(Snowflake(41), Snowflake(127))
    }

    fn session() -> ShareSession {
        let (sender, receiver) = watch::channel(ShareProgress::default());
        ShareSession {
            worker: None,
            progress: receiver,
            sender: Arc::new(sender),
            title: "Editor".to_owned(),
            request: request(),
            key: owner_key(&request(), Snowflake(9)).unwrap(),
            phase: Phase::StartingCapture,
        }
    }

    #[test]
    fn the_share_settings_match_the_layer_announced_to_the_server() {
        let config = share_config();
        assert_eq!(
            (
                config.capture.width,
                config.capture.height,
                config.capture.frame_rate
            ),
            (1280, 720, 30)
        );
        assert_eq!((config.bitrate, config.max_bitrate), (2_500_000, 2_500_000));
        VideoOptions::share(SEND).validate().unwrap();
    }

    #[test]
    fn capture_comes_first_then_the_stream_is_created_exactly_once() {
        let mut session = session();
        let mut progress = ShareProgress::default();
        assert_eq!(
            session.on_progress(&progress),
            Step::Refresh("Starting capture of “Editor”…".to_owned())
        );
        assert!(!session.stream_created());
        progress.apply(ShareEvent::Capturing);
        assert_eq!(session.on_progress(&progress), Step::CreateStream);
        assert!(session.stream_created());
        // Later progress never asks for a second stream.
        progress.apply(ShareEvent::SourceSize {
            width: 800,
            height: 600,
        });
        assert_eq!(
            session.on_progress(&progress),
            Step::Refresh("Starting your stream of “Editor”…".to_owned())
        );
        progress.connection = Connection::Live;
        progress.apply(ShareEvent::Encoder(EncoderReport {
            backend: "Media Foundation hardware: encoder".to_owned(),
            width: 800,
            height: 600,
            bitrate: 2_500_000,
            skipped: Vec::new(),
            fell_back: false,
        }));
        assert_eq!(
            session.on_progress(&progress),
            Step::Refresh(
                "Sharing “Editor” · 800x600 · Media Foundation hardware: encoder".to_owned()
            )
        );
        assert_eq!(session.phase, Phase::Streaming);
    }

    #[test]
    fn a_failure_before_the_stream_exists_needs_no_delete_and_the_first_cause_wins() {
        let mut session = session();
        let mut progress = ShareProgress::default();
        progress.apply(ShareEvent::Ended(ShareEnd::Capture(
            CaptureError::PermissionDenied,
        )));
        progress.apply(ShareEvent::Ended(ShareEnd::Stopped));
        progress.fail("later".to_owned());
        let Step::End(text) = session.on_progress(&progress) else {
            panic!("the share must end");
        };
        assert!(text.starts_with("Windows refused to capture"), "{text}");
        assert!(
            !session.stream_created(),
            "no opcode 19 for a stream never created"
        );
    }

    #[test]
    fn endings_say_what_the_user_can_do_about_them() {
        assert!(end_text(&ShareEnd::Capture(CaptureError::SourceClosed)).contains("closed"));
        assert!(
            end_text(&ShareEnd::Capture(CaptureError::SourceTooLarge {
                width: 7680,
                height: 4320,
            }))
            .contains("7680x4320")
        );
        assert_eq!(
            end_text(&ShareEnd::Capture(CaptureError::Unsupported(
                "screen capture is only implemented for Windows so far"
            ))),
            "screen capture is only implemented for Windows so far"
        );
        assert_eq!(
            end_text(&ShareEnd::Encoder("no video codec is available".to_owned())),
            "Video encoding failed: no video codec is available."
        );
        assert_eq!(
            end_text(&ShareEnd::SinkClosed),
            "The stream connection closed."
        );
        assert!(delete_text(StreamDeleteReason::Unauthorized, false).contains("may not stream"));
        assert!(delete_text(StreamDeleteReason::StreamEnded, true).contains("unavailable"));
        assert_eq!(
            delete_text(StreamDeleteReason::UserRequested, false),
            "Sharing stopped."
        );
    }

    #[test]
    fn stream_keys_name_the_owner_in_the_requested_location() {
        let owner = Snowflake(9);
        assert_eq!(
            owner_key(&request(), owner).map(|key| key.to_wire()),
            Some("guild:41:127:9".to_owned())
        );
        assert_eq!(
            owner_key(&StreamCreateRequest::call(Snowflake(55)), owner).map(|key| key.to_wire()),
            Some("call:55:9".to_owned())
        );
        let mut broken = request();
        broken.guild_id = None;
        assert_eq!(owner_key(&broken, owner), None);
        broken.stream_type = StreamType::Call;
        assert!(owner_key(&broken, owner).is_some());
    }

    #[test]
    fn voice_events_become_exactly_the_share_controls_they_ask_for() {
        assert_eq!(
            route(&VoiceEvent::VideoNegotiated {
                codec: Some(VideoCodec::H264)
            }),
            Some(Route::Negotiated(true))
        );
        assert_eq!(
            route(&VoiceEvent::VideoNegotiated { codec: None }),
            Some(Route::Negotiated(false))
        );
        assert_eq!(
            route(&VoiceEvent::VideoNegotiated {
                codec: Some(VideoCodec::Av1)
            }),
            Some(Route::Negotiated(false)),
            "only H.264 can be sent"
        );
        assert_eq!(route(&VoiceEvent::KeyframeRequested), Some(Route::Keyframe));
        assert_eq!(
            route(&VoiceEvent::VideoBitrate(900_000)),
            Some(Route::Bitrate(900_000))
        );
        assert_eq!(
            route(&VoiceEvent::VideoActive(false)),
            Some(Route::Active(false))
        );
        assert_eq!(
            route(&VoiceEvent::ClientDisconnected(Snowflake(1))),
            None,
            "participant events are not the share's business"
        );
    }

    #[test]
    fn the_transports_answers_map_to_the_encoders_outcomes() {
        assert_eq!(outcome(Ok(())), SinkOutcome::Sent);
        assert_eq!(outcome(Err(MediaSendError::Full)), SinkOutcome::Dropped);
        assert_eq!(
            outcome(Err(MediaSendError::InvalidFrame)),
            SinkOutcome::Dropped
        );
        assert_eq!(outcome(Err(MediaSendError::Closed)), SinkOutcome::Closed);
    }

    #[test]
    fn a_closed_stream_session_explains_itself_and_a_live_one_does_not() {
        assert_eq!(session_end_text(VoiceStatus::Connecting), None);
        assert_eq!(
            session_end_text(VoiceStatus::Closed(CloseReason::Left)),
            Some("The stream session ended.".to_owned())
        );
        assert_eq!(
            session_end_text(VoiceStatus::Closed(CloseReason::DaveUnavailable)),
            Some(
                "The stream connection failed: voice could not establish DAVE encryption."
                    .to_owned()
            )
        );
    }

    #[test]
    fn the_bridge_queues_share_commands_in_order_and_keeps_window_titles_out_of_logs() {
        use super::super::{Command, NavigationBridge};
        let bridge = NavigationBridge::new();
        let mut commands = bridge.take_commands().unwrap();
        let source = CaptureSource {
            id: SourceId::from_raw(SourceKind::Window, 7),
            title: "Secret plans".to_owned(),
            width: 800,
            height: 600,
            primary: false,
        };
        assert_eq!(bridge.share_view(), ShareView::idle());
        assert!(bridge.start_share(source, request()));
        assert_eq!(
            bridge.share_view(),
            ShareView::active("Starting capture of “Secret plans”…".to_owned())
        );
        assert!(bridge.stop_share());
        let Ok(started) = commands.try_recv() else {
            panic!("the start command was queued");
        };
        assert!(!format!("{started:?}").contains("Secret"), "{started:?}");
        assert!(matches!(started, Command::StartShare { .. }));
        assert!(matches!(commands.try_recv(), Ok(Command::StopShare)));
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn a_share_goes_to_the_voice_channel_the_account_joined() {
        use fastcord_model::VoiceStateRequest;
        let bridge = super::super::NavigationBridge::new();
        assert_eq!(bridge.voice_target(), None);
        bridge.request_voice_state(VoiceStateRequest::join(
            Some(Snowflake(41)),
            Snowflake(127),
            false,
            false,
        ));
        assert_eq!(
            bridge.voice_target(),
            Some((Some(Snowflake(41)), Snowflake(127)))
        );
        bridge.request_voice_state(VoiceStateRequest::leave(Some(Snowflake(41))));
        assert_eq!(bridge.voice_target(), None);
    }

    #[test]
    fn the_picker_is_open_whenever_it_is_not_closed() {
        assert!(!SharePicker::Closed.is_open());
        assert!(SharePicker::Loading.is_open());
        let source = CaptureSource {
            id: SourceId::from_raw(SourceKind::Display, 1),
            title: "Screen 1 - 2560x1440 (primary)".to_owned(),
            width: 2560,
            height: 1440,
            primary: true,
        };
        assert!(SharePicker::Listed(vec![source]).is_open());
        assert!(SharePicker::Failed("x".to_owned()).is_open());
        assert_eq!(ShareView::idle(), ShareView::default());
        assert!(ShareView::active("a".into()).active);
        assert!(!ShareView::failed("b".into()).active);
    }
}
