//! The camera of a call (SPEC §8.8, issue #84): sending the local camera on the
//! call's own voice connection, receiving other participants' cameras, and the
//! tiles the call view shows.
//!
//! Nothing here runs on the UI thread. The local camera is a capture thread of
//! `fastcord-video::share` (capture, hardware-first H.264, the voice
//! connection's frame sink); every remote camera has its own lazily started,
//! visibility-gated decoding thread (`stream_video`); the UI only reads the
//! latest frame of each tile through [`playback::Session`] leases.
//!
//! The camera is never turned on by anything but the user's click
//! ([`CameraControls::wanted`]), is released the moment it is turned off, and
//! is off again at the start of every call. A tile that is not on screen has
//! no decoder work and its stream is not requested from the server.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use fastcord_media::video::{ReceivedVideo, SendOptions, VideoOptions};
use fastcord_media::{MediaSender, VoiceEvent};
use fastcord_model::Snowflake;
use fastcord_platform::capture::{CaptureError, camera::CameraDevice};
use fastcord_video::DecodedFrame;
use fastcord_video::effect::{Effect, EffectHandle, compose};
use fastcord_video::frame::{ColorDescription, FramePool, Nv12};
use fastcord_video::share::{
    EncoderReport, PreviewSink, ShareConfig, ShareControl, ShareEnd, ShareError, ShareEvent,
    ShareWorker,
};
use parking_lot::Mutex;
use tokio::sync::watch;

use super::VoiceControls;
use super::stream_share::{Route, route};
use super::stream_video::{StatusSink, StreamDecoder, StreamStatus};
use crate::i18n::{tr, trf};
use crate::playback::{self, Session};

/// What a call connection offers: H.264 both ways and AV1 receive, plus the
/// camera layer where this platform can capture one.
pub(super) fn call_options() -> VideoOptions {
    #[cfg(windows)]
    {
        VideoOptions::camera(SendOptions::CAMERA_720P30)
    }
    #[cfg(all(not(windows), any(target_os = "macos", target_os = "linux")))]
    {
        VideoOptions::call_watch()
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        // No H.264 decoder here yet: AV1 receive only, as for a stream.
        VideoOptions {
            codecs: vec![fastcord_media::video::CodecOffer::av1()],
            send: None,
            camera: true,
        }
    }
}

/// Whether this build can capture a camera at all.
pub const fn can_send() -> bool {
    cfg!(windows)
}

/// The layer announced to the server and the capture/encoder settings that
/// match it.
const SEND: SendOptions = SendOptions::CAMERA_720P30;

fn share_config() -> ShareConfig {
    ShareConfig {
        bitrate: SEND.bitrate,
        max_bitrate: SEND.bitrate,
        ..ShareConfig::CAMERA_720P30
    }
}

/// Remote cameras decoded and shown at once.
pub const MAX_TILES: usize = fastcord_media::video::MAX_INTEREST_USERS;
/// The local preview is refreshed at most this often (15 pictures a second):
/// it is a self-view, not the stream.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(66);

/// How the camera is doing, for the voice bar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CameraStatus {
    #[default]
    Off,
    /// The device is being opened or the call has not accepted the video yet.
    Starting,
    /// Capturing, and the call accepted H.264 from this client.
    Live,
    /// The camera ended on its own or could not start; the text names the
    /// cause and the remedy.
    Failed(String),
}

/// What the user wants of the camera and what the call task reports back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CameraControls {
    /// The user turned the camera on. Never set by anything else, and cleared
    /// when the call ends or the camera fails.
    pub wanted: bool,
    /// The saved device (its stable ID); `None` is the first camera.
    pub device: Option<String>,
    /// The background effect (issue #85). Changing it never restarts the
    /// camera or the call.
    pub effect: Effect,
    /// The self-view is mirrored. Only the local preview: what is sent never is.
    pub mirror: bool,
    /// Counts device choices, so choosing the same camera again restarts it.
    pub generation: u64,
    pub status: CameraStatus,
    /// The encoder in use and the picture size, once known.
    pub detail: String,
    /// Counts remote tiles added and removed, so the UI hears of them.
    pub tiles: u64,
}

impl CameraControls {
    pub fn is_live(&self) -> bool {
        self.status == CameraStatus::Live
    }
}

/// The tiles of the call view: the local preview and up to [`MAX_TILES`]
/// remote cameras, each a latest-frame slot. The call task adds and removes
/// them; the UI reads them and says which are drawn (issue #86): a tile that
/// is not drawn is not decoded and, for a camera, not requested.
pub struct CameraTiles {
    changed: watch::Sender<u64>,
    inner: Mutex<Tiles>,
}

#[derive(Default)]
struct Tiles {
    preview: Option<Arc<Session>>,
    remote: Vec<(Snowflake, Arc<Session>)>,
    view: TileVisibility,
    /// Users whose tile was hidden since the call task last looked: a
    /// hide/show pulse the task could not otherwise tell from no change.
    pulsed: HashSet<Snowflake>,
}

/// Which tiles are drawn. A tile is drawn unless the layout excluded it, so a
/// camera that has just appeared shows at once and the next layout pass
/// settles it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TileVisibility {
    /// The window is up and unobstructed.
    pub on_screen: bool,
    /// The layout did not draw the self-view.
    pub preview_excluded: bool,
    /// Cameras the layout did not draw (over the cap, or behind a full view).
    pub excluded: Vec<Snowflake>,
    /// Cameras drawn small: their lowest layer is wanted.
    pub small: Vec<Snowflake>,
}

impl TileVisibility {
    pub fn shows(&self, user: Snowflake) -> bool {
        self.on_screen && !self.excluded.contains(&user)
    }

    fn shows_preview(&self) -> bool {
        self.on_screen && !self.preview_excluded
    }
}

/// What the UI draws.
#[derive(Clone, Default)]
pub struct TilesView {
    pub preview: Option<Arc<Session>>,
    pub remote: Vec<(Snowflake, Arc<Session>)>,
}

impl Default for CameraTiles {
    fn default() -> Self {
        Self {
            changed: watch::channel(0).0,
            inner: Mutex::default(),
        }
    }
}

impl CameraTiles {
    pub fn snapshot(&self) -> TilesView {
        let tiles = self.inner.lock();
        TilesView {
            preview: tiles.preview.clone(),
            remote: tiles.remote.clone(),
        }
    }

    /// The tiles that are drawn (or not): every session's visibility follows,
    /// which suspends the hidden decoders, and the call task re-requests only
    /// what is showing.
    pub fn apply(&self, view: TileVisibility) {
        let mut tiles = self.inner.lock();
        if tiles.view == view {
            return;
        }
        for (user, session) in &tiles.remote {
            let shown = view.shows(*user);
            session.set_visible(shown);
        }
        let hidden: Vec<Snowflake> = tiles
            .remote
            .iter()
            .filter(|(user, _)| tiles.view.shows(*user) && !view.shows(*user))
            .map(|(user, _)| *user)
            .collect();
        tiles.pulsed.extend(hidden);
        if let Some(preview) = &tiles.preview {
            preview.set_visible(view.shows_preview());
        }
        tiles.view = view;
        drop(tiles);
        self.changed.send_modify(|epoch| *epoch += 1);
    }

    pub fn visibility(&self) -> TileVisibility {
        self.inner.lock().view.clone()
    }

    /// The users whose tile was hidden since the last call.
    pub(super) fn take_pulses(&self) -> HashSet<Snowflake> {
        std::mem::take(&mut self.inner.lock().pulsed)
    }

    pub(super) fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn set_preview(&self, session: Option<Arc<Session>>) {
        let mut tiles = self.inner.lock();
        if let Some(session) = &session {
            session.set_visible(tiles.view.shows_preview());
        }
        let old = std::mem::replace(&mut tiles.preview, session);
        drop(tiles);
        if let Some(old) = old {
            old.close();
        }
    }

    /// A slot for `user`'s camera; `None` when [`MAX_TILES`] are already open.
    fn add_remote(&self, user: Snowflake) -> Option<Arc<Session>> {
        let mut tiles = self.inner.lock();
        if let Some((_, session)) = tiles.remote.iter().find(|(id, _)| *id == user) {
            return Some(session.clone());
        }
        if tiles.remote.len() >= MAX_TILES {
            return None;
        }
        let session = Session::new(format!("camera:{}", user.0));
        session.set_visible(tiles.view.shows(user));
        tiles.remote.push((user, session.clone()));
        Some(session)
    }

    fn remove_remote(&self, user: Snowflake) {
        let mut tiles = self.inner.lock();
        if let Some(index) = tiles.remote.iter().position(|(id, _)| *id == user) {
            tiles.remote.remove(index).1.close();
        }
        tiles.pulsed.remove(&user);
    }

    fn clear(&self) {
        let mut tiles = self.inner.lock();
        tiles.pulsed.clear();
        for session in tiles
            .preview
            .take()
            .into_iter()
            .chain(tiles.remote.drain(..).map(|(_, session)| session))
        {
            session.close();
        }
    }
}

/// What the capture thread reports, newest wins.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct CameraProgress {
    capturing: bool,
    size: Option<(u32, u32)>,
    encoder: Option<EncoderReport>,
    /// The first reason the camera ended, as text for the voice bar.
    ended: Option<String>,
}

impl CameraProgress {
    fn apply(&mut self, event: ShareEvent) {
        match event {
            ShareEvent::Capturing => self.capturing = true,
            ShareEvent::SourceSize { width, height } => self.size = Some((width, height)),
            ShareEvent::Encoder(report) => self.encoder = Some(report),
            ShareEvent::Converting(_) | ShareEvent::ZeroCopy(_) => {}
            ShareEvent::Ended(end) => self.fail(end_text(&end)),
        }
    }

    fn fail(&mut self, text: String) {
        self.ended.get_or_insert(text);
    }
}

fn end_text(end: &ShareEnd) -> String {
    match end {
        ShareEnd::Stopped => tr("Camera off.").to_owned(),
        ShareEnd::Capture(error) => capture_text(error),
        ShareEnd::Encoder(detail) => trf("Video encoding failed: {}.", &[&detail]),
        ShareEnd::SinkClosed => {
            tr("The call connection closed, so the camera was turned off.").to_owned()
        }
        ShareEnd::Effect(detail) => trf(
            "The background effect could not run ({}), so the camera was turned off instead of showing your room. Choose No effect to use the camera plainly.",
            &[&detail],
        ),
    }
}

/// The user-facing text for a camera that could not open or stopped.
pub(super) fn capture_text(error: &CaptureError) -> String {
    match error {
        CaptureError::PermissionDenied => tr("Windows blocked camera access. Allow desktop apps to use the camera in Settings, Privacy and security, Camera.").to_owned(),
        CaptureError::DeviceNotFound => tr("The selected camera is not connected. Pick another camera or plug it back in.").to_owned(),
        CaptureError::SourceClosed => tr("The camera was unplugged or taken by another application, so it was turned off.").to_owned(),
        CaptureError::Unsupported(detail) => tr(detail).to_owned(),
        other => trf("The camera stopped: {}.", &[&tr(&other.to_string())]),
    }
}

/// The text beside the Camera button.
pub fn describe(controls: &CameraControls) -> String {
    match &controls.status {
        CameraStatus::Off => String::new(),
        CameraStatus::Starting => tr("Starting your camera…").to_owned(),
        CameraStatus::Live if controls.detail.is_empty() => {
            trf("Camera on{}", &[&effect_suffix(controls.effect)])
        }
        CameraStatus::Live => trf(
            "Camera on · {}{}",
            &[&controls.detail, &effect_suffix(controls.effect)],
        ),
        CameraStatus::Failed(text) => text.clone(),
    }
}

fn effect_suffix(effect: Effect) -> String {
    match effect {
        Effect::Off => String::new(),
        other => format!(" · {}", effect_text(other)),
    }
}

/// The effect's name as shown (the label itself is the English key).
pub fn effect_text(effect: Effect) -> String {
    tr(&effect.label()).to_owned()
}

/// The caption of the self-view: it says it is a preview and whether it is
/// mirrored, because others see the picture the other way round.
pub fn preview_caption(controls: &CameraControls) -> String {
    let side = if controls.mirror {
        tr("mirrored; others see it unmirrored")
    } else {
        tr("as others see it")
    };
    trf(
        "You · preview ({}){}",
        &[&side, &effect_suffix(controls.effect)],
    )
}

/// A camera that is capturing.
struct Running {
    /// Dropping it stops the capture thread and releases the device.
    _worker: ShareWorker,
    control: ShareControl,
    generation: u64,
    effect: EffectHandle,
    mirror: Arc<AtomicBool>,
}

/// One remote camera being decoded.
struct Remote {
    decoder: StreamDecoder,
    /// Whether the decoder was last told its tile is drawn.
    visible: bool,
}

/// All the video of one call, owned by the call task.
pub(super) struct CallVideo {
    media: MediaSender,
    tiles: Arc<CameraTiles>,
    controls: watch::Sender<VoiceControls>,
    decoders: HashMap<Snowflake, Remote>,
    running: Option<Running>,
    /// The server accepted H.264 from us on this connection.
    negotiated: bool,
    progress: Arc<watch::Sender<CameraProgress>>,
    progress_rx: watch::Receiver<CameraProgress>,
}

impl CallVideo {
    pub(super) fn new(
        media: MediaSender,
        tiles: Arc<CameraTiles>,
        controls: watch::Sender<VoiceControls>,
    ) -> Self {
        let (progress, progress_rx) = watch::channel(CameraProgress::default());
        // A call never inherits a camera that was left on.
        controls.send_if_modified(|controls| {
            let before = controls.camera.clone();
            controls.camera.wanted = false;
            controls.camera.status = CameraStatus::Off;
            controls.camera.detail.clear();
            controls.camera != before
        });
        Self {
            media,
            tiles,
            controls,
            decoders: HashMap::new(),
            running: None,
            negotiated: false,
            progress: Arc::new(progress),
            progress_rx,
        }
    }

    /// Wakes when the tiles drawn (or their sizes) changed.
    pub(super) fn changes(&self) -> watch::Receiver<u64> {
        self.tiles.changes()
    }

    pub(super) fn progress(&self) -> watch::Receiver<CameraProgress> {
        self.progress.subscribe()
    }

    fn publish(&self, update: impl FnOnce(&mut CameraControls)) {
        self.controls.send_if_modified(|controls| {
            let before = controls.camera.clone();
            update(&mut controls.camera);
            controls.camera != before
        });
    }

    // ---- sending ----------------------------------------------------------

    /// Brings the camera in line with what the user asked for.
    pub(super) fn sync(&mut self, wanted: &CameraControls) {
        let running = self.running.as_ref().map(|running| running.generation);
        match (running, wanted.wanted) {
            (Some(_), false) => self.stop_camera(CameraStatus::Off),
            // Another device was chosen: release this one before opening it.
            (Some(generation), true) if generation != wanted.generation => {
                self.stop_camera(CameraStatus::Starting);
                self.start_camera(wanted);
            }
            (None, true) => self.start_camera(wanted),
            // The same camera: only the effect or the mirror may have changed,
            // and the capture thread picks them up on its next picture.
            (Some(_), true) => {
                if let Some(running) = &self.running {
                    running.effect.set(wanted.effect);
                    running
                        .mirror
                        .store(wanted.mirror, std::sync::atomic::Ordering::Release);
                }
            }
            (None, false) => {}
        }
    }

    fn start_camera(&mut self, wanted: &CameraControls) {
        if !can_send() {
            self.fail(tr("The camera can only be used on Windows so far.").to_owned());
            return;
        }
        let progress = Arc::clone(&self.progress);
        progress.send_replace(CameraProgress::default());
        let observer = {
            let progress = Arc::clone(&progress);
            Arc::new(move |event| progress.send_modify(|state| state.apply(event)))
        };
        let preview = Session::new("camera:preview".to_owned());
        self.tiles.set_preview(Some(preview.clone()));
        let effect = EffectHandle::new(wanted.effect);
        let mirror = Arc::new(AtomicBool::new(wanted.mirror));
        match ShareWorker::start_camera(
            wanted.device.as_deref(),
            share_config(),
            observer,
            Some(preview_sink(preview, Arc::clone(&mirror))),
            effect.clone(),
        ) {
            Ok(worker) => {
                let control = worker.control();
                control.attach(super::stream_share::sink(self.media.clone()));
                control.set_negotiated(self.negotiated);
                self.media.set_video_sending(true);
                self.running = Some(Running {
                    _worker: worker,
                    control,
                    generation: wanted.generation,
                    effect,
                    mirror,
                });
                self.publish(|camera| {
                    camera.status = CameraStatus::Starting;
                    camera.detail.clear();
                });
            }
            Err(error) => {
                let text = match error {
                    ShareError::Capture(error) => capture_text(&error),
                    other => trf(
                        "Could not start the camera: {}.",
                        &[&tr(&other.to_string())],
                    ),
                };
                self.fail(text);
            }
        }
    }

    /// Releases the camera now: the frame sink goes, the call is told the
    /// layer stopped, the preview tile closes, and the device is freed off the
    /// async worker.
    fn stop_camera(&mut self, status: CameraStatus) {
        let Some(running) = self.running.take() else {
            return;
        };
        self.media.set_video_sending(false);
        self.tiles.set_preview(None);
        self.progress.send_replace(CameraProgress::default());
        tokio::task::spawn_blocking(move || drop(running));
        self.publish(|camera| {
            camera.status = status;
            camera.detail.clear();
        });
    }

    /// The camera cannot run: stop it, turn the request off so nothing retries
    /// by itself, and say why.
    fn fail(&mut self, text: String) {
        self.stop_camera(CameraStatus::Off);
        self.tiles.set_preview(None);
        self.publish(|camera| {
            camera.wanted = false;
            camera.status = CameraStatus::Failed(text);
            camera.detail.clear();
        });
    }

    /// The capture thread reported progress (or ended).
    pub(super) fn on_progress(&mut self) {
        let progress = self.progress_rx.borrow_and_update().clone();
        if self.running.is_none() {
            return;
        }
        if let Some(text) = progress.ended {
            self.fail(text);
            return;
        }
        let detail = match (&progress.encoder, progress.size) {
            (Some(encoder), _) => {
                format!("{}x{} · {}", encoder.width, encoder.height, encoder.backend)
            }
            (None, Some((width, height))) => format!("{width}x{height}"),
            (None, None) => String::new(),
        };
        let live = progress.capturing && self.negotiated;
        let detail = if progress.capturing && !self.negotiated {
            tr("waiting for the call to accept H.264 video from fastcord").to_owned()
        } else {
            detail
        };
        self.publish(|camera| {
            camera.status = if live {
                CameraStatus::Live
            } else {
                CameraStatus::Starting
            };
            camera.detail = detail;
        });
    }

    // ---- the call connection's events -------------------------------------

    /// Video-related events of the call connection.
    pub(super) fn on_event(&mut self, event: &VoiceEvent) {
        match event {
            VoiceEvent::VideoSource {
                user_id,
                ssrc,
                active,
            } => self.remote_source(*user_id, if *active { *ssrc } else { 0 }),
            VoiceEvent::ClientDisconnected(user) => self.remove_remote(*user),
            other => match route(other) {
                Some(Route::Negotiated(accepted)) => {
                    self.negotiated = accepted;
                    if let Some(running) = &self.running {
                        running.control.set_negotiated(accepted);
                    }
                    // A call that does not take H.264 from us (another codec was
                    // chosen) pauses the camera instead of ending it: nothing is
                    // encoded or sent until a session update accepts H.264 again,
                    // and the bar says why.
                    self.on_progress();
                }
                Some(Route::Keyframe) => {
                    if let Some(running) = &self.running {
                        running.control.request_keyframe();
                    }
                }
                Some(Route::Bitrate(bitrate)) => {
                    if let Some(running) = &self.running {
                        running.control.set_bitrate(bitrate);
                    }
                }
                Some(Route::Active(active)) => {
                    if let Some(running) = &self.running {
                        running.control.set_active(active);
                    }
                }
                None => {}
            },
        }
    }

    // ---- receiving --------------------------------------------------------

    fn remote_source(&mut self, user: Snowflake, ssrc: u32) {
        if ssrc == 0 {
            self.remove_remote(user);
            return;
        }
        if !self.decoders.contains_key(&user) {
            let Some(session) = self.tiles.add_remote(user) else {
                return;
            };
            let decoder = StreamDecoder::new(
                self.media.clone(),
                Arc::clone(&session.visible),
                Some(Box::new(session.sink())),
                status_sink(&session),
            );
            decoder.visibility_changed();
            let visible = session.visible.load(std::sync::atomic::Ordering::Acquire);
            self.decoders.insert(user, Remote { decoder, visible });
            // The UI draws a new tile only once it hears of it.
            self.publish(|camera| camera.tiles += 1);
        }
        if let Some(remote) = self.decoders.get(&user) {
            remote.decoder.source_changed(ssrc);
        }
        self.update_interest();
    }

    fn remove_remote(&mut self, user: Snowflake) {
        if self.decoders.remove(&user).is_some() {
            self.tiles.remove_remote(user);
            self.publish(|camera| camera.tiles += 1);
            self.update_interest();
        }
    }

    pub(super) fn on_frame(&mut self, frame: ReceivedVideo) {
        if let Some(remote) = self.decoders.get_mut(&frame.user_id) {
            remote.decoder.push(frame);
        }
    }

    /// The tiles drawn (or their sizes) changed: suspend what left the screen,
    /// resume what came back, and ask the server for exactly what is drawn.
    pub(super) fn on_layout(&mut self) {
        let view = self.tiles.visibility();
        let pulsed = self.tiles.take_pulses();
        for (user, remote) in &mut self.decoders {
            let shown = view.shows(*user);
            // A hide/show pulse that was coalesced still invalidates references.
            if shown != remote.visible || (shown && pulsed.contains(user)) {
                remote.decoder.visibility_notified(remote.visible, shown);
            }
            remote.visible = shown;
        }
        self.update_interest();
    }

    /// Asks the server for the cameras that are drawn, and nothing else; the
    /// small ones at their lowest layer.
    fn update_interest(&self) {
        let view = self.tiles.visibility();
        let mut users: Vec<Snowflake> = self
            .decoders
            .keys()
            .copied()
            .filter(|user| view.shows(*user))
            .collect();
        users.sort_unstable_by_key(|user| user.0);
        let small: Vec<Snowflake> = users
            .iter()
            .copied()
            .filter(|user| view.small.contains(user))
            .collect();
        self.media.set_video_users(&users);
        self.media.set_video_thumbnails(&small);
    }

    /// The call is over: free the camera and every decoder before the voice
    /// connection says goodbye, and leave the next call with the camera off.
    pub(super) async fn finish(mut self) {
        let running = self.running.take();
        self.media.set_video_sending(false);
        self.decoders.clear();
        self.tiles.clear();
        if let Some(running) = running {
            let _ = tokio::task::spawn_blocking(move || drop(running)).await;
        }
        self.publish(|camera| {
            camera.wanted = false;
            camera.status = CameraStatus::Off;
            camera.detail.clear();
        });
    }
}

fn status_sink(session: &Arc<Session>) -> StatusSink {
    let session = session.clone();
    Arc::new(move |status| {
        session.status(match status {
            StreamStatus::Connecting => playback::Status::Connecting,
            StreamStatus::Decoding { backend } => playback::Status::Playing(backend),
            StreamStatus::Suspended => playback::Status::Waiting,
            StreamStatus::Failed(error) => playback::Status::Failed(error),
            StreamStatus::Ended => playback::Status::Ended,
        });
    })
}

/// Hands the capture's own pictures to the preview tile, at most 15 a second
/// and only while the tile is on screen: the self-view is the capture itself,
/// never a decode of what was sent.
fn preview_sink(session: Arc<Session>, mirror: Arc<AtomicBool>) -> PreviewSink {
    let mut pool = FramePool::new(crate::render::frame_format());
    let mut last: Option<Instant> = None;
    let mut mirrored: Vec<u8> = Vec::new();
    Box::new(move |sample| {
        use std::sync::atomic::Ordering;
        if !session.active.load(Ordering::Acquire) || !session.visible.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        if last.is_some_and(|last| now.duration_since(last) < PREVIEW_INTERVAL) {
            return;
        }
        let (luma, chroma) = sample.planes();
        let flipped = mirror.load(Ordering::Acquire).then(|| {
            let planes = compose::Planes {
                width: sample.width() as usize,
                height: sample.height() as usize,
                y: luma,
                y_stride: sample.stride(),
                uv: chroma,
                uv_stride: sample.stride(),
            };
            compose::mirror(&planes, &mut mirrored)
        });
        let image = match flipped {
            Some(stride) => {
                let (luma, chroma) = mirrored.split_at(stride * sample.height() as usize);
                Nv12::new(
                    sample.width(),
                    sample.height(),
                    luma,
                    stride,
                    chroma,
                    stride,
                )
            }
            None => Nv12::new(
                sample.width(),
                sample.height(),
                luma,
                sample.stride(),
                chroma,
                sample.stride(),
            ),
        };
        let Ok(image) = image else {
            return;
        };
        last = Some(now);
        let picture = DecodedFrame {
            image,
            timestamp: sample.timestamp,
            color: ColorDescription::default(),
        };
        if let Ok(frame) = pool.lease(picture) {
            session.push(frame);
        }
    })
}

/// One entry of the camera picker. Equal when the IDs are, never by label.
#[derive(Clone, Debug)]
pub struct CameraChoice {
    /// `None` is "the first camera".
    pub id: Option<String>,
    pub label: String,
    /// Chosen earlier, but not connected now.
    pub missing: bool,
}

impl PartialEq for CameraChoice {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for CameraChoice {}

impl std::fmt::Display for CameraChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)?;
        if self.missing {
            f.write_str(tr(" (not connected)"))?;
        }
        Ok(())
    }
}

/// The picker contents; probed only when the user asks.
#[derive(Clone, Debug, Default)]
pub struct CameraList {
    pub probed: bool,
    pub probing: bool,
    pub devices: Vec<CameraDevice>,
    pub error: Option<String>,
}

impl CameraList {
    /// "First camera", every connected camera, and the saved choice at its
    /// place even when it is not connected (never replaced by label).
    pub fn choices(&self, saved: &crate::audio_devices::CameraSelection) -> Vec<CameraChoice> {
        let mut choices = vec![CameraChoice {
            id: None,
            label: tr("First camera").to_owned(),
            missing: false,
        }];
        choices.extend(self.devices.iter().map(|device| CameraChoice {
            id: Some(device.id.clone()),
            label: device.name.clone(),
            missing: false,
        }));
        if let Some(id) = &saved.id
            && !choices.iter().any(|choice| choice.id.as_ref() == Some(id))
        {
            choices.push(CameraChoice {
                id: Some(id.clone()),
                label: if saved.label.is_empty() {
                    tr("Saved camera").to_owned()
                } else {
                    saved.label.clone()
                },
                missing: self.probed,
            });
        }
        choices
    }

    pub fn selected(&self, saved: &crate::audio_devices::CameraSelection) -> CameraChoice {
        let choices = self.choices(saved);
        choices
            .iter()
            .find(|choice| choice.id == saved.id)
            .cloned()
            .unwrap_or_else(|| choices[0].clone())
    }
}

/// The cameras connected now, listed off the UI thread.
pub async fn list_cameras() -> Result<Vec<CameraDevice>, String> {
    tokio::task::spawn_blocking(fastcord_platform::capture::list_cameras)
        .await
        .map_err(|_| tr("Camera listing stopped unexpectedly.").to_owned())?
        .map_err(|error| capture_text(&error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_camera_is_off_by_default_and_never_turned_on_by_a_failure() {
        let controls = CameraControls::default();
        assert!(!controls.wanted && controls.status == CameraStatus::Off);
        assert_eq!(describe(&controls), "");
        let live = CameraControls {
            status: CameraStatus::Live,
            detail: "1280x720 · fake".to_owned(),
            ..CameraControls::default()
        };
        assert!(live.is_live());
        assert_eq!(describe(&live), "Camera on · 1280x720 · fake");
    }

    #[test]
    fn the_call_offers_a_camera_layer_only_where_a_camera_can_be_captured() {
        let options = call_options();
        options.validate().unwrap();
        assert!(options.camera);
        assert_eq!(options.send.is_some(), can_send());
        assert!(
            options
                .codecs
                .iter()
                .any(|offer| offer.codec == fastcord_media::video::VideoCodec::Av1)
        );
        // Only H.264 is what a call accepts from this client.
        assert_eq!(
            route(&VoiceEvent::VideoNegotiated {
                codec: Some(fastcord_media::video::VideoCodec::H264)
            }),
            Some(Route::Negotiated(true))
        );
        assert_eq!(
            route(&VoiceEvent::VideoNegotiated { codec: None }),
            Some(Route::Negotiated(false))
        );
    }

    #[test]
    fn capture_failures_explain_their_remedy_without_native_noise() {
        assert!(capture_text(&CaptureError::PermissionDenied).contains("Settings"));
        assert!(capture_text(&CaptureError::DeviceNotFound).contains("not connected"));
        assert!(capture_text(&CaptureError::SourceClosed).contains("unplugged"));
        assert_eq!(
            capture_text(&CaptureError::Unsupported("no camera here")),
            "no camera here"
        );
        assert!(
            capture_text(&CaptureError::Native {
                operation: "ReadSample",
                code: -1
            })
            .starts_with("The camera stopped")
        );
        assert_eq!(end_text(&ShareEnd::Stopped), "Camera off.");
        let mut progress = CameraProgress::default();
        progress.apply(ShareEvent::Ended(ShareEnd::Capture(
            CaptureError::SourceClosed,
        )));
        progress.apply(ShareEvent::Ended(ShareEnd::Stopped));
        assert!(
            progress.ended.unwrap().contains("unplugged"),
            "the first cause wins"
        );
    }

    fn call_video() -> (
        CallVideo,
        fastcord_media::VoiceSession,
        watch::Receiver<VoiceControls>,
    ) {
        use fastcord_media::{Generation, VoiceCredentials, connect_with_video};
        use fastcord_model::VoiceToken;
        let credentials = VoiceCredentials {
            generation: Generation(1),
            server_id: Snowflake(1),
            channel_id: Snowflake(2),
            user_id: Snowflake(3),
            session_id: "test-session".into(),
            token: VoiceToken::new("test-token".into()),
            endpoint: "127.0.0.1:9".into(),
            dave_group_id: None,
        };
        let session = connect_with_video(credentials, call_options()).unwrap();
        let (controls, observer) = watch::channel(VoiceControls::default());
        let video = CallVideo::new(session.media(), Arc::default(), controls);
        (video, session, observer)
    }

    fn source(user: u64, ssrc: u32, active: bool) -> VoiceEvent {
        VoiceEvent::VideoSource {
            user_id: Snowflake(user),
            ssrc,
            active,
        }
    }

    #[tokio::test]
    async fn remote_cameras_get_bounded_tiles_that_follow_announcements_and_departures() {
        let (mut video, session, _observer) = call_video();
        let tiles = Arc::clone(&video.tiles);
        // A user who streams nothing (every participant announces itself) gets no tile.
        video.on_event(&source(10, 0, false));
        assert!(tiles.snapshot().remote.is_empty());
        for user in 1..=MAX_TILES as u64 + 2 {
            video.on_event(&source(user, 5_000 + user as u32, true));
        }
        assert_eq!(
            tiles.snapshot().remote.len(),
            MAX_TILES,
            "at most six decoded"
        );
        assert_eq!(video.decoders.len(), MAX_TILES);
        // Hidden tiles want nothing from the server (checked by the driver
        // tests); showing them is a visibility change, not new tiles.
        tiles.apply(TileVisibility {
            on_screen: true,
            ..TileVisibility::default()
        });
        video.on_layout();
        assert_eq!(video.decoders.len(), MAX_TILES);
        // The streamer stops, another leaves the call.
        video.on_event(&source(1, 0, false));
        video.on_event(&VoiceEvent::ClientDisconnected(Snowflake(2)));
        assert_eq!(tiles.snapshot().remote.len(), MAX_TILES - 2);
        // A slot is free again for a user who was refused.
        video.on_event(&source(99, 7_000, true));
        assert_eq!(tiles.snapshot().remote.len(), MAX_TILES - 1);
        video.finish().await;
        assert!(tiles.snapshot().remote.is_empty());
        session.leave().await;
    }

    #[tokio::test]
    async fn a_camera_that_cannot_open_is_reported_and_stays_off() {
        let (mut video, session, observer) = call_video();
        // Never on by itself: a fresh call is off whatever the controls held.
        assert!(!observer.borrow().camera.wanted);
        let wanted = CameraControls {
            wanted: true,
            device: Some("no-such-camera".to_owned()),
            generation: 1,
            ..CameraControls::default()
        };
        video.sync(&wanted);
        let mut progress = video.progress();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !matches!(observer.borrow().camera.status, CameraStatus::Failed(_)) {
            tokio::time::timeout_at(deadline, progress.changed())
                .await
                .expect("the failure is reported")
                .unwrap();
            video.on_progress();
        }
        let camera = observer.borrow().camera.clone();
        assert!(!camera.wanted, "a failure never retries by itself");
        assert!(video.running.is_none());
        let CameraStatus::Failed(text) = &camera.status else {
            unreachable!()
        };
        // Not connected where Media Foundation exists, unsupported elsewhere.
        assert!(
            text.contains("not connected") || text.contains("Windows"),
            "{text}"
        );
        video.finish().await;
        assert_eq!(observer.borrow().camera.status, CameraStatus::Off);
        session.leave().await;
    }

    #[test]
    fn the_preview_says_it_is_a_preview_and_names_the_effect() {
        use fastcord_video::effect::Preset;
        let mut controls = CameraControls {
            mirror: true,
            ..CameraControls::default()
        };
        let mirrored = preview_caption(&controls);
        assert!(mirrored.contains("preview") && mirrored.contains("mirrored"));
        controls.mirror = false;
        controls.effect = Effect::Preset(Preset::Dusk);
        let plain = preview_caption(&controls);
        assert!(
            plain.contains("as others see it") && plain.contains("Dusk"),
            "{plain}"
        );
        controls.status = CameraStatus::Live;
        assert_eq!(describe(&controls), "Camera on · Background: Dusk");
        controls.effect = Effect::Off;
        assert_eq!(describe(&controls), "Camera on");
        // A failed effect stops the camera with an explanation, never silently.
        assert!(end_text(&ShareEnd::Effect("x".into())).contains("instead of showing your room"));
    }

    #[test]
    fn the_picker_keeps_a_missing_saved_camera_and_never_matches_by_label() {
        use crate::audio_devices::CameraSelection;
        let mut list = CameraList::default();
        let saved = CameraSelection {
            id: Some("usb-1".to_owned()),
            label: "Desk camera".to_owned(),
            ..CameraSelection::default()
        };
        // Before the first probe the saved camera is shown without a verdict.
        let choices = list.choices(&saved);
        assert_eq!(choices.len(), 2);
        assert!(!choices[1].missing);
        assert_eq!(list.selected(&saved).label, "Desk camera");
        list.probed = true;
        list.devices = vec![CameraDevice::bounded("usb-2", "Desk camera").unwrap()];
        let choices = list.choices(&saved);
        assert_eq!(choices.len(), 3, "same label, different device");
        assert!(choices[2].missing);
        assert_eq!(choices[2].to_string(), "Desk camera (not connected)");
        assert_eq!(list.selected(&saved).id.as_deref(), Some("usb-1"));
        list.devices = vec![CameraDevice::bounded("usb-1", "Desk camera").unwrap()];
        assert_eq!(list.choices(&saved).len(), 2);
        assert_eq!(
            list.selected(&CameraSelection::default()).id,
            None,
            "the first camera"
        );
    }

    #[test]
    fn tiles_are_bounded_follow_visibility_and_close_with_the_call() {
        use std::sync::atomic::Ordering;
        let tiles = CameraTiles::default();
        let first = tiles.add_remote(Snowflake(1)).unwrap();
        assert!(!first.visible.load(Ordering::Acquire));
        assert!(Arc::ptr_eq(
            &first,
            &tiles.add_remote(Snowflake(1)).unwrap()
        ));
        for user in 2..=MAX_TILES as u64 {
            assert!(tiles.add_remote(Snowflake(user)).is_some());
        }
        assert!(
            tiles.add_remote(Snowflake(99)).is_none(),
            "at most six cameras"
        );
        let mut changes = tiles.changes();
        let on_screen = TileVisibility {
            on_screen: true,
            ..TileVisibility::default()
        };
        tiles.apply(on_screen.clone());
        assert!(changes.has_changed().unwrap());
        changes.borrow_and_update();
        assert!(first.visible.load(Ordering::Acquire));
        // The same view again changes nothing and wakes nobody.
        tiles.apply(on_screen.clone());
        assert!(!changes.has_changed().unwrap());
        // A tile added while drawn is drawn at once; the layout can exclude it
        // (the user is then neither decoded nor requested).
        tiles.remove_remote(Snowflake(1));
        let late = tiles.add_remote(Snowflake(99)).unwrap();
        assert!(late.visible.load(Ordering::Acquire));
        tiles.apply(TileVisibility {
            excluded: vec![Snowflake(99), Snowflake(2)],
            ..on_screen.clone()
        });
        assert!(!late.visible.load(Ordering::Acquire));
        let hidden = tiles.take_pulses();
        assert!(hidden.contains(&Snowflake(99)) && hidden.contains(&Snowflake(2)));
        assert!(tiles.take_pulses().is_empty());
        // A hidden window hides everything.
        tiles.apply(TileVisibility::default());
        assert!(
            tiles
                .snapshot()
                .remote
                .iter()
                .all(|(_, session)| !session.visible.load(Ordering::Acquire))
        );
        assert_eq!(tiles.snapshot().remote.len(), MAX_TILES);
        tiles.clear();
        assert!(tiles.snapshot().remote.is_empty());
        assert!(!late.active.load(Ordering::Acquire));
    }
}
