//! Offline contracts of the decoder's host parsing, record shaping, and DPB
//! bookkeeping. The live test at the bottom needs a VA-API DRM device with an
//! H.264 decoder; run with `cargo test -- --ignored` on Linux.

use super::super::{invalid_picture, picture};
use super::*;
use crate::codec::{CodecError, DecodedFrame};
use crate::test_vectors::{self, BitWriter, IPCM_FRAMES, escape};
use std::time::Duration;

#[test]
fn a_left_crop_starts_chroma_on_a_u_byte() {
    // Pitch 16, chroma after 4 luma rows; crop 2 left, 2 top (#128).
    let (luma, chroma) = display_window(0, 16, 64, 16, 2, 2);
    assert_eq!(luma, 2 * 16 + 2);
    assert_eq!(chroma, 64 + 16 + 2, "pair 1 starts at byte 2: U, not V");
}

/// The smallest SPS metadata the record/rejection tests build from directly.
fn bare_sps(profile_idc: u8, constraint_flags: u8) -> SpsMeta {
    SpsMeta {
        id: 0,
        profile_idc,
        constraint_flags,
        level_idc: 30,
        log2_max_frame_num_minus4: 0,
        pic_order_cnt_type: 2,
        log2_max_pic_order_cnt_lsb_minus4: 0,
        num_ref_frames: 1,
        direct_8x8_inference_flag: true,
        coded_width: 128,
        coded_height: 96,
        display_x: 0,
        display_y: 0,
        display_width: 128,
        display_height: 96,
    }
}

/// A Baseline-profile SPS NAL unit; every argument is one syntax element, and
/// what a test does not vary matches the fixture stream's SPS (rank 8x6
/// macroblocks, no crop).
fn sps(
    profile_idc: u8,
    constraint_flags: u8,
    id: u32,
    pic_order_cnt_type: u32,
    log2_max_pic_order_cnt_lsb_minus4: u32,
    num_ref_frames: u32,
    gaps_in_frame_num_value_allowed: bool,
    frame_mbs_only: bool,
    crop: Option<[u32; 4]>,
) -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.bits(u64::from(profile_idc), 8); // profile_idc
    bits.bits(u64::from(constraint_flags), 8); // constraint_set0..5 flags
    bits.bits(30, 8); // level_idc
    bits.ue(id); // seq_parameter_set_id
    bits.ue(0); // log2_max_frame_num_minus4
    bits.ue(pic_order_cnt_type);
    if pic_order_cnt_type == 0 {
        bits.ue(log2_max_pic_order_cnt_lsb_minus4);
    }
    if pic_order_cnt_type == 1 {
        bits.flag(false); // delta_pic_order_always_zero_flag
        bits.se(0); // offset_for_non_ref_pic
        bits.se(0); // offset_for_top_to_bottom_field
        bits.ue(0); // num_ref_frames_in_pic_order_cnt_cycle
    }
    bits.ue(num_ref_frames);
    bits.flag(gaps_in_frame_num_value_allowed);
    bits.ue(7); // pic_width_in_mbs_minus1: 128 coded pixels
    bits.ue(5); // pic_height_in_map_units_minus1: 96 coded pixels
    bits.flag(frame_mbs_only);
    if !frame_mbs_only {
        bits.flag(false); // mb_adaptive_frame_field_flag
    }
    bits.flag(true); // direct_8x8_inference_flag
    match crop {
        Some([left, right, top, bottom]) => {
            bits.flag(true);
            bits.ue(left);
            bits.ue(right);
            bits.ue(top);
            bits.ue(bottom);
        }
        None => bits.flag(false),
    }
    bits.flag(false); // vui_parameters_present_flag
    bits.trailing();
    nal(0x67, &bits.finish())
}

/// A PPS NAL unit, again with every knob a rejection test needs and the rest
/// matching the fixture stream's PPS (CAVLC, slice deblocking control).
fn pps(
    entropy_coding: bool,
    deblocking_control: bool,
    weighted_pred: bool,
    weighted_bipred: u32,
    slice_groups: u32,
    redundant: bool,
) -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.ue(0); // pic_parameter_set_id
    bits.ue(0); // seq_parameter_set_id
    bits.flag(entropy_coding);
    bits.flag(false); // bottom_field_pic_order_in_frame_present_flag
    bits.ue(slice_groups);
    bits.ue(0); // num_ref_idx_l0_default_active_minus1
    bits.ue(0); // num_ref_idx_l1_default_active_minus1
    bits.flag(weighted_pred);
    bits.bits(u64::from(weighted_bipred), 2); // weighted_bipred_idc
    bits.se(0); // pic_init_qp_minus26
    bits.se(0); // pic_init_qs_minus26
    bits.se(0); // chroma_qp_index_offset
    bits.flag(deblocking_control);
    bits.flag(false); // constrained_intra_pred_flag
    bits.flag(redundant);
    bits.trailing();
    nal(0x68, &bits.finish())
}

/// A slice header over a type 2 POC, one slice per picture, CAVLC. Slice data
/// is empty: the host stops reading at the header, and the driver would never
/// see these test units anyway.
#[derive(Default)]
struct PHeader {
    first_mb: u32,
    slice_type: u32,
    pps_id: u32,
    frame_num: u32,
    idr_pic_id: u32,
    /// `pic_order_cnt_lsb` (a type 0 SPS assumed when set).
    lsb: Option<u32>,
    override_refs: Option<u32>,
    ref_mod: bool,
    adaptive: bool,
    /// `mmco` operation codes for an adaptive reference marking walk.
    mmco: Vec<u32>,
    /// `Some` when the header carries `cabac_init_idc` (CABAC PPS assumed).
    cabac_init: Option<u32>,
    qp_delta: i32,
    deblocking_idc: u32,
    deblocking_offsets: (i32, i32),
}

impl PHeader {
    /// Encodes the header as a NAL unit led by `header_byte`.
    fn nal(&self, header_byte: u8) -> Vec<u8> {
        let idr = (header_byte & 0x1F) == crate::h264::NAL_IDR;
        let mut bits = BitWriter::default();
        bits.ue(self.first_mb);
        bits.ue(self.slice_type);
        bits.ue(self.pps_id);
        // IDR pictures carry zero `frame_num` bits ahead of `idr_pic_id`.
        bits.bits(u64::from(self.frame_num), 4);
        if idr {
            bits.ue(self.idr_pic_id);
        } else if let Some(lsb) = self.lsb {
            bits.bits(u64::from(lsb), 4);
        }
        if self.slice_type.is_multiple_of(5) {
            match self.override_refs {
                Some(minus1) => {
                    bits.flag(true);
                    bits.ue(minus1);
                }
                None => bits.flag(false),
            }
            bits.flag(self.ref_mod);
        }
        if (header_byte >> 5) & 3 != 0 {
            if idr {
                bits.flag(false); // no_output_of_prior_pics_flag
                bits.flag(false); // long_term_reference_flag
            } else {
                bits.flag(self.adaptive);
                if self.adaptive {
                    for &op in &self.mmco {
                        bits.ue(op);
                        match op {
                            1 | 2 | 4 | 6 => bits.ue(0),
                            3 => {
                                bits.ue(0);
                                bits.ue(0);
                            }
                            _ => {}
                        }
                    }
                    bits.ue(0);
                }
            }
        }
        if let Some(idc) = self.cabac_init {
            bits.ue(idc);
        }
        bits.se(self.qp_delta);
        bits.ue(self.deblocking_idc);
        if self.deblocking_idc != 1 {
            bits.se(self.deblocking_offsets.0);
            bits.se(self.deblocking_offsets.1);
        }
        bits.trailing();
        nal(header_byte, &bits.finish())
    }
}

/// A NAL unit led by `header_byte`, with its RBSP payload escaped.
fn nal(header_byte: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut unit = vec![header_byte];
    unit.extend(escape(rbsp));
    unit
}

/// The parameter caches with `sps` and `pps` remembered.
fn parameter_caches(sps: &[u8], pps: &[u8]) -> ParameterCaches {
    let mut parameters = ParameterCaches::default();
    parameters.remember_sps(sps).unwrap();
    parameters.remember_pps(pps);
    parameters
}

#[test]
fn the_fixture_parameter_sets_parse_to_their_metadata() {
    let stream = test_vectors::ipcm_stream();
    let nals: Vec<&[u8]> = crate::h264::nal_units(&stream).collect();
    let sps = parse_sps_meta(nals[0]).unwrap();
    assert_eq!(
        sps,
        SpsMeta {
            id: 0,
            profile_idc: 66,
            constraint_flags: 0xC0,
            level_idc: 30,
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 2,
            log2_max_pic_order_cnt_lsb_minus4: 0,
            num_ref_frames: 1,
            direct_8x8_inference_flag: true,
            coded_width: 128,
            coded_height: 96,
            display_x: 0,
            display_y: 0,
            display_width: 128,
            display_height: 96,
        }
    );
    let pps = parse_pps_meta(nals[1]).unwrap();
    assert_eq!(
        pps,
        PpsMeta {
            id: 0,
            sps_id: 0,
            entropy_coding_mode_flag: false,
            bottom_field_pic_order_in_frame_present_flag: false,
            num_ref_idx_l0_default_active_minus1: 0,
            pic_init_qp_minus26: 0,
            deblocking_filter_control_present_flag: true,
            chroma_qp_index_offset: 0,
            second_chroma_qp_index_offset: 0,
            constrained_intra_pred_flag: false,
            transform_8x8_mode_flag: false,
        }
    );
}

#[test]
fn profiles_map_to_the_closest_va_api_profile() {
    let constraint = matches!(
        bare_sps(66, 0xC0).profile(),
        Ok(VAProfile_VAProfileH264ConstrainedBaseline)
    );
    assert!(constraint);
    assert!(matches!(
        bare_sps(66, 0).profile(),
        Ok(VAProfile_VAProfileH264Baseline)
    ));
    assert!(matches!(
        bare_sps(77, 0).profile(),
        Ok(VAProfile_VAProfileH264Main)
    ));
    assert!(matches!(
        bare_sps(100, 0).profile(),
        Ok(VAProfile_VAProfileH264High)
    ));
    assert!(matches!(
        bare_sps(88, 0xC0).profile(),
        Err(CodecError::Unsupported(
            "no VA-API profile for this H.264 stream"
        ))
    ));
}

#[test]
fn crop_units_reach_the_display_window() {
    // One crop unit is two luma samples on every axis for 4:2:0 frame coding.
    let sps = parse_sps_meta(&sps(66, 0, 0, 2, 0, 1, false, true, Some([1, 0, 1, 0]))).unwrap();
    assert_eq!(
        (
            sps.display_x,
            sps.display_y,
            sps.display_width,
            sps.display_height
        ),
        (2, 2, 126, 94)
    );
    assert_eq!((sps.coded_width, sps.coded_height), (128, 96));
}

#[test]
fn beyond_policy_sps_units_are_rejected() {
    let gaps = sps(66, 0, 0, 2, 0, 1, true, true, None);
    assert_eq!(
        parse_sps_meta(&gaps),
        Err(CodecError::Unsupported(
            "frame_num gap recovery is not supported"
        ))
    );
    let field = sps(66, 0, 0, 2, 0, 1, false, false, None);
    assert_eq!(
        parse_sps_meta(&field),
        Err(CodecError::Unsupported(
            "field-coded streams are not supported"
        ))
    );
    let references = sps(66, 0, 0, 2, 0, DPB_CAP as u32 + 1, false, true, None);
    assert_eq!(
        parse_sps_meta(&references),
        Err(CodecError::Unsupported(
            "more than five reference pictures exceed this decoder's DPB"
        ))
    );
    let order_type_1 = sps(66, 0, 0, 1, 0, 1, false, true, None);
    assert_eq!(
        parse_sps_meta(&order_type_1),
        Err(CodecError::Unsupported(
            "picture-order-count type 1 streams are not supported"
        ))
    );
}

#[test]
fn beyond_policy_pps_units_are_rejected() {
    let weighted = pps(false, false, true, 0, 0, false);
    assert_eq!(
        parse_pps_meta(&weighted),
        Err(CodecError::Unsupported(
            "weighted prediction is not supported"
        ))
    );
    let bipred = pps(false, false, false, 1, 0, false);
    assert_eq!(
        parse_pps_meta(&bipred),
        Err(CodecError::Unsupported(
            "weighted biprediction is not supported"
        ))
    );
    let groups = pps(false, false, false, 0, 1, false);
    assert_eq!(
        parse_pps_meta(&groups),
        Err(CodecError::Unsupported("slice groups are not supported"))
    );
    let redundant = pps(false, false, false, 0, 0, true);
    assert_eq!(
        parse_pps_meta(&redundant),
        Err(CodecError::Unsupported(
            "redundant pictures are not supported"
        ))
    );
}

#[test]
fn the_fixture_idr_slice_header_parses() {
    let stream = test_vectors::ipcm_stream();
    let nals: Vec<&[u8]> = crate::h264::nal_units(&stream).collect();
    let parameters = parameter_caches(nals[0], nals[1]);
    let (sps, pps, header) =
        parse_slice_header(nals[2], |pps_id| parameters.resolve(pps_id)).unwrap();
    assert_eq!(sps, parse_sps_meta(nals[0]).unwrap());
    assert_eq!(pps, parse_pps_meta(nals[1]).unwrap());
    assert_eq!(header.first_mb_in_slice, 0);
    assert_eq!(header.slice_type, 7); // slice_type I (whole picture)
    assert!(header.idr);
    assert_eq!(header.frame_num, 0);
    assert_eq!(header.pic_order_cnt_lsb, None); // pic_order_cnt_type 2
    assert_eq!(header.num_ref_idx_l0_active_minus1, 0);
    assert_eq!(header.cabac_init_idc, 0); // CAVLC
    assert_eq!(header.slice_qp_delta, 0);
    assert_eq!(header.deblocking_filter, (1, 0, 0)); // the filter is disabled
    // first_mb(1) + slice_type(7) + pps(1) + frame_num(4) + idr_pic_id(1)
    // + marking flags(2) + qp delta(1) + deblocking idc(3) = 20 header bits.
    assert_eq!(header.header_bits, 20);
}

#[test]
fn a_plain_p_slice_header_parses() {
    let sps = sps(66, 0, 0, 2, 0, 1, false, true, None);
    let pps = pps(false, true, false, 0, 0, false);
    let parameters = parameter_caches(&sps, &pps);
    let expected_sps = parse_sps_meta(&sps).unwrap();
    let unit = PHeader {
        frame_num: 7,
        override_refs: Some(1),
        adaptive: true,
        mmco: vec![1, 5],
        deblocking_offsets: (0, -2),
        ..PHeader::default()
    }
    .nal(0x41);
    let (parsed_sps, _, header) =
        parse_slice_header(&unit, |pps_id| parameters.resolve(pps_id)).unwrap();
    assert_eq!(parsed_sps, expected_sps);
    assert!(!header.idr);
    assert_eq!(header.frame_num, 7);
    assert_eq!(header.num_ref_idx_l0_active_minus1, 1);
    assert_eq!(header.deblocking_filter, (0, 0, -2));
}

#[test]
fn beyond_policy_slice_headers_are_rejected() {
    let sps = sps(66, 0, 0, 2, 0, 1, false, true, None);
    let pps = pps(false, true, false, 0, 0, false);
    let parameters = parameter_caches(&sps, &pps);
    let resolve = |pps_id| parameters.resolve(pps_id);

    // B, SP and SI slices never appear inside the contract.
    for slice_type in [1, 3, 4] {
        let unit = PHeader {
            slice_type,
            ..PHeader::default()
        }
        .nal(0x41);
        assert_eq!(
            parse_slice_header(&unit, resolve),
            Err(CodecError::Unsupported(
                "B, SP and SI slices are not supported"
            ))
        );
    }
    // An unreferenced IDR picture is malformed bitstream, not policy.
    let unit = PHeader::default().nal(0x05);
    assert_eq!(
        parse_slice_header(&unit, resolve),
        Err(CodecError::InvalidBitstream(
            "an IDR picture must be referenced"
        ))
    );
    // A macroblock address outside the picture is a framing error.
    let unit = PHeader {
        first_mb: 48,
        ..PHeader::default()
    }
    .nal(0x41);
    assert_eq!(
        parse_slice_header(&unit, resolve),
        Err(CodecError::InvalidBitstream(
            "a slice macroblock address does not fit the picture"
        ))
    );
    // A slice must reach parameter sets the stream already carried.
    let unit = PHeader {
        pps_id: 9,
        ..PHeader::default()
    }
    .nal(0x41);
    assert_eq!(
        parse_slice_header(&unit, resolve),
        Err(CodecError::InvalidBitstream(
            "a slice references an unknown parameter set"
        ))
    );
    // Reference lists longer than the DPB are out of policy.
    let unit = PHeader {
        override_refs: Some(DPB_CAP as u32),
        ..PHeader::default()
    }
    .nal(0x41);
    assert_eq!(
        parse_slice_header(&unit, resolve),
        Err(CodecError::Unsupported(
            "reference lists longer than five pictures exceed this decoder's DPB"
        ))
    );
    // Reference picture list modification is out of policy.
    let unit = PHeader {
        ref_mod: true,
        ..PHeader::default()
    }
    .nal(0x41);
    assert_eq!(
        parse_slice_header(&unit, resolve),
        Err(CodecError::Unsupported(
            "reference picture list modification is not supported"
        ))
    );
    // CABAC initialization beyond the spec range is a bitstream error.
    let cabac_pps = self::pps(true, true, false, 0, 0, false);
    let cabac_parameters = parameter_caches(&sps, &cabac_pps);
    let unit = PHeader {
        cabac_init: Some(3),
        ..PHeader::default()
    }
    .nal(0x41);
    assert_eq!(
        parse_slice_header(&unit, |pps_id| cabac_parameters.resolve(pps_id)),
        Err(CodecError::InvalidBitstream("invalid cabac_init_idc"))
    );
    // A resulting QP must stay inside 0..=51.
    let unit = PHeader {
        qp_delta: 26,
        ..PHeader::default()
    }
    .nal(0x41);
    assert_eq!(
        parse_slice_header(&unit, resolve),
        Err(CodecError::InvalidBitstream("a slice QP is out of range"))
    );
    // Deblocking control values and offsets are range checked.
    let unit = PHeader {
        deblocking_idc: 3,
        ..PHeader::default()
    }
    .nal(0x41);
    assert_eq!(
        parse_slice_header(&unit, resolve),
        Err(CodecError::InvalidBitstream(
            "invalid disable_deblocking_filter_idc"
        ))
    );
    let unit = PHeader {
        deblocking_offsets: (7, 0),
        ..PHeader::default()
    }
    .nal(0x41);
    assert_eq!(
        parse_slice_header(&unit, resolve),
        Err(CodecError::InvalidBitstream(
            "a deblocking offset is out of range"
        ))
    );
    // Memory management operations must stay inside the loop bound.
    let unit = PHeader {
        adaptive: true,
        mmco: vec![1; MAX_MMCO_OPS + 1],
        ..PHeader::default()
    }
    .nal(0x41);
    assert_eq!(
        parse_slice_header(&unit, resolve),
        Err(CodecError::InvalidBitstream(
            "too many memory management control operations"
        ))
    );
    let unit = PHeader {
        adaptive: true,
        mmco: vec![7],
        ..PHeader::default()
    }
    .nal(0x41);
    assert_eq!(
        parse_slice_header(&unit, resolve),
        Err(CodecError::InvalidBitstream(
            "invalid memory management control operation"
        ))
    );
}

#[test]
fn poc_counts_wrap_around_the_lsb_range() {
    // log2_max_pic_order_cnt_lsb = 4, so the lsb range is 16.
    let sps = parse_sps_meta(&sps(66, 0, 0, 0, 0, 1, false, true, None)).unwrap();
    let mut poc = Poc::default();
    assert_eq!(poc.count(&sps, None, 0, true, true), 0);
    assert_eq!(poc.count(&sps, Some(14), 1, true, false), -2);
    assert_eq!(poc.count(&sps, Some(2), 2, true, false), 2);
    // A reset restarts the numbering at a splice or a fresh setup.
    poc.reset();
    assert_eq!(poc.count(&sps, None, 0, true, true), 0);
}

#[test]
fn poc_type_2_counts_decoding_order() {
    let sps = parse_sps_meta(&sps(66, 0, 0, 2, 0, 1, false, true, None)).unwrap();
    let mut poc = Poc::default();
    assert_eq!(poc.count(&sps, None, 0, true, true), 0);
    assert_eq!(poc.count(&sps, None, 1, true, false), 2);
    // Non-reference pictures keep their decoding-order slot.
    assert_eq!(poc.count(&sps, None, 2, false, false), 2);
    assert_eq!(poc.count(&sps, None, 3, true, false), 6);
}

#[test]
fn the_dpb_ring_keeps_references_free_and_ordered() {
    let mut dpb = Dpb::default();
    assert_eq!(dpb.free_slot(), Some(0));
    // Five references on six surfaces leave exactly one spare slot.
    for slot in 0..DPB_CAP {
        let frame_idx = dpb.next_frame_idx();
        dpb.push(
            slot,
            picture(
                1_000 + slot as u32,
                frame_idx,
                VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                0,
                0,
            ),
        );
    }
    assert_eq!(dpb.free_slot(), Some(DPB_CAP));
    // Marking a sixth picture evicts the oldest reference first.
    let frame_idx = dpb.next_frame_idx();
    dpb.push(
        DPB_CAP,
        picture(1_005, frame_idx, VA_PICTURE_H264_SHORT_TERM_REFERENCE, 0, 0),
    );
    assert_eq!(dpb.free_slot(), Some(0));
    // The reference list is newest first, and exactly DPB_CAP long.
    let references = dpb.references();
    assert_eq!(references.len(), DPB_CAP);
    assert_eq!(references[0].picture_id, 1_005);
    assert_eq!(references[1].picture_id, 1_004);
    let list = dpb.list();
    assert_eq!(list[0].picture_id, 1_005);
    assert_eq!(list[DPB_CAP].picture_id, VA_INVALID_SURFACE as VASurfaceID);
    assert_eq!(dpb.next_frame_idx(), DPB_CAP as u32 + 1);
    assert_eq!(dpb.pictures, DPB_CAP as u32 + 2);
    // Clearing (an IDR started a new sequence) frees the whole ring.
    dpb.clear();
    assert_eq!(dpb.free_slot(), Some(0));
}

/// A PPS with the fields #122 found dropped, and optionally the extension.
fn pps_with(
    chroma_offset: i32,
    constrained_intra: bool,
    extension: Option<(bool, bool, i32)>,
) -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.ue(0);
    bits.ue(0);
    bits.flag(true); // CABAC
    bits.flag(false);
    bits.ue(0);
    bits.ue(0);
    bits.ue(0);
    bits.flag(false);
    bits.bits(0, 2);
    bits.se(0);
    bits.se(0);
    bits.se(chroma_offset);
    bits.flag(true);
    bits.flag(constrained_intra);
    bits.flag(false);
    if let Some((transform_8x8, scaling, second)) = extension {
        bits.flag(transform_8x8);
        bits.flag(scaling);
        bits.se(second);
    }
    bits.trailing();
    nal(0x68, &bits.finish())
}

#[test]
fn pps_chroma_offsets_intra_and_8x8_transform_reach_the_driver_record() {
    let sps = parse_sps_meta(&sps(77, 0, 0, 2, 0, 3, false, true, None)).unwrap();
    // No extension: the second offset follows the first.
    let plain = parse_pps_meta(&pps_with(-2, true, None)).unwrap();
    assert_eq!(
        (
            plain.chroma_qp_index_offset,
            plain.second_chroma_qp_index_offset
        ),
        (-2, -2)
    );
    assert!(plain.constrained_intra_pred_flag && !plain.transform_8x8_mode_flag);
    let high = parse_pps_meta(&pps_with(-2, false, Some((true, false, 3)))).unwrap();
    let current = picture(9, 41, VA_PICTURE_H264_SHORT_TERM_REFERENCE, 12, 12);
    let record = picture_record(&sps, &high, current, &[], 41, true);
    assert_eq!(record.chroma_qp_index_offset, -2);
    assert_eq!(record.second_chroma_qp_index_offset, 3);
    // SAFETY: a fresh union record; the bits constructor set these fields.
    let pic = unsafe { record.pic_fields.bits };
    assert_eq!(pic.transform_8x8_mode_flag(), 1);
    assert_eq!(pic.constrained_intra_pred_flag(), 0);
    let record = picture_record(&sps, &plain, current, &[], 41, true);
    // SAFETY: as above.
    let pic = unsafe { record.pic_fields.bits };
    assert_eq!(pic.constrained_intra_pred_flag(), 1);
    // Custom matrices cannot be expressed: refused, so OpenH264 takes over.
    assert!(matches!(
        parse_pps_meta(&pps_with(0, false, Some((true, true, 0)))),
        Err(CodecError::Unsupported(_))
    ));
    assert_eq!(parse_pps_meta(&pps_with(13, false, None)), Err(MALFORMED));
}

#[test]
fn picture_records_mirror_the_sequence() {
    let sps = parse_sps_meta(&sps(66, 0, 0, 2, 0, 3, false, true, None)).unwrap();
    let pps = parse_pps_meta(&pps(true, true, false, 0, 0, false)).unwrap();
    let current = picture(9, 41, VA_PICTURE_H264_SHORT_TERM_REFERENCE, 12, 12);
    let references = vec![picture(7, 40, VA_PICTURE_H264_SHORT_TERM_REFERENCE, 10, 10)];
    let record = picture_record(&sps, &pps, current, &references, 41, false);
    assert_eq!(record.CurrPic.picture_id, 9);
    assert_eq!(record.CurrPic.frame_idx, 41);
    assert_eq!(record.CurrPic.flags, VA_PICTURE_H264_SHORT_TERM_REFERENCE);
    assert_eq!(record.CurrPic.TopFieldOrderCnt, 12);
    assert_eq!(record.CurrPic.BottomFieldOrderCnt, 12);
    assert_eq!(record.ReferenceFrames[0].picture_id, 7);
    assert!(
        record.ReferenceFrames[1..]
            .iter()
            .all(|slot| slot.picture_id == VA_INVALID_SURFACE as VASurfaceID)
    );
    assert_eq!(record.picture_width_in_mbs_minus1, 7);
    assert_eq!(record.picture_height_in_mbs_minus1, 5);
    assert_eq!(record.num_ref_frames, 3);
    assert_eq!(record.pic_init_qp_minus26, 0);
    // SAFETY: a fresh union record is being read; the bits constructor set
    // exactly the fields the checks read.
    let seq = unsafe { record.seq_fields.bits };
    assert_eq!(seq.chroma_format_idc(), 1);
    assert_eq!(seq.gaps_in_frame_num_value_allowed_flag(), 0);
    assert_eq!(seq.frame_mbs_only_flag(), 1);
    assert_eq!(seq.mb_adaptive_frame_field_flag(), 0);
    assert_eq!(seq.direct_8x8_inference_flag(), 1);
    assert_eq!(seq.log2_max_frame_num_minus4(), 0);
    assert_eq!(seq.pic_order_cnt_type(), 2);
    assert_eq!(seq.delta_pic_order_always_zero_flag(), 0);
    // SAFETY: as above, for the picture flags.
    let pic = unsafe { record.pic_fields.bits };
    assert_eq!(pic.entropy_coding_mode_flag(), 1);
    assert_eq!(pic.weighted_pred_flag(), 0);
    assert_eq!(pic.weighted_bipred_idc(), 0);
    assert_eq!(pic.transform_8x8_mode_flag(), 0);
    assert_eq!(pic.field_pic_flag(), 0);
    assert_eq!(pic.deblocking_filter_control_present_flag(), 1);
    assert_eq!(pic.redundant_pic_cnt_present_flag(), 0);
    assert_eq!(pic.reference_pic_flag(), 0);
    // The reference flag follows the caller.
    let record = picture_record(&sps, &pps, current, &[], 41, true);
    // SAFETY: as above, for the picture flags.
    let pic = unsafe { record.pic_fields.bits };
    assert_eq!(pic.reference_pic_flag(), 1);
    assert_eq!(record.frame_num, 41);
}

#[test]
fn slice_records_carry_the_parsed_header() {
    let stream = test_vectors::ipcm_stream();
    let nals: Vec<&[u8]> = crate::h264::nal_units(&stream).collect();
    let parameters = parameter_caches(nals[0], nals[1]);
    let (_, _, header) = parse_slice_header(nals[2], |pps_id| parameters.resolve(pps_id)).unwrap();
    let mut list0 = std::array::from_fn(|_| invalid_picture());
    list0[0] = picture(11, 3, VA_PICTURE_H264_SHORT_TERM_REFERENCE, 8, 8);
    let record = slice_record(37, &header, &list0);
    assert_eq!(record.slice_data_size, 37);
    assert_eq!(record.slice_data_offset, SLICE_DATA_NAL_OFFSET);
    assert_eq!(record.slice_data_flag, VA_SLICE_DATA_FLAG_ALL);
    assert_eq!(
        record.slice_data_bit_offset,
        u8::BITS as u16 + header.header_bits as u16
    );
    assert_eq!(record.first_mb_in_slice, header.first_mb_in_slice as u16);
    assert_eq!(record.slice_type, header.slice_type);
    assert_eq!(
        record.num_ref_idx_l0_active_minus1,
        header.num_ref_idx_l0_active_minus1
    );
    assert_eq!(record.cabac_init_idc, 0);
    assert_eq!(record.RefPicList0[0].picture_id, 11);
    assert_eq!(
        record.RefPicList0[DPB_CAP].picture_id,
        VA_INVALID_SURFACE as VASurfaceID
    );
    assert_eq!(
        record.RefPicList1[0].picture_id,
        VA_INVALID_SURFACE as VASurfaceID
    );
}

#[test]
fn iq_matrix_defaults_are_flat() {
    let matrix = iq_matrix();
    assert!(
        matrix
            .ScalingList4x4
            .iter()
            .flatten()
            .all(|&value| value == 16)
    );
    assert!(
        matrix
            .ScalingList8x8
            .iter()
            .flatten()
            .all(|&value| value == 16)
    );
}

#[test]
fn slice_heads_of_one_picture_must_agree() {
    let head = SliceHeader {
        first_mb_in_slice: 0,
        slice_type: 7,
        idr: true,
        frame_num: 0,
        pic_order_cnt_lsb: None,
        num_ref_idx_l0_active_minus1: 0,
        cabac_init_idc: 0,
        slice_qp_delta: 0,
        deblocking_filter: (1, 0, 0),
        header_bits: 20,
    };
    assert!(head.consistent(&head).is_ok());
    let other = SliceHeader {
        frame_num: 1,
        ..head
    };
    assert_eq!(
        head.consistent(&other),
        Err(CodecError::InvalidBitstream(
            "the slices of one picture disagree"
        ))
    );
    let other = SliceHeader {
        pic_order_cnt_lsb: Some(1),
        ..head
    };
    assert!(head.consistent(&other).is_err());
    let other = SliceHeader { idr: false, ..head };
    assert!(head.consistent(&other).is_err());
}

#[test]
fn nal_headers_mark_the_reference_state() {
    assert!(is_reference(&[0x65])); // IDR slice, nal_ref_idc 2
    assert!(is_reference(&[0x41])); // non-IDR slice, nal_ref_idc 1
    assert!(!is_reference(&[0x01])); // non-reference slice
    assert!(!is_reference(&[0x17])); // nal_ref_idc 0, other type
}

#[test]
fn the_head_slice_nal_decides_the_reference_flag() {
    let sps = sps(66, 0, 0, 2, 0, 1, false, true, None);
    let pps = pps(false, true, false, 0, 0, false);
    let parameters = parameter_caches(&sps, &pps);
    // The same parsed header means different DPB states depending on the NAL
    // header byte, which is what `is_reference` reads.
    let referenced = PHeader {
        frame_num: 3,
        ..PHeader::default()
    }
    .nal(0x41);
    let unreferenced = PHeader {
        frame_num: 3,
        ..PHeader::default()
    }
    .nal(0x01);
    let (_, _, head) = parse_slice_header(&referenced, |id| parameters.resolve(id)).unwrap();
    let (_, _, same) = parse_slice_header(&unreferenced, |id| parameters.resolve(id)).unwrap();
    assert_eq!(head.frame_num, same.frame_num);
    assert!(is_reference(&referenced));
    assert!(!is_reference(&unreferenced));
}

/// The decoded picture must be the exact NV12 pixels the stream carries, row
/// by row, on both planes.
fn assert_matches_reference(frame: DecodedFrame<'_>, reference: &[u8]) {
    assert_eq!((frame.image.width(), frame.image.height()), (128, 96));
    let width = frame.image.width() as usize;
    for row in 0..frame.image.height() as usize {
        let expected = &reference[row * width..row * width + width];
        assert_eq!(frame.image.luma_row(row), expected, "y row {row}");
    }
    for row in 0..frame.image.chroma_rows() {
        let at = width * frame.image.height() as usize + row * width;
        let expected = &reference[at..at + width];
        assert_eq!(frame.image.chroma_row(row), expected, "uv row {row}");
    }
}

#[test]
#[ignore = "needs a VA-API DRM device with an H.264 decoder and a live \
            driver; run with `cargo test -- --ignored` on Linux"]
fn live_decoder_matches_the_bit_exact_ipcm_fixture() {
    let candidates = super::super::decoder_candidates().unwrap();
    let source = candidates
        .into_iter()
        .map(|candidate| candidate.source)
        .find(|source| source.decoder)
        .expect("renderer needs a VA-API device with an H.264 decoder");
    let mut decoder = source.open_decoder().unwrap();

    let stream = test_vectors::ipcm_stream();
    let mut decoded = 0;
    for (index, unit) in test_vectors::ipcm_access_units(&stream)
        .into_iter()
        .enumerate()
    {
        let reference = test_vectors::ipcm_frame(index);
        decoder
            .decode(
                unit,
                Duration::from_millis(index as u64 * 33),
                &mut |frame: DecodedFrame<'_>| {
                    assert_matches_reference(frame, &reference);
                    decoded += 1;
                },
            )
            .unwrap();
    }
    // Synchronous decode means a flush has nothing left to hand out.
    decoder.flush(&mut |_: DecodedFrame<'_>| {}).unwrap();
    assert_eq!(decoded, IPCM_FRAMES);
}
