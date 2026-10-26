//! H.264 encoding through a VA-API hardware encoder.
//!
//! A [`VaEncoder`] owns one VA-API display and every object created on it
//! (surfaces, context, image, config), whose release order mirrors the field
//! declaration order at the `VaEncoder` use site. The encoder is validated
//! against a Constrained-Baseline/Baseline stream with no B-frames: one I or P
//! slice per picture, one short-term reference, picture-order-count type 2, and
//! a driver-managed reconstruction ring of two slots (plus one source slot).
//!
//! The driver packs the slice NAL units itself (`VAEntrypointEncSlice` needs no
//! slice data buffer). Encoded output is read from the coded buffer of the
//! previous picture while the next one is submitted, so [`VideoEncoder::encode`]
//! appends its output with one picture of pipeline depth; [`flush`] collects a
//! still-pending picture. The driver may or may not emit its own SPS/PPS NAL
//! units for an IDR picture, so every access unit goes through
//! [`ParameterSets`], seeded with the locally emitted parameter sets in `open`.
//! That keeps the mirror between the emitted SPS and the sequence-parameter
//! buffer the driver renders on every IDR picture intact.
//!
//! The running encoder cannot change its target bitrate: the rate is configured
//! once for the whole sequence (`VA_RC_CBR` with the sequence parameters), so
//! [`VideoEncoder::set_bitrate`] keeps the trait's default
//! [`CodecError::Unsupported`] answer and the share pipeline reopens the
//! encoder at the new rate.
//!
//! [`flush`]: VideoEncoder::flush
//! [`ParameterSets`]: crate::h264::ParameterSets

use std::ffi;
use std::mem;
use std::path::Path;
use std::ptr;
use std::rc::Rc;
use std::time::Duration;

use super::{
    ConfigId, ContextId, Display, Image, NativeBuffer, Surfaces, Synchronized, backend, check,
    invalid_picture, picture, sync_surface,
};
use crate::bitwriter::{BitWriter, escape};
use crate::codec::{Backend, BackendKind, CodecError, EncodedFrame, EncoderConfig, VideoEncoder};
use crate::frame::Nv12;
use crate::h264::{self, ParameterSets};

use super::bindings::*;

/// Slot 0 of every encoder [`Surfaces`] set: the picture's source surface.
const SOURCE_SURFACE: usize = 0;

/// Cap for one picture's coded buffer, equal to the decoder's access-unit cap:
/// `vaCreateBuffer` allocates the whole buffer immediately, so a picture that
/// could overrun it would fail the codec contract later anyway.
const CODED_BUFFER_CAP: usize = h264::MAX_ACCESS_UNIT_BYTES;

/// `slice_type` of the one I slice covering the picture (all macroblocks I).
const SLICE_TYPE_I: u8 = 7;
/// `slice_type` of the one P slice covering the picture (all macroblocks P).
const SLICE_TYPE_P: u8 = 5;

/// Parameter set ids fixed for the encoder's whole lifetime.
const SEQUENCE_PARAMETER_SET_ID: u8 = 0;
const PICTURE_PARAMETER_SET_ID: u8 = 0;
/// Inline QP of the whole sequence (`pic_init_qp_minus26 + 26`).
const PICTURE_INIT_QP: u8 = 26;
/// Deblocking is fully disabled in the one slice per picture (`idc = 2`).
const DEBLOCK_DISABLE_ALL: u8 = 2;
/// `log2_max_frame_num_minus4` of 4: `frame_num` wraps at 256.
const FRAME_NUM_MASK: u16 = 0xFF;

/// SPS NAL header byte of a Baseline-profile stream (`nal_ref_idc = 3`).
const SPS_NAL_HEADER: u8 = 0x67;
/// PPS NAL header byte of a Baseline-profile stream (`nal_ref_idc = 3`).
const PPS_NAL_HEADER: u8 = 0x68;

/// `profile_idc` of the Baseline profile this encoder emits.
const BASELINE_PROFILE_IDC: u8 = 66;
/// `constraint_set0_flag` and `constraint_set1_flag`: marks the stream as
/// decodable by every Constrained-Baseline decoder.
const CONSTRAINED_BASELINE_FLAGS: u8 = 0xC0;

/// Annex A level limits relevant to this encoder, lowest level first:
/// (`level_idc`, MaxMBPS, MaxFS, MaxBR in bit/s), as they apply to the
/// Constrained-Baseline/Baseline/Main/Extended parameter row.
const LEVELS: [(u8, u64, u64, u32); 16] = [
    (10, 1485, 99, 64_000),
    (11, 3000, 396, 192_000),
    (12, 6000, 396, 384_000),
    (13, 11880, 396, 768_000),
    (20, 11880, 396, 2_000_000),
    (21, 19800, 792, 4_000_000),
    (22, 20250, 1620, 4_000_000),
    (30, 40500, 1620, 10_000_000),
    (31, 108000, 3600, 14_000_000),
    (32, 216000, 5120, 20_000_000),
    (40, 245760, 8192, 20_000_000),
    (41, 245760, 8192, 50_000_000),
    (42, 522240, 8704, 50_000_000),
    (50, 589824, 22080, 135_000_000),
    (51, 983040, 36864, 240_000_000),
    (52, 983040, 36864, 240_000_000),
];

/// Appends one NAL unit (a start code, `header` and the escaped RBSP) to `out`.
fn push_nal(out: &mut Vec<u8>, header: u8, rbsp: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1, header]);
    out.extend_from_slice(&escape(rbsp));
}

/// Pictures are padded up to whole macroblocks; the frame crop halves the
/// difference on the right and the bottom.
fn aligned_dimensions(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(16) * 16, height.div_ceil(16) * 16)
}

/// Coded and displayed geometry of one encoding session.
#[derive(Clone, Copy)]
struct Geometry {
    coded_width: u32,
    coded_height: u32,
    frame_cropping: bool,
    crop_left: u32,
    crop_right: u32,
    crop_top: u32,
    crop_bottom: u32,
}

impl Geometry {
    /// Pads the validated even dimensions up to macroblocks and computes the
    /// 4:2:0 frame crop (crop units of 2 luma samples on the right and bottom;
    /// nothing for a macroblock-aligned picture).
    fn new(config: &EncoderConfig) -> Self {
        let (coded_width, coded_height) = aligned_dimensions(config.width, config.height);
        let crop_right = (coded_width - config.width) / 2;
        let crop_bottom = (coded_height - config.height) / 2;
        Self {
            coded_width,
            coded_height,
            frame_cropping: crop_right != 0 || crop_bottom != 0,
            crop_left: 0,
            crop_right,
            crop_top: 0,
            crop_bottom,
        }
    }

    /// The padded grid in macroblocks.
    fn macroblocks(&self) -> (u16, u16) {
        (
            (self.coded_width / 16) as u16,
            (self.coded_height / 16) as u16,
        )
    }
}

/// The lowest Annex A level supporting the whole picture statistics of
/// `config` (the padded macroblock grid, the macroblock rate at the frame
/// rate, and the bitrate), or [`CodecError::Unsupported`] when they exceed
/// every level of the encoder's Baseline profile.
fn choose_level(config: &EncoderConfig) -> Result<u8, CodecError> {
    let (coded_width, coded_height) = aligned_dimensions(config.width, config.height);
    let macroblocks = u64::from(coded_width) * u64::from(coded_height) / 256;
    let macroblock_rate = macroblocks * u64::from(config.frame_rate);
    let bitrate = u64::from(config.bitrate);
    for &(level, max_macroblock_rate, max_macroblocks, max_bitrate) in &LEVELS {
        if macroblocks <= max_macroblocks
            && macroblock_rate <= max_macroblock_rate
            && bitrate <= u64::from(max_bitrate)
        {
            return Ok(level);
        }
    }
    Err(CodecError::Unsupported(
        "picture size or rate exceeds every H.264 level",
    ))
}

/// The SPS RBSP for the whole session ([`SPS_NAL_HEADER`] carries the profile).
fn sps_rbsp(geometry: &Geometry, level: u8) -> Vec<u8> {
    let mut writer = BitWriter::default();
    writer.bits(u64::from(BASELINE_PROFILE_IDC), 8); // profile_idc
    // constraint_set0_flag and constraint_set1_flag: decodable by every
    // Constrained-Baseline decoder.
    writer.bits(u64::from(CONSTRAINED_BASELINE_FLAGS), 8);
    writer.bits(u64::from(level), 8); // level_idc
    writer.ue(u32::from(SEQUENCE_PARAMETER_SET_ID));
    writer.ue(4); // log2_max_frame_num_minus4: log2_max_frame_num = 8
    writer.ue(2); // pic_order_cnt_type: output order is decoding order
    writer.ue(1); // max_num_ref_frames
    writer.flag(false); // gaps_in_frame_num_value_allowed_flag
    let (mb_width, mb_height) = geometry.macroblocks();
    writer.ue(u32::from(mb_width) - 1);
    writer.ue(u32::from(mb_height) - 1);
    writer.flag(true); // frame_mbs_only_flag
    writer.flag(true); // direct_8x8_inference_flag
    writer.flag(geometry.frame_cropping);
    if geometry.frame_cropping {
        writer.ue(geometry.crop_left);
        writer.ue(geometry.crop_right);
        writer.ue(geometry.crop_top);
        writer.ue(geometry.crop_bottom);
    }
    writer.flag(false); // vui_parameters_present_flag
    writer.trailing();
    writer.finish()
}

/// The PPS RBSP for the whole session.
fn pps_rbsp() -> Vec<u8> {
    let mut writer = BitWriter::default();
    writer.ue(u32::from(PICTURE_PARAMETER_SET_ID));
    writer.ue(u32::from(SEQUENCE_PARAMETER_SET_ID));
    writer.flag(false); // entropy_coding_mode_flag: CAVLC (Baseline)
    writer.flag(false); // bottom_field_pic_order_in_frame_present_flag
    writer.ue(0); // num_slice_groups_minus1
    writer.ue(0); // num_ref_idx_l0_default_active_minus1: one reference
    writer.ue(0); // num_ref_idx_l1_default_active_minus1
    writer.flag(false); // weighted_pred_flag
    writer.bits(0, 2); // weighted_bipred_idc
    writer.se(i32::from(PICTURE_INIT_QP) - 26);
    writer.se(0); // pic_init_qs_minus26
    writer.se(0); // chroma_qp_index_offset
    writer.flag(true); // deblocking_filter_control_present_flag
    writer.flag(false); // constrained_intra_pred_flag
    writer.flag(false); // redundant_pic_cnt_present_flag
    writer.trailing();
    writer.finish()
}

/// The VA-API sequence-parameter record mirroring [`sps_rbsp`], which the
/// driver renders with (only) every IDR picture.
fn sequence_buffer(
    geometry: &Geometry,
    level: u8,
    config: &EncoderConfig,
) -> VAEncSequenceParameterBufferH264 {
    let (mb_width, mb_height) = geometry.macroblocks();
    VAEncSequenceParameterBufferH264 {
        seq_parameter_set_id: SEQUENCE_PARAMETER_SET_ID,
        level_idc: level,
        // No non-IDR I pictures; the GOP length is the IDR keyframe interval.
        intra_period: 0,
        intra_idr_period: config.keyframe_interval,
        ip_period: 0,
        bits_per_second: config.bitrate,
        max_num_ref_frames: 1,
        picture_width_in_mbs: mb_width,
        picture_height_in_mbs: mb_height,
        seq_fields: _VAEncSequenceParameterBufferH264__bindgen_ty_1 {
            bits: _VAEncSequenceParameterBufferH264__bindgen_ty_1__bindgen_ty_1 {
                _bindgen_align: [],
                _bitfield_1:
                    _VAEncSequenceParameterBufferH264__bindgen_ty_1__bindgen_ty_1::new_bitfield_1(
                        1, // chroma_format_idc: 4:2:0
                        1, // frame_mbs_only_flag
                        0, // mb_adaptive_frame_field_flag
                        0, // seq_scaling_matrix_present_flag
                        1, // direct_8x8_inference_flag
                        4, // log2_max_frame_num_minus4
                        2, // pic_order_cnt_type
                        0, // log2_max_pic_order_cnt_lsb_minus4
                        0, // delta_pic_order_always_zero_flag
                    ),
                __bindgen_padding_0: 0,
            },
        },
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        num_ref_frames_in_pic_order_cnt_cycle: 0,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        offset_for_ref_frame: [0; 256],
        frame_cropping_flag: u8::from(geometry.frame_cropping),
        frame_crop_left_offset: geometry.crop_left,
        frame_crop_right_offset: geometry.crop_right,
        frame_crop_top_offset: geometry.crop_top,
        frame_crop_bottom_offset: geometry.crop_bottom,
        vui_parameters_present_flag: 0,
        vui_fields: _VAEncSequenceParameterBufferH264__bindgen_ty_2 {
            bits: _VAEncSequenceParameterBufferH264__bindgen_ty_2__bindgen_ty_1 {
                _bindgen_align: [],
                _bitfield_1:
                    _VAEncSequenceParameterBufferH264__bindgen_ty_2__bindgen_ty_1::new_bitfield_1(
                        0, // aspect_ratio_info_present_flag
                        0, // timing_info_present_flag
                        0, // bitstream_restriction_flag
                        0, // log2_max_mv_length_horizontal
                        0, // log2_max_mv_length_vertical
                        0, // fixed_frame_rate_flag
                        0, // low_delay_hrd_flag
                        0, // motion_vectors_over_pic_boundaries_flag
                        0, // reserved
                    ),
            },
        },
        aspect_ratio_idc: 0,
        sar_width: 0,
        sar_height: 0,
        num_units_in_tick: 0,
        time_scale: 0,
        va_reserved: [0; 4],
    }
}

/// The VA-API picture-parameter record for one picture.
fn picture_buffer(
    idr: bool,
    frame_num: u16,
    reference: Option<VAPictureH264>,
    recon: VASurfaceID,
    frame_idx: u32,
    coded: VABufferID,
) -> VAEncPictureParameterBufferH264 {
    // Picture-order-count type 2: the presentation order is the decoding
    // order, scaled by two.
    let picture_order_count = 2 * i32::from(frame_num);
    let mut references = [invalid_picture(); 16];
    if let Some(reference) = reference {
        references[0] = reference;
    }
    VAEncPictureParameterBufferH264 {
        // The reconstruction surface is where the driver leaves this picture
        // for the next one to reference.
        CurrPic: picture(
            recon,
            frame_idx,
            0,
            picture_order_count,
            picture_order_count,
        ),
        ReferenceFrames: references,
        coded_buf: coded,
        pic_parameter_set_id: PICTURE_PARAMETER_SET_ID,
        seq_parameter_set_id: SEQUENCE_PARAMETER_SET_ID,
        last_picture: 0,
        frame_num,
        pic_init_qp: PICTURE_INIT_QP,
        num_ref_idx_l0_active_minus1: 0,
        num_ref_idx_l1_active_minus1: 0,
        chroma_qp_index_offset: 0,
        second_chroma_qp_index_offset: 0,
        pic_fields: _VAEncPictureParameterBufferH264__bindgen_ty_1 {
            bits: _VAEncPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1 {
                _bindgen_align: [],
                _bitfield_1:
                    _VAEncPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1::new_bitfield_1(
                        u32::from(idr), // idr_pic_flag
                        1,              // reference_pic_flag: every picture becomes
                        // the next one's reference
                        0, // entropy_coding_mode_flag (CAVLC)
                        0, // weighted_pred_flag
                        0, // weighted_bipred_idc
                        0, // constrained_intra_pred_flag
                        0, // transform_8x8_mode_flag
                        1, // deblocking_filter_control_present_flag
                        0, // redundant_pic_cnt_present_flag
                        0, // pic_order_present_flag
                        0, // pic_scaling_matrix_present_flag
                    ),
                __bindgen_padding_0: 0,
            },
        },
        va_reserved: [0; 4],
    }
}

/// The VA-API slice-parameter record for the one slice covering a picture.
fn slice_buffer(
    idr: bool,
    idr_pic_id: u16,
    _frame_num: u16,
    reference: Option<VAPictureH264>,
    macroblocks: (u16, u16),
) -> VAEncSliceParameterBufferH264 {
    // Picture-order-count type 2; the driver only codes what the picture
    // flags select.
    let mut references = [invalid_picture(); 32];
    if let Some(reference) = reference {
        references[0] = reference;
    }
    VAEncSliceParameterBufferH264 {
        macroblock_address: 0,
        num_macroblocks: u32::from(macroblocks.0) * u32::from(macroblocks.1),
        macroblock_info: VA_INVALID_ID as VABufferID,
        slice_type: if idr { SLICE_TYPE_I } else { SLICE_TYPE_P },
        pic_parameter_set_id: PICTURE_PARAMETER_SET_ID,
        idr_pic_id,
        pic_order_cnt_lsb: 0,
        delta_pic_order_cnt_bottom: 0,
        delta_pic_order_cnt: [0; 2],
        direct_spatial_mv_pred_flag: 0,
        num_ref_idx_active_override_flag: 0,
        num_ref_idx_l0_active_minus1: 0,
        num_ref_idx_l1_active_minus1: 0,
        RefPicList0: references,
        RefPicList1: [invalid_picture(); 32],
        luma_log2_weight_denom: 0,
        chroma_log2_weight_denom: 0,
        luma_weight_l0_flag: 0,
        luma_weight_l0: [0; 32],
        luma_offset_l0: [0; 32],
        chroma_weight_l0_flag: 0,
        chroma_weight_l0: [[0; 2]; 32],
        chroma_offset_l0: [[0; 2]; 32],
        luma_weight_l1_flag: 0,
        luma_weight_l1: [0; 32],
        luma_offset_l1: [0; 32],
        chroma_weight_l1_flag: 0,
        chroma_weight_l1: [[0; 2]; 32],
        chroma_offset_l1: [[0; 2]; 32],
        cabac_init_idc: 0,
        slice_qp_delta: 0,
        disable_deblocking_filter_idc: DEBLOCK_DISABLE_ALL,
        slice_alpha_c0_offset_div2: 0,
        slice_beta_offset_div2: 0,
        va_reserved: [0; 4],
    }
}

/// A byte view over a `repr(C)` parameter-set record, for
/// [`NativeBuffer::create`], which copies the bytes at `vaCreateBuffer` time.
fn record_bytes<T>(record: &T) -> &[u8] {
    // SAFETY: parameter-set records are `repr(C)` plain data; the span covers
    // the whole record (padding included, read is allowed), and the bytes are
    // copied before the record locals end.
    unsafe {
        std::slice::from_raw_parts(ptr::from_ref(record).cast::<u8>(), mem::size_of_val(record))
    }
}

/// Internal counters advanced per submitted picture.
struct State {
    /// `frame_num` of the next P slice (IDR pictures encode 0 and reset this).
    frame_num: u16,
    /// `idr_pic_id` increments on every IDR picture.
    idr_pic_id: u16,
    /// Monotonic picture index: the DPB identifier in
    /// [`VAPictureH264::frame_idx`] and the reconstruction ring slot chooser.
    frame_idx: u32,
    /// Pictures encoded since the last IDR picture.
    pictures_since_idr: u32,
    /// The previous picture's reconstruction as a short-term reference.
    reference: Option<VAPictureH264>,
}

impl State {
    fn new() -> Self {
        Self {
            frame_num: 1,
            idr_pic_id: 0,
            frame_idx: 0,
            pictures_since_idr: 0,
            reference: None,
        }
    }
}

/// A submitted picture whose access unit is not drained yet: its parameter
/// and coded buffers (destroyed after the drain) and where its reconstruction
/// lives.
struct Pending {
    params: Vec<NativeBuffer>,
    coded: NativeBuffer,
    /// The picture's reconstruction surface; the fallback sync target when a
    /// driver without `vaSyncBuffer` is used.
    recon: VASurfaceID,
    timestamp: Duration,
}

impl Pending {
    /// Waits for the access unit: the timeout-aware `vaSyncBuffer` on the
    /// coded buffer, falling back to the recon surface (`vaSyncSurface2`
    /// with the module's timeout) when the driver does not implement it.
    fn wait(&self, display: &Rc<Display>) -> Result<(), CodecError> {
        let handle = display.handle();
        // SAFETY: the coded buffer is live in this context and the timeout is
        // a plain value; the driver fills it before the call returns.
        let status = unsafe { vaSyncBuffer(handle, self.coded.id(), super::SYNC_TIMEOUT_NS) };
        if status as u32 == VA_STATUS_ERROR_UNIMPLEMENTED {
            return sync_surface(display, self.recon);
        }
        check("vaSyncBuffer", status)
    }

    /// Reads the coded access unit out of the mapped buffer's
    /// [`VACodedBufferSegment`] chain, capped at [`CODED_BUFFER_CAP`].
    fn read_coded(&self, display: &Rc<Display>) -> Result<Vec<u8>, CodecError> {
        let handle = display.handle();
        let mut mapped: *mut ffi::c_void = ptr::null_mut();
        Synchronized::map(handle, self.coded.id(), &mut mapped)?;
        let walk = walk_segments(mapped.cast::<VACodedBufferSegment>());
        Synchronized::unmap(handle, self.coded.id())?;
        walk
    }
}

/// Walks the [`VACodedBufferSegment`] chain of a mapped coded buffer into
/// bytes. The chain lives inside the mapped window and is walked before the
/// caller unmaps.
fn walk_segments(mut segments: *const VACodedBufferSegment) -> Result<Vec<u8>, CodecError> {
    let mut data = Vec::new();
    while !segments.is_null() {
        // SAFETY: libva maps a coded buffer as a linked list of
        // `VACodedBufferSegment` records inside the mapped window; `next` is
        // either null (the end) or a record in the same window.
        let segment = unsafe { *segments };
        if segment.size as usize > CODED_BUFFER_CAP.saturating_sub(data.len()) {
            return Err(CodecError::Unsupported(
                "the coded access unit exceeds the 8 MiB cap",
            ));
        }
        if segment.bit_offset != 0 {
            return Err(CodecError::Unsupported(
                "the driver produced bit-unaligned coded data",
            ));
        }
        if !segment.buf.is_null() && segment.size > 0 {
            // SAFETY: `segment.buf` points at `segment.size` bytes of coded
            // data inside the mapped window, valid until the caller unmaps.
            let bytes = unsafe {
                std::slice::from_raw_parts(segment.buf.cast::<u8>(), segment.size as usize)
            };
            data.extend_from_slice(bytes);
        }
        segments = segment.next.cast::<VACodedBufferSegment>();
    }
    Ok(data)
}

/// A VA-API hardware H.264 encoder (see [`super::HardwareSource::open_encoder`]).
///
/// [`VideoEncoder::encode`] hands out the previous picture's access unit (one
/// picture of pipeline depth) and rejects every frame that does not match
/// `config` exactly.
pub struct VaEncoder {
    pending: Option<Pending>,
    parameter_sets: ParameterSets,
    state: State,
    /// Locally emitted SPS/PPS RBSPs; the sequence parameters mirror them.
    sps: Vec<u8>,
    pps: Vec<u8>,
    level: u8,
    geometry: Geometry,
    config: EncoderConfig,
    backend: Backend,
    image: Image,
    context: ContextId,
    surfaces: Surfaces,
    config_id: ConfigId,
    // Declared last: VA-API terminates after every object above is released.
    display: Rc<Display>,
}

impl VaEncoder {
    /// Opens the hardware encoder on a DRM device for the probed `profile`.
    pub(crate) fn open(
        device: &Path,
        profile: VAProfile,
        config: EncoderConfig,
    ) -> Result<Self, CodecError> {
        let level = choose_level(&config)?;
        let geometry = Geometry::new(&config);
        let display = Display::open(device)?;
        let name = format!("{} on {}", display.vendor(), device.display());
        let attribs = Self::attributes(&display, profile)?;
        let config_id = ConfigId::create(
            &display,
            profile,
            VAEntrypoint_VAEntrypointEncSlice,
            &attribs,
        )?;
        let surfaces = Surfaces::create(&display, 3, geometry.coded_width, geometry.coded_height)?;
        let context = ContextId::create(
            &display,
            config_id.id(),
            geometry.coded_width as i32,
            geometry.coded_height as i32,
            VA_PROGRESSIVE as i32,
            surfaces.as_slice(),
        )?;
        let image = Image::create(
            &display,
            geometry.coded_width as i32,
            geometry.coded_height as i32,
        )?;
        let mut encoder = Self {
            pending: None,
            parameter_sets: ParameterSets::default(),
            state: State::new(),
            sps: sps_rbsp(&geometry, level),
            pps: pps_rbsp(),
            level,
            geometry,
            backend: backend(BackendKind::Hardware, name),
            config,
            image,
            context,
            surfaces,
            config_id,
            display,
        };
        // Seed the parameter sets so every later IDR picture carries the SPS
        // and PPS even when the driver does not pack them itself; a seeded
        // header is header-only output that `complete` remembers.
        let mut seeded = Vec::new();
        push_nal(&mut seeded, SPS_NAL_HEADER, &encoder.sps);
        push_nal(&mut seeded, PPS_NAL_HEADER, &encoder.pps);
        encoder.parameter_sets.complete(seeded)?;
        Ok(encoder)
    }

    /// The rate-control and pixel-format attributes of the probed
    /// `profile` (`VAEntrypointEncSlice`, a 4:2:0 render target and
    /// `VA_RC_CBR`).
    fn attributes(
        display: &Display,
        profile: VAProfile,
    ) -> Result<[VAConfigAttrib; 2], CodecError> {
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
        // SAFETY: `attribs` is a real two-element array for the driver to fill.
        check("vaGetConfigAttributes", unsafe {
            vaGetConfigAttributes(
                display.handle(),
                profile,
                VAEntrypoint_VAEntrypointEncSlice,
                attribs.as_mut_ptr(),
                attribs.len() as i32,
            )
        })?;
        if attribs[0].value != VA_RT_FORMAT_YUV420 {
            return Err(CodecError::Unsupported(
                "the encoder reports a format other than 4:2:0",
            ));
        }
        if attribs[1].value & VA_RC_CBR != VA_RC_CBR {
            return Err(CodecError::Unsupported(
                "the encoder does not offer CBR rate control",
            ));
        }
        Ok(attribs)
    }

    /// Appends the pending picture's access unit to `out`, if any: it first
    /// waits for the driver to finish the coded buffer, then reads the access
    /// unit and runs it through the parameter sets.
    fn drain(&mut self, out: &mut Vec<EncodedFrame>) -> Result<(), CodecError> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        pending.wait(&self.display)?;
        let data = pending.read_coded(&self.display)?;
        if let Some((data, keyframe)) = self.parameter_sets.complete(data)? {
            out.push(EncodedFrame {
                data,
                timestamp: pending.timestamp,
                keyframe,
            });
        }
        Ok(())
    }

    /// Renders one whole picture: sequence (IDR pictures), picture and slice
    /// parameter buffers, with the coded buffer live at `vaEndPicture`. The
    /// counters advance after the records are built, so this picture's
    /// reconstruction becomes the next picture's reference.
    fn submit(
        &mut self,
        frame: &Nv12<'_>,
        idr: bool,
        timestamp: Duration,
    ) -> Result<Pending, CodecError> {
        let handle = self.display.handle();
        let context = self.context.id();
        let frame_idx = self.state.frame_idx;
        let slot = 1 + (frame_idx % 2) as usize;
        let recon = self.surfaces.id(slot);

        // The picture's own DPB views, built before the counters advance.
        let this_frame_num = if idr { 0 } else { self.state.frame_num };
        let idr_pic_id = self.state.idr_pic_id;
        let reference = self.state.reference;

        if idr {
            self.state.frame_num = 1;
            self.state.idr_pic_id = self.state.idr_pic_id.wrapping_add(1);
            self.state.pictures_since_idr = 0;
        } else {
            self.state.frame_num = (self.state.frame_num + 1) & FRAME_NUM_MASK;
            self.state.pictures_since_idr += 1;
        }

        self.image.upload(self.surfaces.id(SOURCE_SURFACE), frame)?;

        let mut params = Vec::new();
        if idr {
            let sequence = sequence_buffer(&self.geometry, self.level, &self.config);
            params.push(NativeBuffer::create(
                &self.display,
                context,
                VABufferType_VAEncSequenceParameterBufferType,
                record_bytes(&sequence),
            )?);
        }
        let coded = NativeBuffer::create_empty(
            &self.display,
            context,
            VABufferType_VAEncCodedBufferType,
            CODED_BUFFER_CAP,
        )?;
        let picture_params =
            picture_buffer(idr, this_frame_num, reference, recon, frame_idx, coded.id());
        params.push(NativeBuffer::create(
            &self.display,
            context,
            VABufferType_VAEncPictureParameterBufferType,
            record_bytes(&picture_params),
        )?);
        let slice = slice_buffer(
            idr,
            idr_pic_id,
            this_frame_num,
            reference,
            self.geometry.macroblocks(),
        );
        params.push(NativeBuffer::create(
            &self.display,
            context,
            VABufferType_VAEncSliceParameterBufferType,
            record_bytes(&slice),
        )?);

        let mut rendered: Vec<VABufferID> = params.iter().map(NativeBuffer::id).collect();
        // SAFETY: every surface and buffer id belongs to this context; the
        // source surface is the encoder's input for this picture.
        check("vaBeginPicture", unsafe {
            vaBeginPicture(handle, context, self.surfaces.id(SOURCE_SURFACE))
        })?;
        // SAFETY: `rendered` holds the ids of the parameter buffers created
        // above; the driver reads them through libva.
        check("vaRenderPicture", unsafe {
            vaRenderPicture(
                handle,
                context,
                rendered.as_mut_ptr(),
                rendered.len() as i32,
            )
        })?;
        // SAFETY: the picture finishes here and the driver starts processing.
        check("vaEndPicture", unsafe { vaEndPicture(handle, context) })?;

        // This picture's reconstruction is the next picture's short-term
        // reference; `frame_idx` is its DPB identifier.
        let picture_order_count = 2 * i32::from(this_frame_num);
        self.state.reference = Some(picture(
            recon,
            frame_idx,
            VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            picture_order_count,
            picture_order_count,
        ));
        self.state.frame_idx = frame_idx + 1;

        Ok(Pending {
            params,
            coded,
            recon,
            timestamp,
        })
    }
}

impl VideoEncoder for VaEncoder {
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
        // Drain first so exactly one picture is pending at any time.
        self.drain(out)?;
        // `frame_idx` is the ordinal of the picture being submitted; a picture
        // past the interval starts a new IDR sequence (a forced keyframe does
        // not move the automatic rhythm).
        let idr = keyframe
            || self.state.frame_idx == 0
            || self.state.pictures_since_idr + 1 >= self.config.keyframe_interval;
        let pending = self.submit(frame, idr, timestamp)?;
        self.pending = Some(pending);
        Ok(())
    }

    fn flush(&mut self, out: &mut Vec<EncodedFrame>) -> Result<(), CodecError> {
        self.drain(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::CodecError;
    use crate::frame::ColorDescription;
    use crate::h264::{BitReader, Sps};
    use crate::test_vectors::moving_pattern;
    use std::ffi;
    use std::ptr;
    use std::time::Duration;

    const CONFIG: EncoderConfig = EncoderConfig::SCREEN_SHARE_720P30;

    #[test]
    fn chooses_the_lowest_level_meeting_annex_a() {
        // 1280x720 at 30 fps: 3600 MB, 108000 MB/s, 2.5 Mbit/s.
        assert_eq!(choose_level(&CONFIG).unwrap(), 31);
        // 1920x1080 at 60 fps: 8160 MB (1080 pads to 1088), 489600 MB/s.
        let level = choose_level(&EncoderConfig {
            width: 1920,
            height: 1080,
            frame_rate: 60,
            bitrate: 20_000_000,
            keyframe_interval: 300,
        })
        .unwrap();
        assert_eq!(level, 42);
        // 4K at 60 fps: 18432 MB, 1105920 MB/s, over every Baseline level.
        let oversized = choose_level(&EncoderConfig {
            width: 4096,
            height: 2304,
            frame_rate: 60,
            ..EncoderConfig::SCREEN_SHARE_720P30
        });
        assert!(matches!(oversized, Err(CodecError::Unsupported(_))));
        // A rate above the highest MaxBR row is out too (validate() caps it at
        // 50 Mbit/s, so this cannot happen through the open() path).
        let lavish = choose_level(&EncoderConfig {
            bitrate: 300_000_000,
            ..EncoderConfig::SCREEN_SHARE_720P30
        });
        assert!(matches!(lavish, Err(CodecError::Unsupported(_))));
    }

    #[test]
    fn emitted_sps_decodes_back_through_the_bitstream_parser() {
        let geometry = Geometry::new(&CONFIG);
        let level = choose_level(&CONFIG).unwrap();
        let mut nal = vec![SPS_NAL_HEADER];
        nal.extend(escape(&sps_rbsp(&geometry, level)));
        // Constrained Baseline, level 3.1, 1280x720, one reference, cropping
        // absent for a macroblock-aligned screen.
        let sps = crate::h264::parse_sps(&nal).unwrap();
        assert_eq!(
            sps,
            Sps {
                profile_idc: 66,
                constraint_flags: 0xC0,
                level_idc: 31,
                id: 0,
                chroma_format_idc: 1,
                bit_depth_luma: 8,
                bit_depth_chroma: 8,
                coded_width: 1280,
                coded_height: 720,
                width: 1280,
                height: 720,
                color: ColorDescription::default(),
                crop_left: 0,
                crop_top: 0,
                log2_max_frame_num_minus4: 4,
                pic_order_cnt_type: 2,
                log2_max_pic_order_cnt_lsb_minus4: 0,
                max_num_ref_frames: 1,
                gaps_in_frame_num_allowed: false,
                frame_mbs_only: true,
                direct_8x8_inference: true,
                has_scaling_matrix: false,
            }
        );
    }

    #[test]
    fn emitted_pps_matches_the_sequence_parameters() {
        let mut nal = vec![PPS_NAL_HEADER];
        nal.extend(escape(&pps_rbsp()));
        let mut bits = BitReader::new(&nal[1..]);
        assert_eq!(bits.ue().unwrap(), u32::from(PICTURE_PARAMETER_SET_ID));
        assert_eq!(bits.ue().unwrap(), u32::from(SEQUENCE_PARAMETER_SET_ID));
        assert_eq!(bits.flag().unwrap(), false); // entropy_coding_mode_flag
        assert_eq!(bits.flag().unwrap(), false); // bottom_field_pic_order...
        assert_eq!(bits.ue().unwrap(), 0); // num_slice_groups_minus1
        assert_eq!(bits.ue().unwrap(), 0); // num_ref_idx_l0_default... (one ref)
        assert_eq!(bits.ue().unwrap(), 0); // num_ref_idx_l1_default...
        assert_eq!(bits.flag().unwrap(), false); // weighted_pred_flag
        assert_eq!(bits.bits(2).unwrap(), 0); // weighted_bipred_idc
        assert_eq!(bits.se().unwrap(), i32::from(PICTURE_INIT_QP) - 26);
        assert_eq!(bits.se().unwrap(), 0); // pic_init_qs_minus26
        assert_eq!(bits.se().unwrap(), 0); // chroma_qp_index_offset
        assert_eq!(bits.flag().unwrap(), true); // deblocking control present
        assert_eq!(bits.flag().unwrap(), false); // constrained_intra_pred
        assert_eq!(bits.flag().unwrap(), false); // redundant_pic_cnt
        assert_eq!(bits.flag().unwrap(), true); // rbsp_trailing_bits
    }

    #[test]
    fn non_macroblock_screens_pad_and_crop_through_the_sps() {
        let mut config = CONFIG;
        config.width = 1278;
        config.height = 718;
        let geometry = Geometry::new(&config);
        let level = choose_level(&config).unwrap();
        // 1278x718 pads to 1280x720 and crops 1 (unit of 2 luma samples) on
        // the right and the bottom.
        assert_eq!((geometry.coded_width, geometry.coded_height), (1280, 720));
        assert!(geometry.frame_cropping);
        assert_eq!((geometry.crop_right, geometry.crop_bottom), (1, 1));
        let mut nal = vec![SPS_NAL_HEADER];
        nal.extend(escape(&sps_rbsp(&geometry, level)));
        let sps = crate::h264::parse_sps(&nal).unwrap();
        assert_eq!((sps.coded_width, sps.coded_height), (1280, 720));
        assert_eq!((sps.width, sps.height), (1278, 718));
        // A macroblock-aligned screen needs no crop at all.
        let geometry = Geometry::new(&CONFIG);
        assert!(!geometry.frame_cropping);
    }

    #[test]
    fn coded_chain_walk_caps_and_requires_byte_alignment() {
        // An empty chain maps to no bytes.
        assert!(walk_segments(ptr::null()).unwrap().is_empty());
        // One segment with its payload bytes (which live in this frame; the
        // call reads through them before walking on).
        let payload = [7u8; 3];
        let segment = VACodedBufferSegment {
            size: 3,
            bit_offset: 0,
            status: 0,
            reserved: 0,
            buf: payload.as_ptr() as *mut ffi::c_void,
            next: ptr::null_mut(),
            va_reserved: [0; 4],
        };
        assert_eq!(walk_segments(&segment).unwrap(), [7u8; 3]);
        // A bit-offset segment is rejected before any data is taken.
        let misaligned = VACodedBufferSegment {
            size: 3,
            bit_offset: 5,
            ..segment
        };
        assert!(matches!(
            walk_segments(&misaligned),
            Err(CodecError::Unsupported(
                "the driver produced bit-unaligned coded data"
            ))
        ));
        // An oversized segment (null payload) fails the cap.
        let oversized = VACodedBufferSegment {
            size: (CODED_BUFFER_CAP + 1) as u32,
            bit_offset: 0,
            ..segment
        };
        assert!(matches!(
            walk_segments(&oversized),
            Err(CodecError::Unsupported(
                "the coded access unit exceeds the 8 MiB cap"
            ))
        ));
        // Consecutive segments accumulate within the cap (`next` walks on).
        let second = VACodedBufferSegment {
            size: 2,
            bit_offset: 0,
            buf: payload.as_ptr() as *mut ffi::c_void,
            next: ptr::null_mut(),
            ..segment
        };
        let linked = VACodedBufferSegment {
            size: 1,
            bit_offset: 0,
            next: ptr::from_ref(&second).cast_mut().cast(),
            ..segment
        };
        assert_eq!(walk_segments(&linked).unwrap(), [7, 7, 7]);
    }

    #[test]
    fn picture_and_slice_records_mirror_the_parameter_sets() {
        let (mb_width, mb_height) = Geometry::new(&CONFIG).macroblocks();
        assert_eq!((mb_width, mb_height), (80, 45));
        let reference = picture(12, 34, VA_PICTURE_H264_SHORT_TERM_REFERENCE, 40, 40);
        let first = picture_buffer(true, 0, None, 9, 7, 88);
        assert_eq!(first.CurrPic.picture_id, 9);
        assert_eq!(
            first.CurrPic.flags,
            0, // a fresh reconstruction carries no reference flags
        );
        assert_eq!(first.frame_num, 0);
        assert_eq!(first.pic_init_qp, PICTURE_INIT_QP);
        assert!(
            first
                .ReferenceFrames
                .iter()
                .all(|slot| slot.picture_id == VA_INVALID_SURFACE as VASurfaceID)
        );
        // SAFETY: a fresh union record is being read; the bits constructor
        // set exactly the fields the checks read.
        let flags = unsafe { first.pic_fields.bits };
        assert_eq!(flags.idr_pic_flag(), 1);
        assert_eq!(flags.reference_pic_flag(), 1);
        assert_eq!(flags.entropy_coding_mode_flag(), 0);
        assert_eq!(flags.deblocking_filter_control_present_flag(), 1);
        assert_eq!(flags.transform_8x8_mode_flag(), 0);
        let slice = slice_buffer(true, 42, 0, None, (mb_width, mb_height));
        assert_eq!(slice.slice_type, SLICE_TYPE_I);
        assert_eq!(slice.idr_pic_id, 42);
        assert_eq!(
            slice.num_macroblocks,
            u32::from(mb_width) * u32::from(mb_height)
        );
        assert!(
            slice
                .RefPicList0
                .iter()
                .all(|slot| slot.picture_id == VA_INVALID_SURFACE as VASurfaceID)
        );
        // A P slice without a reference in the caller's hand produces the
        // invalid list too; with one the reference leads the list.
        let slice = slice_buffer(false, 42, 5, Some(reference), (mb_width, mb_height));
        assert_eq!(slice.slice_type, SLICE_TYPE_P);
        assert_eq!(slice.RefPicList0[0].picture_id, 12);
        assert_eq!(
            slice.RefPicList0[0].flags,
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        );
        assert_eq!(
            slice.RefPicList0[1].flags,
            VA_PICTURE_H264_INVALID | VA_PICTURE_H264_NON_EXISTING
        );
        let picture_params = picture_buffer(
            false,
            6,
            Some(picture(
                12,
                34,
                VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                10,
                10,
            )),
            9,
            7,
            88,
        );
        assert_eq!(picture_params.frame_num, 6);
        assert_eq!(picture_params.ReferenceFrames[0].picture_id, 12);
        assert!(
            picture_params.ReferenceFrames[1..]
                .iter()
                .all(|slot| slot.picture_id == VA_INVALID_SURFACE as VASurfaceID)
        );
    }

    #[test]
    fn sequence_records_mirror_the_emitted_sps() {
        let geometry = Geometry::new(&CONFIG);
        let level = choose_level(&CONFIG).unwrap();
        // SAFETY: a fresh union record is being read; the bits constructor
        // set exactly the fields the checks read.
        let fields = unsafe { sequence_buffer(&geometry, level, &CONFIG).seq_fields.bits };
        assert_eq!(fields.chroma_format_idc(), 1);
        assert_eq!(fields.frame_mbs_only_flag(), 1);
        assert_eq!(fields.mb_adaptive_frame_field_flag(), 0);
        assert_eq!(fields.direct_8x8_inference_flag(), 1);
        assert_eq!(fields.log2_max_frame_num_minus4(), 4);
        assert_eq!(fields.pic_order_cnt_type(), 2);
        let sequence = sequence_buffer(&geometry, level, &CONFIG);
        assert_eq!(sequence.level_idc, level);
        assert_eq!(sequence.seq_parameter_set_id, SEQUENCE_PARAMETER_SET_ID);
        assert_eq!(sequence.intra_idr_period, CONFIG.keyframe_interval);
        assert_eq!(sequence.bits_per_second, CONFIG.bitrate);
        assert_eq!(sequence.max_num_ref_frames, 1);
        assert_eq!(sequence.picture_width_in_mbs, 80);
        assert_eq!(sequence.picture_height_in_mbs, 45);
        assert_eq!(sequence.frame_cropping_flag, 0);
    }

    #[test]
    #[ignore = "needs a VA-API DRM device with an H.264 encoder and a live \
                driver; run with `cargo test -- --ignored` on Linux"]
    fn live_encoder_produces_decodable_self_contained_keyframes() {
        let config = EncoderConfig {
            width: 160,
            height: 128,
            frame_rate: 30,
            bitrate: 256_000,
            keyframe_interval: 3,
        };
        let candidates = super::super::encoder_candidates().unwrap();
        let source = candidates
            .into_iter()
            .map(|candidate| candidate.source)
            .find(|source| source.encoder.is_some())
            .expect("renderer needs a VA-API device with an H.264 encoder");
        let mut encoder = source.open_encoder(config).unwrap();
        let mut out = Vec::new();
        for frame_idx in 0..4u32 {
            let packed = moving_pattern(config.width, config.height, frame_idx);
            let frame = Nv12::packed(config.width, config.height, &packed).unwrap();
            encoder
                .encode(
                    &frame,
                    Duration::from_millis(u64::from(frame_idx) * 33),
                    false,
                    &mut out,
                )
                .unwrap();
        }
        encoder.flush(&mut out).unwrap();
        assert_eq!(out.len(), 4);
        assert!(out[0].keyframe, "the first picture is an IDR");
        assert!(!out[1..3].iter().any(|frame| frame.keyframe));
        assert!(out[3].keyframe, "the keyframe interval closed a new IDR");
        for frame in &out {
            let summary = crate::h264::inspect(&frame.data).unwrap();
            assert!(summary.vcl);
            if frame.keyframe {
                // Every keyframe a receiver joins on must carry its own
                // parameter sets.
                assert!(summary.sps && summary.pps);
            }
        }
        // All pictures of the sequence must be decodable: sizes, start codes,
        // forbidden bits, and SPS limits all check out.
        for frame in &out {
            crate::h264::check_decodable(&frame.data).unwrap();
        }
    }
}
