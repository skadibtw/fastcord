//! Deterministic H.264 and NV12 test vectors.
//!
//! The I_PCM stream carries raw samples instead of transform coefficients, so
//! any conforming decoder must reproduce [`ipcm_frame`] exactly: a bit-exact
//! decoder check that needs no reference encoder. `fixtures/video/
//! ipcm-128x96.h264` is the committed output of [`ipcm_stream`].

use crate::h264::{NAL_AUD, NAL_PPS, NAL_SEI, NAL_SPS, nal_unit_type};

/// MSB-first RBSP writer.
#[derive(Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    used: u8,
}

impl BitWriter {
    pub fn flag(&mut self, value: bool) {
        self.current = (self.current << 1) | u8::from(value);
        self.used += 1;
        if self.used == 8 {
            self.bytes.push(self.current);
            self.current = 0;
            self.used = 0;
        }
    }

    /// Writes the low `count` bits of `value`, most significant first.
    pub fn bits(&mut self, value: u64, count: u32) {
        for bit in (0..count).rev() {
            self.flag((value >> bit) & 1 == 1);
        }
    }

    /// Unsigned Exp-Golomb.
    pub fn ue(&mut self, value: u32) {
        let code = u64::from(value) + 1;
        let length = 64 - code.leading_zeros();
        self.bits(0, length - 1);
        self.bits(code, length);
    }

    /// Signed Exp-Golomb.
    pub fn se(&mut self, value: i32) {
        let value = i64::from(value);
        let code = if value > 0 { 2 * value - 1 } else { -2 * value };
        self.ue(u32::try_from(code).expect("se value fits"));
    }

    /// Zero bits up to the next byte boundary.
    pub fn align_zero(&mut self) {
        while self.used != 0 {
            self.flag(false);
        }
    }

    /// A byte at a byte boundary.
    pub fn byte(&mut self, value: u8) {
        assert_eq!(self.used, 0, "unaligned byte");
        self.bytes.push(value);
    }

    /// rbsp_trailing_bits: a one bit, then zero bits to the byte boundary.
    pub fn trailing(&mut self) {
        self.flag(true);
        self.align_zero();
    }

    pub fn finish(self) -> Vec<u8> {
        assert_eq!(self.used, 0, "RBSP not byte aligned");
        self.bytes
    }
}

/// Inserts emulation-prevention bytes into an RBSP.
pub fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64);
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

pub const IPCM_WIDTH: u32 = 128;
pub const IPCM_HEIGHT: u32 = 96;

/// The three pictures of the I_PCM stream, in decoding (and output) order.
struct Picture {
    idr: bool,
    frame_num: u64,
    idr_pic_id: u32,
    parameter_sets: bool,
}

const PICTURES: [Picture; 3] = [
    Picture {
        idr: true,
        frame_num: 0,
        idr_pic_id: 0,
        parameter_sets: true,
    },
    // A non-IDR I picture that only references nothing but must still decode.
    Picture {
        idr: false,
        frame_num: 1,
        idr_pic_id: 0,
        parameter_sets: false,
    },
    // An IDR that relies on the parameter sets of the first access unit.
    Picture {
        idr: true,
        frame_num: 0,
        idr_pic_id: 1,
        parameter_sets: false,
    },
];

// The expected pictures are checked by the platform decoder tests.
#[cfg_attr(not(windows), allow(dead_code))]
pub const IPCM_FRAMES: usize = PICTURES.len();

/// Sample of plane `plane` (0 = Y, 1 = Cb, 2 = Cr) at (`x`, `y`) in picture
/// `index`, always within 16..=239 (never zero, so no start-code emulation).
fn ipcm_sample(plane: usize, x: u32, y: u32, index: usize) -> u8 {
    let index = index as u32;
    let value = match plane {
        0 => (x * 2 + y * 3 + index * 37 + (x / 8 + y / 8) % 2 * 50) % 220,
        1 => (x * 5 + y + index * 53) % 224,
        _ => (x + y * 7 + index * 11) % 224,
    };
    16 + value as u8
}

/// The packed NV12 picture `index` that the I_PCM stream decodes to.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn ipcm_frame(index: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity((IPCM_WIDTH * IPCM_HEIGHT * 3 / 2) as usize);
    for y in 0..IPCM_HEIGHT {
        for x in 0..IPCM_WIDTH {
            out.push(ipcm_sample(0, x, y, index));
        }
    }
    for y in 0..IPCM_HEIGHT / 2 {
        for x in 0..IPCM_WIDTH / 2 {
            out.push(ipcm_sample(1, x, y, index));
            out.push(ipcm_sample(2, x, y, index));
        }
    }
    out
}

fn push_nal(stream: &mut Vec<u8>, header: u8, rbsp: &[u8]) {
    stream.extend_from_slice(&[0, 0, 0, 1, header]);
    stream.extend(escape(rbsp));
}

/// Constrained Baseline, level 3.0, 128x96, pic_order_cnt_type 2.
fn sps() -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.bits(66, 8); // profile_idc: Baseline
    bits.bits(0xC0, 8); // constraint_set0_flag, constraint_set1_flag
    bits.bits(30, 8); // level_idc
    bits.ue(0); // seq_parameter_set_id
    bits.ue(0); // log2_max_frame_num_minus4
    bits.ue(2); // pic_order_cnt_type: output order is decoding order
    bits.ue(1); // max_num_ref_frames
    bits.flag(false); // gaps_in_frame_num_value_allowed_flag
    bits.ue(IPCM_WIDTH / 16 - 1);
    bits.ue(IPCM_HEIGHT / 16 - 1);
    bits.flag(true); // frame_mbs_only_flag
    bits.flag(true); // direct_8x8_inference_flag
    bits.flag(false); // frame_cropping_flag
    bits.flag(false); // vui_parameters_present_flag
    bits.trailing();
    bits.finish()
}

fn pps() -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.ue(0); // pic_parameter_set_id
    bits.ue(0); // seq_parameter_set_id
    bits.flag(false); // entropy_coding_mode_flag: CAVLC
    bits.flag(false); // bottom_field_pic_order_in_frame_present_flag
    bits.ue(0); // num_slice_groups_minus1
    bits.ue(0); // num_ref_idx_l0_default_active_minus1
    bits.ue(0); // num_ref_idx_l1_default_active_minus1
    bits.flag(false); // weighted_pred_flag
    bits.bits(0, 2); // weighted_bipred_idc
    bits.se(0); // pic_init_qp_minus26
    bits.se(0); // pic_init_qs_minus26
    bits.se(0); // chroma_qp_index_offset
    bits.flag(true); // deblocking_filter_control_present_flag
    bits.flag(false); // constrained_intra_pred_flag
    bits.flag(false); // redundant_pic_cnt_present_flag
    bits.trailing();
    bits.finish()
}

/// One I slice covering the picture, every macroblock I_PCM.
fn slice(picture: &Picture, index: usize) -> Vec<u8> {
    let mut bits = BitWriter::default();
    bits.ue(0); // first_mb_in_slice
    bits.ue(7); // slice_type: I (all slices of the picture)
    bits.ue(0); // pic_parameter_set_id
    bits.bits(picture.frame_num, 4); // frame_num, log2_max_frame_num = 4
    if picture.idr {
        bits.ue(picture.idr_pic_id);
        bits.flag(false); // no_output_of_prior_pics_flag
        bits.flag(false); // long_term_reference_flag
    } else {
        bits.flag(false); // adaptive_ref_pic_marking_mode_flag
    }
    bits.se(0); // slice_qp_delta
    bits.ue(1); // disable_deblocking_filter_idc: off
    for mb_y in 0..IPCM_HEIGHT / 16 {
        for mb_x in 0..IPCM_WIDTH / 16 {
            bits.ue(25); // mb_type: I_PCM
            bits.align_zero(); // pcm_alignment_zero_bit
            for y in 0..16 {
                for x in 0..16 {
                    bits.byte(ipcm_sample(0, mb_x * 16 + x, mb_y * 16 + y, index));
                }
            }
            for plane in 1..=2 {
                for y in 0..8 {
                    for x in 0..8 {
                        bits.byte(ipcm_sample(plane, mb_x * 8 + x, mb_y * 8 + y, index));
                    }
                }
            }
        }
    }
    bits.trailing();
    bits.finish()
}

/// The I_PCM Annex B stream with four-byte start codes.
pub fn ipcm_stream() -> Vec<u8> {
    let mut stream = Vec::new();
    for (index, picture) in PICTURES.iter().enumerate() {
        if picture.parameter_sets {
            push_nal(&mut stream, 0x67, &sps());
            push_nal(&mut stream, 0x68, &pps());
        }
        let header = if picture.idr { 0x65 } else { 0x61 };
        push_nal(&mut stream, header, &slice(picture, index));
    }
    stream
}

/// Splits a stream with four-byte start codes and one slice per picture into
/// access units: a new one starts at a picture or a non-VCL prefix NAL unit
/// that follows a picture.
pub fn ipcm_access_units(stream: &[u8]) -> Vec<&[u8]> {
    let starts: Vec<usize> = stream
        .windows(4)
        .enumerate()
        .filter(|(_, window)| *window == [0, 0, 0, 1])
        .map(|(at, _)| at)
        .collect();
    let mut units = Vec::new();
    let mut unit_start = 0;
    let mut previous_vcl = false;
    for &at in &starts {
        let kind = nal_unit_type(&stream[at + 4..]);
        let vcl = (1..=5).contains(&kind);
        let prefix = matches!(kind, NAL_SEI | NAL_SPS | NAL_PPS | NAL_AUD);
        if previous_vcl && (vcl || prefix) {
            units.push(&stream[unit_start..at]);
            unit_start = at;
        }
        previous_vcl = vcl;
    }
    units.push(&stream[unit_start..]);
    units
}

/// A packed NV12 screen-like picture: gradients, sharp stripes, and a square
/// that moves with `index`.
pub fn moving_pattern(width: u32, height: u32, index: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((width * height * 3 / 2) as usize);
    let square = 48 + (index * 8) % width.saturating_sub(96).max(1);
    for y in 0..height {
        for x in 0..width {
            let inside = (square..square + 48).contains(&x) && (40..88).contains(&y);
            let stripes = y >= height - 32 && (x / 4) % 2 == 0;
            let value = if inside {
                220
            } else if stripes {
                40
            } else {
                32 + (x * 160 / width + y * 40 / height) as u8
            };
            out.push(value);
        }
    }
    for y in 0..height.div_ceil(2) {
        for x in 0..width.div_ceil(2) {
            let inside = (square / 2..(square + 48) / 2).contains(&x) && (20..44).contains(&y);
            out.push(if inside {
                90
            } else {
                128 + (x * 40 / width) as u8
            });
            out.push(if inside {
                200
            } else {
                128 - (y * 40 / height) as u8
            });
        }
    }
    out
}

/// Peak signal-to-noise ratio in dB between two equally long 8-bit planes.
pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let error: u64 = a
        .iter()
        .zip(b)
        .map(|(&a, &b)| u64::from(a.abs_diff(b)).pow(2))
        .sum();
    if error == 0 {
        return f64::INFINITY;
    }
    let mse = error as f64 / a.len() as f64;
    10.0 * (255.0 * 255.0 / mse).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_codes_match_the_standard_table() {
        let mut bits = BitWriter::default();
        bits.ue(0); // 1
        bits.ue(3); // 00100
        bits.se(-1); // ue(2) = 011
        bits.trailing(); // 1 + 4 zero bits
        assert_eq!(bits.finish(), [0b1001_0001, 0b1100_0000]);
    }

    #[test]
    fn escape_breaks_every_start_code_emulation() {
        assert_eq!(
            escape(&[0, 0, 0, 0, 1, 0, 0, 4]),
            [0, 0, 3, 0, 0, 3, 1, 0, 0, 4]
        );
    }

    #[test]
    fn moving_pattern_moves_and_psnr_measures_it() {
        let first = moving_pattern(320, 180, 0);
        let second = moving_pattern(320, 180, 1);
        assert_eq!(first.len(), 320 * 180 * 3 / 2);
        assert_ne!(first, second);
        assert_eq!(psnr(&first, &first), f64::INFINITY);
        assert!(psnr(&first, &second) < 30.0);
    }
}
