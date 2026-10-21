//! The Windows.Graphics.Capture session loop: one owner thread waits for the
//! stop request, the source closing, a wake-up and new frames, keeps only the
//! newest frame, paces what it delivers, follows source resizes, and hands each
//! NV12 picture to the frame handler.

use std::ffi::c_void;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::IInspectable;

use super::super::{
    CaptureConfig, CaptureError, CaptureState, CaptureStatus, Control, Conversion, Flow,
    FrameHandler, GpuOutcome, HandlerFactory, MAX_NATIVE_DIMENSION, MAX_NATIVE_PIXELS,
    NATIVE_FRAME_COUNT, SourceId, SourceKind, StartOptions, Tick,
};
use super::gpu::{Converter, Device, surface_texture};
use super::runtime::{
    Apartment, Handle, Native, OrCapture, event, from_hns, monotonic_time, native_error, signal,
    wait,
};
use super::sources::{creation_error, source_alive, window_exists};

/// The pixel format the frame pool delivers.
const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

/// The longest the loop sleeps: the handler's `tick` runs at least this often.
const TICK: Duration = Duration::from_millis(100);

/// A source's new size must hold this long before the output size follows it.
/// A window being dragged to a new size reports every intermediate size, and
/// every output size change reopens the encoder.
const SETTLE: Duration = Duration::from_millis(300);

/// Pictures may arrive up to this fraction early (nine tenths of the interval)
/// so a 60 Hz source paced to 30 fps does not lose every other picture to
/// timer jitter.
const PACE_NUMERATOR: u32 = 9;
const PACE_DENOMINATOR: u32 = 10;

/// The least spacing between the timestamps of a repeated picture and the one
/// before it.
const REPEAT_SPACING: Duration = Duration::from_millis(1);

const UNSUPPORTED_OS: CaptureError = CaptureError::Unsupported(
    "Windows.Graphics.Capture is not available (Windows 10 version 1903 or newer is required)",
);

/// Whether Windows.Graphics.Capture works on this machine. Runs on a throwaway
/// thread: WinRT needs an apartment and the caller's thread may already have an
/// incompatible one (the UI thread is a single-threaded apartment).
pub(in super::super) fn availability() -> Result<(), CaptureError> {
    thread::Builder::new()
        .name("fastcord-capture-probe".to_owned())
        .spawn(|| {
            let _apartment = Apartment::mta()?;
            if GraphicsCaptureSession::IsSupported()
                .capture("GraphicsCaptureSession::IsSupported")?
            {
                Ok(())
            } else {
                Err(UNSUPPORTED_OS)
            }
        })
        .map_err(|_| CaptureError::Worker("could not create the capture probe thread"))?
        .join()
        .unwrap_or(Err(CaptureError::Worker("the capture probe panicked")))
}

pub(in super::super) fn start(
    source: SourceId,
    options: &StartOptions,
    control: Arc<Control>,
    status: CaptureStatus,
    factory: HandlerFactory,
) -> Result<Native, CaptureError> {
    let (config, conversion) = (options.config, options.conversion);
    let thread_status = status.clone();
    Native::spawn("fastcord-capture", status, move |stop, poke| {
        run(
            source,
            config,
            conversion,
            &control,
            &thread_status,
            factory,
            stop,
            poke,
        )
    })
}

fn check_native_size(width: u32, height: u32) -> Result<(), CaptureError> {
    if width > MAX_NATIVE_DIMENSION
        || height > MAX_NATIVE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_NATIVE_PIXELS
    {
        return Err(CaptureError::SourceTooLarge { width, height });
    }
    Ok(())
}

fn create_item(source: SourceId) -> Result<GraphicsCaptureItem, CaptureError> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .capture("RoGetActivationFactory(GraphicsCaptureItem)")?;
    match source.kind() {
        SourceKind::Window => {
            let window = HWND(source.raw() as usize as *mut c_void);
            if !window_exists(window) {
                return Err(CaptureError::SourceClosed);
            }
            // SAFETY: the window handle was just validated; the call only
            // reads it.
            unsafe { interop.CreateForWindow::<GraphicsCaptureItem>(window) }.map_err(|error| {
                creation_error("IGraphicsCaptureItemInterop::CreateForWindow", &error)
            })
        }
        SourceKind::Display => {
            let monitor = HMONITOR(source.raw() as usize as *mut c_void);
            // SAFETY: the call only reads the handle and rejects a stale one.
            unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(monitor) }.map_err(|error| {
                creation_error("IGraphicsCaptureItemInterop::CreateForMonitor", &error)
            })
        }
    }
}

/// The WinRT capture objects of one session, released in the right order.
struct Wgc {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: Option<GraphicsCaptureSession>,
    frame_token: Option<i64>,
    closed_token: Option<i64>,
}

impl Drop for Wgc {
    fn drop(&mut self) {
        if let Some(token) = self.frame_token.take() {
            let _ = self.pool.RemoveFrameArrived(token);
        }
        if let Some(token) = self.closed_token.take() {
            let _ = self.item.RemoveClosed(token);
        }
        if let Some(session) = self.session.take() {
            let _ = session.Close();
        }
        let _ = self.pool.Close();
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    source: SourceId,
    config: CaptureConfig,
    conversion: Conversion,
    control: &Arc<Control>,
    status: &CaptureStatus,
    factory: HandlerFactory,
    stop: &Arc<Handle>,
    poke: &Arc<Handle>,
) -> Result<(), CaptureError> {
    // Dropped last: every WinRT object below is released first.
    let _apartment = Apartment::mta()?;
    if !GraphicsCaptureSession::IsSupported().capture("GraphicsCaptureSession::IsSupported")? {
        return Err(UNSUPPORTED_OS);
    }
    let item = create_item(source)?;
    let size = item.Size().capture("GraphicsCaptureItem::Size")?;
    // A minimized window reports no size; the first real frame resizes the pool.
    let (width, height) = (size.Width.max(1) as u32, size.Height.max(1) as u32);
    check_native_size(width, height)?;
    let device = Device::new()?;
    let converter = Converter::new(&device, conversion)?;
    let frame_event = event(false)?;
    let closed_event = event(true)?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &device.winrt,
        FORMAT,
        NATIVE_FRAME_COUNT as i32,
        SizeInt32 {
            Width: width as i32,
            Height: height as i32,
        },
    )
    .capture("Direct3D11CaptureFramePool::CreateFreeThreaded")?;
    let mut wgc = Wgc {
        item,
        pool,
        session: None,
        frame_token: None,
        closed_token: None,
    };
    let on_frame = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new({
        let frame_event = Arc::clone(&frame_event);
        move |_, _| {
            let _ = signal(&frame_event);
            Ok(())
        }
    });
    wgc.frame_token = Some(
        wgc.pool
            .FrameArrived(&on_frame)
            .capture("Direct3D11CaptureFramePool::FrameArrived")?,
    );
    let on_closed = TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new({
        let closed_event = Arc::clone(&closed_event);
        move |_, _| {
            let _ = signal(&closed_event);
            Ok(())
        }
    });
    wgc.closed_token = Some(
        wgc.item
            .Closed(&on_closed)
            .capture("GraphicsCaptureItem::Closed")?,
    );
    let session = wgc
        .pool
        .CreateCaptureSession(&wgc.item)
        .capture("Direct3D11CaptureFramePool::CreateCaptureSession")?;
    // The OS capture border and the cursor stay at their defaults: the user
    // always sees what is shared.
    session
        .StartCapture()
        .capture("GraphicsCaptureSession::StartCapture")?;
    wgc.session = Some(session);
    status.set(CaptureState::Converting(converter.path()));
    status.set(CaptureState::Capturing);

    let handler = factory();
    let mut capture = Capture {
        source,
        handler,
        wgc,
        converter,
        device,
        pool_size: (width, height),
        status: status.clone(),
        control: Arc::clone(control),
        governor: SizeGovernor::new(config),
        held: None,
        dirty: false,
        last_stamp: None,
        reported: None,
    };
    capture.run(&[stop.0, closed_event.0, poke.0, frame_event.0])
}

/// A captured frame kept until the next one replaces it, so the picture can be
/// delivered again (for a keyframe, a keep-alive, or a settled resize) without
/// waiting for the screen to change. The frame pool has a buffer to spare for
/// this.
struct Held {
    frame: Direct3D11CaptureFrame,
    texture: ID3D11Texture2D,
    content: (u32, u32),
    /// When it was captured.
    time: Duration,
}

/// The capture thread's state. Field order is drop order: the handler (and the
/// encoder it owns) goes first, the Direct3D device last.
struct Capture {
    source: SourceId,
    handler: Box<dyn FrameHandler>,
    wgc: Wgc,
    converter: Converter,
    device: Device,
    /// The size the frame pool currently produces.
    pool_size: (u32, u32),
    status: CaptureStatus,
    control: Arc<Control>,
    governor: SizeGovernor,
    held: Option<Held>,
    /// `held` is newer than the last picture delivered.
    dirty: bool,
    /// The timestamp of the last picture delivered.
    last_stamp: Option<Duration>,
    reported: Option<(u32, u32)>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(held) = self.held.take() {
            let _ = held.frame.Close();
        }
    }
}

// Indices into the wait set, lowest first: stopping wins over everything and a
// wake-up wins over a continuous stream of frames.
const STOP: usize = 0;
const CLOSED: usize = 1;
const POKE: usize = 2;
const FRAME: usize = 3;

impl Capture {
    fn run(&mut self, handles: &[HANDLE; 4]) -> Result<(), CaptureError> {
        loop {
            let now = monotonic_time()?;
            let timeout = self
                .next_due(now)
                .map_or(TICK, |due| due.saturating_sub(now).min(TICK));
            let woke = wait(handles, timeout)?;
            match woke {
                Some(STOP) => return Ok(()),
                Some(CLOSED) => return Err(CaptureError::SourceClosed),
                Some(FRAME) => self.take_frames()?,
                _ => {}
            }
            if !source_alive(self.source) {
                return Err(CaptureError::SourceClosed);
            }
            let now = monotonic_time()?;
            if self.next_due(now).is_some_and(|due| due <= now) {
                // A newer picture that pacing held back keeps its capture time
                // (never closer to the last one than the pacing allows); a
                // settled size change redelivers the same picture, stamped now.
                let stamp = match self.held.as_ref() {
                    Some(held) if self.dirty => self.stamp(held.time, self.gap()),
                    _ => self.stamp(now, REPEAT_SPACING),
                };
                if self.deliver(stamp, now)? == Flow::Stop {
                    return Ok(());
                }
            }
            if matches!(woke, None | Some(POKE)) {
                match self.handler.tick() {
                    Tick::Idle => {}
                    Tick::Stop => return Ok(()),
                    Tick::Repeat => {
                        let stamp = self.stamp(now, REPEAT_SPACING);
                        if self.deliver(stamp, now)? == Flow::Stop {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// The least time between the timestamps of two delivered pictures.
    fn gap(&self) -> Duration {
        self.control.min_interval() * PACE_NUMERATOR / PACE_DENOMINATOR
    }

    /// `preferred`, but at least `spacing` after the last delivered picture, so
    /// timestamps strictly increase.
    fn stamp(&self, preferred: Duration, spacing: Duration) -> Duration {
        self.last_stamp
            .map_or(preferred, |last| preferred.max(last + spacing))
    }

    /// When the next picture is due without any new frame: a newer frame that
    /// pacing held back, or a settled size change.
    fn next_due(&self, now: Duration) -> Option<Duration> {
        let held = self.held.as_ref()?;
        let paced = self
            .dirty
            .then(|| self.last_stamp.map_or(now, |last| last + self.gap()));
        let settled = self.governor.due(held.content);
        match (paced, settled) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Keeps only the newest of the frames that arrived.
    fn take_frames(&mut self) -> Result<(), CaptureError> {
        let mut newest: Option<Direct3D11CaptureFrame> = None;
        loop {
            match self.wgc.pool.TryGetNextFrame() {
                Ok(frame) => {
                    if let Some(older) = newest.replace(frame) {
                        let _ = older.Close();
                    }
                }
                // No frame is waiting: the call succeeds with a null result.
                Err(error) if error.code().is_ok() => break,
                Err(error) => return Err(native_error("TryGetNextFrame", &error)),
            }
        }
        match newest {
            Some(frame) => self.accept(frame),
            None => Ok(()),
        }
    }

    fn accept(&mut self, frame: Direct3D11CaptureFrame) -> Result<(), CaptureError> {
        let content = frame
            .ContentSize()
            .capture("Direct3D11CaptureFrame::ContentSize")?;
        let (width, height) = (content.Width.max(0) as u32, content.Height.max(0) as u32);
        if width == 0 || height == 0 {
            let _ = frame.Close();
            return Ok(());
        }
        check_native_size(width, height)?;
        if (width, height) != self.pool_size {
            // The source changed size: later frames take the new size. This
            // frame still holds its old surface, cropped by its content size.
            self.wgc
                .pool
                .Recreate(
                    &self.device.winrt,
                    FORMAT,
                    NATIVE_FRAME_COUNT as i32,
                    content,
                )
                .capture("Direct3D11CaptureFramePool::Recreate")?;
            self.pool_size = (width, height);
        }
        if self.reported != Some((width, height)) {
            self.reported = Some((width, height));
            self.status.set(CaptureState::SourceSize { width, height });
        }
        let time = from_hns(
            frame
                .SystemRelativeTime()
                .capture("Direct3D11CaptureFrame::SystemRelativeTime")?
                .Duration,
        );
        let surface = frame.Surface().capture("Direct3D11CaptureFrame::Surface")?;
        let texture = surface_texture(&surface)?;
        if let Some(older) = self.held.replace(Held {
            frame,
            texture,
            content: (width, height),
            time,
        }) {
            let _ = older.frame.Close();
        }
        self.dirty = true;
        Ok(())
    }

    /// Converts the held frame at the current output size and gives it to the
    /// handler.
    fn deliver(&mut self, timestamp: Duration, now: Duration) -> Result<Flow, CaptureError> {
        let Some(held) = &self.held else {
            return Ok(Flow::Continue);
        };
        let output = self.governor.output(held.content, now);
        let handler = &mut self.handler;
        let path = self.converter.path();
        // Zero-copy first, when the handler wants it: the converted texture
        // goes to the handler as it is. A decline falls through to the copy.
        let gpu = if handler.wants_gpu() {
            self.converter.deliver_gpu(
                &self.device,
                &held.texture,
                held.content,
                output,
                timestamp,
                |frame| handler.gpu_frame(frame),
            )?
        } else {
            None
        };
        let flow = match gpu {
            Some(GpuOutcome::Handled(flow)) => flow,
            _ => self.converter.deliver(
                &self.device,
                &held.texture,
                held.content,
                output,
                timestamp,
                |sample| handler.frame(sample),
            )?,
        };
        if self.converter.path() != path {
            self.status
                .set(CaptureState::Converting(self.converter.path()));
        }
        self.dirty = false;
        self.last_stamp = Some(timestamp);
        Ok(flow)
    }
}

/// Decides the output size from the source size, holding a change back until
/// the source stops changing size.
pub(super) struct SizeGovernor {
    config: CaptureConfig,
    current: Option<(u32, u32)>,
    pending: Option<((u32, u32), Duration)>,
}

impl SizeGovernor {
    pub(super) fn new(config: CaptureConfig) -> Self {
        Self {
            config,
            current: None,
            pending: None,
        }
    }

    /// The output size for a picture of the `content` size at `now`. The first
    /// size is taken at once; a different one takes effect after it has held
    /// for [`SETTLE`], and pictures in between keep the previous size (the
    /// source is stretched to it).
    pub(super) fn output(&mut self, content: (u32, u32), now: Duration) -> (u32, u32) {
        let wanted = self.config.fit(content.0, content.1);
        match self.current {
            None => {
                self.current = Some(wanted);
                wanted
            }
            Some(current) if current == wanted => {
                self.pending = None;
                current
            }
            Some(current) => match self.pending {
                Some((size, since)) if size == wanted => {
                    if now.saturating_sub(since) >= SETTLE {
                        self.current = Some(wanted);
                        self.pending = None;
                        wanted
                    } else {
                        current
                    }
                }
                _ => {
                    self.pending = Some((wanted, now));
                    current
                }
            },
        }
    }

    /// When the pending size change (if any, for this `content` size) takes
    /// effect.
    pub(super) fn due(&self, content: (u32, u32)) -> Option<Duration> {
        let (size, since) = self.pending?;
        (size == self.config.fit(content.0, content.1)).then_some(since + SETTLE)
    }
}
