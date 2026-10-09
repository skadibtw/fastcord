//! UI orchestration of attachments: inline previews, the larger in-app view,
//! and explicit download/open. Every fetch, decode, and file operation runs in
//! an abortable task owned by the account panel, so leaving the channel, the
//! message, or the account cancels it; nothing here touches the disk or the
//! network on the UI thread.
use fastcord_model::{Attachment, Snowflake};
use iced::task::Handle;
use iced::widget::{button, column, container, image, opaque, row, text};
use iced::{ContentFit, Element, Length, Task};

use crate::Message;
use crate::attachments::{
    AttachmentCache, AttachmentError, AttachmentKey, DecodedImage, UrlRefresh,
};
use crate::gateway::{AttachmentAction, GatewayPanel};
use crate::login::Session;
use crate::timeline::{self, AttachmentPreview, AttachmentView};

/// Display box of the larger view, in pixels; the image is decoded to at most
/// this size and then fitted to the window.
pub const VIEWER_PREVIEW: (u32, u32) = (1600, 1200);
/// Inline thumbnails kept as textures at once (the oldest is dropped first).
const READY_THUMBNAILS: usize = 4;
/// Previews and actions in flight per account; the cache itself separately
/// limits fetches to four and decodes to two.
const WORKER_SLOTS: usize = 4;

/// Refreshes expired CDN URLs through the authenticated REST client.
pub struct RestRefresh(pub fastcord_discord::RestClient);

impl UrlRefresh for RestRefresh {
    async fn refresh(&self, url: String) -> Result<String, AttachmentError> {
        let refreshed = self
            .0
            .refresh_attachment_urls(std::slice::from_ref(&url))
            .await
            .map_err(|_| AttachmentError::RefreshFailed)?;
        let only = refreshed.len() == 1;
        refreshed
            .into_iter()
            .find(|item| only || item.original == url)
            .map(|item| item.refreshed)
            .ok_or(AttachmentError::RefreshFailed)
    }
}

/// Where a finished decode goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Inline,
    Viewer,
}

/// A decoded image on its way to the UI; sharing, not copying, its pixels.
#[derive(Clone)]
pub struct LoadedPreview(DecodedImage);

impl std::fmt::Debug for LoadedPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoadedPreview")
    }
}

impl LoadedPreview {
    fn into_view(self) -> AttachmentView {
        let DecodedImage {
            width,
            height,
            rgba,
        } = self.0;
        AttachmentView::Ready(AttachmentPreview {
            handle: image::Handle::from_rgba(width, height, rgba),
            width,
            height,
        })
    }
}

/// How an explicit download or open ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    Saved,
    Opened,
    /// The user closed the save dialog.
    Cancelled,
    Failed(&'static str),
}

/// The larger view of one attachment image. Dropping it cancels its load.
pub struct AttachmentViewer {
    pub key: AttachmentKey,
    attachment: Attachment,
    state: AttachmentView,
    _worker: Handle,
}

#[cfg(test)]
impl AttachmentViewer {
    pub fn for_test(key: AttachmentKey, attachment: Attachment) -> Self {
        let (_, worker) = Task::<()>::none().abortable();
        Self {
            key,
            attachment,
            state: AttachmentView::Loading,
            _worker: worker.abort_on_drop(),
        }
    }
}

impl std::fmt::Debug for AttachmentViewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentViewer")
            .field("key", &self.key)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

fn find_attachment(
    gateway: &GatewayPanel,
    channel: Snowflake,
    message: Snowflake,
    attachment: Snowflake,
) -> Option<Attachment> {
    if gateway.timeline.channel_id != Some(channel) {
        return None;
    }
    // The modal view outlives its row's place in the built window.
    if let Some(viewer) = gateway
        .attachment_viewer
        .as_ref()
        .filter(|viewer| viewer.key.message == message && viewer.key.attachment == attachment)
    {
        return Some(viewer.attachment.clone());
    }
    gateway
        .timeline
        .rows
        .iter()
        .find(|row| row.message.id == message)?
        .message
        .attachments
        .iter()
        .find(|item| item.id == attachment)
        .cloned()
}

fn failure_reason(error: &AttachmentError) -> &'static str {
    match error {
        AttachmentError::TooLarge => "This image is too large to preview here.",
        AttachmentError::UnsupportedImage | AttachmentError::InvalidImage => {
            "This image could not be decoded."
        }
        AttachmentError::Network
        | AttachmentError::Http(_)
        | AttachmentError::RefreshFailed
        | AttachmentError::InvalidUrl => "The image could not be fetched.",
        AttachmentError::CacheUnavailable => "The local attachment cache is unavailable.",
        AttachmentError::WorkerStopped => "The image preview was cancelled.",
    }
}

fn action_failure(error: &AttachmentError) -> &'static str {
    match error {
        AttachmentError::TooLarge => "This file is larger than the in-app download limit (64 MiB).",
        AttachmentError::WorkerStopped => "The attachment action was cancelled.",
        AttachmentError::CacheUnavailable => "The local attachment cache is unavailable.",
        _ => "The attachment could not be fetched.",
    }
}

fn busy(gateway: &GatewayPanel) -> bool {
    gateway.attachment_workers.len() + gateway.attachment_action_workers.len() >= WORKER_SLOTS
}

/// Starts the inline preview of an attachment image (the "Load preview" click).
pub fn load_preview(
    gateway: &mut GatewayPanel,
    session: &Session,
    channel: Snowflake,
    message: Snowflake,
    attachment: Snowflake,
) -> Task<Message> {
    if gateway.timeline.channel_id != Some(channel) || busy(gateway) {
        return Task::none();
    }
    let key = AttachmentKey {
        account: session.user.id,
        channel,
        message,
        attachment,
    };
    if gateway.attachment_workers.contains_key(&key) {
        return Task::none();
    }
    let Some(item) = find_attachment(gateway, channel, message, attachment) else {
        return Task::none();
    };
    let Some(cache) = gateway.attachments.clone() else {
        gateway.attachment_views.insert(
            key,
            AttachmentView::Failed("The local attachment cache is unavailable."),
        );
        return Task::none();
    };
    gateway.attachment_ready_order.retain(|other| *other != key);
    gateway
        .attachment_views
        .insert(key, AttachmentView::Loading);
    let (task, worker) = decode_task(
        cache,
        session,
        gateway.id,
        key,
        item,
        Target::Inline,
        timeline::INLINE_PREVIEW,
    );
    gateway.attachment_workers.insert(key, worker);
    task
}

/// Opens the larger view (a thumbnail was clicked) and decodes the image to it.
pub fn open_viewer(
    gateway: &mut GatewayPanel,
    session: &Session,
    channel: Snowflake,
    message: Snowflake,
    attachment: Snowflake,
) -> Task<Message> {
    let Some(item) = find_attachment(gateway, channel, message, attachment) else {
        return Task::none();
    };
    let key = AttachmentKey {
        account: session.user.id,
        channel,
        message,
        attachment,
    };
    let Some(cache) = gateway.attachments.clone() else {
        return Task::none();
    };
    let shown = item.clone();
    let (task, worker) = decode_task(
        cache,
        session,
        gateway.id,
        key,
        item,
        Target::Viewer,
        VIEWER_PREVIEW,
    );
    // Replacing the viewer drops the previous one and aborts its decode.
    gateway.attachment_viewer = Some(AttachmentViewer {
        key,
        attachment: shown,
        state: AttachmentView::Loading,
        _worker: worker,
    });
    task
}

fn decode_task(
    cache: AttachmentCache,
    session: &Session,
    generation: u64,
    key: AttachmentKey,
    item: Attachment,
    target: Target,
    (width, height): (u32, u32),
) -> (Task<Message>, Handle) {
    let refresh = RestRefresh(session.client.clone());
    let (task, worker) = Task::perform(
        async move {
            cache
                .thumbnail(&refresh, key, &item, width, height)
                .await
                .map(LoadedPreview)
                .map_err(|error| failure_reason(&error))
        },
        move |result| Message::AttachmentLoaded(generation, key, target, result),
    )
    .abortable();
    (task, worker.abort_on_drop())
}

/// Applies a finished decode if its attachment is still shown.
pub fn loaded(
    gateway: &mut GatewayPanel,
    key: AttachmentKey,
    target: Target,
    result: Result<LoadedPreview, &'static str>,
) {
    let view = match result {
        Ok(preview) => preview.into_view(),
        Err(reason) => AttachmentView::Failed(reason),
    };
    match target {
        Target::Inline => {
            gateway.attachment_workers.remove(&key);
            if !gateway.is_attachment_loaded(&key) {
                gateway.attachment_views.remove(&key);
                return;
            }
            gateway.attachment_ready_order.retain(|other| *other != key);
            if matches!(view, AttachmentView::Ready(_)) {
                gateway.attachment_ready_order.push_back(key);
            }
            gateway.attachment_views.insert(key, view);
            while gateway.attachment_ready_order.len() > READY_THUMBNAILS {
                if let Some(oldest) = gateway.attachment_ready_order.pop_front() {
                    gateway.attachment_views.remove(&oldest);
                }
            }
        }
        Target::Viewer => {
            if let Some(viewer) = gateway
                .attachment_viewer
                .as_mut()
                .filter(|viewer| viewer.key == key)
            {
                viewer.state = view;
            }
        }
    }
}

/// Starts Download (native save dialog, then a copy of the cached file) or
/// Open (the OS default application) for one attachment.
pub fn start_action(
    gateway: &mut GatewayPanel,
    session: &Session,
    channel: Snowflake,
    message: Snowflake,
    attachment: Snowflake,
    action: AttachmentAction,
) -> Task<Message> {
    if gateway.timeline.channel_id != Some(channel) {
        return Task::none();
    }
    if busy(gateway) {
        gateway.attachment_notice = Some("Attachment work is busy; try again shortly.");
        return Task::none();
    }
    let key = AttachmentKey {
        account: session.user.id,
        channel,
        message,
        attachment,
    };
    if gateway
        .attachment_action_workers
        .contains_key(&(key, action))
    {
        gateway.attachment_notice = Some("This attachment action is already in progress.");
        return Task::none();
    }
    let Some(cache) = gateway.attachments.clone() else {
        gateway.attachment_notice = Some("The local attachment cache is unavailable.");
        return Task::none();
    };
    let Some(item) = find_attachment(gateway, channel, message, attachment) else {
        return Task::none();
    };
    let refresh = RestRefresh(session.client.clone());
    let generation = gateway.id;
    gateway.attachment_notice = None;
    let (task, worker) = Task::perform(
        run_action(cache, refresh, key, item, action),
        move |outcome| Message::AttachmentActionFinished(generation, key, action, outcome),
    )
    .abortable();
    gateway
        .attachment_action_workers
        .insert((key, action), worker.abort_on_drop());
    task
}

async fn run_action(
    cache: AttachmentCache,
    refresh: RestRefresh,
    key: AttachmentKey,
    item: Attachment,
    action: AttachmentAction,
) -> ActionOutcome {
    let filename = safe_attachment_filename(&item.filename, item.content_type.as_deref());
    match action {
        AttachmentAction::Download => {
            let Some(destination) = rfd::AsyncFileDialog::new()
                .set_file_name(&filename)
                .save_file()
                .await
            else {
                return ActionOutcome::Cancelled;
            };
            let source = match cache.local_file(&refresh, key, &filename, &item.url).await {
                Ok(path) => path,
                Err(error) => return ActionOutcome::Failed(action_failure(&error)),
            };
            let destination = destination.path().to_owned();
            match tokio::task::spawn_blocking(move || std::fs::copy(source, destination)).await {
                Ok(Ok(_)) => ActionOutcome::Saved,
                _ => ActionOutcome::Failed("The file could not be saved there."),
            }
        }
        AttachmentAction::Open => {
            let path = match cache.local_file(&refresh, key, &filename, &item.url).await {
                Ok(path) => path,
                Err(error) => return ActionOutcome::Failed(action_failure(&error)),
            };
            match tokio::task::spawn_blocking(move || fastcord_platform::open_local_file(&path))
                .await
            {
                Ok(Ok(())) => ActionOutcome::Opened,
                _ => ActionOutcome::Failed(
                    "This file could not be opened (programs and scripts are never launched from here). Use Download.",
                ),
            }
        }
    }
}

pub fn finished(gateway: &mut GatewayPanel, outcome: ActionOutcome) {
    gateway.attachment_notice = match outcome {
        ActionOutcome::Saved => Some("Attachment saved."),
        ActionOutcome::Opened => Some("Opened attachment."),
        ActionOutcome::Cancelled => None,
        ActionOutcome::Failed(reason) => Some(reason),
    };
}

/// The larger view, drawn over the account screen so the timeline underneath
/// keeps its widget state (and with it the scroll position).
pub fn viewer_view(viewer: &AttachmentViewer) -> Element<'_, Message> {
    let event = |make: fn(Snowflake, Snowflake, Snowflake) -> timeline::Event| {
        Message::Timeline(make(
            viewer.key.channel,
            viewer.key.message,
            viewer.key.attachment,
        ))
    };
    let header = row![
        text(&viewer.attachment.filename)
            .size(16)
            .width(Length::Fill),
        button("Download").on_press(event(|channel_id, message_id, attachment_id| {
            timeline::Event::DownloadAttachment {
                channel_id,
                message_id,
                attachment_id,
            }
        })),
        button("Open").on_press(event(|channel_id, message_id, attachment_id| {
            timeline::Event::OpenAttachment {
                channel_id,
                message_id,
                attachment_id,
            }
        })),
        button("Close").on_press(Message::CloseViewer),
    ]
    .spacing(8);
    let body: Element<'_, Message> = match &viewer.state {
        AttachmentView::Ready(preview) => image(preview.handle.clone())
            .content_fit(ContentFit::Contain)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        AttachmentView::Loading => text("Loading image…").into(),
        AttachmentView::Failed(reason) => text(*reason).into(),
    };
    opaque(
        container(column![header, container(body).center(Length::Fill)].spacing(12))
            .padding(16)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(container::dark),
    )
}

/// A file name that is safe to offer a native save dialog: no path parts, no
/// characters Windows forbids, a bounded length, and an extension when the
/// content type implies one.
pub fn safe_attachment_filename(filename: &str, content_type: Option<&str>) -> String {
    let basename = filename.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut safe: String = basename
        .chars()
        .filter(|character| {
            !character.is_control() && !matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
        .take(180)
        .collect();
    if safe.is_empty() || safe == "." || safe == ".." {
        safe = "attachment".to_owned();
    }
    if std::path::Path::new(&safe).extension().is_none()
        && let Some(extension) = content_type.and_then(mime_extension)
    {
        safe.push('.');
        safe.push_str(extension);
    }
    safe
}

fn mime_extension(content_type: &str) -> Option<&'static str> {
    let mime = content_type.split(';').next().unwrap_or_default().trim();
    [
        ("image/png", "png"),
        ("image/jpeg", "jpg"),
        ("image/gif", "gif"),
        ("image/webp", "webp"),
        ("image/bmp", "bmp"),
        ("audio/mpeg", "mp3"),
        ("audio/ogg", "ogg"),
        ("audio/wav", "wav"),
        ("audio/mp4", "m4a"),
        ("video/mp4", "mp4"),
        ("video/webm", "webm"),
        ("application/pdf", "pdf"),
        ("application/zip", "zip"),
        ("text/plain", "txt"),
    ]
    .into_iter()
    .find_map(|(known, extension)| mime.eq_ignore_ascii_case(known).then_some(extension))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::NavigationBridge;

    #[test]
    fn save_names_lose_paths_forbidden_characters_and_gain_a_typed_extension() {
        assert_eq!(
            safe_attachment_filename("..\\..\\evil/dir/photo.PNG", None),
            "photo.PNG"
        );
        assert_eq!(safe_attachment_filename("a<b>:c|d?.txt", None), "abcd.txt");
        assert_eq!(safe_attachment_filename("..", None), "attachment");
        assert_eq!(
            safe_attachment_filename("", Some("video/mp4")),
            "attachment.mp4"
        );
        assert_eq!(
            safe_attachment_filename("clip", Some("video/mp4; codecs=avc1")),
            "clip.mp4"
        );
        assert_eq!(
            safe_attachment_filename("clip", Some("application/x-unknown")),
            "clip"
        );
        assert_eq!(
            safe_attachment_filename(&"x".repeat(500), None)
                .chars()
                .count(),
            180
        );
    }

    #[test]
    fn a_failed_action_reports_a_fixed_reason_and_a_cancelled_one_stays_silent() {
        let (_, worker) = Task::<()>::none().abortable();
        let mut gateway = GatewayPanel::new(1, worker, NavigationBridge::new());
        finished(&mut gateway, ActionOutcome::Failed("no"));
        assert_eq!(gateway.attachment_notice, Some("no"));
        finished(&mut gateway, ActionOutcome::Cancelled);
        assert_eq!(gateway.attachment_notice, None);
        finished(&mut gateway, ActionOutcome::Saved);
        assert_eq!(gateway.attachment_notice, Some("Attachment saved."));
    }
}
