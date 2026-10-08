//! H.264 Annex B inspection: NAL unit splitting, access-unit checks before
//! untrusted data reaches a native decoder, sequence-parameter-set limits, and
//! SPS/PPS repetition before every IDR picture of an encoder's output.

use crate::codec::{CodecError, check_dimensions};

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
}
/// Parses an SPS NAL unit (header byte included) up to its frame cropping.
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
        if bits.flag()? {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for list in 0..lists {
                if bits.flag()? {
                    skip_scaling_list(&mut bits, if list < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    bits.ue_max(12)?; // log2_max_frame_num_minus4
    match bits.ue_max(2)? {
        0 => {
            bits.ue_max(12)?; // log2_max_pic_order_cnt_lsb_minus4
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
    bits.ue_max(16)?; // max_num_ref_frames
    bits.flag()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = u64::from(bits.ue()?) + 1;
    let height_map_units = u64::from(bits.ue()?) + 1;
    let frame_mbs_only = bits.flag()?;
    if !frame_mbs_only {
        bits.flag()?; // mb_adaptive_frame_field_flag
    }
    bits.flag()?; // direct_8x8_inference_flag
    let field_factor = if frame_mbs_only { 1 } else { 2 };
    let coded_width = width_mbs * 16;
    let coded_height = height_map_units * 16 * field_factor;
    let (mut width, mut height) = (coded_width, coded_height);
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
    }
    let (Ok(coded_width), Ok(coded_height), Ok(width), Ok(height)) = (
        u32::try_from(coded_width),
        u32::try_from(coded_height),
        u32::try_from(width),
        u32::try_from(height),
    ) else {
        return Err(CodecError::Unsupported("picture is larger than 4096x2304"));
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
    })
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
        }
    }

    fn load(&mut self) -> Result<(), CodecError> {
        let mut byte = *self.data.get(self.pos).ok_or(TRUNCATED)?;
        self.pos += 1;
        if self.zeros >= 2 && byte == 3 {
            byte = *self.data.get(self.pos).ok_or(TRUNCATED)?;
            self.pos += 1;
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

    fn ue_max(&mut self, max: u32) -> Result<u32, CodecError> {
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
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Default)]
pub(crate) struct ParameterSets {
    sps: Vec<u8>,
    pps: Vec<u8>,
}

#[cfg_attr(not(windows), allow(dead_code))]
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

#[cfg_attr(not(windows), allow(dead_code))]
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
            }
        );
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
