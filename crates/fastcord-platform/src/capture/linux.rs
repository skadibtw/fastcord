//! Linux screen and window capture through xdg-desktop-portal ScreenCast and
//! PipeWire (SPEC §8.4, milestone 33).
//!
//! The portal owns the choice: [`list_sources`] offers "a screen" and "a
//! window", and starting either opens the desktop's own picker. One owner
//! thread runs the portal calls on a single-threaded Tokio runtime and then a
//! PipeWire loop on the remote the portal opened. Pictures arrive as shared
//! memory in BGRx or BGRA (no DMA-BUF modifiers are offered, so the compositor
//! copies) and are converted to NV12 by the CPU converter in [`super::convert`].
//! The portal session is closed when the capture ends; closing it from the
//! desktop ends the PipeWire stream, which ends the capture as
//! [`CaptureError::SourceClosed`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use pipewire as pw;
use pw::spa;

use super::camera::CameraDevice;
use super::convert::{self, Bgra, Converter};
use super::{
    CaptureConfig, CaptureError, CaptureSource, CaptureState, CaptureStatus, Control,
    ConversionPath, Flow, FrameHandler, HandlerFactory, MAX_NATIVE_DIMENSION, MAX_NATIVE_PIXELS,
    SourceId, SourceKind, StartOptions, Tick, VideoSample,
};

/// How long a quiet capture waits before asking the handler for a tick.
const TICK: Duration = Duration::from_millis(250);

const NO_PORTAL: CaptureError = CaptureError::Unsupported(
    "screen sharing needs xdg-desktop-portal with ScreenCast and PipeWire (a Wayland desktop such as GNOME or KDE Plasma)",
);

enum Command {
    Wake,
    Stop,
}

pub(super) struct Native {
    commands: pw::channel::Sender<Command>,
    /// Ends a portal dialog that is still open.
    cancel: Arc<tokio::sync::Notify>,
    thread: Option<JoinHandle<Result<(), CaptureError>>>,
}

impl Native {
    pub(super) fn waker(&self) -> Box<dyn Fn() + Send + Sync> {
        let commands = self.commands.clone();
        Box::new(move || {
            let _ = commands.send(Command::Wake);
        })
    }

    pub(super) fn stop(mut self) -> Result<(), CaptureError> {
        self.finish()
    }

    fn finish(&mut self) -> Result<(), CaptureError> {
        let _ = self.commands.send(Command::Stop);
        self.cancel.notify_one();
        match self.thread.take().map(JoinHandle::join) {
            None => Ok(()),
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(CaptureError::Worker("the capture thread panicked")),
        }
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// The portal picks the actual screen or window; these two entries only say
/// which kind its dialog offers.
pub(super) fn list_sources() -> Result<Vec<CaptureSource>, CaptureError> {
    Ok(vec![
        CaptureSource {
            id: SourceId::from_raw(SourceKind::Display, 0),
            title: "A screen (chosen in the system dialog)".to_owned(),
            width: 0,
            height: 0,
            primary: true,
        },
        CaptureSource {
            id: SourceId::from_raw(SourceKind::Window, 0),
            title: "A window (chosen in the system dialog)".to_owned(),
            width: 0,
            height: 0,
            primary: false,
        },
    ])
}

pub(super) fn availability() -> Result<(), CaptureError> {
    let runtime = runtime()?;
    let types = runtime
        .block_on(async { Screencast::new().await?.available_source_types().await })
        .map_err(|_| NO_PORTAL)?;
    if types.is_empty() {
        return Err(NO_PORTAL);
    }
    Ok(())
}

pub(super) fn start(
    source: SourceId,
    options: &StartOptions,
    control: Arc<Control>,
    status: CaptureStatus,
    handler: HandlerFactory,
) -> Result<Native, CaptureError> {
    let (commands, receiver) = pw::channel::channel();
    let cancel = Arc::new(tokio::sync::Notify::new());
    let config = options.config;
    let thread_cancel = Arc::clone(&cancel);
    let thread = std::thread::Builder::new()
        .name("fastcord-capture".to_owned())
        .spawn(move || {
            let result = run(
                source.kind(),
                config,
                control,
                &status,
                handler,
                receiver,
                &thread_cancel,
            );
            status.set(match &result {
                Ok(()) => CaptureState::Stopped,
                Err(error) => CaptureState::Failed(error.clone()),
            });
            result
        })
        .map_err(|_| CaptureError::Worker("the capture thread could not be started"))?;
    Ok(Native {
        commands,
        cancel,
        thread: Some(thread),
    })
}

const CAMERA_UNSUPPORTED: CaptureError =
    CaptureError::Unsupported("the camera is only implemented for Windows so far");

pub(super) fn list_cameras() -> Result<Vec<CameraDevice>, CaptureError> {
    Err(CAMERA_UNSUPPORTED)
}

pub(super) fn start_camera(
    _device: Option<String>,
    _options: &StartOptions,
    _control: Arc<Control>,
    _status: CaptureStatus,
    _handler: HandlerFactory,
) -> Result<Native, CaptureError> {
    Err(CAMERA_UNSUPPORTED)
}

fn runtime() -> Result<tokio::runtime::Runtime, CaptureError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| CaptureError::Worker("the portal runtime could not be started"))
}

fn portal_error(error: ashpd::Error) -> CaptureError {
    match error {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => CaptureError::Cancelled,
        ashpd::Error::Portal(ashpd::PortalError::NotAllowed(_)) => CaptureError::PermissionDenied,
        ashpd::Error::Zbus(_)
        | ashpd::Error::RequiresVersion(..)
        | ashpd::Error::PortalNotFound(_) => NO_PORTAL,
        _ => CaptureError::Worker("the desktop portal refused the screen share"),
    }
}

/// Monotonic time for picture stamps; only differences are meaningful.
fn now() -> Duration {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

fn run(
    kind: SourceKind,
    config: CaptureConfig,
    control: Arc<Control>,
    status: &CaptureStatus,
    handler: HandlerFactory,
    receiver: pw::channel::Receiver<Command>,
    cancel: &tokio::sync::Notify,
) -> Result<(), CaptureError> {
    let runtime = runtime()?;
    let types = match kind {
        SourceKind::Display => SourceType::Monitor,
        SourceKind::Window => SourceType::Window,
    };
    let portal = async {
        let proxy = Screencast::new().await?;
        let session = proxy.create_session().await?;
        let cursors = proxy.available_cursor_modes().await.unwrap_or_default();
        let cursor = if cursors.contains(CursorMode::Embedded) {
            CursorMode::Embedded
        } else {
            CursorMode::Hidden
        };
        proxy
            .select_sources(
                &session,
                cursor,
                types.into(),
                false,
                None,
                PersistMode::DoNot,
            )
            .await?
            .response()?;
        let streams = proxy.start(&session, None).await?.response()?;
        let node = streams
            .streams()
            .first()
            .map(|stream| stream.pipe_wire_node_id());
        let fd = proxy.open_pipe_wire_remote(&session).await?;
        Ok::<_, ashpd::Error>((proxy, session, node, fd))
    };
    let opened = runtime.block_on(async {
        tokio::select! {
            opened = portal => Some(opened),
            () = cancel.notified() => None,
        }
    });
    let Some(opened) = opened else {
        return Ok(());
    };
    let (_proxy, session, node, fd) = opened.map_err(portal_error)?;
    let node = node.ok_or(CaptureError::Cancelled)?;
    let result = stream(node, fd, config, control, status, handler, receiver);
    let _ = runtime.block_on(session.close());
    result
}

/// What the PipeWire callbacks and the loop share on the owner thread.
struct Shared {
    config: CaptureConfig,
    handler: Box<dyn FrameHandler>,
    converter: Converter,
    /// The source size (its bytes are BGRx or BGRA).
    source: Option<(u32, u32)>,
    /// The newest converted picture at `output` size, stride = width.
    picture: Vec<u8>,
    output: (u32, u32),
    /// A converted picture that has not been delivered yet.
    pending: bool,
    delivered: Option<Instant>,
    streaming: bool,
    ended: Option<Result<(), CaptureError>>,
}

impl Shared {
    fn end(&mut self, result: Result<(), CaptureError>) {
        self.ended.get_or_insert(result);
    }

    fn deliver(&mut self) {
        if self.ended.is_some() || self.picture.is_empty() {
            return;
        }
        let (width, height) = self.output;
        let flow = match VideoSample::nv12(width, height, width as usize, &self.picture, now()) {
            Ok(sample) => self.handler.frame(&sample),
            Err(error) => {
                self.end(Err(error));
                return;
            }
        };
        self.pending = false;
        self.delivered = Some(Instant::now());
        if flow == Flow::Stop {
            self.end(Ok(()));
        }
    }

    fn format(&mut self, status: &CaptureStatus, param: &spa::pod::Pod) {
        let mut info = spa::param::video::VideoInfoRaw::default();
        if info.parse(param).is_err() {
            return;
        }
        if !matches!(
            info.format(),
            spa::param::video::VideoFormat::BGRx | spa::param::video::VideoFormat::BGRA
        ) {
            self.end(Err(CaptureError::Unsupported(
                "the desktop offered no BGRx or BGRA picture format",
            )));
            return;
        }
        let (width, height) = (info.size().width, info.size().height);
        if width == 0
            || height == 0
            || width > MAX_NATIVE_DIMENSION
            || height > MAX_NATIVE_DIMENSION
            || u64::from(width) * u64::from(height) > MAX_NATIVE_PIXELS
        {
            self.end(Err(CaptureError::SourceTooLarge { width, height }));
            return;
        }
        if self.source.is_some_and(|size| size != (width, height)) {
            status.set(CaptureState::SourceSize { width, height });
        }
        self.source = Some((width, height));
        self.output = self.config.fit(width, height);
        let (out_w, out_h) = self.output;
        self.picture = vec![0; convert::nv12_len(out_w as usize, out_h as usize)];
        self.pending = false;
    }

    fn picture(&mut self, data: &[u8], stride: usize, min_interval: Duration) {
        let Some((width, height)) = self.source else {
            return;
        };
        let due = self.delivered.is_none_or(|at| at.elapsed() >= min_interval);
        // While paced down (nobody watches), skip converting what would not be sent.
        let paused = min_interval > self.config.frame_interval() * 2;
        if !due && paused {
            return;
        }
        let (out_w, out_h) = self.output;
        let converted = Bgra::new(data, stride, width, height).and_then(|source| {
            self.converter
                .convert(&source, out_w, out_h, out_w as usize, &mut self.picture)
        });
        match converted {
            Ok(()) => self.pending = true,
            // A short or damaged buffer is skipped, not fatal.
            Err(_) => return,
        }
        if due {
            self.deliver();
        }
    }
}

fn pipewire_error(_: pw::Error) -> CaptureError {
    CaptureError::Worker("PipeWire refused the screen-share stream")
}

/// The formats offered to the compositor: BGRx or BGRA, any size, at most the
/// configured rate.
fn format_params(config: CaptureConfig) -> Result<Vec<u8>, CaptureError> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::utils::{Fraction, Rectangle};
    let object = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: MAX_NATIVE_DIMENSION,
                height: MAX_NATIVE_DIMENSION
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction {
                num: config.frame_rate,
                denom: 1
            },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: config.frame_rate,
                denom: 1
            }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|_| CaptureError::Worker("the PipeWire format could not be built"))
}

/// Asks for buffers in memory this process can map (memfd or plain memory).
fn buffers_param() -> Result<Vec<u8>, CaptureError> {
    use spa::pod::{ChoiceValue, Object, Property, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    let types = (1 << spa::sys::SPA_DATA_MemPtr) | (1 << spa::sys::SPA_DATA_MemFd);
    let object = Value::Object(Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![Property::new(
            spa::sys::SPA_PARAM_BUFFERS_dataType,
            Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: types,
                    flags: vec![types],
                },
            ))),
        )],
    });
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|_| CaptureError::Worker("the PipeWire buffer request could not be built"))
}

fn stream(
    node: u32,
    fd: std::os::fd::OwnedFd,
    config: CaptureConfig,
    control: Arc<Control>,
    status: &CaptureStatus,
    handler: HandlerFactory,
    receiver: pw::channel::Receiver<Command>,
) -> Result<(), CaptureError> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopBox::new(None).map_err(pipewire_error)?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None).map_err(pipewire_error)?;
    let core = context.connect_fd(fd, None).map_err(pipewire_error)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "fastcord-screen",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(pipewire_error)?;

    let shared = Rc::new(RefCell::new(Shared {
        config,
        handler: handler(),
        converter: Converter::default(),
        source: None,
        picture: Vec::new(),
        output: (0, 0),
        pending: false,
        delivered: None,
        streaming: false,
        ended: None,
    }));
    let woken = Rc::new(Cell::new(false));
    let _commands = {
        let shared = Rc::clone(&shared);
        let woken = Rc::clone(&woken);
        receiver.attach(mainloop.loop_(), move |command| match command {
            Command::Wake => woken.set(true),
            Command::Stop => shared.borrow_mut().end(Ok(())),
        })
    };
    let _listener = stream
        .add_local_listener::<()>()
        .state_changed({
            let shared = Rc::clone(&shared);
            let status = status.clone();
            move |_, _, _, state| {
                let mut shared = shared.borrow_mut();
                match state {
                    pw::stream::StreamState::Streaming if !shared.streaming => {
                        shared.streaming = true;
                        status.set(CaptureState::Capturing);
                    }
                    pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected => {
                        shared.end(Err(CaptureError::SourceClosed));
                    }
                    _ => {}
                }
            }
        })
        .param_changed({
            let shared = Rc::clone(&shared);
            let status = status.clone();
            let buffers = buffers_param()?;
            move |stream, _, id, param| {
                if id == spa::param::ParamType::Format.as_raw()
                    && let Some(param) = param
                {
                    shared.borrow_mut().format(&status, param);
                    // Shared memory the stream maps for us; no DMA-BUF.
                    if let Some(buffers) = spa::pod::Pod::from_bytes(&buffers) {
                        let _ = stream.update_params(&mut [buffers]);
                    }
                }
            }
        })
        .process({
            let shared = Rc::clone(&shared);
            let control = Arc::clone(&control);
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let chunk = data.chunk();
                let (offset, size, stride) = (
                    chunk.offset() as usize,
                    chunk.size() as usize,
                    chunk.stride(),
                );
                let Some(bytes) = data.data() else {
                    return;
                };
                let Some(bytes) = offset
                    .checked_add(size)
                    .and_then(|end| bytes.get(offset..end))
                else {
                    return;
                };
                let mut shared = shared.borrow_mut();
                let width = shared.source.map_or(0, |(width, _)| width as usize);
                let stride = usize::try_from(stride)
                    .ok()
                    .filter(|&stride| stride > 0)
                    .unwrap_or(width * 4);
                shared.picture(bytes, stride, control.min_interval());
            }
        })
        .register()
        .map_err(pipewire_error)?;

    let params = format_params(config)?;
    let mut params = [
        spa::pod::Pod::from_bytes(&params).ok_or(CaptureError::Worker(
            "the PipeWire format could not be built",
        ))?,
    ];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(pipewire_error)?;
    status.set(CaptureState::Converting(ConversionPath::Cpu));

    let mut quiet_since = Instant::now();
    loop {
        let wait = {
            let shared = shared.borrow();
            let interval = control.min_interval();
            let due_in = match (shared.pending, shared.delivered) {
                (true, Some(at)) => interval.saturating_sub(at.elapsed()),
                _ => TICK,
            };
            due_in.min(TICK.saturating_sub(quiet_since.elapsed()))
        };
        mainloop.loop_().iterate(pw::loop_::Timeout::Finite(
            wait.max(Duration::from_millis(1)),
        ));
        let ended = shared.borrow_mut().ended.take();
        if let Some(result) = ended {
            // Disconnecting reports a state change, which borrows `shared`.
            let _ = stream.disconnect();
            return result;
        }
        let mut shared = shared.borrow_mut();
        let interval = control.min_interval();
        if shared.pending && shared.delivered.is_none_or(|at| at.elapsed() >= interval) {
            shared.deliver();
        }
        if shared.delivered.is_some_and(|at| at > quiet_since) {
            quiet_since = shared.delivered.unwrap_or(quiet_since);
        }
        if woken.replace(false) || quiet_since.elapsed() >= TICK {
            quiet_since = Instant::now();
            match shared.handler.tick() {
                Tick::Idle => {}
                Tick::Repeat => shared.deliver(),
                Tick::Stop => shared.end(Ok(())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Width, height and first luma byte of each delivered picture.
    type Frames = Rc<RefCell<Vec<(u32, u32, u8)>>>;

    struct Recorder(Frames);

    impl FrameHandler for Recorder {
        fn frame(&mut self, sample: &VideoSample<'_>) -> Flow {
            let luma = sample.planes().0[0];
            self.0
                .borrow_mut()
                .push((sample.width(), sample.height(), luma));
            Flow::Continue
        }

        fn tick(&mut self) -> Tick {
            Tick::Idle
        }
    }

    fn format(format: spa::param::video::VideoFormat, width: u32, height: u32) -> Vec<u8> {
        use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
        let object = spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::Format,
            spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            spa::pod::property!(FormatProperties::VideoFormat, Id, format),
            spa::pod::property!(
                FormatProperties::VideoSize,
                Rectangle,
                spa::utils::Rectangle { width, height }
            ),
        );
        spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(object),
        )
        .unwrap()
        .0
        .into_inner()
    }

    fn shared(frames: &Frames) -> Shared {
        Shared {
            config: CaptureConfig::SCREEN_SHARE_720P30,
            handler: Box::new(Recorder(Rc::clone(frames))),
            converter: Converter::default(),
            source: None,
            picture: Vec::new(),
            output: (0, 0),
            pending: false,
            delivered: None,
            streaming: false,
            ended: None,
        }
    }

    fn apply(shared: &mut Shared, status: &CaptureStatus, bytes: &[u8]) {
        shared.format(status, spa::pod::Pod::from_bytes(bytes).unwrap());
    }

    #[test]
    fn the_portal_dialog_offers_one_screen_and_one_window_entry() {
        let sources = list_sources().unwrap();
        let kinds: Vec<_> = sources.iter().map(|source| source.id.kind()).collect();
        assert_eq!(kinds, [SourceKind::Display, SourceKind::Window]);
        assert!(
            sources
                .iter()
                .all(|source| source.title.contains("system dialog"))
        );
    }

    #[test]
    fn pictures_are_scaled_converted_and_paced() {
        use spa::param::video::VideoFormat;
        let frames = Rc::new(RefCell::new(Vec::new()));
        let status = CaptureStatus::new(None);
        let mut shared = shared(&frames);
        apply(&mut shared, &status, &format(VideoFormat::BGRx, 2560, 1440));
        assert_eq!(shared.output, (1280, 720));
        // White, then black: BT.709 limited range luma 235 and 16.
        let white = vec![255u8; 2560 * 4 * 1440];
        let black = vec![0u8; 2560 * 4 * 1440];
        let interval = Duration::from_millis(33);
        shared.picture(&white, 2560 * 4, interval);
        shared.picture(&black, 2560 * 4, interval);
        assert_eq!(*frames.borrow(), [(1280, 720, 235)], "the second waits");
        assert!(shared.pending);
        shared.delivered = Some(Instant::now() - interval);
        shared.deliver();
        assert_eq!(frames.borrow()[1], (1280, 720, 16), "the newest is sent");

        // Paced down to one picture a second, a picture inside the interval is
        // not even converted.
        shared.picture(&white, 2560 * 4, Duration::from_secs(1));
        assert!(!shared.pending);
        assert_eq!(frames.borrow().len(), 2);
    }

    #[test]
    fn resizes_are_reported_and_bad_formats_end_the_capture() {
        use spa::param::video::VideoFormat;
        let frames = Rc::new(RefCell::new(Vec::new()));
        let status = CaptureStatus::new(None);
        let mut shared = shared(&frames);
        apply(&mut shared, &status, &format(VideoFormat::BGRA, 800, 600));
        assert_eq!(status.snapshot().0, 0, "the first size is not a resize");
        apply(&mut shared, &status, &format(VideoFormat::BGRA, 1024, 600));
        assert_eq!(
            status.snapshot().1,
            CaptureState::SourceSize {
                width: 1024,
                height: 600
            }
        );
        assert_eq!(shared.output, (1024, 600));
        assert!(shared.ended.is_none());

        apply(&mut shared, &status, &format(VideoFormat::BGRx, 9000, 100));
        assert_eq!(
            shared.ended.take(),
            Some(Err(CaptureError::SourceTooLarge {
                width: 9000,
                height: 100
            }))
        );
        apply(&mut shared, &status, &format(VideoFormat::RGBA, 800, 600));
        assert!(matches!(
            shared.ended,
            Some(Err(CaptureError::Unsupported(_)))
        ));
    }

    /// A 640x360 BGRx video source of solid blue at 30 fps, driving the graph
    /// itself, published on the PipeWire daemon of `XDG_RUNTIME_DIR`.
    fn blue_source(node: std::sync::mpsc::Sender<u32>, stop: Arc<std::sync::atomic::AtomicBool>) {
        use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
        use spa::pod::{Object, Property, Value};
        const W: u32 = 640;
        const H: u32 = 360;
        pw::init();
        let mainloop = pw::main_loop::MainLoopBox::new(None).unwrap();
        let context = pw::context::ContextBox::new(mainloop.loop_(), None).unwrap();
        let core = context.connect(None).unwrap();
        let stream = pw::stream::StreamBox::new(
            &core,
            "fastcord-test-source",
            pw::properties::properties! {
                *pw::keys::MEDIA_CLASS => "Video/Source",
                *pw::keys::MEDIA_TYPE => "Video",
            },
        )
        .unwrap();
        let _listener = stream
            .add_local_listener::<()>()
            .state_changed(move |stream, _, _, state| {
                if state == pw::stream::StreamState::Paused {
                    let _ = node.send(stream.node_id());
                }
            })
            .param_changed(|stream, _, id, param| {
                if id != spa::param::ParamType::Format.as_raw() || param.is_none() {
                    return;
                }
                let int = |key, value| Property::new(key, Value::Int(value));
                let buffers = Value::Object(Object {
                    type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
                    id: spa::param::ParamType::Buffers.as_raw(),
                    properties: vec![
                        int(spa::sys::SPA_PARAM_BUFFERS_buffers, 4),
                        int(spa::sys::SPA_PARAM_BUFFERS_blocks, 1),
                        int(spa::sys::SPA_PARAM_BUFFERS_size, (W * H * 4) as i32),
                        int(spa::sys::SPA_PARAM_BUFFERS_stride, (W * 4) as i32),
                        int(
                            spa::sys::SPA_PARAM_BUFFERS_dataType,
                            1 << spa::sys::SPA_DATA_MemFd,
                        ),
                    ],
                });
                let bytes = spa::pod::serialize::PodSerializer::serialize(
                    std::io::Cursor::new(Vec::new()),
                    &buffers,
                )
                .unwrap()
                .0
                .into_inner();
                let _ = stream.update_params(&mut [spa::pod::Pod::from_bytes(&bytes).unwrap()]);
            })
            .process(|stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let data = &mut buffer.datas_mut()[0];
                if let Some(bytes) = data.data() {
                    for pixel in bytes.as_chunks_mut::<4>().0 {
                        pixel.copy_from_slice(&[255, 0, 0, 255]);
                    }
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = (W * 4) as i32;
                *chunk.size_mut() = W * H * 4;
            })
            .register()
            .unwrap();
        let object = spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::EnumFormat,
            spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            spa::pod::property!(
                FormatProperties::VideoFormat,
                Id,
                spa::param::video::VideoFormat::BGRx
            ),
            spa::pod::property!(
                FormatProperties::VideoSize,
                Rectangle,
                spa::utils::Rectangle {
                    width: W,
                    height: H
                }
            ),
            spa::pod::property!(
                FormatProperties::VideoFramerate,
                Fraction,
                spa::utils::Fraction { num: 30, denom: 1 }
            ),
        );
        let bytes = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(object),
        )
        .unwrap()
        .0
        .into_inner();
        stream
            .connect(
                spa::utils::Direction::Output,
                None,
                pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut [spa::pod::Pod::from_bytes(&bytes).unwrap()],
            )
            .unwrap();
        let mut next = Instant::now();
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            mainloop.loop_().iterate(pw::loop_::Timeout::Finite(
                next.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
            ));
            if Instant::now() >= next {
                next += Duration::from_millis(33);
                if stream.is_driving() {
                    let _ = stream.trigger_process();
                }
            }
        }
    }

    struct StopAfter(Arc<parking_lot::Mutex<Vec<(u32, u32, u8)>>>, usize);

    impl FrameHandler for StopAfter {
        fn frame(&mut self, sample: &VideoSample<'_>) -> Flow {
            let mut frames = self.0.lock();
            frames.push((sample.width(), sample.height(), sample.planes().0[0]));
            if frames.len() == self.1 {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }

        fn tick(&mut self) -> Tick {
            Tick::Idle
        }
    }

    #[test]
    #[ignore = "needs a PipeWire daemon and session manager on XDG_RUNTIME_DIR"]
    fn a_real_pipewire_source_is_converted_paced_and_stopped() {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (node_tx, node_rx) = std::sync::mpsc::channel();
        let source = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || blue_source(node_tx, stop)
        });
        let node = node_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let socket =
            std::path::Path::new(&std::env::var("XDG_RUNTIME_DIR").unwrap()).join("pipewire-0");
        let fd: std::os::fd::OwnedFd = std::os::unix::net::UnixStream::connect(socket)
            .unwrap()
            .into();
        let config = CaptureConfig::SCREEN_SHARE_720P30;
        let control = Arc::new(Control::new(config.frame_interval()));
        let status = CaptureStatus::new(None);
        let frames = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (_commands, receiver) = pw::channel::channel();
        let started = Instant::now();
        let handler_frames = Arc::clone(&frames);
        let result = stream(
            node,
            fd,
            config,
            control,
            &status,
            Box::new(move || Box::new(StopAfter(handler_frames, 30)) as Box<dyn FrameHandler>),
            receiver,
        );
        let elapsed = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        source.join().unwrap();
        assert_eq!(result, Ok(()));
        let frames = frames.lock();
        assert_eq!(frames.len(), 30);
        // Solid blue in BT.709 limited range: Y = 16 + 219 * 0.0722 = 32.
        assert!(
            frames
                .iter()
                .all(|&(w, h, y)| (w, h) == (640, 360) && y.abs_diff(32) <= 1),
            "{frames:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(900),
            "paced to 30 fps: {elapsed:?}"
        );
        assert_eq!(status.snapshot().1, CaptureState::Capturing);
    }

    #[test]
    fn a_closed_dialog_is_a_cancellation_not_a_failure() {
        assert_eq!(
            portal_error(ashpd::Error::Response(
                ashpd::desktop::ResponseError::Cancelled
            )),
            CaptureError::Cancelled
        );
        assert_eq!(
            portal_error(ashpd::Error::Response(ashpd::desktop::ResponseError::Other)),
            CaptureError::Worker("the desktop portal refused the screen share")
        );
    }
}
