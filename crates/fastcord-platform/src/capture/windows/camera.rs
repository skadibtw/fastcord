//! Media Foundation camera capture (SPEC §8.8, issue #84).
//!
//! One owner thread opens the camera through a synchronous source reader with
//! video processing enabled, so the system converts whatever the device offers
//! (NV12, YUY2, MJPEG) into NV12 at the device's own size. The thread reads one
//! sample at a time into a borrowed [`VideoSample`] and calls the frame
//! handler; there is no queue of its own, and a picture the pacing skips is
//! dropped, never held. Dropping the session stops the thread, shuts the media
//! source down and releases the device.
//!
//! `mf.dll`, `mfplat.dll` and `mfreadwrite.dll` are resolved at run time, like
//! the codecs' Media Foundation entry points, so a Windows N edition without
//! the Media Feature Pack still starts fastcord (the camera is then reported
//! as unsupported).
//!
//! A blocking `ReadSample` cannot be interrupted, so stopping takes up to one
//! frame interval; a camera that stops delivering altogether is abandoned
//! after the stop timeout (the thread frees the device if it ever returns).
//! The permission prompt of Windows' camera privacy setting appears as
//! `E_ACCESSDENIED` when the source is activated.

use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{E_ACCESSDENIED, E_POINTER, HMODULE};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaBuffer, IMFMediaSource, IMFMediaType, IMFSample,
    IMFSourceReader, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_E_NO_MORE_TYPES,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_FLAG,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_VERSION, MFMediaType_Video, MFSTARTUP_LITE, MFVideoFormat_MJPG,
    MFVideoFormat_NV12, MFVideoFormat_YUY2, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{GUID, HRESULT, Interface, PCSTR, PWSTR, s, w};

use super::super::camera::{CameraDevice, CameraFormat, CameraMode, MAX_CAMERAS, choose_mode};
use super::super::{
    CaptureConfig, CaptureError, CaptureState, CaptureStatus, Control, FrameHandler,
    HandlerFactory, StartOptions, Tick, VideoSample,
};
use super::runtime::{
    Apartment, Handle, Native, OrCapture, from_hns, monotonic_time, native_error, wait,
};

/// Native media types read from one camera, so a device that lists thousands
/// cannot make the open unbounded.
const MAX_NATIVE_TYPES: u32 = 256;

/// Pictures may arrive up to this fraction early (nine tenths of the interval)
/// so a 60 Hz camera paced to 30 fps does not lose every other picture.
const PACE_NUMERATOR: u32 = 9;
const PACE_DENOMINATOR: u32 = 10;

const UNAVAILABLE: CaptureError =
    CaptureError::Unsupported("Media Foundation is not available, so cameras cannot be used");

/// The Media Foundation exports used, resolved once.
struct Mf {
    startup: unsafe extern "system" fn(u32, u32) -> HRESULT,
    shutdown: unsafe extern "system" fn() -> HRESULT,
    create_attributes: unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT,
    create_media_type: unsafe extern "system" fn(*mut *mut c_void) -> HRESULT,
    enum_device_sources:
        unsafe extern "system" fn(*mut c_void, *mut *mut Option<IMFActivate>, *mut u32) -> HRESULT,
    create_reader: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT,
}

static MF: LazyLock<Option<Mf>> = LazyLock::new(load);

fn mf() -> Result<&'static Mf, CaptureError> {
    MF.as_ref().ok_or(UNAVAILABLE)
}

fn load() -> Option<Mf> {
    /// Resolves `name` of `module` as a function of type `F`.
    ///
    /// # Safety
    /// `F` must be the export's exact `extern "system"` signature.
    unsafe fn export<F: Copy>(module: HMODULE, name: PCSTR) -> Option<F> {
        // SAFETY: `module` is a loaded library and `name` a NUL-terminated
        // literal.
        let address = unsafe { GetProcAddress(module, name) }?;
        assert_eq!(size_of::<F>(), size_of_val(&address));
        // SAFETY: the caller guarantees `F` is the export's signature; both
        // are plain function pointers of the same size.
        Some(unsafe { std::mem::transmute_copy(&address) })
    }
    // SAFETY: loads system libraries by name from System32 only; they stay
    // loaded for the life of the process, so the resolved pointers never
    // dangle.
    let (platform, foundation, readwrite) = unsafe {
        (
            LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?,
            LoadLibraryExW(w!("mf.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?,
            LoadLibraryExW(w!("mfreadwrite.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?,
        )
    };
    // SAFETY: each field type is the documented signature of its export
    // (mfapi.h, mfidl.h, mfreadwrite.h).
    unsafe {
        Some(Mf {
            startup: export(platform, s!("MFStartup"))?,
            shutdown: export(platform, s!("MFShutdown"))?,
            create_attributes: export(platform, s!("MFCreateAttributes"))?,
            create_media_type: export(platform, s!("MFCreateMediaType"))?,
            enum_device_sources: export(foundation, s!("MFEnumDeviceSources"))?,
            create_reader: export(readwrite, s!("MFCreateSourceReaderFromMediaSource"))?,
        })
    }
}

/// Media Foundation started on this thread (which is in the MTA); balanced on
/// drop.
struct Startup(&'static Mf);

impl Startup {
    fn new(api: &'static Mf) -> Result<Self, CaptureError> {
        // SAFETY: a plain reference-counted start-up with the SDK version this
        // was built for.
        unsafe { (api.startup)(MF_VERSION, MFSTARTUP_LITE) }
            .ok()
            .capture("MFStartup")?;
        Ok(Self(api))
    }
}

impl Drop for Startup {
    fn drop(&mut self) {
        // SAFETY: balances the successful MFStartup in `new`.
        let _ = unsafe { (self.0.shutdown)() };
    }
}

/// An owned interface from a creation function's out-pointer.
///
/// # Safety
/// On success `raw` must be an owned pointer to an object implementing `T`.
unsafe fn created<T: Interface>(
    result: HRESULT,
    raw: *mut c_void,
    operation: &'static str,
) -> Result<T, CaptureError> {
    result.ok().capture(operation)?;
    if raw.is_null() {
        return Err(CaptureError::Native {
            operation,
            code: E_POINTER.0,
        });
    }
    // SAFETY: guaranteed by the caller.
    Ok(unsafe { T::from_raw(raw) })
}

fn attributes(api: &Mf) -> Result<IMFAttributes, CaptureError> {
    let mut raw = ptr::null_mut();
    // SAFETY: `raw` receives an owned IMFAttributes on success.
    unsafe {
        let result = (api.create_attributes)(&mut raw, 2);
        created(result, raw, "MFCreateAttributes")
    }
}

fn string(activate: &IMFActivate, key: &GUID) -> Option<String> {
    let mut value = PWSTR::null();
    let mut length = 0u32;
    // SAFETY: the out-pointers are valid; on success the string is allocated
    // with CoTaskMemAlloc and belongs to the caller.
    unsafe { activate.GetAllocatedString(key, &mut value, &mut length) }.ok()?;
    // SAFETY: a successful call returns a NUL-terminated wide string.
    let text = unsafe { value.to_string() }.ok();
    // SAFETY: frees the allocation made by `GetAllocatedString`.
    unsafe { CoTaskMemFree(Some(value.0 as *const c_void)) };
    text
}

/// The connected cameras and the activation objects that open them, in the
/// system's order. Needs Media Foundation started on this thread.
fn enumerate(api: &Mf) -> Result<Vec<(IMFActivate, CameraDevice)>, CaptureError> {
    let query = attributes(api)?;
    // SAFETY: plain attribute stores of constant GUIDs.
    unsafe {
        query
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .capture("IMFAttributes::SetGUID")?;
    }
    let mut array: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: all pointers are valid for the call; on success the array and
    // its `count` interface references belong to the caller.
    unsafe { (api.enum_device_sources)(query.as_raw(), &mut array, &mut count) }
        .ok()
        .capture("MFEnumDeviceSources")?;
    let mut found = Vec::new();
    if !array.is_null() {
        for index in 0..count as usize {
            // SAFETY: `index < count`; each element is moved out exactly once
            // before the array is freed.
            let Some(activate) = (unsafe { array.add(index).read() }) else {
                continue;
            };
            if found.len() >= MAX_CAMERAS {
                continue;
            }
            let id = string(
                &activate,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            );
            let name = string(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME);
            if let Some(device) =
                id.and_then(|id| CameraDevice::bounded(&id, name.as_deref().unwrap_or_default()))
            {
                found.push((activate, device));
            }
        }
        // SAFETY: the array was allocated by MFEnumDeviceSources with
        // CoTaskMemAlloc and its elements were moved out above.
        unsafe { CoTaskMemFree(Some(array as *const c_void)) };
    }
    Ok(found)
}

pub(in crate::capture) fn list_cameras() -> Result<Vec<CameraDevice>, CaptureError> {
    // A throwaway thread: Media Foundation's objects need the multithreaded
    // apartment and the caller's thread may already be in another one.
    thread::Builder::new()
        .name("fastcord-camera-list".to_owned())
        .spawn(|| {
            let api = mf()?;
            let _apartment = Apartment::mta()?;
            let _startup = Startup::new(api)?;
            Ok(enumerate(api)?
                .into_iter()
                .map(|(_, device)| device)
                .collect())
        })
        .map_err(|_| CaptureError::Worker("could not create the camera listing thread"))?
        .join()
        .unwrap_or(Err(CaptureError::Worker(
            "the camera listing thread panicked",
        )))
}

pub(in crate::capture) fn start_camera(
    device: Option<String>,
    options: &StartOptions,
    control: Arc<Control>,
    status: CaptureStatus,
    factory: HandlerFactory,
) -> Result<Native, CaptureError> {
    // Fail before a thread exists on an N edition.
    mf()?;
    let config = options.config;
    let thread_status = status.clone();
    Native::spawn("fastcord-camera", status, move |stop, _poke| {
        run(device, config, &control, &thread_status, factory, stop)
    })
}

/// The reader's first video stream.
const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

fn unpack(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

fn mode_of(media_type: &IMFMediaType) -> Option<CameraMode> {
    // SAFETY: attribute reads on a live media type.
    let (subtype, size, rate) = unsafe {
        (
            media_type.GetGUID(&MF_MT_SUBTYPE).ok()?,
            media_type.GetUINT64(&MF_MT_FRAME_SIZE).ok()?,
            media_type.GetUINT64(&MF_MT_FRAME_RATE).ok()?,
        )
    };
    let (width, height) = unpack(size);
    Some(CameraMode {
        width,
        height,
        rate: unpack(rate),
        format: if subtype == MFVideoFormat_NV12 {
            CameraFormat::Nv12
        } else if subtype == MFVideoFormat_YUY2 {
            CameraFormat::Yuy2
        } else if subtype == MFVideoFormat_MJPG {
            CameraFormat::Mjpeg
        } else {
            CameraFormat::Other
        },
    })
}

/// The shut-down order of one camera: the reader first, then the source, then
/// the activation object that created it.
struct Camera {
    reader: IMFSourceReader,
    source: IMFMediaSource,
    activate: IMFActivate,
}

impl Drop for Camera {
    fn drop(&mut self) {
        // SAFETY: the source and its activation object are live; both calls
        // only release the device and may fail harmlessly.
        unsafe {
            let _ = self.source.Shutdown();
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// The delivered picture size and its row stride.
#[derive(Clone, Copy)]
struct Layout {
    width: u32,
    height: u32,
    stride: usize,
}

fn current_layout(reader: &IMFSourceReader) -> Result<Layout, CaptureError> {
    // SAFETY: attribute reads on the reader's current type.
    let (width, height, stride) = unsafe {
        let current = reader
            .GetCurrentMediaType(STREAM)
            .capture("IMFSourceReader::GetCurrentMediaType")?;
        let (width, height) = unpack(
            current
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .capture("MF_MT_FRAME_SIZE")?,
        );
        // Tightly packed unless the converter says otherwise; a negative
        // stride is never produced for NV12.
        let stride = current
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|stride| stride as i32)
            .ok()
            .filter(|stride| *stride >= width as i32)
            .map_or(width as usize, |stride| stride as usize);
        (width, height, stride)
    };
    if width == 0 || height == 0 {
        return Err(CaptureError::InvalidSample("the camera reported no size"));
    }
    Ok(Layout {
        width,
        height,
        stride,
    })
}

fn open(
    api: &Mf,
    device: Option<&str>,
    config: CaptureConfig,
) -> Result<(Camera, Layout), CaptureError> {
    let devices = enumerate(api)?;
    let index = match device {
        Some(id) => devices
            .iter()
            .position(|(_, found)| found.id == id)
            .ok_or(CaptureError::DeviceNotFound)?,
        None if devices.is_empty() => return Err(CaptureError::DeviceNotFound),
        None => 0,
    };
    let activate = devices
        .into_iter()
        .nth(index)
        .map(|(activate, _)| activate)
        .ok_or(CaptureError::DeviceNotFound)?;
    // SAFETY: activation creates the media source for this device; the
    // camera privacy setting shows up here as E_ACCESSDENIED.
    let source: IMFMediaSource = unsafe { activate.ActivateObject() }
        .map_err(|error| native_error("IMFActivate::ActivateObject", &error))?;
    let options = attributes(api)?;
    // SAFETY: plain attribute store.
    unsafe {
        options
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .capture("IMFAttributes::SetUINT32")?;
    }
    let mut raw = ptr::null_mut();
    // SAFETY: `source` and `options` are live; `raw` receives an owned reader.
    let reader = unsafe {
        let result = (api.create_reader)(source.as_raw(), options.as_raw(), &mut raw);
        created::<IMFSourceReader>(result, raw, "MFCreateSourceReaderFromMediaSource")
    };
    // From here the device is released by `Camera::drop`, whatever happens.
    let reader = match reader {
        Ok(reader) => reader,
        Err(error) => {
            // SAFETY: releasing the source that was just activated.
            unsafe {
                let _ = source.Shutdown();
                let _ = activate.ShutdownObject();
            }
            return Err(error);
        }
    };
    let camera = Camera {
        reader,
        source,
        activate,
    };
    configure(api, &camera.reader, config)?;
    let layout = current_layout(&camera.reader)?;
    Ok((camera, layout))
}

/// Selects the best native mode, then asks the reader for NV12 at its size and
/// rate.
fn configure(
    api: &Mf,
    reader: &IMFSourceReader,
    config: CaptureConfig,
) -> Result<(), CaptureError> {
    let mut modes = Vec::new();
    let mut types = Vec::new();
    for index in 0..MAX_NATIVE_TYPES {
        // SAFETY: a read of the reader's native type list.
        match unsafe { reader.GetNativeMediaType(STREAM, index) } {
            Ok(media_type) => {
                if let Some(mode) = mode_of(&media_type) {
                    modes.push(mode);
                    types.push(media_type);
                }
            }
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
            Err(error) => return Err(native_error("IMFSourceReader::GetNativeMediaType", &error)),
        }
    }
    let chosen = choose_mode(&modes, config).ok_or(CaptureError::Unsupported(
        "the camera offers no mode up to 1280x720",
    ))?;
    let mode = modes[chosen];
    let mut raw = ptr::null_mut();
    // SAFETY: `raw` receives an owned IMFMediaType; the stores are plain
    // attributes; the reader and the chosen native type are live.
    unsafe {
        reader
            .SetStreamSelection(STREAM, true)
            .capture("IMFSourceReader::SetStreamSelection")?;
        reader
            .SetCurrentMediaType(STREAM, None, &types[chosen])
            .capture("IMFSourceReader::SetCurrentMediaType(native)")?;
        let result = (api.create_media_type)(&mut raw);
        let output: IMFMediaType = created(result, raw, "MFCreateMediaType")?;
        output
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .capture("MF_MT_MAJOR_TYPE")?;
        output
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .capture("MF_MT_SUBTYPE")?;
        output
            .SetUINT64(&MF_MT_FRAME_SIZE, pack(mode.width, mode.height))
            .capture("MF_MT_FRAME_SIZE")?;
        output
            .SetUINT64(&MF_MT_FRAME_RATE, pack(mode.rate.0, mode.rate.1))
            .capture("MF_MT_FRAME_RATE")?;
        output
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .capture("MF_MT_INTERLACE_MODE")?;
        reader
            .SetCurrentMediaType(STREAM, None, &output)
            .capture("IMFSourceReader::SetCurrentMediaType(NV12)")?;
    }
    Ok(())
}

fn run(
    device: Option<String>,
    config: CaptureConfig,
    control: &Arc<Control>,
    status: &CaptureStatus,
    factory: HandlerFactory,
    stop: &Arc<Handle>,
) -> Result<(), CaptureError> {
    // Dropped last: every Media Foundation object below is released first.
    let _apartment = Apartment::mta()?;
    let api = mf()?;
    let _startup = Startup::new(api)?;
    let (camera, layout) = open(api, device.as_deref(), config)?;
    status.set(CaptureState::Capturing);
    status.set(CaptureState::SourceSize {
        width: layout.width,
        height: layout.height,
    });
    let handler = factory();
    let mut capture = Capture {
        handler,
        layout,
        control: Arc::clone(control),
        status: status.clone(),
        last: None,
        camera,
    };
    capture.run(stop)
}

/// The capture thread's state. Field order is drop order: the handler (and
/// the encoder it owns) goes first, the camera last.
struct Capture {
    handler: Box<dyn FrameHandler>,
    layout: Layout,
    control: Arc<Control>,
    status: CaptureStatus,
    /// The timestamp of the last picture delivered.
    last: Option<Duration>,
    camera: Camera,
}

/// Why a `ReadSample` failed, after the camera had started: a refused
/// permission stays one; anything else means the device stopped delivering
/// (unplugged, taken by another application, driver reset).
fn read_error(error: &windows::core::Error) -> CaptureError {
    if error.code() == E_ACCESSDENIED {
        CaptureError::PermissionDenied
    } else {
        CaptureError::SourceClosed
    }
}

impl Capture {
    fn run(&mut self, stop: &Arc<Handle>) -> Result<(), CaptureError> {
        loop {
            if wait(&[stop.0], Duration::ZERO)?.is_some() {
                return Ok(());
            }
            let mut flags = 0u32;
            let mut stamp = 0i64;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: the out-pointers are valid for the call. This blocks
            // until the camera delivers a sample (or fails).
            unsafe {
                self.camera.reader.ReadSample(
                    STREAM,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut stamp),
                    Some(&mut sample),
                )
            }
            .map_err(|error| read_error(&error))?;
            let flags = MF_SOURCE_READER_FLAG(flags as i32);
            if flags.0 & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) != 0 {
                return Err(CaptureError::SourceClosed);
            }
            if flags.0 & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 != 0 {
                let layout = current_layout(&self.camera.reader)?;
                if (layout.width, layout.height) != (self.layout.width, self.layout.height) {
                    self.status.set(CaptureState::SourceSize {
                        width: layout.width,
                        height: layout.height,
                    });
                }
                self.layout = layout;
            }
            if let Some(sample) = sample
                && self.deliver(&sample, stamp)? == super::super::Flow::Stop
            {
                return Ok(());
            }
            if self.handler.tick() == Tick::Stop {
                return Ok(());
            }
        }
    }

    /// Hands one sample to the handler unless pacing skips it.
    fn deliver(
        &mut self,
        sample: &IMFSample,
        stamp: i64,
    ) -> Result<super::super::Flow, CaptureError> {
        let time = if stamp > 0 {
            from_hns(stamp)
        } else {
            monotonic_time()?
        };
        let gap = self.control.min_interval() * PACE_NUMERATOR / PACE_DENOMINATOR;
        if self.last.is_some_and(|last| time < last + gap) {
            return Ok(super::super::Flow::Continue);
        }
        // SAFETY: a contiguous view of the sample's buffers.
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .capture("IMFSample::ConvertToContiguousBuffer")?;
        let locked = Locked::new(&buffer)?;
        let layout = self.layout;
        let picture = VideoSample::nv12(
            layout.width,
            layout.height,
            layout.stride,
            locked.bytes(),
            time,
        )?;
        self.last = Some(time);
        Ok(self.handler.frame(&picture))
    }
}

/// A locked media buffer, unlocked on drop.
struct Locked<'a> {
    buffer: &'a IMFMediaBuffer,
    data: *const u8,
    length: usize,
}

impl<'a> Locked<'a> {
    fn new(buffer: &'a IMFMediaBuffer) -> Result<Self, CaptureError> {
        let mut data = ptr::null_mut();
        let mut length = 0u32;
        // SAFETY: the out-pointers are valid; a successful lock must be
        // balanced by `Unlock`, which `drop` does.
        unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }
            .capture("IMFMediaBuffer::Lock")?;
        Ok(Self {
            buffer,
            data,
            length: length as usize,
        })
    }

    fn bytes(&self) -> &[u8] {
        if self.data.is_null() {
            return &[];
        }
        // SAFETY: the lock keeps `length` bytes valid at `data` until it is
        // released in `drop`, and the borrow cannot outlive `self`.
        unsafe { std::slice::from_raw_parts(self.data, self.length) }
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: balances the successful `Lock` in `new`.
        let _ = unsafe { self.buffer.Unlock() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_sizes_and_rates_unpack_from_their_packed_attributes() {
        assert_eq!(unpack(pack(1280, 720)), (1280, 720));
        assert_eq!(unpack(pack(30000, 1001)), (30000, 1001));
        assert_eq!(pack(1, 2), 0x0000_0001_0000_0002);
    }

    #[test]
    fn a_failed_read_means_the_camera_stopped_unless_permission_was_refused() {
        let denied = windows::core::Error::from(E_ACCESSDENIED);
        assert_eq!(read_error(&denied), CaptureError::PermissionDenied);
        let gone = windows::core::Error::from(HRESULT(0xC00D3EA2_u32 as i32));
        assert_eq!(read_error(&gone), CaptureError::SourceClosed);
    }

    /// Opens the first camera, captures a few pictures and releases it. Needs a
    /// real camera and its privacy permission, so it is run by hand:
    /// `cargo test -p fastcord-platform camera_hardware -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a connected camera"]
    fn camera_hardware_delivers_bounded_nv12_pictures_and_releases_the_device() {
        use std::sync::mpsc;
        use std::time::Instant;

        struct Collect(mpsc::Sender<(u32, u32, usize)>);
        impl FrameHandler for Collect {
            fn frame(&mut self, sample: &VideoSample<'_>) -> super::super::super::Flow {
                let _ = self
                    .0
                    .send((sample.width(), sample.height(), sample.stride()));
                super::super::super::Flow::Continue
            }
            fn tick(&mut self) -> Tick {
                Tick::Idle
            }
        }
        let cameras = super::super::super::list_cameras().expect("listing cameras");
        println!("cameras: {cameras:?}");
        assert!(!cameras.is_empty(), "no camera is connected");
        let (sender, receiver) = mpsc::channel();
        let session = super::super::super::start_camera(
            Some(&cameras[0].id),
            StartOptions::new(CaptureConfig::SCREEN_SHARE_720P30),
            move || Collect(sender),
        )
        .expect("starting the camera");
        let started = Instant::now();
        let mut pictures = Vec::new();
        while pictures.len() < 10 && started.elapsed() < Duration::from_secs(10) {
            if let Ok(picture) = receiver.recv_timeout(Duration::from_millis(500)) {
                pictures.push(picture);
            }
        }
        println!("status: {:?}", session.status().snapshot());
        session.stop().expect("clean stop");
        assert!(pictures.len() >= 10, "only {} pictures", pictures.len());
        for (width, height, stride) in pictures {
            assert!(width <= 1280 && height <= 720 && stride >= width as usize);
        }
        // The device is free again: opening it a second time works.
        let (sender, _receiver) = mpsc::channel();
        super::super::super::start_camera(
            Some(&cameras[0].id),
            StartOptions::new(CaptureConfig::SCREEN_SHARE_720P30),
            move || Collect(sender),
        )
        .expect("reopening")
        .stop()
        .expect("second stop");
    }
}
