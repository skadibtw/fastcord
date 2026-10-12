//! In-process AV1 receive decoding through statically bundled dav1d 1.5.4.
//!
//! [`Decoder::open`] constructs only the Rust owner; the native context, one
//! 2 MiB encoded-input buffer and picture pool are created on the first valid
//! [`VideoDecoder::decode`] call. There is no encoder, DLL lookup or subprocess.
//! The input is a complete low-overhead OBU temporal unit: every OBU has its
//! size field and a LEB128 payload length. A temporal delimiter is optional.
//!
//! dav1d decodes 8/10/12-bit I400/I420/I422/I444 YUV. The existing borrowed NV12
//! contract is 8-bit 4:2:0: 10/12-bit samples are right-shifted by 2/4 (no HDR
//! tone mapping or dithering), and 4:2:2/4:4:4 chroma is box-averaged to 4:2:0,
//! replicating the last row/column for odd dimensions. Monochrome uses neutral
//! UV=128. RGB/identity-matrix streams return [`CodecError::Unsupported`], not
//! a wrongly labelled YUV picture. Film grain and native in-loop filters stay
//! enabled. Eight-bit luma is borrowed directly from the native picture; only
//! UV interleaving/subsampling and high-bit-depth reduction need scratch data.
//! Each callback carries the native sequence's matrix and quantization range.
//! Sample aspect comes from that picture's AV1 render size (including
//! show-existing frames). Unspecified matrix defaults to BT.709; unsupported
//! explicit matrices are rejected rather than mislabelled. See
//! [`ColorDescription`] for the limited-range/square-pixel defaults.
//!
//! Encoded input is capped at [`MAX_ENCODED_BYTES`]; retained native picture
//! allocations (including unused reusable slots, alignment and padding) are
//! capped at [`MAX_PICTURE_POOL_BYTES`]. At most twelve slots accommodate AV1's
//! eight references, reconstruction/super-resolution and film-grain output.
//! The cap is a resource limit, not a promise to accept every worst-case
//! high-bit-depth/4:4:4 reference structure at the maximum dimensions. Failure
//! is explicit. Scratch capacity is at most `MAX_PIXELS` Y bytes plus rounded
//! half-resolution interleaved UV bytes. [`Decoder::reset`] and drop release
//! all stream memory. The object belongs to the thread that opened it.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::Duration;

use crate::codec::{
    Backend, BackendKind, CodecError, DecodedFrame, MAX_DIMENSION, MAX_PIXELS, VideoDecoder,
    check_dimensions,
};
use crate::frame::{ColorDescription, ColorMatrix, ColorRange, Nv12};

pub const API: &str = "dav1d";
pub const VERSION: &str = "1.5.4";
pub const MAX_ENCODED_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_PICTURE_POOL_BYTES: usize = 256 * 1024 * 1024;

const DIMENSIONS: i32 = -20001;
const POOL_LIMIT: i32 = -20002;
const FORMAT: i32 = -20003;
const BITSTREAM: i32 = -20004;
const NO_PROGRESS: i32 = -20005;

#[repr(C)]
struct NativePicture {
    planes: [*const u8; 3],
    lengths: [usize; 3],
    strides: [usize; 2],
    width: u32,
    height: u32,
    bitdepth: u32,
    layout: u32,
    timestamp_ns: i64,
    matrix: u32,
    full_range: u32,
    render_width: u32,
    render_height: u32,
}

impl Default for NativePicture {
    fn default() -> Self {
        Self {
            planes: [std::ptr::null(); 3],
            lengths: [0; 3],
            strides: [0; 2],
            width: 0,
            height: 0,
            bitdepth: 0,
            layout: 0,
            timestamp_ns: 0,
            matrix: 2,
            full_range: 0,
            render_width: 0,
            render_height: 0,
        }
    }
}

unsafe extern "C" {
    fn fc_av1_create(
        encoded_limit: usize,
        pool_limit: usize,
        max_dimension: u32,
        max_pixels: u32,
        status: *mut i32,
    ) -> *mut c_void;
    fn fc_av1_destroy(handle: *mut c_void);
    fn fc_av1_submit(
        handle: *mut c_void,
        bytes: *const u8,
        len: usize,
        timestamp_ns: i64,
        has_sequence_header: i32,
    ) -> i32;
    fn fc_av1_next(handle: *mut c_void, picture: *mut NativePicture) -> i32;
    #[cfg(test)]
    fn fc_av1_pool_bytes(handle: *const c_void) -> usize;
    #[cfg(test)]
    fn fc_av1_pool_allocations(handle: *const c_void) -> usize;
}

struct Handle(NonNull<c_void>);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this is the unique owner of the live native context. No
        // picture lease escapes a decode callback, so its pool can be freed.
        unsafe { fc_av1_destroy(self.0.as_ptr()) };
    }
}

/// A lazy, bounded dav1d software decoder implementing [`VideoDecoder`].
pub struct Decoder {
    handle: Option<Handle>,
    backend: Backend,
    luma: Vec<u8>,
    chroma: Vec<u8>,
    chroma_is_neutral: bool,
    pool_limit: usize,
}

impl Decoder {
    /// Constructs the thread-owned decoder without allocating native stream
    /// state. The first decode reports any native allocation/open error.
    pub fn open() -> Result<Self, CodecError> {
        Ok(Self {
            handle: None,
            backend: Backend {
                kind: BackendKind::Software,
                api: API,
                name: "dav1d 1.5.4 (bundled portable C)".into(),
            },
            luma: Vec::new(),
            chroma: Vec::new(),
            chroma_is_neutral: false,
            pool_limit: MAX_PICTURE_POOL_BYTES,
        })
    }

    /// Releases references, encoded input, native pool and scratch buffers.
    /// Decoding can restart with a new sequence header and keyframe; native
    /// construction is lazy again. Unlike `flush`, this discards stream state.
    pub fn reset(&mut self) {
        self.handle = None;
        self.luma = Vec::new();
        self.chroma = Vec::new();
        self.chroma_is_neutral = false;
    }

    fn native_handle(&mut self) -> Result<NonNull<c_void>, CodecError> {
        if let Some(handle) = &self.handle {
            return Ok(handle.0);
        }
        let mut status = 0;
        // SAFETY: limits are bounded compile-time values, status is a live
        // output pointer, and a non-null result has unique ownership.
        let raw = unsafe {
            fc_av1_create(
                MAX_ENCODED_BYTES,
                self.pool_limit,
                MAX_DIMENSION,
                MAX_PIXELS as u32,
                &mut status,
            )
        };
        let handle = Handle(NonNull::new(raw).ok_or_else(|| native_error("dav1d_open", status))?);
        let pointer = handle.0;
        self.handle = Some(handle);
        Ok(pointer)
    }

    fn decode_native(
        &mut self,
        temporal_unit: &[u8],
        timestamp_ns: i64,
        has_sequence_header: bool,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError> {
        let handle = self.native_handle()?;
        // SAFETY: the bridge copies this borrowed input to its reusable owned
        // buffer before returning, and preserves timestamp in dav1d metadata.
        let status = unsafe {
            fc_av1_submit(
                handle.as_ptr(),
                temporal_unit.as_ptr(),
                temporal_unit.len(),
                timestamp_ns,
                i32::from(has_sequence_header),
            )
        };
        if status != 0 {
            return Err(native_error("dav1d_send_data", status));
        }
        self.drain(out)
    }

    fn drain(&mut self, out: &mut dyn FnMut(DecodedFrame<'_>)) -> Result<(), CodecError> {
        let Some(handle) = self.handle.as_ref().map(|handle| handle.0) else {
            return Ok(());
        };
        loop {
            let mut picture = NativePicture::default();
            // SAFETY: handle is owned by self and picture matches the bridge's
            // fixed layout. A returned picture stays alive until the next
            // native call; emit consumes its borrow before that call occurs.
            let status = unsafe { fc_av1_next(handle.as_ptr(), &mut picture) };
            match status {
                0 => return Ok(()),
                1 => self.emit(&picture, out)?,
                error => return Err(native_error("dav1d_get_picture", error)),
            }
        }
    }

    fn emit(
        &mut self,
        picture: &NativePicture,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError> {
        check_dimensions(picture.width, picture.height)?;
        if !matches!(picture.bitdepth, 8 | 10 | 12) || picture.layout > 3 {
            return Err(CodecError::Unsupported(
                "dav1d returned an unknown pixel format",
            ));
        }
        let color = picture_color(picture)?;
        let timestamp = u64::try_from(picture.timestamp_ns)
            .map(Duration::from_nanos)
            .map_err(|_| CodecError::InvalidBitstream("AV1 picture has no input timestamp"))?;
        let width = picture.width as usize;
        let height = picture.height as usize;
        // SAFETY: the audited bridge supplies visible plane lengths inside
        // its aligned, dimension-checked native pool. That picture's owning
        // reference survives through this entire synchronous Rust callback.
        let native_y = unsafe { std::slice::from_raw_parts(picture.planes[0], picture.lengths[0]) };
        if picture.bitdepth > 8 {
            fit_buffer(&mut self.luma, width * height, 0)?;
            let shift = picture.bitdepth - 8;
            for row in 0..height {
                let start = row * picture.strides[0];
                let source = &native_y[start..start + width * 2];
                for (dst, bytes) in self.luma[row * width..(row + 1) * width]
                    .iter_mut()
                    .zip(source.as_chunks::<2>().0)
                {
                    *dst = (u16::from_ne_bytes(*bytes) >> shift) as u8;
                }
            }
        }
        let uv_stride = width.div_ceil(2) * 2;
        let uv_rows = height.div_ceil(2);
        fit_buffer(&mut self.chroma, uv_stride * uv_rows, 128)?;
        if picture.layout == 0 {
            if !self.chroma_is_neutral {
                self.chroma.fill(128);
                self.chroma_is_neutral = true;
            }
        } else {
            // SAFETY: non-monochrome pictures supply both visible U/V planes;
            // neither reference is released until after emit returns.
            let u = unsafe { std::slice::from_raw_parts(picture.planes[1], picture.lengths[1]) };
            let v = unsafe { std::slice::from_raw_parts(picture.planes[2], picture.lengths[2]) };
            interleave_chroma(picture, u, v, &mut self.chroma, uv_stride, uv_rows);
            self.chroma_is_neutral = false;
        }
        let (y, y_stride) = if picture.bitdepth == 8 {
            (native_y, picture.strides[0])
        } else {
            (self.luma.as_slice(), width)
        };
        let image = Nv12::new(
            picture.width,
            picture.height,
            y,
            y_stride,
            &self.chroma,
            uv_stride,
        )?;
        out(DecodedFrame {
            image,
            color,
            timestamp,
        });
        Ok(())
    }
}

fn picture_color(picture: &NativePicture) -> Result<ColorDescription, CodecError> {
    let matrix = ColorMatrix::from_h273(picture.matrix).ok_or(CodecError::Unsupported(
        "AV1 color matrix is not BT.601/709/2020 NCL",
    ))?;
    let range = match picture.full_range {
        0 => ColorRange::Limited,
        1 => ColorRange::Full,
        _ => return Err(CodecError::InvalidBitstream("invalid AV1 sample range")),
    };
    // AV1 signals display geometry via each frame's render size, not a SAR
    // field in the sequence header. This also survives show-existing frames.
    let numerator = u64::from(picture.render_width) * u64::from(picture.height);
    let denominator = u64::from(picture.render_height) * u64::from(picture.width);
    if numerator == 0 || denominator == 0 {
        return Err(CodecError::InvalidBitstream(
            "invalid AV1 render dimensions",
        ));
    }
    let (mut a, mut b) = (numerator, denominator);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    let pixel_aspect_ratio = (
        u32::try_from(numerator / a)
            .map_err(|_| CodecError::Unsupported("AV1 sample aspect ratio is too large"))?,
        u32::try_from(denominator / a)
            .map_err(|_| CodecError::Unsupported("AV1 sample aspect ratio is too large"))?,
    );
    Ok(ColorDescription {
        matrix,
        range,
        pixel_aspect_ratio,
    })
}

impl VideoDecoder for Decoder {
    fn backend(&self) -> &Backend {
        &self.backend
    }

    fn decode(
        &mut self,
        temporal_unit: &[u8],
        timestamp: Duration,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError> {
        let has_sequence_header = check_temporal_unit(temporal_unit)?;
        let timestamp_ns = i64::try_from(timestamp.as_nanos())
            .map_err(|_| CodecError::InvalidConfig("AV1 timestamp exceeds 64-bit nanoseconds"))?;
        let result = self.decode_native(temporal_unit, timestamp_ns, has_sequence_header, out);
        if result.is_err() {
            // dav1d keeps what it holds of a frame it gave up on, including a
            // reference to the input buffer, until the next frame header, which
            // a bridge still waiting for that buffer could never feed it. Only
            // closing the context releases it. The next keyframe opens a new
            // one; the references were lost with the frame anyway.
            self.handle = None;
        }
        result
    }

    fn flush(&mut self, out: &mut dyn FnMut(DecodedFrame<'_>)) -> Result<(), CodecError> {
        // dav1d_get_picture drains; dav1d_flush would incorrectly discard the
        // references that subsequent inter frames need.
        let result = self.drain(out);
        if result.is_err() {
            self.handle = None;
        }
        result
    }
}

fn fit_buffer(buffer: &mut Vec<u8>, len: usize, fill: u8) -> Result<(), CodecError> {
    if len > buffer.capacity() {
        // Exact reservation avoids Vec's geometric growth exceeding the
        // dimension-derived scratch byte bound during resolution changes.
        buffer
            .try_reserve_exact(len - buffer.len())
            .map_err(|_| CodecError::Unsupported("AV1 output buffer allocation failed"))?;
    }
    buffer.resize(len, fill);
    Ok(())
}

fn sample(plane: &[u8], offset: usize, high_bitdepth: bool) -> u32 {
    if high_bitdepth {
        u32::from(u16::from_ne_bytes([plane[offset], plane[offset + 1]]))
    } else {
        u32::from(plane[offset])
    }
}

fn interleave_chroma(
    picture: &NativePicture,
    u: &[u8],
    v: &[u8],
    destination: &mut [u8],
    uv_stride: usize,
    uv_rows: usize,
) {
    let stride = picture.strides[1];
    if picture.layout == 1 && picture.bitdepth == 8 {
        // The ordinary AV1 screen-share path copies no luma and only performs
        // the unavoidable planar-U/V to NV12 interleave.
        for row in 0..uv_rows {
            let start = row * stride;
            for ((dst, &u), &v) in destination[row * uv_stride..(row + 1) * uv_stride]
                .as_chunks_mut::<2>()
                .0
                .iter_mut()
                .zip(&u[start..start + uv_stride / 2])
                .zip(&v[start..start + uv_stride / 2])
            {
                *dst = [u, v];
            }
        }
        return;
    }
    let high_bitdepth = picture.bitdepth > 8;
    let bytes = if high_bitdepth { 2 } else { 1 };
    let shift = picture.bitdepth - 8;
    let vertical = picture.layout != 1;
    let horizontal = picture.layout == 3;
    for row in 0..uv_rows {
        let y0 = if vertical { row * 2 } else { row };
        let y1 = (y0 + 1).min(picture.height as usize - 1);
        for (column, dst) in destination[row * uv_stride..(row + 1) * uv_stride]
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .enumerate()
        {
            let x0 = if horizontal { column * 2 } else { column };
            let x1 = (x0 + 1).min(picture.width as usize - 1);
            for (plane, value) in [u, v].into_iter().zip(dst) {
                let mut sum = sample(plane, y0 * stride + x0 * bytes, high_bitdepth);
                let mut count = 1;
                if vertical {
                    sum += sample(plane, y1 * stride + x0 * bytes, high_bitdepth);
                    count *= 2;
                }
                if horizontal {
                    sum += sample(plane, y0 * stride + x1 * bytes, high_bitdepth);
                    sum += sample(plane, y1 * stride + x1 * bytes, high_bitdepth);
                    count *= 2;
                }
                *value = ((sum / count) >> shift) as u8;
            }
        }
    }
}

fn native_error(operation: &'static str, code: i32) -> CodecError {
    match code {
        DIMENSIONS => CodecError::Unsupported("AV1 picture is larger than 4096x2304"),
        POOL_LIMIT => CodecError::Unsupported("AV1 native picture pool byte limit exceeded"),
        FORMAT => CodecError::Unsupported("AV1 RGB/identity matrix cannot be output as NV12"),
        BITSTREAM => CodecError::InvalidBitstream("dav1d rejected malformed or incomplete AV1"),
        NO_PROGRESS => CodecError::Stalled("dav1d made no progress on a temporal unit"),
        code => CodecError::Platform { operation, code },
    }
}

/// Validates bounded OBU framing before constructing native stream state.
/// Syntax inside the OBUs is parsed by dav1d, not by a second AV1 decoder.
fn check_temporal_unit(mut bytes: &[u8]) -> Result<bool, CodecError> {
    if bytes.is_empty() {
        return Err(CodecError::InvalidBitstream("AV1 temporal unit is empty"));
    }
    if bytes.len() > MAX_ENCODED_BYTES {
        return Err(CodecError::Unsupported("AV1 temporal unit exceeds 2 MiB"));
    }
    let mut has_sequence_header = false;
    while !bytes.is_empty() {
        let header = bytes[0];
        bytes = &bytes[1..];
        if header & 0x81 != 0 || header & 0x02 == 0 {
            return Err(CodecError::InvalidBitstream(
                "AV1 OBU needs a valid sized header",
            ));
        }
        if header & 0x04 != 0 {
            let Some((&extension, rest)) = bytes.split_first() else {
                return Err(CodecError::InvalidBitstream(
                    "AV1 OBU extension is truncated",
                ));
            };
            if extension & 0x07 != 0 {
                return Err(CodecError::InvalidBitstream(
                    "AV1 OBU extension has reserved bits",
                ));
            }
            bytes = rest;
        }
        let mut length = 0u64;
        let mut terminated = false;
        for shift in (0..56).step_by(7) {
            let Some((&byte, rest)) = bytes.split_first() else {
                return Err(CodecError::InvalidBitstream("AV1 OBU length is truncated"));
            };
            bytes = rest;
            length |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                terminated = true;
                break;
            }
        }
        if !terminated {
            return Err(CodecError::InvalidBitstream(
                "AV1 OBU length exceeds eight bytes",
            ));
        }
        let length = usize::try_from(length)
            .map_err(|_| CodecError::InvalidBitstream("AV1 OBU length overflows"))?;
        bytes = bytes
            .get(length..)
            .ok_or(CodecError::InvalidBitstream("AV1 OBU payload is truncated"))?;
        has_sequence_header |= (header >> 3) & 0x0f == 1;
    }
    Ok(has_sequence_header)
}

#[cfg(test)]
mod tests;
