//! H.264 Annex B inspection: NAL unit splitting, access-unit checks before
//! untrusted data reaches a native decoder, sequence-parameter-set limits, and
//! SPS/PPS repetition before every IDR picture of an encoder's output.

use crate::codec::{CodecError, check_dimensions};
use crate::frame::{ColorDescription, ColorMatrix, ColorRange};

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// Largest access unit handed to a decoder.
pub const MAX_ACCESS_UNIT_BYTES: usize = 8 << 20;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// `nal_unit_type` of a NAL unit (its header byte included).
pub fn nal_unit_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |header| header & 0x1F)
}

/// Iterates the NAL units of an Annex B byte stream, header byte included,
/// without start codes or trailing zero bytes. Bytes before the first start
/// code are skipped; [`inspect`] rejects them.
pub fn nal_units(stream: &[u8]) -> NalUnits<'_> {
    let rest = match find_start_code(stream) {
        Some(at) => &stream[at + 3..],
        None => &[],
    };
    NalUnits { rest }
}

/// Iterator returned by [`nal_units`].
pub struct NalUnits<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        let (nal, rest) = match find_start_code(self.rest) {
            Some(at) => (&self.rest[..at], &self.rest[at + 3..]),
            None => (self.rest, &[][..]),
        };
        self.rest = rest;
        let end = nal
            .iter()
            .rposition(|&byte| byte != 0)
            .map_or(0, |last| last + 1);
        Some(&nal[..end])
    }
}

/// Offset of the next `00 00 01`.
fn find_start_code(data: &[u8]) -> Option<usize> {
    let mut at = 0;
    while at + 3 <= data.len() {
        match data[at + 2] {
            // The window cannot start a start code at at, at+1, or at+2.
            byte if byte > 1 => at += 3,
            1 if data[at] == 0 && data[at + 1] == 0 => return Some(at),
            _ => at += 1,
        }
    }
    None
}

/// What an access unit contains.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccessUnitSummary {
    pub idr: bool,
    pub sps: bool,
    pub pps: bool,
    /// Contains at least one coded slice.
    pub vcl: bool,
}

/// Checks Annex B framing (only zero bytes before the first start code, no
/// empty NAL units, forbidden bit clear) and summarizes the NAL unit types.
pub fn inspect(access_unit: &[u8]) -> Result<AccessUnitSummary, CodecError> {
    let first = find_start_code(access_unit)
        .ok_or(CodecError::InvalidBitstream("no Annex B start code"))?;
    if access_unit[..first].iter().any(|&byte| byte != 0) {
        return Err(CodecError::InvalidBitstream(
            "data before the first start code",
        ));
    }
    let mut summary = AccessUnitSummary::default();
    for nal in nal_units(access_unit) {
        let Some(&header) = nal.first() else {
            return Err(CodecError::InvalidBitstream("empty NAL unit"));
        };
        if header & 0x80 != 0 {
            return Err(CodecError::InvalidBitstream("forbidden_zero_bit is set"));
        }
        match nal_unit_type(nal) {
            NAL_IDR => {
                summary.idr = true;
                summary.vcl = true;
            }
            NAL_SLICE..=4 => summary.vcl = true,
            NAL_SPS => summary.sps = true,
            NAL_PPS => summary.pps = true,
            _ => {}
        }
    }
    Ok(summary)
}

/// Rejects access units a decoder must not see: oversized, malformed framing,
/// or a sequence parameter set describing a picture over the size limits or a
/// format other than 8-bit 4:2:0 (the NV12 output contract).
pub fn check_decodable(access_unit: &[u8]) -> Result<AccessUnitSummary, CodecError> {
    check_decodable_with(access_unit, |_| {})
}

fn check_decodable_with(
    access_unit: &[u8],
    mut remember: impl FnMut(Sps),
) -> Result<AccessUnitSummary, CodecError> {
    if access_unit.len() > MAX_ACCESS_UNIT_BYTES {
        return Err(CodecError::Unsupported("access unit is larger than 8 MiB"));
    }
    let summary = inspect(access_unit)?;
    if !summary.vcl {
        return Err(CodecError::InvalidBitstream(
            "access unit has no coded slice",
        ));
    }
    for nal in nal_units(access_unit).filter(|nal| nal_unit_type(nal) == NAL_SPS) {
        let sps = parse_sps(nal)?;
        if sps.chroma_format_idc != 1 || sps.bit_depth_luma != 8 || sps.bit_depth_chroma != 8 {
            return Err(CodecError::Unsupported(
                "only 8-bit 4:2:0 H.264 is supported",
            ));
        }
        check_dimensions(sps.coded_width, sps.coded_height)?;
        remember(sps);
    }
    Ok(summary)
}

/// Fields of a sequence parameter set needed to vet and describe a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    /// constraint_set0..5 flags in the high bits, as coded.
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub id: u32,
    pub chroma_format_idc: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    /// Coded picture size before frame cropping.
    pub coded_width: u32,
    pub coded_height: u32,
    /// Display size after frame cropping.
    pub width: u32,
    pub height: u32,
    /// VUI matrix/range/sample aspect, with documented defaults when absent.
    pub color: ColorDescription,
    /// Luma samples cropped from the left and top of the coded picture.
    pub crop_left: u32,
    pub crop_top: u32,
    pub log2_max_frame_num_minus4: u32,
    /// 0, 1 or 2.
    pub pic_order_cnt_type: u32,
    /// Zero unless `pic_order_cnt_type` is 0.
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_allowed: bool,
    pub frame_mbs_only: bool,
    pub direct_8x8_inference: bool,
    /// An SPS scaling matrix is present (its lists are skipped, not kept).
    pub has_scaling_matrix: bool,
}

/// Parses an SPS NAL unit (header included) through its VUI color/aspect fields.
pub fn parse_sps(nal: &[u8]) -> Result<Sps, CodecError> {
    const MALFORMED: CodecError = CodecError::InvalidBitstream("malformed sequence parameter set");
    if nal_unit_type(nal) != NAL_SPS {
        return Err(MALFORMED);
    }
    let mut bits = BitReader::new(&nal[1..]);
    let profile_idc = bits.bits(8)? as u8;
    let constraint_flags = bits.bits(8)? as u8;
    let level_idc = bits.bits(8)? as u8;
    let id = bits.ue_max(31)?;
    let (mut chroma_format_idc, mut bit_depth_luma, mut bit_depth_chroma) = (1, 8, 8);
    let mut separate_colour_planes = false;
    let mut has_scaling_matrix = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = bits.ue_max(3)?;
        if chroma_format_idc == 3 {
            separate_colour_planes = bits.flag()?;
        }
        bit_depth_luma = 8 + bits.ue_max(6)?;
        bit_depth_chroma = 8 + bits.ue_max(6)?;
        bits.flag()?; // qpprime_y_zero_transform_bypass_flag
        has_scaling_matrix = bits.flag()?;
        if has_scaling_matrix {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for list in 0..lists {
                if bits.flag()? {
                    skip_scaling_list(&mut bits, if list < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let log2_max_frame_num_minus4 = bits.ue_max(12)?;
    let pic_order_cnt_type = bits.ue_max(2)?;
    let mut log2_max_pic_order_cnt_lsb_minus4 = 0;
    match pic_order_cnt_type {
        0 => {
            log2_max_pic_order_cnt_lsb_minus4 = bits.ue_max(12)?;
        }
        1 => {
            bits.flag()?; // delta_pic_order_always_zero_flag
            bits.se()?; // offset_for_non_ref_pic
            bits.se()?; // offset_for_top_to_bottom_field
            for _ in 0..bits.ue_max(255)? {
                bits.se()?; // offset_for_ref_frame
            }
        }
        _ => {}
    }
    let max_num_ref_frames = bits.ue_max(16)?;
    let gaps_in_frame_num_allowed = bits.flag()?;
    let width_mbs = u64::from(bits.ue()?) + 1;
    let height_map_units = u64::from(bits.ue()?) + 1;
    let frame_mbs_only = bits.flag()?;
    if !frame_mbs_only {
        bits.flag()?; // mb_adaptive_frame_field_flag
    }
    let direct_8x8_inference = bits.flag()?;
    let field_factor = if frame_mbs_only { 1 } else { 2 };
    let coded_width = width_mbs * 16;
    let coded_height = height_map_units * 16 * field_factor;
    let (mut width, mut height) = (coded_width, coded_height);
    let (mut crop_left, mut crop_top) = (0, 0);
    if bits.flag()? {
        // Crop units per ITU-T H.264 (7-19)..(7-22).
        let (unit_x, unit_y) = match (chroma_format_idc, separate_colour_planes) {
            (1, false) => (2, 2 * field_factor),
            (2, false) => (2, field_factor),
            _ => (1, field_factor),
        };
        let left = u64::from(bits.ue()?);
        let right = u64::from(bits.ue()?);
        let top = u64::from(bits.ue()?);
        let bottom = u64::from(bits.ue()?);
        let crop_x = (left + right) * unit_x;
        let crop_y = (top + bottom) * unit_y;
        if crop_x >= coded_width || crop_y >= coded_height {
            return Err(MALFORMED);
        }
        width -= crop_x;
        height -= crop_y;
        crop_left = left * unit_x;
        crop_top = top * unit_y;
    }
    let (Ok(coded_width), Ok(coded_height), Ok(width), Ok(height), Ok(crop_left), Ok(crop_top)) = (
        u32::try_from(coded_width),
        u32::try_from(coded_height),
        u32::try_from(width),
        u32::try_from(height),
        u32::try_from(crop_left),
        u32::try_from(crop_top),
    ) else {
        return Err(CodecError::Unsupported("picture is larger than 4096x2304"));
    };
    let color = if bits.flag()? {
        parse_vui_color(&mut bits)?
    } else {
        ColorDescription::default()
    };
    Ok(Sps {
        profile_idc,
        constraint_flags,
        level_idc,
        id,
        chroma_format_idc,
        bit_depth_luma,
        bit_depth_chroma,
        coded_width,
        coded_height,
        width,
        height,
        color,
        crop_left,
        crop_top,
        log2_max_frame_num_minus4,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4,
        max_num_ref_frames,
        gaps_in_frame_num_allowed,
        frame_mbs_only,
        direct_8x8_inference,
        has_scaling_matrix,
    })
}

/// Picture parameter set fields the VA-API host parser and slice headers
/// need, through the High-profile extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    pub transform_8x8_mode_flag: bool,
    /// Equal to `chroma_qp_index_offset` when the PPS has no extension.
    pub second_chroma_qp_index_offset: i32,
    /// `pic_scaling_matrix_present_flag`; the lists and the second chroma
    /// offset after it are not parsed (the offset keeps the default).
    pub has_scaling_matrix: bool,
}

/// Parses a PPS NAL unit (header included). Slice groups (FMO) are refused as
/// `Unsupported` because their map syntax is not walked.
pub fn parse_pps(nal: &[u8]) -> Result<Pps, CodecError> {
    const MALFORMED: CodecError = CodecError::InvalidBitstream("malformed parameter set");
    if nal_unit_type(nal) != NAL_PPS {
        return Err(MALFORMED);
    }
    let mut bits = BitReader::new(&nal[1..]);
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
    let weighted_pred_flag = bits.flag()?;
    let weighted_bipred_idc = bits.bits(2)?;
    let pic_init_qp_minus26 = bits.se()?;
    bits.se()?; // pic_init_qs_minus26
    let chroma_qp_index_offset = bits.se()?;
    let deblocking_filter_control_present_flag = bits.flag()?;
    let constrained_intra_pred_flag = bits.flag()?;
    let redundant_pic_cnt_present_flag = bits.flag()?;
    let (mut transform_8x8_mode_flag, mut has_scaling_matrix) = (false, false);
    let mut second_chroma_qp_index_offset = chroma_qp_index_offset;
    if bits.more_rbsp_data() {
        transform_8x8_mode_flag = bits.flag()?;
        has_scaling_matrix = bits.flag()?;
        if !has_scaling_matrix {
            second_chroma_qp_index_offset = bits.se()?;
        }
    }
    Ok(Pps {
        id,
        sps_id,
        entropy_coding_mode_flag,
        bottom_field_pic_order_in_frame_present_flag,
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        weighted_pred_flag,
        weighted_bipred_idc,
        pic_init_qp_minus26,
        chroma_qp_index_offset,
        deblocking_filter_control_present_flag,
        constrained_intra_pred_flag,
        redundant_pic_cnt_present_flag,
        transform_8x8_mode_flag,
        second_chroma_qp_index_offset,
        has_scaling_matrix,
    })
}

fn parse_vui_color(bits: &mut BitReader<'_>) -> Result<ColorDescription, CodecError> {
    let mut color = ColorDescription::default();
    if bits.flag()? {
        color.pixel_aspect_ratio = match bits.bits(8)? {
            0 | 1 => (1, 1),
            2 => (12, 11),
            3 => (10, 11),
            4 => (16, 11),
            5 => (40, 33),
            6 => (24, 11),
            7 => (20, 11),
            8 => (32, 11),
            9 => (80, 33),
            10 => (18, 11),
            11 => (15, 11),
            12 => (64, 33),
            13 => (160, 99),
            14 => (4, 3),
            15 => (3, 2),
            16 => (2, 1),
            255 => {
                let ratio = (bits.bits(16)?, bits.bits(16)?);
                if ratio.0 == 0 || ratio.1 == 0 {
                    return Err(CodecError::InvalidBitstream(
                        "zero H.264 sample aspect ratio",
                    ));
                }
                ratio
            }
            _ => return Err(CodecError::InvalidBitstream("reserved H.264 aspect ratio")),
        };
    }
    if bits.flag()? {
        bits.flag()?; // overscan_appropriate_flag
    }
    if bits.flag()? {
        bits.bits(3)?; // video_format
        color.range = if bits.flag()? {
            ColorRange::Full
        } else {
            ColorRange::Limited
        };
        if bits.flag()? {
            bits.bits(8)?; // colour_primaries
            bits.bits(8)?; // transfer_characteristics
            color.matrix = ColorMatrix::from_h273(bits.bits(8)?).ok_or(CodecError::Unsupported(
                "H.264 color matrix is not BT.601/709/2020 NCL",
            ))?;
        }
    }
    Ok(color)
}

#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
/// Fixed-size parameter-set metadata, selecting the SPS actually referenced
/// by the access unit's first slice rather than the last SPS in the byte stream.
pub(crate) struct ColorState {
    sps: [Option<ColorDescription>; 32],
    pps: [Option<u32>; 256],
    current: ColorDescription,
}

#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
impl Default for ColorState {
    fn default() -> Self {
        Self {
            sps: [None; 32],
            pps: [None; 256],
            current: ColorDescription::default(),
        }
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
impl ColorState {
    pub(crate) fn inspect(&mut self, access_unit: &[u8]) -> Result<ColorDescription, CodecError> {
        check_decodable_with(access_unit, |sps| {
            self.sps[sps.id as usize] = Some(sps.color)
        })?;
        for nal in nal_units(access_unit).filter(|nal| nal_unit_type(nal) == NAL_PPS) {
            let mut bits = BitReader::new(&nal[1..]);
            let pps = bits.ue_max(255)?;
            self.pps[pps as usize] = Some(bits.ue_max(31)?);
        }
        if let Some(slice) =
            nal_units(access_unit).find(|nal| matches!(nal_unit_type(nal), NAL_SLICE | 2 | NAL_IDR))
        {
            let mut bits = BitReader::new(&slice[1..]);
            bits.ue()?; // first_mb_in_slice
            bits.ue_max(9)?; // slice_type
            let pps = bits.ue_max(255)?;
            if let Some(color) = self.pps[pps as usize].and_then(|sps| self.sps[sps as usize]) {
                self.current = color;
            }
        }
        Ok(self.current)
    }
}

fn skip_scaling_list(bits: &mut BitReader<'_>, size: usize) -> Result<(), CodecError> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            let delta = i64::from(bits.se()?);
            if !(-128..=127).contains(&delta) {
                return Err(CodecError::InvalidBitstream(
                    "malformed sequence parameter set",
                ));
            }
            next = (last + delta).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

/// MSB-first reader over an RBSP that drops emulation-prevention bytes.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    zeros: u8,
    current: u8,
    left: u8,
    /// Emulation-prevention bytes dropped so far.
    escapes: usize,
}

const TRUNCATED: CodecError = CodecError::InvalidBitstream("truncated NAL unit");

impl<'a> BitReader<'a> {
    /// `data` starts after the NAL header byte.
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            zeros: 0,
            current: 0,
            left: 0,
            escapes: 0,
        }
    }

    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    /// Bits consumed so far, emulation-prevention bytes not counted
    /// (`slice_data_bit_offset` is relative to the RBSP).
    pub(crate) fn position(&self) -> usize {
        (self.pos - self.escapes) * 8 - usize::from(self.left)
    }

    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    /// Whether syntax remains before the `rbsp_stop_one_bit` (H.264 7.2).
    pub(crate) fn more_rbsp_data(&self) -> bool {
        let Some(last) = self.data.iter().rposition(|&byte| byte != 0) else {
            return false;
        };
        let stop = last * 8 + 7 - self.data[last].trailing_zeros() as usize;
        // Raw bit position, escapes included, to match the raw data; an
        // escape byte about to be dropped is already behind the cursor.
        let pending = self.left == 0 && self.zeros >= 2 && self.data.get(self.pos) == Some(&3);
        (self.pos + usize::from(pending)) * 8 - usize::from(self.left) < stop
    }

    fn load(&mut self) -> Result<(), CodecError> {
        let mut byte = *self.data.get(self.pos).ok_or(TRUNCATED)?;
        self.pos += 1;
        if self.zeros >= 2 && byte == 3 {
            byte = *self.data.get(self.pos).ok_or(TRUNCATED)?;
            self.pos += 1;
            self.escapes += 1;
            self.zeros = 0;
        }
        self.zeros = if byte == 0 {
            self.zeros.saturating_add(1)
        } else {
            0
        };
        self.current = byte;
        self.left = 8;
        Ok(())
    }

    pub(crate) fn flag(&mut self) -> Result<bool, CodecError> {
        if self.left == 0 {
            self.load()?;
        }
        self.left -= 1;
        Ok((self.current >> self.left) & 1 == 1)
    }

    /// Reads `count` (at most 32) bits.
    pub(crate) fn bits(&mut self, count: u32) -> Result<u32, CodecError> {
        let mut value = 0u64;
        for _ in 0..count {
            value = (value << 1) | u64::from(self.flag()?);
        }
        Ok(value as u32)
    }

    /// Unsigned Exp-Golomb.
    pub(crate) fn ue(&mut self) -> Result<u32, CodecError> {
        let mut leading = 0;
        while !self.flag()? {
            leading += 1;
            if leading > 31 {
                return Err(CodecError::InvalidBitstream("Exp-Golomb value overflows"));
            }
        }
        Ok(((1u64 << leading) - 1 + u64::from(self.bits(leading)?)) as u32)
    }

    pub(crate) fn ue_max(&mut self, max: u32) -> Result<u32, CodecError> {
        match self.ue()? {
            value if value <= max => Ok(value),
            _ => Err(CodecError::InvalidBitstream(
                "sequence parameter set value out of range",
            )),
        }
    }

    /// Signed Exp-Golomb.
    pub(crate) fn se(&mut self) -> Result<i32, CodecError> {
        let code = i64::from(self.ue()?);
        let value = if code % 2 == 1 {
            (code + 1) / 2
        } else {
            -(code / 2)
        };
        Ok(value as i32)
    }
}

/// Keeps every IDR access unit of an encoder's output self-contained: H.264
/// encoders may emit SPS/PPS only once, but each keyframe a receiver joins on
/// (or requests by PLI) must carry them.
#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
#[derive(Default)]
pub(crate) struct ParameterSets {
    sps: Vec<u8>,
    pps: Vec<u8>,
}

#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
impl ParameterSets {
    /// Records the parameter sets in `access_unit` and returns it with any
    /// missing SPS/PPS inserted before an IDR picture, plus whether it is a
    /// keyframe. Header-only output yields `None` (its parameter sets are
    /// remembered for the next picture).
    pub(crate) fn complete(
        &mut self,
        access_unit: Vec<u8>,
    ) -> Result<Option<(Vec<u8>, bool)>, CodecError> {
        let summary = inspect(&access_unit)?;
        for nal in nal_units(&access_unit) {
            let slot = match nal_unit_type(nal) {
                NAL_SPS => &mut self.sps,
                NAL_PPS => &mut self.pps,
                _ => continue,
            };
            slot.clear();
            slot.extend_from_slice(nal);
        }
        if !summary.vcl {
            return Ok(None);
        }
        if !summary.idr || (summary.sps && summary.pps) {
            return Ok(Some((access_unit, summary.idr)));
        }
        if self.sps.is_empty() || self.pps.is_empty() {
            return Err(CodecError::InvalidBitstream(
                "encoder emitted an IDR picture before any SPS/PPS",
            ));
        }
        // Rebuild as [AUD] SPS PPS <other NAL units>, keeping the access unit
        // delimiter first as Annex B requires.
        let mut rebuilt =
            Vec::with_capacity(access_unit.len() + self.sps.len() + self.pps.len() + 8);
        let units = || nal_units(&access_unit);
        for nal in units().filter(|nal| nal_unit_type(nal) == NAL_AUD) {
            push_nal(&mut rebuilt, nal);
        }
        push_nal(&mut rebuilt, &self.sps);
        push_nal(&mut rebuilt, &self.pps);
        for nal in units().filter(|nal| !matches!(nal_unit_type(nal), NAL_AUD | NAL_SPS | NAL_PPS))
        {
            push_nal(&mut rebuilt, nal);
        }
        Ok(Some((rebuilt, true)))
    }
}

#[cfg_attr(
    not(any(windows, target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
fn push_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(nal);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_vectors::{self, BitWriter};

    const FIXTURE: &[u8] = include_bytes!("../../../fixtures/video/ipcm-128x96.h264");

    fn types(stream: &[u8]) -> Vec<u8> {
        nal_units(stream).map(nal_unit_type).collect()
    }

    #[test]
    fn splits_three_and_four_byte_start_codes_and_trims_trailing_zeros() {
        let stream = [
            0, 0, 0, 1, 0x67, 0xAA, 0, 0, // SPS + trailing_zero_8bits
            0, 0, 1, 0x68, 0xBB, // PPS after a 3-byte start code
            0, 0, 0, 1, 0x65, 0x00, 0x00, 0x03, 0x01, // IDR containing an escape
        ];
        let units: Vec<&[u8]> = nal_units(&stream).collect();
        assert_eq!(
            units,
            [
                &[0x67, 0xAA][..],
                &[0x68, 0xBB],
                &[0x65, 0x00, 0x00, 0x03, 0x01]
            ]
        );
        assert_eq!(
            inspect(&stream).unwrap(),
            AccessUnitSummary {
                idr: true,
                sps: true,
                pps: true,
                vcl: true
            }
        );
    }

    #[test]
    fn malformed_framing_is_rejected() {
        for (stream, why) in [
            (&[0x65, 0x88][..], "no Annex B start code"),
            (&[7, 0, 0, 1, 0x65], "data before the first start code"),
            (&[0, 0, 1, 0, 0, 1, 0x65], "empty NAL unit"),
            (&[0, 0, 1, 0xE5], "forbidden_zero_bit is set"),
        ] {
            assert_eq!(inspect(stream), Err(CodecError::InvalidBitstream(why)));
        }
        assert_eq!(
            check_decodable(&[0, 0, 1, 0x67, 0x42]),
            Err(CodecError::InvalidBitstream(
                "access unit has no coded slice"
            ))
        );
        let huge = vec![0u8; MAX_ACCESS_UNIT_BYTES + 1];
        assert!(matches!(
            check_decodable(&huge),
            Err(CodecError::Unsupported(_))
        ));
    }

    #[test]
    fn fixture_is_the_generated_ipcm_stream() {
        // The committed fixture is reproducible from its generator.
        assert_eq!(test_vectors::ipcm_stream(), FIXTURE);
        let access_units = test_vectors::ipcm_access_units(FIXTURE);
        assert_eq!(access_units.len(), 3);
        assert_eq!(types(access_units[0]), [NAL_SPS, NAL_PPS, NAL_IDR]);
        assert_eq!(types(access_units[1]), [NAL_SLICE]);
        assert_eq!(types(access_units[2]), [NAL_IDR]);
        for access_unit in &access_units {
            check_decodable(access_unit).unwrap();
        }
    }

    #[test]
    fn parses_the_fixture_sps() {
        let sps = nal_units(FIXTURE)
            .find(|nal| nal_unit_type(nal) == NAL_SPS)
            .unwrap();
        assert_eq!(
            parse_sps(sps).unwrap(),
            Sps {
                profile_idc: 66,
                constraint_flags: 0xC0,
                level_idc: 30,
                id: 0,
                chroma_format_idc: 1,
                bit_depth_luma: 8,
                bit_depth_chroma: 8,
                coded_width: 128,
                coded_height: 96,
                width: 128,
                height: 96,
                color: ColorDescription::default(),
                crop_left: 0,
                crop_top: 0,
                log2_max_frame_num_minus4: 0,
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
    fn vui_extracts_every_supported_matrix_range_and_extended_aspect() {
        for matrix in [ColorMatrix::Bt601, ColorMatrix::Bt709, ColorMatrix::Bt2020] {
            for range in [ColorRange::Limited, ColorRange::Full] {
                let color = ColorDescription {
                    matrix,
                    range,
                    pixel_aspect_ratio: (64, 45),
                };
                let sps = parse_sps(&test_vectors::color_sps(0, color)).unwrap();
                assert_eq!(sps.color, color);
                let stream = test_vectors::ipcm_stream_with_color(color);
                let mut state = ColorState::default();
                for unit in test_vectors::ipcm_access_units(&stream) {
                    assert_eq!(state.inspect(unit).unwrap(), color);
                }
            }
        }
    }

    #[test]
    fn vui_standard_aspects_and_absent_color_follow_documented_defaults() {
        for (id, ratio) in [
            (0, (1, 1)),
            (1, (1, 1)),
            (2, (12, 11)),
            (14, (4, 3)),
            (16, (2, 1)),
        ] {
            let mut bits = BitWriter::default();
            bits.flag(true);
            bits.bits(id, 8);
            bits.flag(false); // overscan
            bits.flag(false); // video signal type
            bits.trailing();
            let rbsp = bits.finish();
            let color = parse_vui_color(&mut BitReader::new(&rbsp)).unwrap();
            assert_eq!(color.pixel_aspect_ratio, ratio);
            assert_eq!(color.matrix, ColorMatrix::Bt709);
            assert_eq!(color.range, ColorRange::Limited);
        }
        let invalid = ColorDescription {
            pixel_aspect_ratio: (0, 1),
            ..ColorDescription::default()
        };
        assert!(matches!(
            parse_sps(&test_vectors::color_sps(0, invalid)),
            Err(CodecError::InvalidBitstream(_))
        ));
    }

    #[test]
    fn explicit_non_affine_or_unsupported_vui_matrices_are_rejected() {
        for matrix in [0, 4, 7, 8, 10, 11, 12, 13, 14, 255] {
            let mut bits = BitWriter::default();
            bits.flag(false); // aspect ratio
            bits.flag(false); // overscan
            bits.flag(true); // video signal type
            bits.bits(5, 3);
            bits.flag(false); // limited range
            bits.flag(true); // color description
            bits.bits(2, 8);
            bits.bits(2, 8);
            bits.bits(matrix, 8);
            bits.trailing();
            let rbsp = bits.finish();
            assert!(matches!(
                parse_vui_color(&mut BitReader::new(&rbsp)),
                Err(CodecError::Unsupported(_))
            ));
        }
    }

    #[test]
    fn color_state_uses_the_sps_selected_by_the_slice_pps() {
        let first = ColorDescription {
            matrix: ColorMatrix::Bt601,
            range: ColorRange::Full,
            pixel_aspect_ratio: (12, 11),
        };
        let second = ColorDescription {
            matrix: ColorMatrix::Bt2020,
            ..ColorDescription::default()
        };
        let mut stream = Vec::new();
        push_nal(&mut stream, &test_vectors::color_sps(0, first));
        push_nal(&mut stream, &test_vectors::color_sps(1, second));
        let mut pps = BitWriter::default();
        pps.ue(7);
        pps.ue(0);
        pps.trailing();
        let mut nal = vec![0x68];
        nal.extend(test_vectors::escape(&pps.finish()));
        push_nal(&mut stream, &nal);
        let mut slice = BitWriter::default();
        slice.ue(0);
        slice.ue(2);
        slice.ue(7);
        slice.trailing();
        let mut nal = vec![0x65];
        nal.extend(test_vectors::escape(&slice.finish()));
        push_nal(&mut stream, &nal);
        let mut state = ColorState::default();
        assert_eq!(state.inspect(&stream).unwrap(), first);
    }

    /// A High-profile SPS with scaling lists, field coding, and frame cropping.
    fn high_profile_sps(width_mbs: u32, height_map_units: u32, crop: [u32; 4]) -> Vec<u8> {
        let mut bits = BitWriter::default();
        bits.bits(100, 8); // profile_idc
        bits.bits(0, 8);
        bits.bits(40, 8); // level 4.0
        bits.ue(1); // seq_parameter_set_id
        bits.ue(1); // chroma_format_idc 4:2:0
        bits.ue(0); // bit_depth_luma_minus8
        bits.ue(0); // bit_depth_chroma_minus8
        bits.flag(false);
        bits.flag(true); // seq_scaling_matrix_present_flag
        for list in 0..8 {
            let present = list == 0 || list == 6;
            bits.flag(present);
            if present {
                let size = if list < 6 { 16 } else { 64 };
                for _ in 0..size {
                    bits.se(3);
                }
            }
        }
        bits.ue(0); // log2_max_frame_num_minus4
        bits.ue(1); // pic_order_cnt_type 1
        bits.flag(false);
        bits.se(-2);
        bits.se(1);
        bits.ue(2);
        bits.se(2);
        bits.se(-2);
        bits.ue(4); // max_num_ref_frames
        bits.flag(false);
        bits.ue(width_mbs - 1);
        bits.ue(height_map_units - 1);
        bits.flag(false); // frame_mbs_only_flag: field coding allowed
        bits.flag(true); // mb_adaptive_frame_field_flag
        bits.flag(true);
        bits.flag(true); // frame_cropping_flag
        for value in crop {
            bits.ue(value);
        }
        bits.flag(false); // vui_parameters_present_flag
        bits.trailing();
        let mut nal = vec![0x67];
        nal.extend(test_vectors::escape(&bits.finish()));
        nal
    }

    #[test]
    fn parses_high_profile_sps_with_scaling_lists_and_field_cropping() {
        // 1920x1088 coded as 68 field map units, cropped by 8 lines (2 * 2 * 2).
        let sps = parse_sps(&high_profile_sps(120, 34, [0, 0, 0, 2])).unwrap();
        assert_eq!((sps.profile_idc, sps.id), (100, 1));
        assert_eq!((sps.width, sps.height), (1920, 1080));
    }

    #[test]
    fn rejects_oversized_or_inconsistent_sps() {
        let mut au = vec![0, 0, 0, 1];
        au.extend(high_profile_sps(512, 80, [0, 0, 0, 0])); // 8192x2560
        au.extend([0, 0, 0, 1, 0x65, 0x88]);
        assert!(matches!(
            check_decodable(&au),
            Err(CodecError::Unsupported(_))
        ));
        // Cropping cannot hide a coded picture that exceeds the decoder bound.
        let mut cropped = vec![0, 0, 0, 1];
        cropped.extend(high_profile_sps(512, 68, [0, 3456, 0, 0]));
        cropped.extend([0, 0, 0, 1, 0x65, 0x88]);
        assert_eq!(
            parse_sps(
                nal_units(&cropped)
                    .find(|nal| nal_unit_type(nal) == NAL_SPS)
                    .unwrap()
            )
            .map(|sps| (sps.coded_width, sps.width)),
            Ok((8192, 1280))
        );
        assert!(matches!(
            check_decodable(&cropped),
            Err(CodecError::Unsupported(_))
        ));
        assert_eq!(
            parse_sps(&high_profile_sps(2, 2, [8, 8, 0, 0])),
            Err(CodecError::InvalidBitstream(
                "malformed sequence parameter set"
            ))
        );
        let sps = high_profile_sps(120, 34, [0, 0, 0, 2]);
        assert_eq!(
            parse_sps(&sps[..8]),
            Err(CodecError::InvalidBitstream("truncated NAL unit"))
        );
        assert!(parse_sps(&[0x68, 0x42]).is_err());
    }

    #[test]
    fn exp_golomb_and_emulation_prevention() {
        let mut bits = BitWriter::default();
        for value in [0, 1, 2, 7, 255, 65_535, u32::MAX - 1] {
            bits.ue(value);
        }
        for value in [0, 1, -1, 5, -6, i32::MAX] {
            bits.se(value);
        }
        bits.bits(0, 24); // forces an escape inside the reader's input
        bits.bits(0x01, 8);
        bits.trailing();
        let escaped = test_vectors::escape(&bits.finish());
        assert!(escaped.windows(3).any(|w| w == [0, 0, 3]));
        let mut reader = BitReader::new(&escaped);
        for value in [0, 1, 2, 7, 255, 65_535, u32::MAX - 1] {
            assert_eq!(reader.ue().unwrap(), value);
        }
        for value in [0, 1, -1, 5, -6, i32::MAX] {
            assert_eq!(reader.se().unwrap(), value);
        }
        assert_eq!(reader.bits(24).unwrap(), 0);
        assert_eq!(reader.bits(8).unwrap(), 1);
    }

    #[test]
    fn position_skips_escapes_and_more_rbsp_data_finds_the_stop_bit() {
        // `00 00 03` loses its escape byte; a following escape is dropped too.
        let mut reader = BitReader::new(&[0, 0, 3, 5, 0, 0, 3, 0x80]);
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.bits(16).unwrap(), 0);
        assert_eq!(reader.position(), 16);
        assert_eq!(reader.bits(8).unwrap(), 5);
        assert!(reader.more_rbsp_data());
        assert_eq!(reader.bits(16).unwrap(), 0);
        assert_eq!(reader.position(), 40);
        assert!(!reader.more_rbsp_data(), "only the stop bit is left");
        assert!(reader.flag().unwrap());
        // A short zero run is not an escape.
        let mut plain = BitReader::new(&[0, 0, 0, 4]);
        assert_eq!(plain.bits(32).unwrap(), 4);
        assert_eq!(plain.position(), 32);
        assert!(!BitReader::new(&[0, 0]).more_rbsp_data());
    }

    #[test]
    fn the_fixture_pps_parses_and_malformed_parameter_sets_never_panic() {
        let pps = nal_units(FIXTURE)
            .find(|nal| nal_unit_type(nal) == NAL_PPS)
            .unwrap();
        let parsed = parse_pps(pps).unwrap();
        assert_eq!((parsed.id, parsed.sps_id), (0, 0));
        assert!(!parsed.transform_8x8_mode_flag && !parsed.has_scaling_matrix);
        assert_eq!(
            parsed.second_chroma_qp_index_offset,
            parsed.chroma_qp_index_offset
        );
        let sps = nal_units(FIXTURE)
            .find(|nal| nal_unit_type(nal) == NAL_SPS)
            .unwrap();
        for nal in [sps, pps] {
            for len in 0..nal.len() {
                let _ = parse_sps(&nal[..len]);
                let _ = parse_pps(&nal[..len]);
            }
            for fill in [0x00u8, 0xFF, 0x03, 0x55] {
                let mut junk = vec![nal[0]];
                junk.extend(std::iter::repeat_n(fill, 64));
                let _ = parse_sps(&junk);
                let _ = parse_pps(&junk);
            }
        }
    }

    #[test]
    fn a_high_profile_pps_extension_reaches_the_parsed_fields() {
        let mut bits = BitWriter::default();
        bits.ue(3); // pic_parameter_set_id
        bits.ue(2); // seq_parameter_set_id
        bits.flag(true); // entropy_coding_mode_flag
        bits.flag(false);
        bits.ue(0); // slice groups
        bits.ue(0);
        bits.ue(0);
        bits.flag(false);
        bits.bits(0, 2);
        bits.se(-2); // pic_init_qp_minus26
        bits.se(0);
        bits.se(4); // chroma_qp_index_offset
        bits.flag(true);
        bits.flag(true); // constrained_intra_pred_flag
        bits.flag(false);
        bits.flag(true); // transform_8x8_mode_flag
        bits.flag(false); // pic_scaling_matrix_present_flag
        bits.se(-3); // second_chroma_qp_index_offset
        bits.trailing();
        let mut nal = vec![0x68];
        nal.extend(test_vectors::escape(&bits.finish()));
        let pps = parse_pps(&nal).unwrap();
        assert_eq!((pps.id, pps.sps_id, pps.pic_init_qp_minus26), (3, 2, -2));
        assert!(pps.entropy_coding_mode_flag && pps.constrained_intra_pred_flag);
        assert!(pps.transform_8x8_mode_flag && !pps.has_scaling_matrix);
        assert_eq!(
            (
                pps.chroma_qp_index_offset,
                pps.second_chroma_qp_index_offset
            ),
            (4, -3)
        );
    }

    #[test]
    fn parameter_sets_are_inserted_before_an_idr_that_lacks_them() {
        let mut sets = ParameterSets::default();
        // Header-only output is remembered but not emitted.
        assert_eq!(
            sets.complete(vec![0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2])
                .unwrap(),
            None
        );
        let p_frame = vec![0, 0, 0, 1, 0x41, 9];
        assert_eq!(
            sets.complete(p_frame.clone()).unwrap(),
            Some((p_frame, false))
        );
        let (keyframe, idr) = sets
            .complete(vec![
                0, 0, 0, 1, 0x09, 0x10, 0, 0, 0, 1, 0x06, 5, 0, 0, 1, 0x65, 7,
            ])
            .unwrap()
            .unwrap();
        assert!(idr);
        assert_eq!(
            keyframe,
            [
                0, 0, 0, 1, 0x09, 0x10, // delimiter stays first
                0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, // remembered SPS, PPS
                0, 0, 0, 1, 0x06, 5, 0, 0, 0, 1, 0x65, 7,
            ]
        );
        // A newer SPS in the same access unit wins over the remembered one.
        let (keyframe, _) = sets
            .complete(vec![0, 0, 0, 1, 0x67, 3, 0, 0, 0, 1, 0x65, 8])
            .unwrap()
            .unwrap();
        assert_eq!(
            keyframe,
            [
                0, 0, 0, 1, 0x67, 3, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 8
            ]
        );
        let complete = vec![
            0, 0, 0, 1, 0x67, 4, 0, 0, 0, 1, 0x68, 5, 0, 0, 0, 1, 0x65, 9,
        ];
        assert_eq!(
            sets.complete(complete.clone()).unwrap(),
            Some((complete, true))
        );
    }

    #[test]
    fn an_idr_before_any_parameter_set_is_an_encoder_error() {
        let mut sets = ParameterSets::default();
        assert!(matches!(
            sets.complete(vec![0, 0, 0, 1, 0x65, 1]),
            Err(CodecError::InvalidBitstream(_))
        ));
    }
}
