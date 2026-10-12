//! Platform-neutral pixel conversion for screen capture: output sizing and the
//! CPU BGRA → NV12 path (BT.709, limited range) that backends without a GPU
//! converter fall back to.
//!
//! The scaler is an integer box filter: every output pixel averages the source
//! pixels it covers (at least one), and every source pixel is read once.
//! Upscaling repeats pixels. It exists for correctness on machines without a
//! usable GPU video processor, not as the preferred path.

use super::CaptureError;

/// Smallest output edge: hardware H.264 encoders refuse anything smaller.
pub const MIN_OUTPUT_EDGE: u32 = 16;

/// The size a source of `width` × `height` is encoded at inside
/// `max_width` × `max_height`: scaled down to fit with its aspect ratio kept,
/// never scaled up, rounded down to even edges (4:2:0) and never below
/// [`MIN_OUTPUT_EDGE`].
pub fn fit_output(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let (width, height) = (width.max(1), height.max(1));
    let (mut out_w, mut out_h) = (u64::from(width), u64::from(height));
    if width > max_width || height > max_height {
        // Compare width/max_width with height/max_height without division.
        if u64::from(width) * u64::from(max_height) >= u64::from(height) * u64::from(max_width) {
            out_w = u64::from(max_width);
            out_h = u64::from(height) * u64::from(max_width) / u64::from(width);
        } else {
            out_h = u64::from(max_height);
            out_w = u64::from(width) * u64::from(max_height) / u64::from(height);
        }
    }
    let even = |edge: u64| (edge.max(u64::from(MIN_OUTPUT_EDGE)) & !1) as u32;
    (even(out_w), even(out_h))
}

/// Bytes of an NV12 picture with `height` luma rows, each `stride` bytes: the
/// luma plane, then `height / 2` interleaved chroma rows at the same stride.
pub const fn nv12_len(stride: usize, height: usize) -> usize {
    stride * height + stride * (height / 2)
}

/// Fixed-point (16 fractional bits) BT.709 coefficients, limited range.
const fn fixed(value: f64) -> i32 {
    // Round to nearest, ties away from zero.
    if value >= 0.0 {
        (value * 65536.0 + 0.5) as i32
    } else {
        (value * 65536.0 - 0.5) as i32
    }
}

const KR: f64 = 0.2126;
const KB: f64 = 0.0722;
const KG: f64 = 1.0 - KR - KB;
/// 219/255 and 224/255: the limited-range scales of luma and chroma.
const Y_SCALE: f64 = 219.0 / 255.0;
const C_SCALE: f64 = 224.0 / 255.0;

const Y_R: i32 = fixed(KR * Y_SCALE);
const Y_G: i32 = fixed(KG * Y_SCALE);
const Y_B: i32 = fixed(KB * Y_SCALE);
const CB_R: i32 = fixed(-KR / (2.0 * (1.0 - KB)) * C_SCALE);
const CB_G: i32 = fixed(-KG / (2.0 * (1.0 - KB)) * C_SCALE);
const CB_B: i32 = fixed(0.5 * C_SCALE);
const CR_R: i32 = fixed(0.5 * C_SCALE);
const CR_G: i32 = fixed(-KG / (2.0 * (1.0 - KR)) * C_SCALE);
const CR_B: i32 = fixed(-KB / (2.0 * (1.0 - KR)) * C_SCALE);

const ROUND: i32 = 1 << 15;

fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((Y_R * r + Y_G * g + Y_B * b + ROUND) >> 16) + 16).clamp(16, 235) as u8
}

fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    let cb = ((CB_R * r + CB_G * g + CB_B * b + ROUND) >> 16) + 128;
    let cr = ((CR_R * r + CR_G * g + CR_B * b + ROUND) >> 16) + 128;
    (cb.clamp(16, 240) as u8, cr.clamp(16, 240) as u8)
}

/// A borrowed BGRA image (byte order B, G, R, A; alpha is ignored).
#[derive(Clone, Copy, Debug)]
pub struct Bgra<'a> {
    data: &'a [u8],
    stride: usize,
    width: u32,
    height: u32,
}

impl<'a> Bgra<'a> {
    /// `data` starts at the top-left visible pixel; rows are `stride` bytes
    /// apart and the last row need not be padded.
    pub fn new(
        data: &'a [u8],
        stride: usize,
        width: u32,
        height: u32,
    ) -> Result<Self, CaptureError> {
        if width == 0 || height == 0 {
            return Err(CaptureError::InvalidSample("the image has no pixels"));
        }
        let row = width as usize * 4;
        let needed = stride
            .checked_mul(height as usize - 1)
            .and_then(|rows| rows.checked_add(row))
            .ok_or(CaptureError::InvalidSample("BGRA image size overflows"))?;
        if stride < row || data.len() < needed {
            return Err(CaptureError::InvalidSample(
                "BGRA rows are shorter than the image",
            ));
        }
        Ok(Self {
            data,
            stride,
            width,
            height,
        })
    }

    fn row(&self, y: usize) -> &[u8] {
        let start = y * self.stride;
        &self.data[start..start + self.width as usize * 4]
    }
}

/// Reusable scratch for [`Converter::convert`]; sized by the widest source it
/// has seen, never shrunk.
#[derive(Default)]
pub struct Converter {
    /// Per-column sums of the source rows under one output row: B, G, R, A.
    columns: Vec<u32>,
    /// The averaged RGB of the two output rows being converted.
    averaged: Vec<[u8; 3]>,
}

impl Converter {
    /// Scales `source` to `width` × `height` (even edges) and writes NV12 into
    /// `out`: `stride`-byte rows, the chroma plane directly after the luma plane
    /// (`nv12_len`). Nothing outside those bytes is touched.
    pub fn convert(
        &mut self,
        source: &Bgra<'_>,
        width: u32,
        height: u32,
        stride: usize,
        out: &mut [u8],
    ) -> Result<(), CaptureError> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(CaptureError::InvalidSample(
                "NV12 output needs nonzero even dimensions",
            ));
        }
        let (dw, dh) = (width as usize, height as usize);
        if stride < dw || out.len() < nv12_len(stride, dh) {
            return Err(CaptureError::InvalidSample(
                "the NV12 buffer is smaller than its picture",
            ));
        }
        let (sw, sh) = (source.width as usize, source.height as usize);
        self.columns.clear();
        self.columns.resize(sw * 4, 0);
        self.averaged.clear();
        self.averaged.resize(dw * 2, [0; 3]);
        let (luma_plane, chroma_plane) = out.split_at_mut(stride * dh);
        for oy in 0..dh {
            // The source rows this output row covers: at least one.
            let y0 = oy * sh / dh;
            let y1 = ((oy + 1) * sh / dh).max(y0 + 1).min(sh);
            self.columns.fill(0);
            for y in y0..y1 {
                for (sum, &byte) in self.columns.iter_mut().zip(source.row(y)) {
                    *sum += u32::from(byte);
                }
            }
            let rows = (y1 - y0) as u32;
            let half = oy % 2;
            let averaged = &mut self.averaged[half * dw..(half + 1) * dw];
            for (ox, pixel) in averaged.iter_mut().enumerate() {
                let x0 = ox * sw / dw;
                let x1 = ((ox + 1) * sw / dw).max(x0 + 1).min(sw);
                let count = (x1 - x0) as u32 * rows;
                let (mut b, mut g, mut r) = (0u32, 0u32, 0u32);
                for column in self.columns[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                    b += column[0];
                    g += column[1];
                    r += column[2];
                }
                let mean = |sum: u32| ((sum + count / 2) / count) as u8;
                *pixel = [mean(r), mean(g), mean(b)];
            }
            let luma_row = &mut luma_plane[oy * stride..oy * stride + dw];
            for (target, pixel) in luma_row.iter_mut().zip(averaged.iter()) {
                *target = luma(pixel[0].into(), pixel[1].into(), pixel[2].into());
            }
            if half == 1 {
                // Chroma: the average RGB of each 2x2 block of output pixels.
                let chroma_row = &mut chroma_plane[(oy / 2) * stride..(oy / 2) * stride + dw];
                let (upper, lower) = self.averaged.split_at(dw);
                for (pair, (top, bottom)) in chroma_row.as_chunks_mut::<2>().0.iter_mut().zip(
                    upper
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .zip(lower.as_chunks::<2>().0.iter()),
                ) {
                    let mean = |channel: usize| {
                        (i32::from(top[0][channel])
                            + i32::from(top[1][channel])
                            + i32::from(bottom[0][channel])
                            + i32::from(bottom[1][channel])
                            + 2)
                            / 4
                    };
                    let (cb, cr) = chroma(mean(0), mean(1), mean(2));
                    pair[0] = cb;
                    pair[1] = cr;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_fits_the_baseline_without_upscaling_and_stays_even() {
        let fit = |w, h| fit_output(w, h, 1280, 720);
        assert_eq!(fit(1920, 1080), (1280, 720));
        assert_eq!(fit(2560, 1440), (1280, 720));
        assert_eq!(fit(3840, 2160), (1280, 720));
        // 16:10 is height-limited, an ultra-wide display width-limited.
        assert_eq!(fit(1920, 1200), (1152, 720));
        assert_eq!(fit(5120, 1440), (1280, 360));
        // Portrait windows keep their shape.
        assert_eq!(fit(600, 900), (480, 720));
        // Smaller sources are encoded as they are (rounded down to even).
        assert_eq!(fit(800, 600), (800, 600));
        assert_eq!(fit(801, 599), (800, 598));
        assert_eq!(fit(1366, 768), (1280, 718));
        // A sliver is padded up to the codec minimum.
        assert_eq!(fit(4, 3), (16, 16));
        assert_eq!(fit(0, 0), (16, 16));
        for (w, h) in [(1280, 720), (1281, 721), (7680, 4320), (333, 1777)] {
            let (ow, oh) = fit(w, h);
            assert!(
                ow <= 1280 && oh <= 720 && ow.is_multiple_of(2) && oh.is_multiple_of(2),
                "{w}x{h}"
            );
        }
    }

    fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.iter()
            .copied()
            .cycle()
            .take(width as usize * height as usize * 4)
            .collect()
    }

    fn convert(source: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
        let image = Bgra::new(source, sw as usize * 4, sw, sh).unwrap();
        let mut out = vec![0xEE; nv12_len(dw as usize, dh as usize)];
        Converter::default()
            .convert(&image, dw, dh, dw as usize, &mut out)
            .unwrap();
        out
    }

    #[test]
    fn primaries_neutrals_and_range_follow_bt709_limited() {
        // (B, G, R) -> expected (Y, Cb, Cr), from the BT.709 matrix scaled to
        // 219/224 levels and rounded.
        for ([b, g, r], expected) in [
            ([0, 0, 0], (16, 128, 128)),
            ([255, 255, 255], (235, 128, 128)),
            ([128, 128, 128], (126, 128, 128)),
            ([0, 0, 255], (63, 102, 240)),
            ([0, 255, 0], (173, 42, 26)),
            ([255, 0, 0], (32, 240, 118)),
        ] {
            let out = convert(&solid(4, 4, [b, g, r, 0xFF]), 4, 4, 4, 4);
            assert!(out[..16].iter().all(|&y| y == expected.0), "{b},{g},{r}");
            assert!(
                out[16..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .all(|c| (c[0], c[1]) == (expected.1, expected.2)),
                "{b},{g},{r}"
            );
        }
        // Every gray stays exactly neutral (no rounding drift in chroma).
        for v in 0..=255u8 {
            let out = convert(&solid(2, 2, [v, v, v, 255]), 2, 2, 2, 2);
            assert_eq!((out[4], out[5]), (128, 128), "gray {v}");
        }
    }

    #[test]
    fn scaling_averages_the_covered_pixels_and_alpha_is_ignored() {
        // A 4x4 source of alternating black/white columns, scaled to 2x2: every
        // output pixel covers one black and one white column per row.
        let mut source = Vec::new();
        for _ in 0..4 {
            for x in 0..4u8 {
                let v = if x % 2 == 0 { 0 } else { 255 };
                source.extend_from_slice(&[v, v, v, 7]);
            }
        }
        let out = convert(&source, 4, 4, 2, 2);
        // Mean of 0 and 255 is 128 (rounded), a mid gray in limited range.
        assert_eq!(out[..4], [126, 126, 126, 126]);
        assert_eq!(out[4..], [128, 128]);
    }

    #[test]
    fn non_integer_ratios_and_upscales_cover_every_pixel_without_panicking() {
        for (sw, sh, dw, dh) in [(3, 3, 2, 2), (5, 7, 4, 2), (2, 2, 16, 16), (1, 1, 2, 2)] {
            let out = convert(&solid(sw, sh, [10, 200, 90, 255]), sw, sh, dw, dh);
            let y = out[0];
            assert!(out[..(dw * dh) as usize].iter().all(|&v| v == y));
            assert!(
                out[(dw * dh) as usize..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .all(|c| c == &out[(dw * dh) as usize..][..2])
            );
        }
    }

    #[test]
    fn strides_and_cropped_views_are_honored() {
        // A 4x2 view into a wider padded buffer; the padding must not leak in.
        let mut padded = vec![0xAA; 16 * 2];
        for (row, bytes) in padded.as_chunks_mut::<16>().0.iter_mut().enumerate() {
            for pixel in bytes.as_chunks_mut::<4>().0 {
                pixel.copy_from_slice(&[0, 0, 255, 255]);
            }
            if row == 0 {
                bytes[12..16].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        // Only the first three pixels (red) are visible.
        let image = Bgra::new(&padded, 16, 3, 2).unwrap();
        let mut out = vec![0; nv12_len(8, 2)];
        let mut converter = Converter::default();
        // 3 wide cannot be encoded as is: scale to 2x2 with a stride of 8.
        converter.convert(&image, 2, 2, 8, &mut out).unwrap();
        assert_eq!(&out[..2], &[63, 63]);
        assert_eq!(&out[8..10], &[63, 63]);
        assert_eq!(&out[16..18], &[102, 240]);
        assert!(
            out[2..8].iter().all(|&byte| byte == 0),
            "outside the picture"
        );
    }

    #[test]
    fn invalid_inputs_are_errors_not_panics() {
        assert!(Bgra::new(&[0; 16], 4, 2, 2).is_err(), "stride below a row");
        assert!(Bgra::new(&[0; 15], 8, 2, 2).is_err(), "buffer too short");
        assert!(Bgra::new(&[], 8, 0, 2).is_err(), "no pixels");
        let source = solid(2, 2, [0; 4]);
        let image = Bgra::new(&source, 8, 2, 2).unwrap();
        let mut converter = Converter::default();
        let mut out = vec![0; 6];
        assert!(converter.convert(&image, 3, 2, 3, &mut out).is_err(), "odd");
        assert!(
            converter.convert(&image, 2, 2, 1, &mut out).is_err(),
            "stride"
        );
        assert!(
            converter.convert(&image, 2, 2, 2, &mut out[..5]).is_err(),
            "short"
        );
        converter.convert(&image, 2, 2, 2, &mut out).unwrap();
    }
}
