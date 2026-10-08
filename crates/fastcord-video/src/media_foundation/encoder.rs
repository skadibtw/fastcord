//! H.264 encoding through a Media Foundation encoder transform.

use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFMediaType,
    IMFSample, IMFTransform, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MFMediaType_Video, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR,
    eAVEncH264VProfile_Base, eAVEncH264VProfile_ConstrainedBase,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface};

use super::runtime::{OrPlatform, Session};
use super::transform::{InputPool, Sink, Transform};
use super::{from_hns, pack, to_hns};
use crate::codec::{Backend, CodecError, EncodedFrame, EncoderConfig, VideoEncoder};
use crate::frame::Nv12;
use crate::h264::ParameterSets;

/// A Media Foundation H.264 encoder (see [`super::open_encoder`]).
pub struct MfEncoder {
    transform: Transform,
    codec_api: ICodecAPI,
    inputs: InputPool,
    state: EncoderState,
    config: EncoderConfig,
    backend: Backend,
    // Declared last: Media Foundation shuts down after every object above is
    // released.
    session: Session,
}

#[derive(Default)]
struct EncoderState {
    parameter_sets: ParameterSets,
    submitted: u64,
    emitted: u64,
}

impl MfEncoder {
    pub(super) fn open(
        session: Session,
        transform: Transform,
        backend: Backend,
        config: EncoderConfig,
    ) -> Result<Self, CodecError> {
        let mft = transform.mft();
        let codec_api: ICodecAPI = mft.cast().or_platform("IMFTransform as ICodecAPI")?;
        // Rate control, latency, and B-frame settings are applied where the
        // transform supports them; the Baseline profile excludes B-frames in
        // any case. The GOP length and forced keyframes (receiver PLI) are
        // required.
        let _ = set(
            &codec_api,
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_CBR.0 as u32,
        );
        let _ = set(&codec_api, &CODECAPI_AVEncCommonMeanBitRate, config.bitrate);
        let _ = set(&codec_api, &CODECAPI_AVLowLatencyMode, true);
        let _ = set(&codec_api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0u32);
        set(
            &codec_api,
            &CODECAPI_AVEncMPVGOPSize,
            config.keyframe_interval,
        )
        .or_platform("CODECAPI_AVEncMPVGOPSize")?;
        // SAFETY: COM call on a valid interface.
        unsafe { codec_api.IsSupported(&CODECAPI_AVEncVideoForceKeyFrame) }
            .map_err(|_| CodecError::Unsupported("the encoder cannot force keyframes"))?;

        // Encoders take the output type first. Constrained Baseline is what
        // real-time H.264 receivers negotiate; Baseline (which an encoder
        // without the constrained variant may offer) still has no B-frames.
        let mut accepted = Err(CodecError::Unsupported(
            "the encoder has no Baseline profile",
        ));
        for profile in [eAVEncH264VProfile_ConstrainedBase, eAVEncH264VProfile_Base] {
            let output = video_type(&session, &MFVideoFormat_H264, &config)?;
            // SAFETY: COM calls on valid interfaces.
            let result = unsafe {
                output
                    .SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate)
                    .and_then(|()| output.SetUINT32(&MF_MT_MPEG2_PROFILE, profile.0 as u32))
                    .and_then(|()| mft.SetOutputType(transform.output_id(), &output, 0))
            };
            accepted = result.or_platform("IMFTransform::SetOutputType");
            if accepted.is_ok() {
                break;
            }
        }
        accepted?;
        let input = video_type(&session, &MFVideoFormat_NV12, &config)?;
        // SAFETY: COM call on valid interfaces.
        unsafe { mft.SetInputType(transform.input_id(), &input, 0) }
            .or_platform("IMFTransform::SetInputType")?;

        let mut encoder = Self {
            transform,
            codec_api,
            inputs: InputPool::default(),
            state: EncoderState::default(),
            config,
            backend,
            session,
        };
        encoder.transform.start()?;
        // Some encoders publish SPS/PPS only in the output type; keyframes
        // must carry them in-band.
        if let Some(header) = encoder.sequence_header() {
            // A malformed header is not fatal here: an IDR picture without
            // usable parameter sets is reported when it is produced.
            let _ = encoder.state.parameter_sets.complete(header);
        }
        Ok(encoder)
    }

    fn sequence_header(&self) -> Option<Vec<u8>> {
        // SAFETY: COM calls on valid interfaces; the blob is copied into a
        // buffer of the reported size.
        unsafe {
            let current = self
                .transform
                .mft()
                .GetOutputCurrentType(self.transform.output_id())
                .ok()?;
            let size = current.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
            let mut header = vec![0; size as usize];
            current
                .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None)
                .ok()?;
            Some(header)
        }
    }
}

fn set(codec_api: &ICodecAPI, key: &GUID, value: impl Into<VARIANT>) -> windows::core::Result<()> {
    let value = value.into();
    // SAFETY: COM call with a valid property key and VARIANT.
    unsafe { codec_api.SetValue(key, &value) }
}

/// A progressive video media type of `subtype` at the configured size and
/// rate with square pixels.
fn video_type(
    session: &Session,
    subtype: &GUID,
    config: &EncoderConfig,
) -> Result<IMFMediaType, CodecError> {
    let media_type = session.media_type()?;
    let pack_pair = |high: u32, low: u32| (u64::from(high) << 32) | u64::from(low);
    let set_all = || -> windows::core::Result<()> {
        // SAFETY: COM calls on a valid media type.
        unsafe {
            media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
            media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_pair(config.width, config.height))?;
            media_type.SetUINT64(&MF_MT_FRAME_RATE, pack_pair(config.frame_rate, 1))?;
            media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_pair(1, 1))?;
            media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
        }
    };
    set_all().or_platform("IMFMediaType::Set")?;
    Ok(media_type)
}

impl VideoEncoder for MfEncoder {
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
        if actual != expected {
            return Err(CodecError::FrameSize { expected, actual });
        }
        let alignment = self.transform.input_alignment()?;
        let sample =
            self.inputs
                .sample(&self.session, frame.packed_len(), alignment, |buffer| {
                    pack(frame, buffer)
                })?;
        // SAFETY: COM calls on a valid sample.
        unsafe {
            sample
                .SetSampleTime(to_hns(timestamp))
                .and_then(|()| sample.SetSampleDuration(to_hns(self.config.frame_duration())))
        }
        .or_platform("IMFSample::SetSampleTime")?;
        if keyframe {
            set(&self.codec_api, &CODECAPI_AVEncVideoForceKeyFrame, 1u32)
                .or_platform("CODECAPI_AVEncVideoForceKeyFrame")?;
        }
        self.state.submitted += 1;
        let mut collect = Collect {
            state: &mut self.state,
            out,
        };
        self.transform
            .push(&self.session, &sample, &mut collect, false)
    }

    fn flush(&mut self, out: &mut Vec<EncodedFrame>) -> Result<(), CodecError> {
        let mut collect = Collect {
            state: &mut self.state,
            out,
        };
        self.transform.drain(&self.session, &mut collect, false)
    }
}

/// Collects encoded access units into the caller's vector.
struct Collect<'a> {
    state: &'a mut EncoderState,
    out: &'a mut Vec<EncodedFrame>,
}

impl Sink for Collect<'_> {
    fn stream_changed(&mut self, mft: &IMFTransform, output_id: u32) -> Result<(), CodecError> {
        // SAFETY: COM calls on a valid interface.
        unsafe {
            let available = mft
                .GetOutputAvailableType(output_id, 0)
                .or_platform("IMFTransform::GetOutputAvailableType")?;
            mft.SetOutputType(output_id, &available, 0)
                .or_platform("IMFTransform::SetOutputType")
        }
    }

    fn output(&mut self, sample: &IMFSample) -> Result<(), CodecError> {
        // SAFETY: COM calls on a valid sample; the buffer is unlocked right
        // after its bytes (the reported current length) are copied.
        let (data, timestamp) = unsafe {
            let timestamp = sample.GetSampleTime().unwrap_or(0);
            let buffer = sample
                .ConvertToContiguousBuffer()
                .or_platform("IMFSample::ConvertToContiguousBuffer")?;
            let mut bytes = std::ptr::null_mut();
            let mut length = 0u32;
            buffer
                .Lock(&mut bytes, None, Some(&mut length))
                .or_platform("IMFMediaBuffer::Lock")?;
            let data = std::slice::from_raw_parts(bytes, length as usize).to_vec();
            buffer.Unlock().or_platform("IMFMediaBuffer::Unlock")?;
            (data, timestamp)
        };
        if let Some((data, keyframe)) = self.state.parameter_sets.complete(data)? {
            self.state.emitted += 1;
            self.out.push(EncodedFrame {
                data,
                timestamp: from_hns(timestamp),
                keyframe,
            });
        }
        Ok(())
    }

    fn caught_up(&self) -> bool {
        self.state.emitted >= self.state.submitted
    }
}
