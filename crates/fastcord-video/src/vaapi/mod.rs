//! VA-API hardware H.264 encode/decode for Linux (SPEC §8.5; ADR 0001).
//!
//! Every DRM render node under `/dev/dri` is probed once at listing time for
//! an H.264 encoder (`VAEntrypointEncSlice` on the Constrained Baseline
//! profile, with `VA_RC_CBR` rate control) and an H.264 decoder
//! (`VAEntrypointVLD`); each passing device becomes a hardware candidate in
//! ascending render-node order. The codec wrappers here own their VA-API
//! display and every resource created on it, so the module mirrors the native
//! object lifetimes with tiny RAII guards and contains all of the crate's
//! remaining `unsafe` FFI in one place.
//!
//! Discovered devices are re-initialized per opened codec: one display per
//! codec object keeps each codec independent (the share pipeline reopens
//! encoders on size or bitrate changes) and matches the codec contract that
//! codec objects belong to the thread that opened them. Resources are
//! released in reverse acquisition order: buffers, image, context, surfaces,
//! config, then the display, which is the codec field's declaration order.
//!
//! Encode drains the previous picture while the next one is submitted
//! (pipeline depth 1): the source surface of picture *k* − 1 is synced before
//! its coded buffer is read and its buffers are destroyed. Decode is fully
//! synchronous per access unit: the driver renders, the target surface is
//! synced, and `vaGetImage` copies into a mapped image handed to the caller
//! as a borrowed [`Nv12`]. Both keep every queue byte-bounded (coded-buffer
//! cap: [`crate::h264::MAX_ACCESS_UNIT_BYTES`]).
#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]
// Some fields only keep native resources alive until drop; never read.
#![allow(
    dead_code,
    non_upper_case_globals,
    clippy::bool_assert_comparison,
    clippy::too_many_arguments
)]
// Bindings, encoder and decoder are children of this module, so the allow
// above covers the whole VA-API tree; `platform_h264` (a sibling of `vaapi`
// in the crate) stays unsafe-free.

pub(crate) mod bindings;
mod decoder;
mod encoder;

pub use decoder::VaDecoder;
pub use encoder::VaEncoder;

use std::ffi;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;

use crate::codec::{Backend, BackendKind, BackendPreference, CodecError, EncoderConfig};
use crate::select::{Candidate, OpenError, Selected, select};

use bindings::*;

/// Reported as [`Backend::api`].
pub const API: &str = "VA-API";

/// How long [`sync_surface`] waits for one surface before reporting
/// [`CodecError::Stalled`] (three seconds, in `vaSyncSurface2` nanoseconds).
const SYNC_TIMEOUT_NS: u64 = 3_000_000_000;

/// Upper bound for `vaMaxNumEntrypoints` answers; real drivers report a
/// handful of entrypoints.
const MAX_ENTRYPOINTS: usize = 64;

/// H.264 profiles a stream might need, in probe order. Constrained Baseline
/// first because a device that decodes it also decodes everything a
/// constrained Baseline device decodes.
const DECODER_PROFILES: [VAProfile; 4] = [
    VAProfile_VAProfileH264ConstrainedBaseline,
    VAProfile_VAProfileH264Main,
    VAProfile_VAProfileH264High,
    VAProfile_VAProfileH264Baseline,
];

/// The H.264 encoder profiles a device may report, Constrained Baseline
/// preferred. Both are the decoding-safe, B-frame-free profiles the codec
/// contract describes ([`EncoderConfig`]).
const ENCODER_PROFILES: [VAProfile; 2] = [
    VAProfile_VAProfileH264ConstrainedBaseline,
    VAProfile_VAProfileH264Baseline,
];

/// Maps a failing VA-API status onto the codec contract. `DECODING_ERROR`
/// (the device rejected the bitstream) and `TIMEDOUT` (the codec stopped
/// responding) have their own categories; every other status reaches logs
/// through the honest status code in [`CodecError::Platform`].
pub(crate) fn to_error(operation: &'static str, status: VAStatus) -> CodecError {
    match status as u32 {
        VA_STATUS_ERROR_DECODING_ERROR => {
            CodecError::InvalidBitstream("the hardware decoder rejected the access unit")
        }
        VA_STATUS_ERROR_TIMEDOUT => CodecError::Stalled("the hardware codec stopped responding"),
        code => CodecError::Platform {
            operation,
            code: code as i32,
        },
    }
}

/// [`Ok`] for `VA_STATUS_SUCCESS`, the mapped error otherwise.
pub(crate) fn check(operation: &'static str, status: VAStatus) -> Result<(), CodecError> {
    if status == VA_STATUS_SUCCESS as VAStatus {
        Ok(())
    } else {
        Err(to_error(operation, status))
    }
}

/// An initialized VA-API display on one DRM render node.
///
/// Kept behind [`Rc`] so every resource wrapper destroys itself through the
/// one live display instance: the codec owns the last clone, declared as its
/// last field, so `vaTerminate` runs only after the codec's other resources
/// (image, context, surfaces, config) have already been released.
pub(crate) struct Display {
    #[allow(dead_code)] // owns the DRM fd the display handle uses
    file: fs::File,
    handle: VADisplay,
}

impl Display {
    /// Opens the DRM device and initializes VA-API on it.
    pub(crate) fn open(device: &Path) -> Result<Rc<Self>, CodecError> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device)
            .map_err(|_| {
                CodecError::PlatformUnavailable("the VA-API device could not be opened")
            })?;
        // SAFETY: `vaGetDisplayDRM` borrows the file descriptor, which stays
        // open until `self.file` is dropped (after `vaTerminate` in `Drop`).
        let handle = unsafe { vaGetDisplayDRM(file.as_raw_fd()) };
        if handle.is_null() {
            return Err(to_error(
                "vaGetDisplayDRM",
                VA_STATUS_ERROR_INVALID_DISPLAY as VAStatus,
            ));
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: one-time initialization of the display just acquired; the
        // out-pointers are plain locals for the driver to write.
        check("vaInitialize", unsafe {
            vaInitialize(handle, &mut major, &mut minor)
        })?;
        Ok(Rc::new(Self { file, handle }))
    }

    fn handle(&self) -> VADisplay {
        self.handle
    }

    /// `vaQueryVendorString`, or an empty string when the driver does not say.
    ///
    /// The string never reaches logs or outputs on its own; it is only shown
    /// inside the candidate (`Backend`) name next to the device path.
    pub(crate) fn vendor(&self) -> String {
        // SAFETY: the returned pointer is owned by the display and stays
        // valid until `vaTerminate`, which happens after this read.
        let string = unsafe { vaQueryVendorString(self.handle()) };
        if string.is_null() {
            String::new()
        } else {
            // SAFETY: `vaQueryVendorString` contracts a NUL-terminated C string.
            unsafe { ffi::CStr::from_ptr(string) }
                .to_string_lossy()
                .into_owned()
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: initialized in `open` and terminated exactly once here;
        // other drop impls run before this one (see the struct's field order
        // at each use site) so nothing needs the display afterwards.
        let _ = unsafe { vaTerminate(self.handle) };
    }
}

/// One set of VA-API surfaces tied to one display (encoder: one source and a
/// two-slot reconstruction ring; decoder: the six-slot output ring).
pub(crate) struct Surfaces {
    display: Rc<Display>,
    ids: Vec<VASurfaceID>,
}

impl Surfaces {
    pub(crate) fn create(
        display: &Rc<Display>,
        count: usize,
        width: u32,
        height: u32,
    ) -> Result<Self, CodecError> {
        let mut ids = vec![VA_INVALID_SURFACE as VASurfaceID; count];
        // SAFETY: `ids` holds exactly `count` writable surface slots for
        // libva to fill; the attribute list is legitimately absent.
        check("vaCreateSurfaces", unsafe {
            vaCreateSurfaces(
                display.handle(),
                VA_RT_FORMAT_YUV420,
                width,
                height,
                ids.as_mut_ptr(),
                ids.len() as u32,
                ptr::null_mut(),
                0,
            )
        })?;
        Ok(Self {
            display: Rc::clone(display),
            ids,
        })
    }

    pub(crate) fn id(&self, index: usize) -> VASurfaceID {
        self.ids[index]
    }

    pub(crate) fn as_slice(&self) -> &[VASurfaceID] {
        &self.ids
    }
}

impl Drop for Surfaces {
    fn drop(&mut self) {
        // SAFETY: the surfaces exist until now and are destroyed exactly once.
        let _ = unsafe {
            vaDestroySurfaces(
                self.display.handle(),
                self.ids.as_mut_ptr(),
                self.ids.len() as i32,
            )
        };
    }
}

/// A VA-API configuration (`VAConfigID`): profile, entrypoint and attributes.
pub(crate) struct ConfigId {
    display: Rc<Display>,
    id: VAConfigID,
}

impl ConfigId {
    pub(crate) fn create(
        display: &Rc<Display>,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        attribs: &[VAConfigAttrib],
    ) -> Result<Self, CodecError> {
        let mut id = VA_INVALID_ID as VAConfigID;
        // SAFETY: `attribs` outlives the copied call; `id` is one writable slot.
        check("vaCreateConfig", unsafe {
            vaCreateConfig(
                display.handle(),
                profile,
                entrypoint,
                attribs.as_ptr() as *mut VAConfigAttrib,
                attribs.len() as i32,
                &mut id,
            )
        })?;
        Ok(Self {
            display: Rc::clone(display),
            id,
        })
    }

    pub(crate) fn id(&self) -> VAConfigID {
        self.id
    }
}

impl Drop for ConfigId {
    fn drop(&mut self) {
        // SAFETY: created in `create`, destroyed exactly once.
        let _ = unsafe { vaDestroyConfig(self.display.handle(), self.id) };
    }
}

/// A VA-API decode/encode context (`VAContextID`).
pub(crate) struct ContextId {
    display: Rc<Display>,
    id: VAContextID,
}

impl ContextId {
    pub(crate) fn create(
        display: &Rc<Display>,
        config_id: VAConfigID,
        width: i32,
        height: i32,
        flag: i32,
        render_targets: &[VASurfaceID],
    ) -> Result<Self, CodecError> {
        let mut id = VA_INVALID_ID as VAContextID;
        // SAFETY: `render_targets` is an existing surface array as wide as
        // its length; `id` is one writable slot.
        check("vaCreateContext", unsafe {
            vaCreateContext(
                display.handle(),
                config_id,
                width,
                height,
                flag,
                render_targets.as_ptr() as *mut VASurfaceID,
                render_targets.len() as i32,
                &mut id,
            )
        })?;
        Ok(Self {
            display: Rc::clone(display),
            id,
        })
    }

    pub(crate) fn id(&self) -> VAContextID {
        self.id
    }
}

impl Drop for ContextId {
    fn drop(&mut self) {
        // SAFETY: created in `create`, destroyed exactly once; runs before the
        // surfaces and config it references (owner field order).
        let _ = unsafe { vaDestroyContext(self.display.handle(), self.id) };
    }
}

/// An NV12 [`VAImage`] on one display, reused across pictures: the encoder
/// uploads its source frames through it, the decoder reads decoded surfaces
/// through it.
pub(crate) struct Image {
    display: Rc<Display>,
    info: VAImage,
}

impl Image {
    pub(crate) fn create(
        display: &Rc<Display>,
        width: i32,
        height: i32,
    ) -> Result<Self, CodecError> {
        let mut format = VAImageFormat {
            fourcc: VA_FOURCC_NV12,
            byte_order: 0, // VA_LSB_FIRST
            bits_per_pixel: 12,
            depth: 8,
            red_mask: 0,
            green_mask: 0,
            blue_mask: 0,
            alpha_mask: 0,
            va_reserved: [0; 4],
        };
        let mut info = VAImage {
            image_id: VA_INVALID_ID as VAImageID,
            format: VAImageFormat {
                fourcc: 0,
                byte_order: 0,
                bits_per_pixel: 0,
                depth: 0,
                red_mask: 0,
                green_mask: 0,
                blue_mask: 0,
                alpha_mask: 0,
                va_reserved: [0; 4],
            },
            buf: VA_INVALID_ID as VABufferID,
            width: 0,
            height: 0,
            data_size: 0,
            num_planes: 0,
            pitches: [0; 3],
            offsets: [0; 3],
            num_palette_entries: 0,
            entry_bytes: 0,
            component_order: [0; 4],
            va_reserved: [0; 4],
        };
        // SAFETY: `format` is a valid input record the driver reads;
        // `info` is one writable output record.
        check("vaCreateImage", unsafe {
            vaCreateImage(display.handle(), &mut format, width, height, &mut info)
        })?;
        Ok(Self {
            display: Rc::clone(display),
            info,
        })
    }

    pub(crate) fn info(&self) -> &VAImage {
        &self.info
    }

    /// Copies a decoded surface into the image and hands the mapped bytes to
    /// `sink`, which may borrow them only for the duration of the call (see
    /// `VaDecoder::decode` for the NV12 view built from the pitches).
    pub(crate) fn read_surface(
        &self,
        surface: VASurfaceID,
        width: u32,
        height: u32,
        sink: &mut dyn FnMut(&[u8]),
    ) -> Result<(), CodecError> {
        let handle = self.display.handle();
        check("vaGetImage", unsafe {
            vaGetImage(handle, surface, 0, 0, width, height, self.info.image_id)
        })?;
        Synchronized::new(handle, self.info.buf, self.info.data_size as usize).read(sink)
    }

    /// Copies an NV12 frame into the mapped image (zeroing the driver's
    /// alignment padding first) and puts the image onto `surface`, making it
    /// the encoder's source picture.
    fn upload(
        &self,
        surface: VASurfaceID,
        source: &crate::frame::Nv12<'_>,
    ) -> Result<(), CodecError> {
        let handle = self.display.handle();
        Synchronized::new(handle, self.info.buf, self.info.data_size as usize).write(|bytes| {
            bytes.fill(0);
            let pitch_y = self.info.pitches[0] as usize;
            let offset_y = self.info.offsets[0] as usize;
            let pitch_uv = self.info.pitches[1] as usize;
            let offset_uv = self.info.offsets[1] as usize;
            for row in 0..source.height() as usize {
                let destination = offset_y + pitch_y * row;
                bytes[destination..destination + source.luma_row_bytes()]
                    .copy_from_slice(source.luma_row(row));
            }
            for row in 0..source.chroma_rows() {
                let destination = offset_uv + pitch_uv * row;
                bytes[destination..destination + source.chroma_row_bytes()]
                    .copy_from_slice(source.chroma_row(row));
            }
        })?;
        check("vaPutImage", unsafe {
            vaPutImage(
                handle,
                surface,
                self.info.image_id,
                0,
                0,
                u32::from(self.info.width),
                u32::from(self.info.height),
                0,
                0,
                u32::from(self.info.width),
                u32::from(self.info.height),
            )
        })
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: created in `create`, destroyed exactly once; the image does
        // not own the surfaces it reads.
        let _ = unsafe { vaDestroyImage(self.display.handle(), self.info.image_id) };
    }
}

/// A mapped VA buffer with its copy into (or out of) Rust slices.
///
/// Holds the display handle and the buffer id for exactly one
/// `vaMapBuffer`..`vaUnmapBuffer` window, where the buffer stays mapped.
/// The mapped byte count is fixed before mapping: image buffers report their
/// mapped size as the image's `data_size` field, and a coded buffer's total
/// is the size the buffer was created with.
struct Synchronized {
    display: VADisplay,
    buffer: VABufferID,
    size: usize,
}

impl Synchronized {
    fn new(display: VADisplay, buffer: VABufferID, size: usize) -> Self {
        Self {
            display,
            buffer,
            size,
        }
    }

    fn read(&self, sink: &mut dyn FnMut(&[u8])) -> Result<(), CodecError> {
        let mut mapped: *mut ffi::c_void = ptr::null_mut();
        Self::map(self.display, self.buffer, &mut mapped)?;
        // SAFETY: `vaMapBuffer` succeeded, so `mapped` points at exactly
        // `self.size` readable bytes until `vaUnmapBuffer` below; `sink` has
        // finished using them by then.
        let bytes = unsafe { std::slice::from_raw_parts_mut(mapped.cast::<u8>(), self.size) };
        sink(bytes);
        Self::unmap(self.display, self.buffer)
    }

    fn write(&self, writer: impl FnOnce(&mut [u8])) -> Result<(), CodecError> {
        let mut mapped: *mut ffi::c_void = ptr::null_mut();
        Self::map(self.display, self.buffer, &mut mapped)?;
        // SAFETY: as in `read`, `mapped` points at exactly `self.size`
        // writable bytes until `vaUnmapBuffer` below.
        let bytes = unsafe { std::slice::from_raw_parts_mut(mapped.cast::<u8>(), self.size) };
        writer(bytes);
        Self::unmap(self.display, self.buffer)
    }

    /// `vaMapBuffer`, with the mapped pointer written to `mapped`.
    fn map(
        display: VADisplay,
        buffer: VABufferID,
        mapped: &mut *mut ffi::c_void,
    ) -> Result<(), CodecError> {
        // SAFETY: `mapped` is one writable pointer slot; the returned pointer
        // is only dereferenced inside `read`/`write` between map and unmap.
        check("vaMapBuffer", unsafe {
            vaMapBuffer(display, buffer, mapped)
        })
    }

    fn unmap(display: VADisplay, buffer: VABufferID) -> Result<(), CodecError> {
        check("vaUnmapBuffer", unsafe { vaUnmapBuffer(display, buffer) })
    }
}

/// A VA-API buffer created for one picture's parameter or coded data, with
/// its `vaDestroyBuffer` released as soon as the picture has been drained.
pub(crate) struct NativeBuffer {
    display: Rc<Display>,
    id: VABufferID,
}

impl NativeBuffer {
    /// Creates the buffer with `data` copied in at creation time (CVL copies
    /// at `vaCreateBuffer`, matching the FFmpeg pattern).
    pub(crate) fn create(
        display: &Rc<Display>,
        context: VAContextID,
        kind: VABufferType,
        data: &[u8],
    ) -> Result<Self, CodecError> {
        let mut id = VA_INVALID_ID as VABufferID;
        // SAFETY: `data` is an existing slice the driver copies from during
        // the call; `id` is one writable slot.
        check("vaCreateBuffer", unsafe {
            vaCreateBuffer(
                display.handle(),
                context,
                kind,
                data.len() as u32,
                1,
                data.as_ptr() as *mut ffi::c_void,
                &mut id,
            )
        })?;
        Ok(Self {
            display: Rc::clone(display),
            id,
        })
    }

    /// Creates an empty output buffer of `size` bytes (coded data buffers),
    /// which the driver fills during `vaEndPicture`.
    pub(crate) fn create_empty(
        display: &Rc<Display>,
        context: VAContextID,
        kind: VABufferType,
        size: usize,
    ) -> Result<Self, CodecError> {
        let mut id = VA_INVALID_ID as VABufferID;
        // SAFETY: an empty (not pre-filled) buffer creation with no input data,
        // exactly what `vaCreateBuffer` supports for driver-written buffers.
        check("vaCreateBuffer", unsafe {
            vaCreateBuffer(
                display.handle(),
                context,
                kind,
                size as u32,
                1,
                ptr::null_mut(),
                &mut id,
            )
        })?;
        Ok(Self {
            display: Rc::clone(display),
            id,
        })
    }

    pub(crate) fn id(&self) -> VABufferID {
        self.id
    }
}

impl Drop for NativeBuffer {
    fn drop(&mut self) {
        // SAFETY: created in `create`/`create_empty`, destroyed exactly once,
        // after the driver's `vaEndPicture` has consumed the contents.
        let _ = unsafe { vaDestroyBuffer(self.display.handle(), self.id) };
    }
}

/// Blocks until all pending operations on `surface` have completed, or
/// reports [`CodecError::Stalled`] after [`SYNC_TIMEOUT_NS`]. Falls back to
/// the blocking `vaSyncSurface` when the driver does not implement the
/// timeout-aware `vaSyncSurface2`.
pub(crate) fn sync_surface(display: &Rc<Display>, surface: VASurfaceID) -> Result<(), CodecError> {
    let handle = display.handle();
    // SAFETY: the display and surface are live; the timeout is a plain value.
    let status = unsafe { vaSyncSurface2(handle, surface, SYNC_TIMEOUT_NS) };
    if status as u32 == VA_STATUS_ERROR_UNIMPLEMENTED {
        // SAFETY: as above, the legacy fully blocking variant.
        return check("vaSyncSurface", unsafe { vaSyncSurface(handle, surface) });
    }
    check("vaSyncSurface2", status)
}

/// A [`VAPictureH264`] for an unused `ReferenceFrames` entry: removed from
/// the list rather than pointing at a surface.
pub(crate) fn invalid_picture() -> VAPictureH264 {
    VAPictureH264 {
        picture_id: VA_INVALID_SURFACE as VASurfaceID,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID | VA_PICTURE_H264_NON_EXISTING,
        TopFieldOrderCnt: 0,
        BottomFieldOrderCnt: 0,
        va_reserved: [0; 4],
    }
}

/// A [`VAPictureH264`] for a live reference or shortcut picture reference.
pub(crate) fn picture(
    surface: VASurfaceID,
    frame_idx: u32,
    flags: u32,
    top_field_order_count: i32,
    bottom_field_order_count: i32,
) -> VAPictureH264 {
    VAPictureH264 {
        picture_id: surface,
        frame_idx,
        flags,
        TopFieldOrderCnt: top_field_order_count,
        BottomFieldOrderCnt: bottom_field_order_count,
        va_reserved: [0; 4],
    }
}

/// Fills every `ReferenceFrames` slot still left over from the previous
/// picture with [`invalid_picture`].
pub(crate) fn clear_references(reference_frames: &mut [VAPictureH264]) {
    for reference in reference_frames.iter_mut() {
        *reference = invalid_picture();
    }
}

/// Lists the DRM render nodes (`/dev/dri/renderD128`), sorted numerically.
///
/// Devices that do not follow the naming pattern (or that cannot be listed)
/// return `Ok`/some shape, but only the sorted render nodes are kept;
/// everything else comes from the caller.
fn render_nodes_in(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut nodes: Vec<(u32, PathBuf)> = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let index = entry
                .file_name()
                .to_str()?
                .strip_prefix("renderD")?
                .parse()
                .ok()?;
            Some((index, entry.path()))
        })
        .collect();
    nodes.sort_unstable_by_key(|&(index, _)| index);
    Ok(nodes.into_iter().map(|(_, path)| path).collect())
}

/// [`render_nodes_in`] on the canonical `/dev/dri`.
pub(crate) fn render_nodes() -> std::io::Result<Vec<PathBuf>> {
    render_nodes_in(Path::new("/dev/dri"))
}

/// One DRM render device, with the H.264 encoder and decoder support it
/// reported when it was probed at listing time.
pub(crate) struct HardwareSource {
    /// The DRM render node (`/dev/dri/renderD128`).
    pub(crate) path: PathBuf,
    /// `vaQueryVendorString`, when the device reports one.
    pub(crate) vendor: Option<String>,
    /// The hardware encoder's profile (Constrained Baseline preferred),
    /// `None` when the device has no usable H.264 encoder.
    pub(crate) encoder: Option<VAProfile>,
    /// Whether the device reported `VAEntrypointVLD` for at least one
    /// decodable H.264 profile.
    pub(crate) decoder: bool,
}

impl HardwareSource {
    /// Opens the hardware encoder on this device.
    pub(crate) fn open_encoder(self, config: EncoderConfig) -> Result<VaEncoder, CodecError> {
        let profile = self
            .encoder
            .ok_or(CodecError::Unsupported("this device has no H.264 encoder"))?;
        encoder::VaEncoder::open(&self.path, profile, config)
    }

    /// Opens the hardware decoder on this device. The display is kept for
    /// the first access unit, which chooses the stream's actual profile.
    pub(crate) fn open_decoder(self) -> Result<VaDecoder, CodecError> {
        if !self.decoder {
            return Err(CodecError::Unsupported("this device has no H.264 decoder"));
        }
        decoder::VaDecoder::open(&self.path)
    }

    /// The candidate for `Backend::name`: `"<vendor> on <device>"`.
    pub(crate) fn name(&self) -> String {
        format!(
            "{} on {}",
            self.vendor.as_deref().unwrap_or("unknown vendor"),
            self.path.display()
        )
    }
}

/// Probes `profile`/`VAEntrypointEncSlice`, requiring a YUV420 render target
/// and `VA_RC_CBR` rate control. Answers the profile to encode with, or none.
fn probe_encoder(display: &Display, profile: VAProfile) -> Option<VAProfile> {
    let wanted = VAEntrypoint_VAEntrypointEncSlice;
    if !entrypoints(display, profile).contains(&wanted) {
        return None;
    }
    let mut attribs = [
        VAConfigAttrib {
            type_: VAConfigAttribType_VAConfigAttribRTFormat,
            value: VA_ATTRIB_NOT_SUPPORTED,
        },
        VAConfigAttrib {
            type_: VAConfigAttribType_VAConfigAttribRateControl,
            value: VA_ATTRIB_NOT_SUPPORTED,
        },
    ];
    // SAFETY: `attribs` is a real two-element array the driver fills in.
    let status = unsafe {
        vaGetConfigAttributes(
            display.handle(),
            profile,
            wanted,
            attribs.as_mut_ptr(),
            attribs.len() as i32,
        )
    };
    if status != VA_STATUS_SUCCESS as VAStatus {
        return None;
    }
    (attribs[0].value == VA_RT_FORMAT_YUV420 && attribs[1].value & VA_RC_CBR == VA_RC_CBR)
        .then_some(profile)
}

/// Whether the display supports at least one of `profiles` with `VAEntrypointVLD`.
fn probe_decoder(display: &Display, profiles: &[VAProfile]) -> bool {
    let wanted = VAEntrypoint_VAEntrypointVLD;
    profiles
        .iter()
        .any(|profile| entrypoints(display, *profile).contains(&wanted))
}

/// `vaQueryConfigEntrypoints` for `profile`, with sane bounds.
fn entrypoints(display: &Display, profile: VAProfile) -> Vec<VAEntrypoint> {
    // SAFETY: a read-only query; the count is a driver-written scalar.
    let maximum = unsafe { vaMaxNumEntrypoints(display.handle()) };
    if maximum <= 0 {
        return Vec::new();
    }
    let capacity = (maximum as usize).min(MAX_ENTRYPOINTS);
    let mut list = vec![0 as VAEntrypoint; capacity];
    let mut count: i32 = 0;
    // SAFETY: `list` has `capacity` writable entries; `count` is one scalar.
    let status = unsafe {
        vaQueryConfigEntrypoints(display.handle(), profile, list.as_mut_ptr(), &mut count)
    };
    if status != VA_STATUS_SUCCESS as VAStatus {
        return Vec::new();
    }
    list.truncate(count.clamp(0, capacity as i32) as usize);
    list
}

/// Hardware candidates in the platform's device order (`/dev/dri/renderD*`,
/// numerically ascending). A device that does not come up or that reports no
/// usable H.264 support is skipped, not an error; a failed discovery
/// (`/dev/dri` unreadable) fails before any candidate exists.
pub(crate) fn encoder_candidates() -> Result<Vec<Candidate<HardwareSource>>, CodecError> {
    let nodes = render_nodes()
        .map_err(|_| CodecError::PlatformUnavailable("VA-API DRM device discovery"))?;
    let mut candidates = Vec::new();
    for path in nodes {
        let Ok(display) = Display::open(&path) else {
            continue;
        };
        let vendor = display.vendor();
        let encoder = ENCODER_PROFILES
            .into_iter()
            .find_map(|p| probe_encoder(&display, p));
        let decoder = probe_decoder(&display, &DECODER_PROFILES);
        if encoder.is_none() && !decoder {
            continue;
        }
        candidates.push(Candidate {
            kind: BackendKind::Hardware,
            name: format!(
                "{} on {}",
                if vendor.is_empty() {
                    "unknown vendor"
                } else {
                    &vendor
                },
                path.display()
            ),
            source: HardwareSource {
                path,
                vendor: (!vendor.is_empty()).then_some(vendor),
                encoder,
                decoder,
            },
        });
    }
    Ok(candidates)
}

/// Hardware decoder candidates, in the same order as [`encoder_candidates`].
pub(crate) fn decoder_candidates() -> Result<Vec<Candidate<HardwareSource>>, CodecError> {
    let nodes = render_nodes()
        .map_err(|_| CodecError::PlatformUnavailable("VA-API DRM device discovery"))?;
    let mut candidates = Vec::new();
    for path in nodes {
        let Ok(display) = Display::open(&path) else {
            continue;
        };
        let vendor = display.vendor();
        if !probe_decoder(&display, &DECODER_PROFILES) {
            continue;
        }
        candidates.push(Candidate {
            kind: BackendKind::Hardware,
            name: format!(
                "{} on {}",
                if vendor.is_empty() {
                    "unknown vendor"
                } else {
                    &vendor
                },
                path.display()
            ),
            source: HardwareSource {
                path,
                vendor: (!vendor.is_empty()).then_some(vendor),
                encoder: ENCODER_PROFILES
                    .into_iter()
                    .find_map(|p| probe_encoder(&display, p)),
                decoder: true,
            },
        });
    }
    Ok(candidates)
}

/// The backend both VA-API codecs report.
pub(crate) fn backend(kind: BackendKind, name: String) -> Backend {
    Backend {
        kind,
        api: API,
        name,
    }
}

/// Opens the VA-API hardware encoder, as `preference` allows. Settings are
/// validated before any device is touched, so a rejected configuration is
/// reported as [`OpenError::Invalid`] without an attempt.
pub fn open_encoder(
    config: EncoderConfig,
    preference: BackendPreference,
) -> Result<Selected<VaEncoder>, OpenError> {
    config.validate().map_err(OpenError::Invalid)?;
    let candidates = encoder_candidates().map_err(OpenError::Discovery)?;
    select(preference, candidates, |source| source.open_encoder(config))
}

/// Opens the VA-API hardware decoder, as `preference` allows. The decoder
/// resolves the stream's profile for its first access unit.
pub fn open_decoder(preference: BackendPreference) -> Result<Selected<VaDecoder>, OpenError> {
    let candidates = decoder_candidates().map_err(OpenError::Discovery)?;
    select(preference, candidates, HardwareSource::open_decoder)
}

/// The merged hardware + software platform encoder backend, as the share
/// pipeline uses it (hardware first, OpenH264 last).
pub fn platform_encoder(
    config: EncoderConfig,
    preference: BackendPreference,
) -> Result<Selected<Box<dyn crate::codec::VideoEncoder>>, OpenError> {
    crate::platform_h264::open_encoder(config, preference)
}

/// The merged hardware + software platform decoder backend.
pub fn platform_decoder(
    preference: BackendPreference,
) -> Result<Selected<Box<dyn crate::codec::VideoDecoder>>, OpenError> {
    crate::platform_h264::open_decoder(preference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{BackendPreference, CodecError};

    #[test]
    fn render_nodes_are_sorted_numerically() {
        let directory =
            std::env::temp_dir().join(format!("fastcord-vaapi-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        for name in [
            "renderD10",
            "renderD9",
            "card0",
            "controlD64",
            "renderD",
            "renderDX",
        ] {
            fs::write(directory.join(name), []).unwrap();
        }
        let nodes = render_nodes_in(&directory).unwrap();
        let names: Vec<String> = nodes
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["renderD9", "renderD10"]);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn decode_and_timeout_statuses_get_their_own_categories() {
        assert!(matches!(
            to_error(
                "vaDecodePicture",
                VA_STATUS_ERROR_DECODING_ERROR as VAStatus
            ),
            CodecError::InvalidBitstream(_)
        ));
        assert!(matches!(
            to_error("vaSyncSurface2", VA_STATUS_ERROR_TIMEDOUT as VAStatus),
            CodecError::Stalled(_)
        ));
        assert!(matches!(
            to_error("vaCreateContext", VA_STATUS_ERROR_UNIMPLEMENTED as VAStatus),
            CodecError::Platform {
                operation: "vaCreateContext",
                code: 20
            }
        ));
        assert!(check("vaEndPicture", VA_STATUS_SUCCESS as VAStatus).is_ok());
    }

    #[test]
    fn pictures_and_references_use_the_documented_flags() {
        let reserved = invalid_picture();
        assert_eq!(reserved.picture_id, VA_INVALID_SURFACE as VASurfaceID);
        assert_eq!(
            reserved.flags,
            VA_PICTURE_H264_INVALID | VA_PICTURE_H264_NON_EXISTING
        );
        let live = picture(7, 12, VA_PICTURE_H264_SHORT_TERM_REFERENCE, -3, -3);
        assert_eq!(live.picture_id, 7);
        assert_eq!(live.flags, VA_PICTURE_H264_SHORT_TERM_REFERENCE);
        let mut list = [live; 2];
        clear_references(&mut list);
        assert!(
            list.iter()
                .all(|reference| reference.picture_id == VA_INVALID_SURFACE as VASurfaceID)
        );
    }

    #[test]
    fn hardware_source_names_must_report_a_vendor() {
        let source = HardwareSource {
            path: PathBuf::from("/dev/dri/renderD128"),
            vendor: None,
            encoder: None,
            decoder: false,
        };
        assert_eq!(source.name(), "unknown vendor on /dev/dri/renderD128");
        let named = HardwareSource {
            vendor: Some("Intel iHD driver".into()),
            ..source
        };
        assert_eq!(named.name(), "Intel iHD driver on /dev/dri/renderD128");
    }

    #[test]
    fn preferences_without_hardware_report_no_backend() {
        // A hardware-pref decoder with no devices fails with NoBackend, as
        // `select` documents, before any device could be opened.
        let error = OpenError::NoBackend(BackendPreference::Hardware);
        assert_eq!(error.to_string(), "no hardware video codec is available");
    }
}
