//! H.264 through Apple's VideoToolbox, with NV12 IOSurface pixel buffers.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::time::Duration;

use crate::codec::{
    Backend, BackendKind, BackendPreference, CodecError, DecodedFrame, EncodedFrame, EncoderConfig,
    VideoDecoder, VideoEncoder,
};
use crate::frame::Nv12;
use crate::h264::check_decodable;
use crate::select::{Candidate, OpenError, Selected, select};

pub const API: &str = "VideoToolbox";

unsafe extern "C" {
    fn fc_vt_encoder_create(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        key_interval: u32,
        hardware: i32,
        status: *mut i32,
        actual_hardware: *mut i32,
    ) -> *mut c_void;
    fn fc_vt_encoder_encode(
        handle: *mut c_void,
        y: *const u8,
        y_stride: usize,
        uv: *const u8,
        uv_stride: usize,
        timestamp_ns: u64,
        duration_ns: u64,
        keyframe: i32,
        callback: unsafe extern "C" fn(*mut c_void, *const u8, usize, u64, i32),
        context: *mut c_void,
    ) -> i32;
    fn fc_vt_encoder_flush(
        handle: *mut c_void,
        callback: unsafe extern "C" fn(*mut c_void, *const u8, usize, u64, i32),
        context: *mut c_void,
    ) -> i32;
    fn fc_vt_encoder_destroy(handle: *mut c_void);
    fn fc_vt_decoder_create(
        hardware: i32,
        allow_software_fallback: i32,
        status: *mut i32,
    ) -> *mut c_void;
    fn fc_vt_decoder_decode(
        handle: *mut c_void,
        bytes: *const u8,
        len: usize,
        timestamp_ns: u64,
        callback: unsafe extern "C" fn(
            *mut c_void,
            *const u8,
            usize,
            usize,
            *const u8,
            usize,
            usize,
            usize,
            usize,
            u64,
        ),
        context: *mut c_void,
    ) -> i32;
    fn fc_vt_decoder_is_hardware(handle: *mut c_void) -> i32;
    fn fc_vt_decoder_destroy(handle: *mut c_void);
}

struct EncoderHandle(NonNull<c_void>);

impl Drop for EncoderHandle {
    fn drop(&mut self) {
        // SAFETY: the native create function returned this live handle and this
        // wrapper is its unique owner.
        unsafe { fc_vt_encoder_destroy(self.0.as_ptr()) };
    }
}

struct DecoderHandle(NonNull<c_void>);

impl Drop for DecoderHandle {
    fn drop(&mut self) {
        // SAFETY: the native create function returned this live handle and this
        // wrapper is its unique owner.
        unsafe { fc_vt_decoder_destroy(self.0.as_ptr()) };
    }
}

fn platform_error(operation: &'static str, status: i32) -> CodecError {
    CodecError::Platform {
        operation,
        code: status,
    }
}

fn create_encoder(config: EncoderConfig, kind: BackendKind) -> Result<VtEncoder, CodecError> {
    let mut status = 0;
    let mut actual_hardware = 0;
    // SAFETY: scalar arguments and output pointers are valid for the duration
    // of the call; the shim returns an owned session wrapper or null.
    let raw = unsafe {
        fc_vt_encoder_create(
            config.width,
            config.height,
            config.frame_rate,
            config.bitrate,
            config.keyframe_interval,
            i32::from(kind == BackendKind::Hardware),
            &mut status,
            &mut actual_hardware,
        )
    };
    let handle = NonNull::new(raw)
        .map(EncoderHandle)
        .ok_or_else(|| platform_error("VTCompressionSessionCreate", status))?;
    let actual_kind = if actual_hardware != 0 {
        BackendKind::Hardware
    } else {
        BackendKind::Software
    };
    if actual_kind != kind {
        return Err(CodecError::Unsupported(
            "VideoToolbox did not provide the requested encoder type",
        ));
    }
    Ok(VtEncoder {
        handle,
        config,
        backend: Backend {
            kind: actual_kind,
            api: API,
            name: if actual_kind == BackendKind::Hardware {
                "VideoToolbox hardware H.264 encoder".into()
            } else {
                "VideoToolbox software H.264 encoder".into()
            },
        },
    })
}

/// Opens VideoToolbox H.264 encoder candidates in hardware-first order.
pub fn open_encoder(
    config: EncoderConfig,
    preference: BackendPreference,
) -> Result<Selected<VtEncoder>, OpenError> {
    config.validate().map_err(OpenError::Invalid)?;
    let candidates = [BackendKind::Hardware, BackendKind::Software]
        .into_iter()
        .filter(|kind| preference.allows(*kind))
        .map(|kind| Candidate {
            kind,
            name: format!("VideoToolbox {kind} H.264 encoder"),
            source: kind,
        })
        .collect();
    select(preference, candidates, |kind| create_encoder(config, kind))
}

pub struct VtEncoder {
    handle: EncoderHandle,
    config: EncoderConfig,
    backend: Backend,
}

impl VideoEncoder for VtEncoder {
    fn backend(&self) -> &Backend {
        &self.backend
    }

    fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn encode(
        &mut self,
        frame: &Nv12<'_>,
        timestamp: Duration,
        keyframe: bool,
        out: &mut Vec<EncodedFrame>,
    ) -> Result<(), CodecError> {
        let expected = (self.config.width, self.config.height);
        let actual = (frame.width(), frame.height());
        if expected != actual {
            return Err(CodecError::FrameSize { expected, actual });
        }
        let mut output = EncodeOutput { out, error: None };
        // SAFETY: the input plane slices remain borrowed for this synchronous
        // call; the native shim copies them into its bounded IOSurface pool
        // before submitting to VideoToolbox. Callback context remains live
        // through CompleteFrames, which drains this frame before returning.
        let status = unsafe {
            fc_vt_encoder_encode(
                self.handle.0.as_ptr(),
                frame.luma_row(0).as_ptr(),
                frame.luma_stride(),
                frame.chroma_row(0).as_ptr(),
                frame.chroma_stride(),
                timestamp.as_nanos().min(u64::MAX as u128) as u64,
                self.config
                    .frame_duration()
                    .as_nanos()
                    .min(u64::MAX as u128) as u64,
                i32::from(keyframe),
                encode_output,
                (&mut output as *mut EncodeOutput<'_>).cast(),
            )
        };
        if let Some(error) = output.error {
            return Err(error);
        }
        if status == 0 {
            Ok(())
        } else {
            Err(platform_error("VTCompressionSessionEncodeFrame", status))
        }
    }
    fn flush(&mut self, out: &mut Vec<EncodedFrame>) -> Result<(), CodecError> {
        let mut output = EncodeOutput { out, error: None };
        // SAFETY: this synchronously drains all frames; the output context is
        // live for the full callback interval.
        let status = unsafe {
            fc_vt_encoder_flush(
                self.handle.0.as_ptr(),
                encode_output,
                (&mut output as *mut EncodeOutput<'_>).cast(),
            )
        };
        if let Some(error) = output.error {
            return Err(error);
        }
        if status == 0 {
            Ok(())
        } else {
            Err(platform_error("VTCompressionSessionCompleteFrames", status))
        }
    }
}

struct EncodeOutput<'a> {
    out: &'a mut Vec<EncodedFrame>,
    error: Option<CodecError>,
}

unsafe extern "C" fn encode_output(
    context: *mut c_void,
    bytes: *const u8,
    len: usize,
    timestamp_ns: u64,
    keyframe: i32,
) {
    if context.is_null() || bytes.is_null() || len == 0 {
        return;
    }
    // SAFETY: context is the live EncodeOutput passed to the synchronous
    // native call. The byte slice is valid until the callback returns.
    let output = unsafe { &mut *context.cast::<EncodeOutput<'_>>() };
    let result = catch_unwind(AssertUnwindSafe(|| {
        let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
        output.out.push(EncodedFrame {
            data: bytes.to_vec(),
            timestamp: Duration::from_nanos(timestamp_ns),
            keyframe: keyframe != 0,
        });
    }));
    if result.is_err() {
        output.error = Some(CodecError::Stalled(
            "VideoToolbox encoder callback panicked",
        ));
    }
}

/// Opens a VideoToolbox H.264 decoder. Hardware is attempted before the
/// VideoToolbox software decoder in automatic mode.
pub fn open_decoder(preference: BackendPreference) -> Result<Selected<VtDecoder>, OpenError> {
    let candidates = [BackendKind::Hardware, BackendKind::Software]
        .into_iter()
        .filter(|kind| preference.allows(*kind))
        .map(|kind| Candidate {
            kind,
            name: format!("VideoToolbox {kind} H.264 decoder"),
            source: kind,
        })
        .collect();
    select(preference, candidates, |kind| {
        let mut status = 0;
        // SAFETY: status is writable; the wrapper is an owned native decoder
        // context until DecoderHandle drops it.
        let raw = unsafe {
            fc_vt_decoder_create(
                i32::from(kind == BackendKind::Hardware),
                i32::from(preference == BackendPreference::Auto),
                &mut status,
            )
        };
        let handle = NonNull::new(raw)
            .map(DecoderHandle)
            .ok_or_else(|| platform_error("VTDecompressionSessionCreate", status))?;
        Ok(VtDecoder {
            handle,
            backend: Backend {
                kind,
                api: API,
                name: format!("VideoToolbox {kind} H.264 decoder"),
            },
        })
    })
}

pub struct VtDecoder {
    handle: DecoderHandle,
    backend: Backend,
}

impl VideoDecoder for VtDecoder {
    fn backend(&self) -> &Backend {
        &self.backend
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        timestamp: Duration,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError> {
        check_decodable(access_unit)?;
        let mut output = DecodeOutput { out, error: None };
        // SAFETY: the Annex B slice and callback context live through the
        // synchronous VT decode; callback plane pointers remain valid until
        // the callback returns, where the borrowed frame is consumed.
        let status = unsafe {
            fc_vt_decoder_decode(
                self.handle.0.as_ptr(),
                access_unit.as_ptr(),
                access_unit.len(),
                timestamp.as_nanos().min(u64::MAX as u128) as u64,
                decode_output,
                (&mut output as *mut DecodeOutput<'_>).cast(),
            )
        };
        let actual_hardware = unsafe { fc_vt_decoder_is_hardware(self.handle.0.as_ptr()) };
        if actual_hardware == 0 {
            self.backend.kind = BackendKind::Software;
            self.backend.name = "VideoToolbox software H.264 decoder".into();
        } else if actual_hardware > 0 {
            self.backend.kind = BackendKind::Hardware;
            self.backend.name = "VideoToolbox hardware H.264 decoder".into();
        }
        if let Some(error) = output.error {
            return Err(error);
        }
        if status == 0 {
            Ok(())
        } else {
            Err(platform_error("VTDecompressionSessionDecodeFrame", status))
        }
    }

    fn flush(&mut self, _out: &mut dyn FnMut(DecodedFrame<'_>)) -> Result<(), CodecError> {
        Ok(())
    }
}

struct DecodeOutput<'a> {
    out: &'a mut dyn FnMut(DecodedFrame<'_>),
    error: Option<CodecError>,
}

unsafe extern "C" fn decode_output(
    context: *mut c_void,
    y: *const u8,
    y_stride: usize,
    y_width: usize,
    uv: *const u8,
    uv_stride: usize,
    uv_width: usize,
    width: usize,
    height: usize,
    timestamp_ns: u64,
) {
    if context.is_null() || y.is_null() || uv.is_null() {
        return;
    }
    // SAFETY: callback context and plane pointers are valid for this callback
    // invocation, and row sizes were read from the locked CVPixelBuffer.
    let output = unsafe { &mut *context.cast::<DecodeOutput<'_>>() };
    let y_len = y_stride
        .saturating_mul(height.saturating_sub(1))
        .saturating_add(y_width);
    let uv_rows = height.div_ceil(2);
    let uv_len = uv_stride
        .saturating_mul(uv_rows.saturating_sub(1))
        .saturating_add(uv_width);
    let y_plane = unsafe { std::slice::from_raw_parts(y, y_len) };
    let uv_plane = unsafe { std::slice::from_raw_parts(uv, uv_len) };
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        output.error = Some(CodecError::Unsupported(
            "VideoToolbox returned invalid dimensions",
        ));
        return;
    };
    let image = Nv12::new(width, height, y_plane, y_stride, uv_plane, uv_stride);
    let result = catch_unwind(AssertUnwindSafe(|| match image {
        Ok(image) => (output.out)(DecodedFrame {
            image,
            timestamp: Duration::from_nanos(timestamp_ns),
        }),
        Err(error) => output.error = Some(CodecError::InvalidFrame(error)),
    }));
    if result.is_err() {
        output.error = Some(CodecError::Stalled(
            "VideoToolbox decoder callback panicked",
        ));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::BackendPreference;
    use crate::h264::inspect;

    const CONFIG: EncoderConfig = EncoderConfig {
        width: 128,
        height: 96,
        frame_rate: 30,
        bitrate: 500_000,
        keyframe_interval: 30,
    };

    fn source_frame() -> Vec<u8> {
        let mut data = vec![0; CONFIG.width as usize * CONFIG.height as usize * 3 / 2];
        data[..CONFIG.width as usize * CONFIG.height as usize].fill(96);
        data[CONFIG.width as usize * CONFIG.height as usize..].fill(128);
        data
    }

    #[test]
    fn software_encoder_and_decoder_round_trip_and_restart() {
        for _ in 0..2 {
            let mut encoder = open_encoder(CONFIG, BackendPreference::Software)
                .expect("VideoToolbox software encoder")
                .codec;
            assert_eq!(encoder.backend().kind, BackendKind::Software);
            let data = source_frame();
            let frame = Nv12::packed(CONFIG.width, CONFIG.height, &data).unwrap();
            let mut encoded = Vec::new();
            encoder
                .encode(&frame, Duration::ZERO, true, &mut encoded)
                .unwrap();
            assert_eq!(encoded.len(), 1);
            assert!(encoded[0].keyframe);
            assert!(inspect(&encoded[0].data).unwrap().vcl);

            let mut decoder = open_decoder(BackendPreference::Software)
                .expect("VideoToolbox software decoder")
                .codec;
            assert_eq!(decoder.backend().kind, BackendKind::Software);
            let mut got_frame = false;
            decoder
                .decode(&encoded[0].data, Duration::ZERO, &mut |frame| {
                    assert_eq!(frame.image.width(), CONFIG.width);
                    assert_eq!(frame.image.height(), CONFIG.height);
                    got_frame = true;
                })
                .unwrap();
            assert!(
                got_frame,
                "synchronous VideoToolbox decode produced no frame"
            );
        }
    }

    #[test]
    #[ignore = "requires Apple H.264 hardware encode/decode; run on supported Apple Silicon and Intel Macs"]
    fn hardware_encode_decode_720p30() {
        let config = EncoderConfig::SCREEN_SHARE_720P30;
        let mut encoder = open_encoder(config, BackendPreference::Hardware)
            .expect("hardware VideoToolbox encoder")
            .codec;
        let mut data = vec![96; config.width as usize * config.height as usize];
        data.resize(config.width as usize * config.height as usize * 3 / 2, 128);
        let frame = Nv12::packed(config.width, config.height, &data).unwrap();
        let mut encoded = Vec::new();
        encoder
            .encode(&frame, Duration::ZERO, true, &mut encoded)
            .unwrap();
        assert_eq!(encoder.backend().kind, BackendKind::Hardware);
        let mut decoder = open_decoder(BackendPreference::Hardware)
            .expect("hardware VideoToolbox decoder")
            .codec;
        let mut got_frame = false;
        decoder
            .decode(&encoded[0].data, Duration::ZERO, &mut |frame| {
                assert_eq!(frame.image.width(), config.width);
                assert_eq!(frame.image.height(), config.height);
                got_frame = true;
            })
            .unwrap();
        assert!(got_frame);
        assert_eq!(decoder.backend().kind, BackendKind::Hardware);
    }
}
