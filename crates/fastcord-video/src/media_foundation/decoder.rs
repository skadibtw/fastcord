//! H.264 decoding through a Media Foundation decoder transform, on D3D11VA
//! surfaces (hardware) or in system memory (software).

use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFDXGIBuffer, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform,
    MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MF2DBuffer_LockFlags_Read, MFMediaType_Video,
    MFT_MESSAGE_SET_D3D_MANAGER, MFVideoArea, MFVideoFormat_H264, MFVideoFormat_NV12,
};
use windows::core::Interface;

use super::d3d::DeviceManager;
use super::runtime::{OrPlatform, Session};
use super::transform::{InputPool, Sink, Transform};
use super::{from_hns, to_hns};
use crate::codec::{
    Backend, BackendKind, CodecError, DecodedFrame, VideoDecoder, check_dimensions,
};
use crate::frame::Nv12;
use crate::h264::check_decodable;

/// A Media Foundation H.264 decoder (see [`super::open_decoder`]).
pub struct MfDecoder {
    transform: Transform,
    inputs: InputPool,
    state: DecoderState,
    _device: Option<DeviceManager>,
    // Declared last: Media Foundation shuts down after every object above is
    // released.
    session: Session,
}

struct DecoderState {
    backend: Backend,
    preference: crate::codec::BackendPreference,
    format: OutputFormat,
}

/// The negotiated NV12 output layout.
#[derive(Clone, Copy, Debug, Default)]
struct OutputFormat {
    /// Coded size; system-memory planes are laid out at this height.
    width: u32,
    height: u32,
    /// Row pitch of system-memory output, when the type states one.
    stride: Option<u32>,
    /// Display area: x, y, width, height.
    display: (u32, u32, u32, u32),
}

impl MfDecoder {
    /// `device` enables D3D11VA; without it the transform decodes in software.
    pub(super) fn open(
        session: Session,
        transform: Transform,
        mut backend: Backend,
        device: Option<DeviceManager>,
        preference: crate::codec::BackendPreference,
    ) -> Result<Self, CodecError> {
        let mft = transform.mft();
        if let Some(attributes) = transform.attributes() {
            // Output each picture as soon as it is decoded. Best effort: a
            // decoder without the attribute still works, only with more delay.
            // SAFETY: COM call on a valid interface.
            let _ = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) };
        }
        if let Some(device) = &device {
            // SAFETY: the manager outlives the transform (`_device` is
            // dropped after `transform`).
            unsafe {
                mft.ProcessMessage(
                    MFT_MESSAGE_SET_D3D_MANAGER,
                    device.manager.as_raw() as usize,
                )
            }
            .or_platform("MFT_MESSAGE_SET_D3D_MANAGER")?;
        }
        let input = session.media_type()?;
        // SAFETY: COM calls on valid interfaces.
        unsafe {
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|()| input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
                .and_then(|()| mft.SetInputType(transform.input_id(), &input, 0))
        }
        .or_platform("IMFTransform::SetInputType")?;
        let format = select_nv12(mft, transform.output_id())?;
        if device.is_none() {
            backend.kind = BackendKind::Software;
        }
        let mut decoder = Self {
            transform,
            inputs: InputPool::default(),
            state: DecoderState {
                backend,
                preference,
                format,
            },
            _device: device,
            session,
        };
        decoder.transform.start()?;
        Ok(decoder)
    }
}

/// Selects the transform's NV12 output type and reads its layout.
fn select_nv12(mft: &IMFTransform, output_id: u32) -> Result<OutputFormat, CodecError> {
    for index in 0.. {
        // SAFETY: COM calls on a valid interface; enumeration ends with
        // MF_E_NO_MORE_TYPES.
        let available = unsafe { mft.GetOutputAvailableType(output_id, index) }
            .or_platform("IMFTransform::GetOutputAvailableType")?;
        // SAFETY: as above.
        if unsafe { available.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(MFVideoFormat_NV12) {
            // SAFETY: as above.
            unsafe { mft.SetOutputType(output_id, &available, 0) }
                .or_platform("IMFTransform::SetOutputType")?;
            return output_format(&available);
        }
    }
    unreachable!("the type enumeration ends with an error")
}

fn output_format(media_type: &IMFMediaType) -> Result<OutputFormat, CodecError> {
    // SAFETY: COM calls on a valid media type; the aperture blob is read into
    // a value of its documented type and size.
    unsafe {
        let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let (width, height) = ((size >> 32) as u32, size as u32);
        let stride = media_type
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .ok()
            .and_then(|stride| i32::try_from(stride).ok())
            .and_then(|stride| u32::try_from(stride).ok());
        let mut area = MFVideoArea::default();
        let area_bytes =
            std::slice::from_raw_parts_mut((&raw mut area).cast::<u8>(), size_of::<MFVideoArea>());
        let display = match media_type.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, area_bytes, None) {
            Ok(()) => (
                u32::try_from(area.OffsetX.value).unwrap_or(u32::MAX),
                u32::try_from(area.OffsetY.value).unwrap_or(u32::MAX),
                u32::try_from(area.Area.cx).unwrap_or(0),
                u32::try_from(area.Area.cy).unwrap_or(0),
            ),
            Err(_) => (0, 0, width, height),
        };
        Ok(OutputFormat {
            width,
            height,
            stride,
            display,
        })
    }
}

impl MfDecoder {
    fn update_backend_after_fallback(&mut self) {
        if !self.transform.software_fallback() {
            return;
        }
        self.state.backend.kind = BackendKind::Software;
        if !self
            .state
            .backend
            .name
            .ends_with(" (D3D11VA unavailable for this stream)")
        {
            self.state
                .backend
                .name
                .push_str(" (D3D11VA unavailable for this stream)");
        }
        self._device = None;
    }
}

impl VideoDecoder for MfDecoder {
    fn backend(&self) -> &Backend {
        &self.state.backend
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        timestamp: Duration,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError> {
        check_decodable(access_unit)?;
        let alignment = self.transform.input_alignment()?;
        let sample = self
            .inputs
            .sample(&self.session, access_unit.len(), alignment, |buffer| {
                buffer.copy_from_slice(access_unit)
            })?;
        // SAFETY: COM call on a valid sample.
        unsafe { sample.SetSampleTime(to_hns(timestamp)) }
            .or_platform("IMFSample::SetSampleTime")?;
        let allow_software_fallback = self.state.preference
            == crate::codec::BackendPreference::Auto
            && self._device.is_some();
        let result = {
            let mut deliver = Deliver {
                state: &mut self.state,
                device: self._device.as_mut(),
                out,
            };
            self.transform.push(
                &self.session,
                &sample,
                &mut deliver,
                allow_software_fallback,
            )
        };
        self.update_backend_after_fallback();
        result
    }

    fn flush(&mut self, out: &mut dyn FnMut(DecodedFrame<'_>)) -> Result<(), CodecError> {
        let allow_software_fallback = self.state.preference
            == crate::codec::BackendPreference::Auto
            && self._device.is_some();
        let result = {
            let mut deliver = Deliver {
                state: &mut self.state,
                device: self._device.as_mut(),
                out,
            };
            self.transform
                .drain(&self.session, &mut deliver, allow_software_fallback)
        };
        self.update_backend_after_fallback();
        result
    }
}

/// Hands decoded pictures to the caller's callback.
struct Deliver<'a, 'b> {
    state: &'a mut DecoderState,
    device: Option<&'a mut DeviceManager>,
    out: &'a mut (dyn FnMut(DecodedFrame<'_>) + 'b),
}

impl Sink for Deliver<'_, '_> {
    fn stream_changed(&mut self, mft: &IMFTransform, output_id: u32) -> Result<(), CodecError> {
        self.state.format = select_nv12(mft, output_id)?;
        Ok(())
    }

    fn output(&mut self, sample: &IMFSample) -> Result<(), CodecError> {
        let Self { state, device, out } = self;
        // SAFETY: COM calls on a valid sample.
        let (buffer, timestamp) = unsafe {
            (
                sample
                    .GetBufferByIndex(0)
                    .or_platform("IMFSample::GetBufferByIndex")?,
                sample.GetSampleTime().unwrap_or(0),
            )
        };
        let surface = buffer.cast::<IMFDXGIBuffer>().ok();
        if surface.is_none() && device.is_some() {
            if state.preference == crate::codec::BackendPreference::Hardware {
                return Err(CodecError::Unsupported(
                    "hardware decoder returned a system-memory frame",
                ));
            }
            state.backend.kind = BackendKind::Software;
            if !state
                .backend
                .name
                .ends_with(" (D3D11VA unavailable for this stream)")
            {
                state
                    .backend
                    .name
                    .push_str(" (D3D11VA unavailable for this stream)");
            }
        } else if surface.is_some() && state.backend.kind == BackendKind::Software {
            state.backend.kind = BackendKind::Hardware;
            if let Some(name) = state
                .backend
                .name
                .strip_suffix(" (D3D11VA unavailable for this stream)")
            {
                state.backend.name = name.to_owned();
            }
        }
        let format = state.format;
        if let Some(surface) = surface.as_ref() {
            let device = device.as_deref_mut().ok_or(CodecError::Unsupported(
                "decoder returned a D3D11 surface without its device",
            ))?;
            let mapped = device.map_nv12(surface)?;
            let image = mapped.image((format.width, format.height), format.display)?;
            (out)(DecodedFrame {
                image,
                timestamp: from_hns(timestamp),
            });
            return Ok(());
        }
        let locked = Locked::new(&buffer, format.stride.unwrap_or(format.width))?;
        let (x, y, width, height) = format.display;
        check_dimensions(width, height)?;
        let pitch = locked.pitch;
        let fits = x.is_multiple_of(2)
            && y.is_multiple_of(2)
            && x.checked_add(width)
                .is_some_and(|right| right as usize <= pitch)
            && y.checked_add(height)
                .is_some_and(|bottom| bottom <= format.height);
        if !fits {
            return Err(CodecError::Unsupported(
                "decoder display area is outside the picture",
            ));
        }
        let luma_size = pitch * format.height as usize;
        let (luma, chroma) =
            locked
                .data
                .split_at_checked(luma_size)
                .ok_or(CodecError::Unsupported(
                    "decoder output buffer is too small",
                ))?;
        let luma = &luma[y as usize * pitch + x as usize..];
        let chroma =
            chroma
                .get((y / 2) as usize * pitch + x as usize..)
                .ok_or(CodecError::Unsupported(
                    "decoder output buffer is too small",
                ))?;
        let image = Nv12::new(width, height, luma, pitch, chroma, pitch)?;
        (out)(DecodedFrame {
            image,
            timestamp: from_hns(timestamp),
        });
        Ok(())
    }

    fn caught_up(&self) -> bool {
        // A decoder's output does not map one-to-one onto its input: wait
        // until it asks for more.
        false
    }
}

/// A buffer locked for reading; unlocked on drop.
struct Locked<'a> {
    buffer: &'a IMFMediaBuffer,
    two_d: Option<IMF2DBuffer2>,
    data: &'a [u8],
    pitch: usize,
}

impl<'a> Locked<'a> {
    /// Locks a 2D buffer at its own pitch, or a plain buffer at
    /// `default_pitch`.
    fn new(buffer: &'a IMFMediaBuffer, default_pitch: u32) -> Result<Self, CodecError> {
        let mut scanline0 = std::ptr::null_mut();
        let mut start = std::ptr::null_mut();
        let mut pitch = 0i32;
        let mut length = 0u32;
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer2>() {
            // SAFETY: on success the buffer stays locked (and the pointers
            // valid for `length` bytes from `start`) until Unlock2D in Drop.
            unsafe {
                two_d
                    .Lock2DSize(
                        MF2DBuffer_LockFlags_Read,
                        &mut scanline0,
                        &mut pitch,
                        &mut start,
                        &mut length,
                    )
                    .or_platform("IMF2DBuffer2::Lock2DSize")?;
            }
            let mut locked = Self {
                buffer,
                two_d: Some(two_d),
                data: &[],
                pitch: 0,
            };
            locked.pitch = usize::try_from(pitch)
                .map_err(|_| CodecError::Unsupported("bottom-up decoder output"))?;
            let skipped = (scanline0 as usize).wrapping_sub(start as usize);
            if skipped > length as usize {
                return Err(CodecError::Unsupported(
                    "decoder output buffer is too small",
                ));
            }
            // SAFETY: `scanline0` lies within the locked region of `length`
            // bytes from `start`, checked above.
            locked.data =
                unsafe { std::slice::from_raw_parts(scanline0, length as usize - skipped) };
            return Ok(locked);
        }
        // SAFETY: on success the buffer stays locked (the pointer valid for
        // `length` bytes) until Unlock in Drop.
        unsafe {
            buffer
                .Lock(&mut start, None, Some(&mut length))
                .or_platform("IMFMediaBuffer::Lock")?;
            Ok(Self {
                buffer,
                two_d: None,
                data: std::slice::from_raw_parts(start, length as usize),
                pitch: default_pitch as usize,
            })
        }
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: balances the successful lock in `new`.
        unsafe {
            let _ = match &self.two_d {
                Some(two_d) => two_d.Unlock2D(),
                None => self.buffer.Unlock(),
            };
        }
    }
}
