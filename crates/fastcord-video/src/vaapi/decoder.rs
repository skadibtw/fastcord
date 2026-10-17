//! H.264 decoding through a VA-API hardware decoder.
//!
//! A [`VaDecoder`] owns one VA-API display and the picture setup for the
//! stream's current profile and size (config, six output surfaces, context,
//! image), recreated whenever the sequence parameters change. Decode is fully
//! synchronous per access unit: [`VideoDecoder::decode`] renders the whole
//! picture (one picture parameter buffer, a flat default IQ matrix for High
//! profiles, and a slice parameter plus slice data buffer per coded slice NAL
//! unit), syncs the target surface, and copies it through
//! [`Image::read_surface`], where the mapped image pixels are handed to the
//! caller as a borrowed [`Nv12`]. No picture is ever queued.
//!
//! The host parses the stream itself: the driver consumes the syntax as
//! parameter records, and [`ColorState::inspect`] has already vetted Annex B
//! framing, picture dimensions, VUI color metadata, and the 8-bit 4:2:0
//! contract before any NAL unit is parsed here. [`Dpb`] mirrors the picture
//! DPB so the driver's `ReferenceFrames` and the initial `RefPicList0` are
//! exact for the supported streams.
//!
//! Feature policy (everything the codec contract needs for received screen
//! shares and streams): Constrained-Baseline/Baseline/Main/High profiles with
//! frame coding only; no B/SP/SI slices, weighted prediction, data
//! partitioning, redundant pictures, frame_num gap recovery, memory
//! management free of the short-term ring, or reference-list modification; at
//! most five short-term references (the six-surface ring) and picture-order
//! count types 0 and 2. Known rough edges, both quality-only for streams that
//! use them: High-profile scaling lists that are not the spec defaults are
//! answered with the flat default matrices, and `second_chroma_qp_index`
//! offsets are not parsed (that field only exists behind the High-profile PPS
//! extension).
//!
//! [`flush`]: VideoDecoder::flush

use std::rc::Rc;
use std::time::Duration;

use super::{
    ConfigId, ContextId, Display, Image, NativeBuffer, Surfaces, backend, check, invalid_picture,
    picture, sync_surface,
};
use crate::codec::{Backend, BackendKind, CodecError, DecodedFrame, VideoDecoder};
use crate::frame::{ColorDescription, Nv12};
use crate::h264::{self, ColorState};

use bindings::*;

/// Output surfaces per picture setup: one for the picture being decoded plus
/// one spare beyond every reference the DPB can hold.
const OUTPUT_SURFACES: usize = 6;
/// DPB cap (and the reference-list cap): streams that need more are rejected.
const DPB_CAP: usize = 5;
/// Upper bound for the `mmco` operation loop of a non-IDR reference slice.
const MAX_MMCO_OPS: usize = 32;

/// Annex B start code prefixing every slice NAL unit in a slice data buffer.
const SLICE_DATA_START_CODE: [u8; 3] = [0, 0, 1];
/// `slice_data_offset` of that layout: the NAL header follows the prefix.
const SLICE_DATA_NAL_OFFSET: u32 = 3;

/// The High-profile `profile_idc` values with extended syntax (chroma format,
/// bit depths, scaling lists and the PPS extension); 8-bit 4:2:0 streams among
/// them resolve to [`VAProfile_VAProfileH264High`].
const EXTENDED_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// Maps a failure to unmap or read a surface onto the codec contract.
const TRUNCATED: CodecError = CodecError::InvalidBitstream("truncated NAL unit");
const MALFORMED: CodecError = CodecError::InvalidBitstream("malformed parameter set");

/// Removes emulation-prevention bytes (`00 00 03` becomes `00 00`) from a
/// coded NAL unit, yielding the RBSP a reader parses.
fn strip_epb(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &byte in data {
        if zeros >= 2 && byte == 3 {
            // The two zeros keep counting: an inserted escape byte pair
            // (`00 00 03 00 00 03`) would otherwise hide the second one.
            continue;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// MSB-first reader over an EPB-stripped RBSP that reports the consumed bit
/// count (a slice header must tell the driver where its slice data starts,
/// and the host parser needs the exact position).
struct Rbsp<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Rbsp<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bits consumed so far, relative to the start of `data`.
    fn position(&self) -> usize {
        self.pos
    }

    fn flag(&mut self) -> Result<bool, CodecError> {
        Ok(self.bit()? == 1)
    }

    fn bit(&mut self) -> Result<u32, CodecError> {
        let byte = *self.data.get(self.pos / 8).ok_or(TRUNCATED)?;
        let value = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Ok(u32::from(value))
    }

    /// Reads `count` (at most 32) bits.
    fn bits(&mut self, count: u32) -> Result<u32, CodecError> {
        let mut value = 0u64;
        for _ in 0..count {
            value = (value << 1) | u64::from(self.bit()?);
        }
        Ok(value as u32)
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> Result<u32, CodecError> {
        let mut leading = 0;
        while self.bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return Err(CodecError::InvalidBitstream("Exp-Golomb value overflows"));
            }
        }
        Ok((((1u64 << leading) - 1) + u64::from(self.bits(leading)?)) as u32)
    }

    /// Signed Exp-Golomb.
    fn se(&mut self) -> Result<i32, CodecError> {
        let code = i64::from(self.ue()?);
        Ok(if code % 2 == 1 {
            ((code + 1) / 2) as i32
        } else {
            (-(code / 2)) as i32
        })
    }
}

/// Copies one scaling list's syntax elements past the reader (an SPS or PPS
/// with scaling lists is accepted; the driver gets the spec default matrices).
fn skip_scaling_list(bits: &mut Rbsp<'_>, size: usize) -> Result<(), CodecError> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            let delta = i64::from(bits.se()?);
            if !(-128..=127).contains(&delta) {
                return Err(MALFORMED);
            }
            next = (last + delta).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

/// The sequence parameters a stream needs, as parsed (the driver's record is
/// built from these fields in [`picture_record`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpsMeta {
    id: u32,
    profile_idc: u8,
    /// constraint_set0..5 flags in the high bits, as coded.
    constraint_flags: u8,
    level_idc: u8,
    log2_max_frame_num_minus4: u32,
    /// 0 (`lsb` wrap) or 2 (output order is decoding order); type 1 rejected.
    pic_order_cnt_type: u32,
    log2_max_pic_order_cnt_lsb_minus4: u32,
    /// At most [`DPB_CAP`].
    num_ref_frames: u32,
    direct_8x8_inference_flag: bool,
    /// Coded picture size in pixels (macroblock aligned, frame coded).
    coded_width: u32,
    coded_height: u32,
    /// The visible display window inside the coded picture (even offsets and
    /// sizes: the 4:2:0 chroma plane stays sample aligned).
    display_x: u32,
    display_y: u32,
    display_width: u32,
    display_height: u32,
}

impl SpsMeta {
    /// The VA-API profile a config is created with for this stream.
    fn profile(&self) -> Result<VAProfile, CodecError> {
        match (self.profile_idc, self.constraint_flags & 0xC0) {
            (66, 0xC0) => Ok(VAProfile_VAProfileH264ConstrainedBaseline),
            (66, _) => Ok(VAProfile_VAProfileH264Baseline),
            (77, _) => Ok(VAProfile_VAProfileH264Main),
            (100, _) => Ok(VAProfile_VAProfileH264High),
            _ => Err(CodecError::Unsupported(
                "no VA-API profile for this H.264 stream",
            )),
        }
    }
}

/// Parses a sequence parameter set NAL unit (header included). Frame coding
/// only, at most [`DPB_CAP`] references, picture-order count types 0 and 2,
/// and no frame_num gap recovery: everything else than that is rejected the
/// same way [`h264::check_decodable`] (via [`ColorState::inspect`]) vetted the
/// access units that carried this SPS.
fn parse_sps_meta(nal: &[u8]) -> Result<SpsMeta, CodecError> {
    if h264::nal_unit_type(nal) != h264::NAL_SPS {
        return Err(MALFORMED);
    }
    let stripped = strip_epb(&nal[1..]);
    let mut bits = Rbsp::new(&stripped);
    let (profile_idc, constraint_flags, level_idc) = (
        bits.bits(8)? as u8,
        bits.bits(8)? as u8,
        bits.bits(8)? as u8,
    );
    let id = bits.ue()?;
    // Extended-syntax profiles carry chroma/bit-depth/scaling fields that
    // separate the header from the fields below; walk them exactly as
    // `h264::parse_sps` does (it has already vetted the values).
    let mut chroma_format_idc = 1;
    if EXTENDED_PROFILES.contains(&profile_idc) {
        chroma_format_idc = bits.ue()?;
        if chroma_format_idc == 3 {
            bits.flag()?;
        }
        let _bit_depth_luma_minus8 = bits.ue()?;
        let _bit_depth_chroma_minus8 = bits.ue()?;
        bits.flag()?; // qpprime_y_zero_transform_bypass_flag
        if bits.flag()? {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for list in 0..lists {
                if bits.flag()? {
                    skip_scaling_list(&mut bits, if list < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    if chroma_format_idc != 1 {
        return Err(CodecError::Unsupported(
            "only 8-bit 4:2:0 H.264 is supported",
        ));
    }
    let log2_max_frame_num_minus4 = bits.ue()?;
    if log2_max_frame_num_minus4 > 12 {
        return Err(MALFORMED);
    }
    let pic_order_cnt_type = bits.ue()?;
    let log2_max_pic_order_cnt_lsb_minus4 = match pic_order_cnt_type {
        0 => bits.ue()?,
        1 => {
            return Err(CodecError::Unsupported(
                "picture-order-count type 1 streams are not supported",
            ));
        }
        2 => 0,
        _ => return Err(MALFORMED),
    };
    if pic_order_cnt_type == 0 && log2_max_pic_order_cnt_lsb_minus4 > 12 {
        return Err(MALFORMED);
    }
    let num_ref_frames = bits.ue()?;
    if num_ref_frames > DPB_CAP as u32 {
        return Err(CodecError::Unsupported(
            "more than five reference pictures exceed this decoder's DPB",
        ));
    }
    if bits.flag()? {
        return Err(CodecError::Unsupported(
            "frame_num gap recovery is not supported",
        ));
    }
    let width_mbs = u64::from(bits.ue()?) + 1;
    let height_map_units = u64::from(bits.ue()?) + 1;
    if !bits.flag()? {
        return Err(CodecError::Unsupported(
            "field-coded streams are not supported",
        ));
    }
    let direct_8x8_inference_flag = bits.flag()?;
    let (mut crop_left, mut crop_right, mut crop_top, mut crop_bottom) = (0u64, 0u64, 0u64, 0u64);
    if bits.flag()? {
        // Crop units per ITU-T H.264 (7-19)..(7-22) for 4:2:0 frame coding:
        // two luma samples on every axis.
        crop_left = u64::from(bits.ue()?);
        crop_right = u64::from(bits.ue()?);
        crop_top = u64::from(bits.ue()?);
        crop_bottom = u64::from(bits.ue()?);
    }
    let coded_width = width_mbs * 16;
    let coded_height = height_map_units * 16;
    let (crop_x, crop_y) = ((crop_left + crop_right) * 2, (crop_top + crop_bottom) * 2);
    if crop_x >= coded_width || crop_y >= coded_height {
        return Err(MALFORMED);
    }
    let (Ok(coded_width), Ok(coded_height)) =
        (u32::try_from(coded_width), u32::try_from(coded_height))
    else {
        return Err(CodecError::Unsupported("picture is larger than 4096x2304"));
    };
    crate::codec::check_dimensions(coded_width, coded_height)?;
    Ok(SpsMeta {
        id,
        profile_idc,
        constraint_flags,
        level_idc,
        log2_max_frame_num_minus4,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4,
        num_ref_frames,
        direct_8x8_inference_flag,
        coded_width,
        coded_height,
        display_x: (crop_left * 2) as u32,
        display_y: (crop_top * 2) as u32,
        display_width: (coded_width as u64 - crop_x) as u32,
        display_height: (coded_height as u64 - crop_y) as u32,
    })
}

/// The picture parameters a stream needs, as parsed. The High-profile PPS
/// extension (transform/scaling lists, second chroma offset) stays unparsed:
/// the driver record answers zeroed defaults there (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PpsMeta {
    id: u32,
    sps_id: u32,
    entropy_coding_mode_flag: bool,
    bottom_field_pic_order_in_frame_present_flag: bool,
    num_ref_idx_l0_default_active_minus1: u32,
    pic_init_qp_minus26: i32,
    deblocking_filter_control_present_flag: bool,
}

/// Parses a picture parameter set NAL unit (header included), for the parts
/// the driver record and slice headers need.
fn parse_pps_meta(nal: &[u8]) -> Result<PpsMeta, CodecError> {
    if h264::nal_unit_type(nal) != h264::NAL_PPS {
        return Err(MALFORMED);
    }
    let stripped = strip_epb(&nal[1..]);
    let mut bits = Rbsp::new(&stripped);
    let id = bits.ue()?;
    let sps_id = bits.ue()?;
    if id > 255 || sps_id > 31 {
        return Err(MALFORMED);
    }
    let entropy_coding_mode_flag = bits.flag()?;
    let bottom_field_pic_order_in_frame_present_flag = bits.flag()?;
    if bits.ue()? != 0 {
        return Err(CodecError::Unsupported("slice groups are not supported"));
    }
    let num_ref_idx_l0_default_active_minus1 = bits.ue()?;
    let num_ref_idx_l1_default_active_minus1 = bits.ue()?;
    if num_ref_idx_l0_default_active_minus1 > 31 || num_ref_idx_l1_default_active_minus1 > 31 {
        return Err(MALFORMED);
    }
    if bits.flag()? {
        return Err(CodecError::Unsupported(
            "weighted prediction is not supported",
        ));
    }
    if bits.bits(2)? != 0 {
        return Err(CodecError::Unsupported(
            "weighted biprediction is not supported",
        ));
    }
    let pic_init_qp_minus26 = bits.se()?;
    if !(-26..=25).contains(&pic_init_qp_minus26) {
        return Err(MALFORMED);
    }
    let _pic_init_qs_minus26 = bits.se()?;
    let _chroma_qp_index_offset = bits.se()?;
    let deblocking_filter_control_present_flag = bits.flag()?;
    bits.flag()?; // constrained_intra_pred_flag: honored by the driver record
    if bits.flag()? {
        return Err(CodecError::Unsupported(
            "redundant pictures are not supported",
        ));
    }
    Ok(PpsMeta {
        id,
        sps_id,
        entropy_coding_mode_flag,
        bottom_field_pic_order_in_frame_present_flag,
        num_ref_idx_l0_default_active_minus1,
        pic_init_qp_minus26,
        deblocking_filter_control_present_flag,
    })
}

/// One coded slice's parsed header, grouped into the picture it belongs to.
#[derive(Clone, Copy, Debug)]
struct SliceHeader {
    first_mb_in_slice: u32,
    /// `slice_type` as coded (0-9); fully-sliced pictures use 5/7.
    slice_type: u8,
    idr: bool,
    /// Zero for IDR pictures, the coded `frame_num` otherwise.
    frame_num: u16,
    /// `pic_order_cnt_lsb` for picture-order-count type 0 streams.
    pic_order_cnt_lsb: Option<u16>,
    num_ref_idx_l0_active_minus1: u8,
    cabac_init_idc: u8,
    slice_qp_delta: i8,
    /// `(idc, alpha_c0_offset_div2, beta_offset_div2)`, all zero when absent.
    deblocking_filter: (u8, i8, i8),
    /// Header bits consumed after the NAL header byte (EPB-free count).
    header_bits: usize,
}

/// Parses one slice NAL unit's header (the NAL type must be a whole VCL
/// unit), resolving `(sps, pps)` through the header's PPS id with `resolver`.
/// Returns both parameter sets again because the picture record is built from
/// the ones this slice references.
fn parse_slice_header(
    nal: &[u8],
    resolver: impl FnOnce(u32) -> Result<(SpsMeta, PpsMeta), CodecError>,
) -> Result<(SpsMeta, PpsMeta, SliceHeader), CodecError> {
    let header_byte = *nal.first().ok_or(TRUNCATED)?;
    let nal_type = h264::nal_unit_type(nal);
    if !matches!(nal_type, h264::NAL_SLICE | h264::NAL_IDR) {
        return Err(CodecError::Unsupported(
            "data-partitioned slices are not supported",
        ));
    }
    let idr = nal_type == h264::NAL_IDR;
    if idr && (header_byte >> 5) & 3 == 0 {
        return Err(CodecError::InvalidBitstream(
            "an IDR picture must be referenced",
        ));
    }
    let stripped = strip_epb(&nal[1..]);
    let mut bits = Rbsp::new(&stripped);
    let first_mb_in_slice = bits.ue()?;
    let raw_slice_type = bits.ue()?;
    if raw_slice_type > 9 {
        return Err(CodecError::InvalidBitstream("invalid slice_type"));
    }
    // 0 P, 1 B, 2 I, 3 SP, 4 SI (the 5-9 range repeats them whole-picture).
    let kind = raw_slice_type % 5;
    if !matches!(kind, 0 | 2) {
        return Err(CodecError::Unsupported(
            "B, SP and SI slices are not supported",
        ));
    }
    let (sps, pps) = resolver(bits.ue()?)?;
    let picture_size_in_mbs = u64::from(sps.coded_width) * u64::from(sps.coded_height) / 256;
    if u64::from(first_mb_in_slice) >= picture_size_in_mbs {
        return Err(CodecError::InvalidBitstream(
            "a slice macroblock address does not fit the picture",
        ));
    }
    // IDR pictures still carry (zero) `frame_num` bits before `idr_pic_id`.
    let frame_num = {
        let value = bits.bits(4 + sps.log2_max_frame_num_minus4)?;
        bits.ue()?; // idr_pic_id: only the driver record's POC cares
        if idr {
            // Spec 7.4.3: IDR pictures restart the numbering at zero.
            if value != 0 {
                return Err(CodecError::InvalidBitstream(
                    "an IDR picture carries a nonzero frame_num",
                ));
            }
            0
        } else {
            u16::try_from(value)
                .map_err(|_| CodecError::InvalidBitstream("truncated slice NAL unit"))?
        }
    };
    let pic_order_cnt_lsb = match sps.pic_order_cnt_type {
        0 => {
            let value = bits.bits(4 + sps.log2_max_pic_order_cnt_lsb_minus4)?;
            Some(
                u16::try_from(value)
                    .map_err(|_| CodecError::InvalidBitstream("truncated slice NAL unit"))?,
            )
        }
        2 => None,
        _ => return Err(MALFORMED), // type 1 is rejected with the SPS
    };
    // `redundant_pic_cnt` never appears: the PPS flag is rejected above.
    let num_ref_idx_l0_active_minus1 = if kind == 0 {
        let value = if bits.flag()? {
            bits.ue()?
        } else {
            pps.num_ref_idx_l0_default_active_minus1
        };
        if value as usize >= DPB_CAP {
            return Err(CodecError::Unsupported(
                "reference lists longer than five pictures exceed this decoder's DPB",
            ));
        }
        value
    } else {
        0
    };
    if kind == 0 && bits.flag()? {
        return Err(CodecError::Unsupported(
            "reference picture list modification is not supported",
        ));
    }
    // Weighted prediction is rejected with the PPS, so no pred_weight_table
    // can appear here.
    if (header_byte >> 5) & 3 != 0 {
        if idr {
            bits.flag()?; // no_output_of_prior_pics_flag
            bits.flag()?; // long_term_reference_flag: tracked inside the DPB
        } else if bits.flag()? {
            // adaptive_ref_pic_marking_mode_flag: walk the mmco operations so
            // the header bit count stays exact; the short-term ring below is
            // the marking approximation.
            let mut completed = false;
            for _ in 0..MAX_MMCO_OPS {
                match bits.ue()? {
                    0 => {
                        completed = true;
                        break;
                    }
                    1 | 2 | 4 | 6 => {
                        bits.ue()?;
                    }
                    3 => {
                        bits.ue()?;
                        bits.ue()?;
                    }
                    5 => {}
                    _ => {
                        return Err(CodecError::InvalidBitstream(
                            "invalid memory management control operation",
                        ));
                    }
                }
            }
            if !completed {
                return Err(CodecError::InvalidBitstream(
                    "too many memory management control operations",
                ));
            }
        }
    }
    let cabac_init_idc = if pps.entropy_coding_mode_flag && kind != 2 {
        let value = bits.ue()?;
        if value > 2 {
            return Err(CodecError::InvalidBitstream("invalid cabac_init_idc"));
        }
        value
    } else {
        0
    };
    let slice_qp_delta = bits.se()?;
    if !(0..=51).contains(&(i32::from(pps.pic_init_qp_minus26) + 26 + i32::from(slice_qp_delta))) {
        return Err(CodecError::InvalidBitstream("a slice QP is out of range"));
    }
    let deblocking_filter = if pps.deblocking_filter_control_present_flag {
        let idc = bits.ue()?;
        if idc > 2 {
            return Err(CodecError::InvalidBitstream(
                "invalid disable_deblocking_filter_idc",
            ));
        }
        if idc == 1 {
            (1, 0, 0)
        } else {
            let alpha = bits.se()?;
            let beta = bits.se()?;
            if !(-6..=6).contains(&alpha) || !(-6..=6).contains(&beta) {
                return Err(CodecError::InvalidBitstream(
                    "a deblocking offset is out of range",
                ));
            }
            (idc as u8, alpha as i8, beta as i8)
        }
    } else {
        (0, 0, 0)
    };
    // Slice groups are rejected with the PPS; nothing else follows the slice
    // header grammar before the slice data.
    Ok((
        sps,
        pps,
        SliceHeader {
            first_mb_in_slice,
            slice_type: raw_slice_type as u8,
            idr,
            frame_num,
            pic_order_cnt_lsb,
            num_ref_idx_l0_active_minus1: num_ref_idx_l0_active_minus1 as u8,
            cabac_init_idc: cabac_init_idc as u8,
            slice_qp_delta: slice_qp_delta as i8,
            deblocking_filter,
            header_bits: bits.position(),
        },
    ))
}

impl SliceHeader {
    /// Every slice covering one picture must agree on its picture properties.
    fn consistent(&self, other: &Self) -> Result<(), CodecError> {
        if self.idr != other.idr
            || self.frame_num != other.frame_num
            || self.pic_order_cnt_lsb != other.pic_order_cnt_lsb
        {
            return Err(CodecError::InvalidBitstream(
                "the slices of one picture disagree",
            ));
        }
        Ok(())
    }
}

/// Picture-order-count bookkeeping between pictures for type 0 streams: the
/// `pic_order_cnt_lsb` values only identify a picture modulo their range, so
/// the MSB (a whole range step) is tracked in `expected_msb` exactly as
/// ITU-T H.264 8.2.1 prescribes. Type 2 streams count `frame_num` alone.
#[derive(Default)]
struct Poc {
    expected_msb: i64,
    prev_lsb: Option<u32>,
}

impl Poc {
    /// Restarts the bookkeeping (a new sequence, or a fresh setup).
    fn reset(&mut self) {
        self.expected_msb = 0;
        self.prev_lsb = None;
    }

    /// The picture-order count of one picture, with its state updated.
    ///
    /// `lsb` is the coded `pic_order_cnt_lsb` for type 0 streams (ignored for
    /// type 2). IDR pictures count zero and re-anchor the bookkeeping.
    fn count(
        &mut self,
        sps: &SpsMeta,
        lsb: Option<u16>,
        frame_num: u16,
        is_ref: bool,
        idr: bool,
    ) -> i32 {
        match sps.pic_order_cnt_type {
            0 => {
                let lsb = i64::from(lsb.unwrap_or(0));
                if idr {
                    self.expected_msb = 0;
                    self.prev_lsb = Some(lsb as u32);
                    return 0;
                }
                let range = 1i64 << (4 + sps.log2_max_pic_order_cnt_lsb_minus4);
                if let Some(previous) = self.prev_lsb.map(i64::from) {
                    if lsb < previous && previous - lsb >= range / 2 {
                        self.expected_msb += range;
                    } else if lsb > previous && lsb - previous > range / 2 {
                        self.expected_msb -= range;
                    }
                }
                self.prev_lsb = Some(lsb as u32);
                (self.expected_msb + lsb).clamp(i32::MIN as i64, i32::MAX as i64) as i32
            }
            // Type 2: references count 2 * frame_num, non-references one step
            // earlier (ITU-T H.264 8.2.1). IDR pictures count zero.
            _ => {
                if idr {
                    0
                } else if is_ref {
                    2 * i32::from(frame_num)
                } else {
                    2 * (i32::from(frame_num) - 1)
                }
            }
        }
    }
}

/// One short-term reference the decoder is still tracking.
#[derive(Clone, Copy)]
struct DpbEntry {
    /// The output surface slot the picture's reconstruction lives on.
    slot: usize,
    record: VAPictureH264,
}

/// The decoder's view of the picture DPB and its surface ring: at most
/// [`DPB_CAP`] short-term references on [`OUTPUT_SURFACES`] surfaces, with
/// the newest references entering last (decoding order). `mmco`-style marking
/// from slice data is approximated: references are evicted oldest-first once
/// the DPB cap is reached, which is exact for the streams the contract
/// describes and only loses picture detail for longer, exotic ones.
#[derive(Default)]
struct Dpb {
    /// Every decoded picture so far, as [`VAPictureH264::frame_idx`].
    pictures: u32,
    entries: Vec<DpbEntry>,
}

impl Dpb {
    /// Clears all references (an IDR picture started a new sequence).
    fn clear(&mut self) {
        self.entries.clear();
    }

    /// The first surface slot no reference occupies (at least one is always
    /// free while `entries` stays within [`DPB_CAP`]).
    fn free_slot(&self) -> Option<usize> {
        (0..OUTPUT_SURFACES).find(|slot| self.entries.iter().all(|entry| entry.slot != *slot))
    }

    /// The next monotonic [`VAPictureH264::frame_idx`].
    fn next_frame_idx(&mut self) -> u32 {
        let index = self.pictures;
        self.pictures += 1;
        index
    }

    /// The short-term references, most recent first (the initial `RefPicList0`
    /// order for P slices), capped at the driver's 16 `ReferenceFrames` slots.
    fn references(&self) -> Vec<VAPictureH264> {
        self.entries
            .iter()
            .rev()
            .take(16)
            .map(|entry| entry.record)
            .collect()
    }

    /// The full initial `RefPicList0`: references most recent first, every
    /// remaining slot invalid.
    fn list(&self) -> [VAPictureH264; 32] {
        let mut list = std::array::from_fn(|_| invalid_picture());
        for (index, record) in self.references().into_iter().enumerate() {
            list[index] = record;
        }
        list
    }

    /// Marks a picture as a short-term reference, evicting the oldest entry
    /// beyond the cap.
    fn push(&mut self, slot: usize, record: VAPictureH264) {
        if self.entries.len() >= DPB_CAP {
            self.entries.remove(0);
        }
        self.entries.push(DpbEntry { slot, record });
    }
}

/// A byte view over a `repr(C)` parameter record, for [`NativeBuffer::create`],
/// which copies the bytes at `vaCreateBuffer` time.
fn record_bytes<T>(record: &T) -> &[u8] {
    // SAFETY: parameter records are `repr(C)` plain data; the span covers the
    // whole record (padding included, a read is allowed), and the bytes are
    // copied before the record locals end.
    unsafe {
        std::slice::from_raw_parts(
            std::ptr::from_ref(record).cast::<u8>(),
            std::mem::size_of_val(record),
        )
    }
}

/// The picture parameter record for the picture an access unit's slices cover.
fn picture_record(
    sps: &SpsMeta,
    pps: &PpsMeta,
    current: VAPictureH264,
    references: &[VAPictureH264],
    frame_num: u16,
    is_ref: bool,
) -> VAPictureParameterBufferH264 {
    let mut reference_frames = std::array::from_fn(|_| invalid_picture());
    for (record, entry) in reference_frames.iter_mut().zip(references) {
        *entry = *record;
    }
    VAPictureParameterBufferH264 {
        CurrPic: current,
        ReferenceFrames: reference_frames,
        picture_width_in_mbs_minus1: (sps.coded_width / 16 - 1) as u16,
        picture_height_in_mbs_minus1: (sps.coded_height / 16 - 1) as u16,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        num_ref_frames: sps.num_ref_frames as u8,
        seq_fields: _VAPictureParameterBufferH264__bindgen_ty_1 {
            bits: _VAPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1 {
                _bindgen_align: [],
                _bitfield_1:
                    _VAPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1::new_bitfield_1(
                        1, // chroma_format_idc: 4:2:0
                        0, // residual_colour_transform_flag (not in this profile set)
                        0, // gaps_in_frame_num_value_allowed_flag: rejected with the SPS
                        u32::from(sps.direct_8x8_inference_flag),
                        0, // mb_adaptive_frame_field_flag: frame coding only
                        u32::from(sps.direct_8x8_inference_flag),
                        0, // MinLumaBiPredSize8x8: no B slices anywhere in the stream
                        sps.log2_max_frame_num_minus4,
                        sps.pic_order_cnt_type,
                        sps.log2_max_pic_order_cnt_lsb_minus4,
                        0, // delta_pic_order_always_zero_flag: type 1 streams rejected
                    ),
                __bindgen_padding_0: 0,
            },
        },
        num_slice_groups_minus1: 0,
        slice_group_map_type: 0,
        slice_group_change_rate_minus1: 0,
        pic_init_qp_minus26: pps.pic_init_qp_minus26 as i8,
        pic_init_qs_minus26: 0,
        chroma_qp_index_offset: 0,
        second_chroma_qp_index_offset: 0,
        pic_fields: _VAPictureParameterBufferH264__bindgen_ty_2 {
            bits: _VAPictureParameterBufferH264__bindgen_ty_2__bindgen_ty_1 {
                _bindgen_align: [],
                _bitfield_1:
                    _VAPictureParameterBufferH264__bindgen_ty_2__bindgen_ty_1::new_bitfield_1(
                        u32::from(pps.entropy_coding_mode_flag),
                        0, // weighted_pred_flag: rejected with the PPS
                        0, // weighted_bipred_idc: rejected with the PPS
                        0, // transform_8x8_mode_flag (see the module header)
                        0, // field_pic_flag: frame coding only
                        0, // constrained_intra_pred_flag
                        0, // pic_order_present_flag: frame coding only
                        u32::from(pps.deblocking_filter_control_present_flag),
                        0, // redundant_pic_cnt_present_flag: rejected with the PPS
                        u32::from(is_ref),
                    ),
                __bindgen_padding_0: 0,
            },
        },
        frame_num,
        va_reserved: [0; 8],
    }
}

/// The slice parameter record for one coded slice NAL unit.
fn slice_record(
    nal_bytes: usize,
    header: &SliceHeader,
    list0: &[VAPictureH264; 32],
) -> VASliceParameterBufferH264 {
    VASliceParameterBufferH264 {
        slice_data_size: nal_bytes as u32,
        slice_data_offset: SLICE_DATA_NAL_OFFSET,
        slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
        // The offset into the slice data buffer includes the NAL header byte
        // the buffer carries, plus the EPB-free header bit count.
        slice_data_bit_offset: (u8::BITS as usize + header.header_bits) as u16,
        first_mb_in_slice: header.first_mb_in_slice as u16,
        slice_type: header.slice_type,
        direct_spatial_mv_pred_flag: 0, // only meaningful for B slices
        num_ref_idx_l0_active_minus1: header.num_ref_idx_l0_active_minus1,
        num_ref_idx_l1_active_minus1: 0,
        cabac_init_idc: header.cabac_init_idc,
        slice_qp_delta: header.slice_qp_delta,
        disable_deblocking_filter_idc: header.deblocking_filter.0,
        slice_alpha_c0_offset_div2: header.deblocking_filter.1,
        slice_beta_offset_div2: header.deblocking_filter.2,
        RefPicList0: *list0,
        RefPicList1: std::array::from_fn(|_| invalid_picture()),
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
        va_reserved: [0; 4],
    }
}

/// The flat default inverse-quantization matrices submitted for High-profile
/// streams (the driver uses spec defaults when the stream carries no lists of
/// its own; custom lists are answered with the same defaults, see the module
/// header).
fn iq_matrix() -> VAIQMatrixBufferH264 {
    VAIQMatrixBufferH264 {
        ScalingList4x4: [[16; 16]; 6],
        ScalingList8x8: [[16; 64]; 2],
        va_reserved: [0; 4],
    }
}

/// The VA-API objects for one (profile, size) combination, with the DPB and
/// surface ring that belong to them. Dropped in field order, the display last.
struct Setup {
    sps: SpsMeta,
    image: Image,
    context: ContextId,
    surfaces: Surfaces,
    config: ConfigId,
    dpb: Dpb,
    // Declared last: VA-API terminates after every object above is released.
    display: Rc<Display>,
}

impl Setup {
    /// Creates the full picture setup for `sps` on the display.
    fn open(display: &Rc<Display>, sps: &SpsMeta) -> Result<Self, CodecError> {
        let profile = sps.profile()?;
        let config = ConfigId::create(display, profile, VAEntrypoint_VAEntrypointVLD, &[])?;
        let surfaces =
            Surfaces::create(display, OUTPUT_SURFACES, sps.coded_width, sps.coded_height)?;
        let context = ContextId::create(
            display,
            config.id(),
            sps.coded_width as i32,
            sps.coded_height as i32,
            VA_PROGRESSIVE as i32,
            surfaces.as_slice(),
        )?;
        let image = Image::create(display, sps.coded_width as i32, sps.coded_height as i32)?;
        Ok(Self {
            sps: *sps,
            image,
            context,
            surfaces,
            config,
            dpb: Dpb::default(),
            display: Rc::clone(display),
        })
    }

    /// Whether this setup already matches `sps` (nothing to recreate).
    fn matches(&self, sps: &SpsMeta) -> bool {
        self.sps == *sps
    }
}

/// The parameter-set caches: the newest SPS per id (parsed) and the newest
/// PPS per id (kept as the NAL unit bytes a slice needs them resolved from).
struct ParameterCaches {
    sps: [Option<SpsMeta>; 32],
    pps: [Option<Vec<u8>>; 256],
}

impl Default for ParameterCaches {
    fn default() -> Self {
        Self {
            sps: [None; 32],
            pps: std::array::from_fn(|_| None),
        }
    }
}

impl ParameterCaches {
    /// Remembers a parsed SPS NAL unit.
    fn remember_sps(&mut self, nal: &[u8]) -> Result<(), CodecError> {
        let meta = parse_sps_meta(nal)?;
        self.sps[meta.id as usize] = Some(meta);
        Ok(())
    }

    /// Remembers a PPS NAL unit's bytes.
    fn remember_pps(&mut self, nal: &[u8]) {
        let mut bits = Rbsp::new(&strip_epb(&nal[1..]));
        let id = bits.ue().unwrap_or(u32::MAX);
        if id < 256 {
            self.pps[id as usize] = Some(nal.to_owned());
        }
    }

    /// The `(sps, pps)` a slice's PPS id resolves to.
    fn resolve(&self, pps_id: u32) -> Result<(SpsMeta, PpsMeta), CodecError> {
        let nal = self
            .pps
            .get(pps_id as usize)
            .and_then(Option::as_ref)
            .ok_or(CodecError::InvalidBitstream(
                "a slice references an unknown parameter set",
            ))?;
        let pps = parse_pps_meta(nal)?;
        let sps = *self
            .sps
            .get(pps.sps_id as usize)
            .and_then(Option::as_ref)
            .ok_or(CodecError::InvalidBitstream(
                "a slice references an unknown parameter set",
            ))?;
        Ok((sps, pps))
    }
}

/// A VA-API hardware H.264 decoder (see [`super::HardwareSource::open_decoder`]).
///
/// [`VideoDecoder::decode`] outputs one picture per access unit, in decoding
/// order (B slices are rejected, so decoding order is output order); every
/// picture is fully drained before the call returns.
pub struct VaDecoder {
    backend: Backend,
    colors: ColorState,
    parameters: ParameterCaches,
    poc: Poc,
    setup: Option<Setup>,
    // Declared last: VA-API terminates after every object above is released.
    display: Rc<Display>,
}

impl VaDecoder {
    /// Opens the hardware decoder on a DRM device. The picture setup is
    /// created when the first access unit reveals the stream's profile.
    pub(crate) fn open(device: &std::path::Path) -> Result<Self, CodecError> {
        let display = Display::open(device)?;
        let name = format!("{} on {}", display.vendor(), device.display());
        Ok(Self {
            backend: backend(BackendKind::Hardware, name),
            colors: ColorState::default(),
            parameters: ParameterCaches::default(),
            poc: Poc::default(),
            setup: None,
            display,
        })
    }

    /// Ensures the picture setup matches the stream's Sequence parameters,
    /// recreating it (and restarting the DPB and POC bookkeeping) otherwise.
    fn ensure_setup(&mut self, sps: &SpsMeta) -> Result<(), CodecError> {
        if self.setup.as_ref().is_some_and(|setup| setup.matches(sps)) {
            return Ok(());
        }
        self.poc.reset();
        self.setup = Some(Setup::open(&self.display, sps)?);
        Ok(())
    }

    /// Renders one whole picture and hands its decoded surface to the caller.
    fn render(
        &mut self,
        access_unit: &[u8],
        timestamp: Duration,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError> {
        // Contract vetting first: framing, size limits, 8-bit 4:2:0 and the
        // VUI color metadata of the parameter sets this access unit carries.
        let color = self.colors.inspect(access_unit)?;

        for nal in h264::nal_units(access_unit) {
            match h264::nal_unit_type(nal) {
                h264::NAL_SPS => self.parameters.remember_sps(nal)?,
                h264::NAL_PPS => self.parameters.remember_pps(nal),
                2..=4 => {
                    return Err(CodecError::Unsupported(
                        "data-partitioned slices are not supported",
                    ));
                }
                _ => {}
            }
        }

        // Parse every slice of the picture before touching decoder state: a
        // malformed partner slice must not leave half a picture committed.
        let mut slices = Vec::new();
        for nal in h264::nal_units(access_unit)
            .filter(|nal| matches!(h264::nal_unit_type(nal), h264::NAL_SLICE | h264::NAL_IDR))
        {
            let (sps, pps, header) =
                parse_slice_header(nal, |pps_id| self.parameters.resolve(pps_id))?;
            if let Some((head_sps, _, head, _)) = slices.first() {
                head.consistent(&header)?;
                if *head_sps != sps {
                    return Err(CodecError::InvalidBitstream(
                        "the slices of one picture disagree",
                    ));
                }
            }
            slices.push((sps, pps, header, nal));
        }
        let Some((sps, pps, head, head_nal)) = slices.first() else {
            return Err(CodecError::InvalidBitstream(
                "access unit has no coded slice",
            ));
        };
        let (sps, pps, head) = (*sps, *pps, *head);
        let is_ref = is_reference(head_nal);

        self.ensure_setup(&sps)?;

        // The bookkeeping updates happen once per picture, before the setup
        // borrow below.
        let order_count = self.poc.count(
            &sps,
            head.pic_order_cnt_lsb,
            head.frame_num,
            is_ref,
            head.idr,
        );

        let display = Rc::clone(&self.display);
        // `ensure_setup` guaranteed a live setup above.
        let Some(setup) = self.setup.as_mut() else {
            return Err(CodecError::Unsupported("the picture setup is missing"));
        };

        if head.idr {
            setup.dpb.clear();
        }
        let slot = setup.dpb.free_slot().ok_or(CodecError::Unsupported(
            "the decoder ran out of output surfaces",
        ))?;
        let frame_idx = setup.dpb.next_frame_idx();
        let references = setup.dpb.references();
        let list0 = setup.dpb.list();

        // Build the records and buffers before any picture is submitted.
        let mut buffers: Vec<NativeBuffer> = Vec::new();
        if sps.profile_idc == 100 {
            let matrix = iq_matrix();
            buffers.push(NativeBuffer::create(
                &display,
                setup.context.id(),
                VABufferType_VAIQMatrixBufferType,
                record_bytes(&matrix),
            )?);
        }
        let current = picture(
            setup.surfaces.id(slot),
            frame_idx,
            u32::from(is_ref) * VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            order_count,
            order_count,
        );
        buffers.push(NativeBuffer::create(
            &display,
            setup.context.id(),
            VABufferType_VAPictureParameterBufferType,
            record_bytes(&picture_record(
                &sps,
                &pps,
                current,
                &references,
                head.frame_num,
                is_ref,
            )),
        )?);
        for (_, _, header, nal) in &slices {
            let parameter = slice_record(nal.len(), header, &list0);
            buffers.push(NativeBuffer::create(
                &display,
                setup.context.id(),
                VABufferType_VASliceParameterBufferType,
                record_bytes(&parameter),
            )?);
            let mut data = Vec::with_capacity(nal.len() + SLICE_DATA_START_CODE.len());
            data.extend_from_slice(&SLICE_DATA_START_CODE);
            data.extend_from_slice(nal);
            buffers.push(NativeBuffer::create(
                &display,
                setup.context.id(),
                VABufferType_VASliceDataBufferType,
                &data,
            )?);
        }

        let handle = display.handle();
        let rendered: Vec<VABufferID> = buffers.iter().map(NativeBuffer::id).collect();
        // SAFETY: every surface and buffer id belongs to this context; the
        // target surface is this picture's decode output.
        check(
            "vaBeginPicture",
            vaBeginPicture(handle, setup.context.id(), setup.surfaces.id(slot)),
        )?;
        // SAFETY: `rendered` holds the ids of the buffers created above; the
        // driver reads them through libva.
        check(
            "vaRenderPicture",
            vaRenderPicture(
                handle,
                setup.context.id(),
                rendered.as_mut_ptr(),
                rendered.len() as i32,
            ),
        )?;
        // SAFETY: the picture finishes here and the driver starts decoding.
        check("vaEndPicture", vaEndPicture(handle, setup.context.id()))?;

        sync_surface(&display, setup.surfaces.id(slot))?;
        if is_ref {
            setup.dpb.push(slot, current);
        }

        Self::deliver(setup, slot, color, timestamp, out)
    }

    /// Copies the decoded surface into a borrowed `Nv12` display window and
    /// hands it to `out`. The surface was synced by the caller.
    fn deliver(
        setup: &Setup,
        slot: usize,
        color: ColorDescription,
        timestamp: Duration,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError> {
        let sps = setup.sps;
        let image = setup.image.info();
        let (pitch_y, pitch_uv) = (image.pitches[0] as usize, image.pitches[1] as usize);
        let (offset_y, offset_uv) = (image.offsets[0] as usize, image.offsets[1] as usize);
        let data_size = image.data_size as usize;
        let (x, y) = (sps.display_x as usize, sps.display_y as usize);
        let (width, height) = (sps.display_width as usize, sps.display_height as usize);
        // The mapped image must hold the whole display window of both planes.
        // The window is reached by starting each row at its x offset (the
        // crop offsets are even, so the chroma offset stays sample aligned).
        let chroma_rows = height.div_ceil(2);
        let luma_window = offset_y + y * pitch_y + x;
        let chroma_window = offset_uv + (y / 2) * pitch_uv + x / 2;
        let fits = height > 0
            && pitch_y >= width
            && pitch_uv >= width
            && luma_window + pitch_y * (height - 1) + width <= data_size
            && chroma_window + pitch_uv * (chroma_rows.max(1) - 1) + width <= data_size;
        if !fits {
            // The image was allocated for the coded size and the driver keeps
            // its own pitches; this is a driver contract breach, not bitstream
            // state.
            return Err(CodecError::Unsupported(
                "the decoded surface does not hold the display window",
            ));
        }
        let target = setup.surfaces.id(slot);
        let image = &setup.image;
        let mut deliver = |bytes: &[u8]| {
            let luma = &bytes[luma_window..];
            let chroma = &bytes[chroma_window..];
            // The window bounds were pre-verified against the mapped image, so
            // this view construction cannot fail.
            if let Ok(frame) = Nv12::new(
                sps.display_width,
                sps.display_height,
                luma,
                pitch_y,
                chroma,
                pitch_uv,
            ) {
                (out)(DecodedFrame {
                    image: frame,
                    color,
                    timestamp,
                });
            }
        };
        image.read_surface(target, sps.coded_width, sps.coded_height, &mut deliver)
    }
}

/// Whether the picture an access unit's head slice describes is a reference
/// (its NAL header carries a nonzero `nal_ref_idc`). IDR pictures always are.
fn is_reference(nal: &[u8]) -> bool {
    nal.first().map_or(false, |header| (header >> 5) & 3 != 0)
}

impl VideoDecoder for VaDecoder {
    fn backend(&self) -> &Backend {
        &self.backend
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        timestamp: Duration,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError> {
        self.render(access_unit, timestamp, out)
    }

    /// Decode is synchronous, so nothing is ever pending.
    fn flush(&mut self, _out: &mut dyn FnMut(DecodedFrame<'_>)) -> Result<(), CodecError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
