//! One watched video's latest-frame handoff and event-driven presentation.
//!
//! Pixels never enter iced's message queue. The decoder owns a three-buffer,
//! byte-bounded pool; the slot replaces stale leases and coalesces notifications
//! until the UI consumes one. Visibility suspends the decoder and its subscription.

mod gpu;

/// Videos the NV12 pass can draw in one frame (plane pairs).
pub const MAX_VIDEOS: usize = gpu::MAX_VIDEOS;

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use fastcord_video::DecodedFrame;
use fastcord_video::frame::{FrameLease, FramePool, LeaseError};
use iced::advanced::{Layout, Widget, layout, mouse, renderer, widget};
use iced::{Color, Element, Length, Rectangle, Size, Subscription};
use parking_lot::Mutex;
use tokio::sync::{Notify, watch};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Waiting,
    Playing(String),
    Failed(String),
    Ended,
}

impl Status {
    pub fn describe(&self) -> String {
        match self {
            Self::Connecting => "Connecting to the stream…".into(),
            Self::Waiting => "Waiting for a video keyframe…".into(),
            Self::Playing(backend) => format!("Watching · {backend}"),
            Self::Failed(error) => format!("Video stopped: {error}"),
            Self::Ended => "The stream ended.".into(),
        }
    }
}

struct Latest {
    frame: Option<FrameLease>,
    status: Status,
    notified: bool,
    pressure: bool,
    frame_error: Option<String>,
}

/// A generation of one watch. Old workers can only affect their own slot.
pub struct Session {
    pub id: u64,
    pub key: String,
    pub visible: Arc<AtomicBool>,
    pub active: Arc<AtomicBool>,
    visibility: watch::Sender<bool>,
    latest: Mutex<Latest>,
    wake: Notify,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoSession")
            .field("id", &self.id)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl Session {
    pub fn new(key: String) -> Arc<Self> {
        let (visibility, _) = watch::channel(false);
        Arc::new(Self {
            id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            key,
            visible: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(true)),
            visibility,
            latest: Mutex::new(Latest {
                frame: None,
                status: Status::Connecting,
                notified: false,
                pressure: false,
                frame_error: None,
            }),
            wake: Notify::new(),
        })
    }

    pub fn visibility(&self) -> watch::Receiver<bool> {
        self.visibility.subscribe()
    }

    pub fn set_visible(&self, visible: bool) {
        let visible = visible && self.active.load(Ordering::Acquire);
        if self.visible.swap(visible, Ordering::AcqRel) != visible {
            self.visibility.send_replace(visible);
            let mut latest = self.latest.lock();
            // Hidden frames must not keep a pool alive or flash on resume.
            if !visible {
                latest.frame = None;
                latest.notified = false;
            } else {
                self.notify(&mut latest);
            }
        }
    }

    pub fn close(&self) {
        self.active.store(false, Ordering::Release);
        self.set_visible(false);
        let mut latest = self.latest.lock();
        latest.frame = None;
        latest.status = Status::Ended;
        latest.notified = false;
        self.wake.notify_waiters();
    }

    fn notify(&self, latest: &mut Latest) {
        if self.visible.load(Ordering::Acquire) && !latest.notified {
            latest.notified = true;
            self.wake.notify_one();
        }
    }

    pub fn status(&self, status: Status) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let mut latest = self.latest.lock();
        if !matches!(status, Status::Playing(_)) {
            latest.frame = None;
            latest.frame_error = None;
        }
        if latest.status != status {
            latest.status = status;
            self.notify(&mut latest);
        }
    }

    pub fn push(&self, frame: FrameLease) {
        let mut latest = self.latest.lock();
        if self.active.load(Ordering::Acquire) && self.visible.load(Ordering::Acquire) {
            latest.frame = Some(frame);
            latest.pressure = false;
            latest.frame_error = None;
            self.notify(&mut latest);
        }
    }

    fn pressure(&self) {
        let mut latest = self.latest.lock();
        if !latest.pressure {
            latest.pressure = true;
            self.notify(&mut latest);
        }
    }

    fn frame_error(&self, error: String) {
        let mut latest = self.latest.lock();
        latest.frame = None;
        if latest.frame_error.as_ref() != Some(&error) {
            latest.frame_error = Some(error);
            self.notify(&mut latest);
        }
    }

    pub fn consume(&self, id: u64) {
        if id == self.id {
            self.latest.lock().notified = false;
        }
    }

    pub fn frame(&self) -> Option<FrameLease> {
        self.latest.lock().frame.clone()
    }

    /// Whether the decoder reported a failure and no picture is available.
    pub fn failed(&self) -> bool {
        matches!(self.latest.lock().status, Status::Failed(_))
    }

    pub fn description(&self) -> String {
        let latest = self.latest.lock();
        if let Some(error) = &latest.frame_error {
            return format!("Video frame unavailable: {error}");
        }
        let mut description = latest.status.describe();
        if latest.pressure {
            description.push_str(" · Frame pool busy; newest pictures are being dropped.");
        }
        description
    }

    pub fn subscription(self: &Arc<Self>) -> Subscription<u64> {
        if !self.visible.load(Ordering::Acquire) {
            return Subscription::none();
        }
        Subscription::run_with(Notification(self.clone()), notifications)
    }

    /// Captures the reusable pool once, on the decoder's thread. NV12 is copied
    /// once into a lease; the no-adapter path instead converts directly to BGRA.
    pub fn sink(self: &Arc<Self>) -> impl FnMut(u32, DecodedFrame<'_>) + Send + 'static {
        let session = self.clone();
        let mut pool = FramePool::new(crate::render::frame_format());
        move |_, picture| {
            if !session.active.load(Ordering::Acquire) || !session.visible.load(Ordering::Acquire) {
                return;
            }
            match pool.lease(picture) {
                Ok(frame) => session.push(frame),
                Err(LeaseError::Exhausted) => session.pressure(),
                Err(error) => session.frame_error(error.to_string()),
            }
        }
    }
}

struct Notification(Arc<Session>);

impl Hash for Notification {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.id.hash(state);
    }
}

fn notifications(data: &Notification) -> impl iced::futures::Stream<Item = u64> + use<> {
    iced::futures::stream::unfold(data.0.clone(), |session| async move {
        session.wake.notified().await;
        Some((session.id, session))
    })
}

/// Letterboxes using the display crop and sample aspect ratio, never stretches.
pub fn fit(bounds: Rectangle, width: u32, height: u32, aspect: (u32, u32)) -> Rectangle {
    if width == 0 || height == 0 || bounds.width <= 0.0 || bounds.height <= 0.0 {
        return Rectangle::new(bounds.position(), Size::ZERO);
    }
    let ratio = if aspect.0 == 0 || aspect.1 == 0 {
        1.0
    } else {
        aspect.0 as f64 / aspect.1 as f64
    };
    let image_ratio = width as f64 * ratio / height as f64;
    let display_ratio = bounds.width as f64 / bounds.height as f64;
    let (width, height) = if image_ratio > display_ratio {
        (bounds.width, (bounds.width as f64 / image_ratio) as f32)
    } else {
        ((bounds.height as f64 * image_ratio) as f32, bounds.height)
    };
    Rectangle {
        x: bounds.x + (bounds.width - width) * 0.5,
        y: bounds.y + (bounds.height - height) * 0.5,
        width,
        height,
    }
}

pub fn view<'a, Message: 'a>(session: Arc<Session>) -> Element<'a, Message> {
    Element::new(Video { session })
}

struct Video {
    session: Arc<Session>,
}

impl<Message, Theme> Widget<Message, Theme, iced::Renderer> for Video {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut widget::Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }

    fn draw(
        &self,
        _tree: &widget::Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        let bounds = layout.bounds();
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                ..Default::default()
            },
            Color::BLACK,
        );
        if !self.session.visible.load(Ordering::Acquire) {
            return;
        }
        let Some(frame) = self.session.frame() else {
            return;
        };
        let display = fit(
            bounds,
            frame.width(),
            frame.height(),
            frame.color().pixel_aspect_ratio,
        );
        let Some(clip) = display.intersection(viewport) else {
            return;
        };
        match renderer {
            iced_renderer::fallback::Renderer::Primary(renderer) => {
                use iced::widget::shader::Program;
                use iced_renderer::wgpu::primitive::Renderer as _;
                let program = gpu::VideoProgram {
                    frame,
                    active: self.session.active.clone(),
                };
                renderer.draw_primitive(
                    display,
                    <gpu::VideoProgram as Program<Message>>::draw(
                        &program,
                        &(),
                        mouse::Cursor::Unavailable,
                        display,
                    ),
                );
            }
            iced_renderer::fallback::Renderer::Secondary(_) => {
                crate::render::software_video(crate::render::SoftwareVideo {
                    frame,
                    bounds: display,
                    clip,
                    active: self.session.active.clone(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_video::frame::{ColorDescription, FrameFormat, Nv12};
    use std::time::Duration;

    fn frame(pool: &mut FramePool, value: u8) -> FrameLease {
        pool.lease(DecodedFrame {
            image: Nv12::packed(2, 2, &[value; 6]).unwrap(),
            timestamp: Duration::ZERO,
            color: ColorDescription::default(),
        })
        .unwrap()
    }

    #[test]
    fn aspect_fit_letterboxes_resize_and_anamorphic_frames() {
        let bounds = Rectangle {
            x: 10.0,
            y: 20.0,
            width: 800.0,
            height: 600.0,
        };
        assert_eq!(
            fit(bounds, 1280, 720, (1, 1)),
            Rectangle {
                x: 10.0,
                y: 95.0,
                width: 800.0,
                height: 450.0
            }
        );
        assert_eq!(fit(bounds, 720, 576, (16, 15)), bounds);
        assert_eq!(fit(bounds, 0, 720, (1, 1)).size(), Size::ZERO);
        assert_eq!(
            fit(bounds, 1280, 720, (0, 0)),
            fit(bounds, 1280, 720, (1, 1))
        );
    }

    #[test]
    fn hidden_arrivals_and_closed_generations_hold_no_frames() {
        let session = Session::new("guild:1:2:3".into());
        let mut pool = FramePool::new(FrameFormat::Nv12);
        session.push(frame(&mut pool, 1));
        assert!(session.frame().is_none());
        assert_eq!(session.subscription().units(), 0);
        session.set_visible(true);
        session.push(frame(&mut pool, 2));
        assert_eq!(session.frame().unwrap().pixels()[0], 2);
        assert_eq!(session.subscription().units(), 1);
        session.set_visible(false);
        assert!(session.frame().is_none());
        session.set_visible(true);
        session.close();
        session.set_visible(true);
        session.push(frame(&mut pool, 3));
        assert!(session.frame().is_none());
        assert_eq!(session.subscription().units(), 0);
    }

    #[test]
    fn stale_frames_replace_and_notifications_remain_coalesced() {
        let session = Session::new("guild:1:2:3".into());
        let mut pool = FramePool::new(FrameFormat::Nv12);
        session.set_visible(true);
        session.push(frame(&mut pool, 1));
        session.push(frame(&mut pool, 2));
        assert_eq!(session.frame().unwrap().pixels()[0], 2);
        session.pressure();
        assert_eq!(session.frame().unwrap().pixels()[0], 2);
        assert!(session.description().contains("Frame pool busy"));
        assert!(session.latest.lock().notified);
        session.consume(session.id + 1);
        assert!(session.latest.lock().notified);
        session.consume(session.id);
        assert!(!session.latest.lock().notified);
        session.push(frame(&mut pool, 3));
        assert!(!session.description().contains("Frame pool busy"));
        assert!(session.latest.lock().notified);
        session.status(Status::Failed("fixture failure".into()));
        assert!(session.frame().is_none());
    }

    #[test]
    fn decoder_backend_reports_do_not_hide_frame_lease_failures() {
        let session = Session::new("guild:1:2:3".into());
        let mut pool = FramePool::new(FrameFormat::Nv12);
        session.set_visible(true);
        session.frame_error("fixture capacity limit".into());
        session.status(Status::Playing("fixture decoder".into()));
        assert!(session.description().contains("fixture capacity limit"));
        assert!(session.frame().is_none());
        session.push(frame(&mut pool, 1));
        assert!(!session.description().contains("fixture capacity limit"));
        assert!(session.description().contains("fixture decoder"));
    }
}
