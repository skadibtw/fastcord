//! NV12 background blur and replacement (issue #85).
//!
//! Everything here is plain integer arithmetic on borrowed pictures and
//! reusable scratch buffers: no allocation after the picture size is first
//! seen, and nothing but the foreground mask depends on the model. The
//! background is always a *small* picture (about 320 pixels wide): either the
//! camera frame box-averaged and blurred, or a bundled preset. The final
//! picture is the camera frame where the mask says "person" and the bilinearly
//! enlarged small picture elsewhere, so the blur costs a few hundred thousand
//! operations a frame rather than a full-resolution convolution.

use super::MASK_SIZE;

/// Width the background is reduced to, give or take the integer factor.
const SMALL_WIDTH: usize = 320;
/// Box-blur half-width on the small picture, in pixels, and the number of
/// passes (two passes approximate a Gaussian of sigma about 4.5 small pixels).
const BLUR_RADIUS: usize = 4;
const BLUR_PASSES: usize = 2;

/// A borrowed NV12 picture.
#[derive(Clone, Copy)]
pub struct Planes<'a> {
    pub width: usize,
    pub height: usize,
    pub y: &'a [u8],
    pub y_stride: usize,
    pub uv: &'a [u8],
    pub uv_stride: usize,
}

impl Planes<'_> {
    fn chroma_width(&self) -> usize {
        self.width.div_ceil(2)
    }

    fn chroma_height(&self) -> usize {
        self.height.div_ceil(2)
    }
}

/// A small NV12 picture, tightly packed: the blurred background or a preset.
#[derive(Clone, Debug, Default)]
pub struct SmallImage {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub chroma_width: usize,
    pub chroma_height: usize,
    /// Interleaved U/V, `chroma_width * 2` bytes per row.
    pub uv: Vec<u8>,
}

impl SmallImage {
    /// A tightly packed NV12 picture of even `width` × `height`.
    pub fn from_nv12(width: usize, height: usize, bytes: &[u8]) -> Option<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return None;
        }
        let luma = width * height;
        if bytes.len() != luma + luma / 2 {
            return None;
        }
        Some(Self {
            width,
            height,
            y: bytes[..luma].to_vec(),
            chroma_width: width / 2,
            chroma_height: height / 2,
            uv: bytes[luma..].to_vec(),
        })
    }

    fn resize(&mut self, width: usize, height: usize, chroma_width: usize, chroma_height: usize) {
        self.width = width;
        self.height = height;
        self.chroma_width = chroma_width;
        self.chroma_height = chroma_height;
        self.y.resize(width * height, 0);
        self.uv.resize(chroma_width * chroma_height * 2, 0);
    }
}

/// Reusable buffers for [`blur_background`].
#[derive(Default)]
pub struct BlurScratch {
    line: Vec<u8>,
    plane: Vec<u8>,
}

/// Box-averages `src` down to a small picture and blurs it.
pub fn blur_background(src: &Planes<'_>, small: &mut SmallImage, scratch: &mut BlurScratch) {
    let factor = src.width.div_ceil(SMALL_WIDTH).max(1);
    let chroma_width = src.chroma_width().div_ceil(factor);
    let chroma_height = src.chroma_height().div_ceil(factor);
    small.resize(
        src.width.div_ceil(factor),
        src.height.div_ceil(factor),
        chroma_width,
        chroma_height,
    );
    reduce::<1>(
        src.y,
        src.y_stride,
        src.width,
        src.height,
        factor,
        &mut small.y,
    );
    reduce::<2>(
        src.uv,
        src.uv_stride,
        src.chroma_width(),
        src.chroma_height(),
        factor,
        &mut small.uv,
    );
    for _ in 0..BLUR_PASSES {
        box_blur::<1>(&mut small.y, small.width, small.height, scratch);
        box_blur::<2>(&mut small.uv, chroma_width, chroma_height, scratch);
    }
}

/// Averages `factor` × `factor` blocks of a `C`-channel plane.
fn reduce<const C: usize>(
    src: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    factor: usize,
    out: &mut [u8],
) {
    let out_width = width.div_ceil(factor);
    let out_height = height.div_ceil(factor);
    for oy in 0..out_height {
        let rows = (oy * factor)..((oy + 1) * factor).min(height);
        for ox in 0..out_width {
            let columns = (ox * factor)..((ox + 1) * factor).min(width);
            let count = (rows.len() * columns.len()) as u32;
            let mut sums = [0u32; C];
            for row in rows.clone() {
                let line = &src[row * stride..];
                for column in columns.clone() {
                    for (channel, sum) in sums.iter_mut().enumerate() {
                        *sum += u32::from(line[column * C + channel]);
                    }
                }
            }
            for (channel, sum) in sums.iter().enumerate() {
                out[(oy * out_width + ox) * C + channel] = ((sum + count / 2) / count) as u8;
            }
        }
    }
}

/// One horizontal and one vertical box pass with replicated edges.
fn box_blur<const C: usize>(
    plane: &mut [u8],
    width: usize,
    height: usize,
    scratch: &mut BlurScratch,
) {
    let window = (2 * BLUR_RADIUS + 1) as u32;
    // Horizontal, in place, one row at a time.
    scratch.line.resize(width * C, 0);
    for row in 0..height {
        let line = &mut plane[row * width * C..(row + 1) * width * C];
        scratch.line.copy_from_slice(line);
        blur_line::<C>(&scratch.line, line, width, window);
    }
    // Vertical: gather a column, blur it, scatter it back.
    scratch.plane.resize(height * C, 0);
    scratch.line.resize(height * C, 0);
    for column in 0..width {
        for row in 0..height {
            for channel in 0..C {
                scratch.plane[row * C + channel] = plane[(row * width + column) * C + channel];
            }
        }
        blur_line::<C>(&scratch.plane, &mut scratch.line, height, window);
        for row in 0..height {
            for channel in 0..C {
                plane[(row * width + column) * C + channel] = scratch.line[row * C + channel];
            }
        }
    }
}

/// Blurs `count` pixels of `C` interleaved channels with a sliding window,
/// replicating the first and last pixel beyond the ends.
fn blur_line<const C: usize>(src: &[u8], dst: &mut [u8], count: usize, window: u32) {
    let at = |index: isize, channel: usize| -> u32 {
        u32::from(src[index.clamp(0, count as isize - 1) as usize * C + channel])
    };
    for channel in 0..C {
        let mut sum: u32 = (-(BLUR_RADIUS as isize)..=BLUR_RADIUS as isize)
            .map(|offset| at(offset, channel))
            .sum();
        for index in 0..count {
            dst[index * C + channel] = ((sum + window / 2) / window) as u8;
            sum += at(index as isize + BLUR_RADIUS as isize + 1, channel);
            sum -= at(index as isize - BLUR_RADIUS as isize, channel);
        }
    }
}

/// Bilinear sampling positions for one axis, in 1/256ths.
#[derive(Default)]
struct Axis {
    first: Vec<u32>,
    second: Vec<u32>,
    weight: Vec<u32>,
}

impl Axis {
    fn new(source: usize, target: usize) -> Self {
        let mut axis = Self::default();
        for index in 0..target {
            let position = ((index as f32 + 0.5) * source as f32 / target as f32 - 0.5).max(0.0);
            let first = (position as usize).min(source - 1);
            axis.first.push(first as u32);
            axis.second.push((first + 1).min(source - 1) as u32);
            axis.weight
                .push(((position - first as f32) * 256.0).round() as u32);
        }
        axis
    }
}

/// Everything that depends only on the picture and background sizes.
#[derive(Default)]
struct Geometry {
    key: [usize; 6],
    mask_x: Axis,
    mask_y: Axis,
    mask_cx: Axis,
    mask_cy: Axis,
    bg_x: Axis,
    bg_y: Axis,
    bg_cx: Axis,
    bg_cy: Axis,
}

/// Reusable buffers for [`composite`].
#[derive(Default)]
pub struct CompositeScratch {
    geometry: Geometry,
    small_row: Vec<u8>,
    mask_row: Vec<u8>,
    bg_row: Vec<u8>,
}

/// Blends two rows `a` (weight `256 - f`) and `b` (weight `f`).
fn blend_rows(a: &[u8], b: &[u8], f: u32, out: &mut [u8]) {
    for ((out, &a), &b) in out.iter_mut().zip(a).zip(b) {
        *out = ((u32::from(a) * (256 - f) + u32::from(b) * f + 128) >> 8) as u8;
    }
}

/// Stretches one row horizontally with `axis`.
fn expand_row<const C: usize>(small: &[u8], axis: &Axis, out: &mut [u8]) {
    for x in 0..out.len() / C {
        let pixel = &mut out[x * C..][..C];
        let (a, b, f) = (
            axis.first[x] as usize * C,
            axis.second[x] as usize * C,
            axis.weight[x],
        );
        for (channel, value) in pixel.iter_mut().enumerate() {
            *value = ((u32::from(small[a + channel]) * (256 - f)
                + u32::from(small[b + channel]) * f
                + 128)
                >> 8) as u8;
        }
    }
}

/// Row `y` of a `C`-channel small plane stretched to the target width.
fn sample_row<const C: usize>(
    plane: &[u8],
    plane_width: usize,
    y_axis: &Axis,
    x_axis: &Axis,
    y: usize,
    small_row: &mut Vec<u8>,
    out: &mut [u8],
) {
    let row = |index: u32| &plane[index as usize * plane_width * C..][..plane_width * C];
    small_row.resize(plane_width * C, 0);
    blend_rows(
        row(y_axis.first[y]),
        row(y_axis.second[y]),
        y_axis.weight[y],
        small_row,
    );
    expand_row::<C>(small_row, x_axis, out);
}

/// `fg * m + bg * (255 - m)`, rounded, with `m` in 0..=255.
fn mix(fg: u8, bg: u8, m: u8) -> u8 {
    let m = u32::from(m);
    ((u32::from(fg) * m + u32::from(bg) * (255 - m) + 127) / 255) as u8
}

/// Writes into `out` the camera picture `src` where `mask` (a
/// [`MASK_SIZE`]² probability stretched over the whole picture, 255 = person)
/// is high and `background` elsewhere. `out` becomes a tightly packed NV12
/// picture whose stride is the returned value.
pub fn composite(
    src: &Planes<'_>,
    mask: &[u8],
    background: &SmallImage,
    scratch: &mut CompositeScratch,
    out: &mut Vec<u8>,
) -> usize {
    debug_assert_eq!(mask.len(), MASK_SIZE * MASK_SIZE);
    let (width, height) = (src.width, src.height);
    let (chroma_width, chroma_height) = (src.chroma_width(), src.chroma_height());
    let key = [
        width,
        height,
        background.width,
        background.height,
        background.chroma_width,
        background.chroma_height,
    ];
    if scratch.geometry.key != key {
        scratch.geometry = Geometry {
            key,
            mask_x: Axis::new(MASK_SIZE, width),
            mask_y: Axis::new(MASK_SIZE, height),
            mask_cx: Axis::new(MASK_SIZE, chroma_width),
            mask_cy: Axis::new(MASK_SIZE, chroma_height),
            bg_x: Axis::new(background.width, width),
            bg_y: Axis::new(background.height, height),
            bg_cx: Axis::new(background.chroma_width, chroma_width),
            bg_cy: Axis::new(background.chroma_height, chroma_height),
        };
    }
    let stride = chroma_width * 2;
    out.resize(stride * (height + chroma_height), 0);
    let (out_y, out_uv) = out.split_at_mut(stride * height);
    let g = &scratch.geometry;
    scratch.mask_row.resize(width.max(chroma_width), 0);
    scratch.bg_row.resize(stride.max(width), 0);
    for y in 0..height {
        sample_row::<1>(
            mask,
            MASK_SIZE,
            &g.mask_y,
            &g.mask_x,
            y,
            &mut scratch.small_row,
            &mut scratch.mask_row[..width],
        );
        sample_row::<1>(
            &background.y,
            background.width,
            &g.bg_y,
            &g.bg_x,
            y,
            &mut scratch.small_row,
            &mut scratch.bg_row[..width],
        );
        let source = &src.y[y * src.y_stride..][..width];
        for (((out, &fg), &bg), &m) in out_y[y * stride..][..width]
            .iter_mut()
            .zip(source)
            .zip(&scratch.bg_row[..width])
            .zip(&scratch.mask_row[..width])
        {
            *out = mix(fg, bg, m);
        }
    }
    for y in 0..chroma_height {
        sample_row::<1>(
            mask,
            MASK_SIZE,
            &g.mask_cy,
            &g.mask_cx,
            y,
            &mut scratch.small_row,
            &mut scratch.mask_row[..chroma_width],
        );
        sample_row::<2>(
            &background.uv,
            background.chroma_width,
            &g.bg_cy,
            &g.bg_cx,
            y,
            &mut scratch.small_row,
            &mut scratch.bg_row[..stride],
        );
        let source = &src.uv[y * src.uv_stride..][..stride];
        let target = &mut out_uv[y * stride..][..stride];
        for (x, &m) in scratch.mask_row[..chroma_width].iter().enumerate() {
            target[2 * x] = mix(source[2 * x], scratch.bg_row[2 * x], m);
            target[2 * x + 1] = mix(source[2 * x + 1], scratch.bg_row[2 * x + 1], m);
        }
    }
    stride
}

/// Mirrors an NV12 picture left to right into `out` (tightly packed, stride
/// `ceil(width / 2) * 2`); the local self-view only, never the stream.
pub fn mirror(src: &Planes<'_>, out: &mut Vec<u8>) -> usize {
    let (width, height) = (src.width, src.height);
    let (chroma_width, chroma_height) = (src.chroma_width(), src.chroma_height());
    let stride = chroma_width * 2;
    out.resize(stride * (height + chroma_height), 0);
    let (out_y, out_uv) = out.split_at_mut(stride * height);
    for y in 0..height {
        let source = &src.y[y * src.y_stride..][..width];
        for (target, &value) in out_y[y * stride..][..width]
            .iter_mut()
            .zip(source.iter().rev())
        {
            *target = value;
        }
    }
    for y in 0..chroma_height {
        let source = &src.uv[y * src.uv_stride..][..stride];
        let target = &mut out_uv[y * stride..][..stride];
        for x in 0..chroma_width {
            let from = (chroma_width - 1 - x) * 2;
            target[2 * x] = source[from];
            target[2 * x + 1] = source[from + 1];
        }
    }
    stride
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: usize, height: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        for y in 0..height {
            for x in 0..width {
                bytes.push(((x * 255) / width.max(1) + y) as u8);
            }
        }
        for y in 0..height / 2 {
            for x in 0..width / 2 {
                bytes.push((100 + x) as u8);
                bytes.push((140 + y) as u8);
            }
        }
        bytes
    }

    fn planes(width: usize, height: usize, bytes: &[u8]) -> Planes<'_> {
        let (y, uv) = bytes.split_at(width * height);
        Planes {
            width,
            height,
            y,
            y_stride: width,
            uv,
            uv_stride: width,
        }
    }

    #[test]
    fn a_full_mask_keeps_the_camera_and_an_empty_one_shows_only_the_background() {
        let (w, h) = (64, 48);
        let bytes = gradient(w, h);
        let src = planes(w, h, &bytes);
        let preset = SmallImage::from_nv12(8, 6, &[90; 8 * 6 * 3 / 2]).unwrap();
        let mut scratch = CompositeScratch::default();
        let mut out = Vec::new();
        let full = vec![255u8; MASK_SIZE * MASK_SIZE];
        let stride = composite(&src, &full, &preset, &mut scratch, &mut out);
        assert_eq!(stride, w);
        assert_eq!(
            out, bytes,
            "person everywhere: the camera picture, untouched"
        );
        let empty = vec![0u8; MASK_SIZE * MASK_SIZE];
        composite(&src, &empty, &preset, &mut scratch, &mut out);
        assert!(out.iter().all(|&b| b == 90), "no person: only the preset");
    }

    #[test]
    fn the_blurred_background_is_smooth_and_keeps_the_average_color() {
        let (w, h) = (640, 360);
        let mut bytes = vec![128u8; w * h * 3 / 2];
        // A checkerboard: maximal detail.
        for y in 0..h {
            for x in 0..w {
                bytes[y * w + x] = if (x + y) % 2 == 0 { 40 } else { 200 };
            }
        }
        let src = planes(w, h, &bytes);
        let mut small = SmallImage::default();
        blur_background(&src, &mut small, &mut BlurScratch::default());
        let mean = |v: &[u8]| v.iter().map(|&x| u32::from(x)).sum::<u32>() / v.len() as u32;
        assert!((i64::from(mean(&small.y)) - 120).abs() <= 2);
        let spread = small.y.iter().max().unwrap() - small.y.iter().min().unwrap();
        assert!(spread <= 6, "detail is gone, spread {spread}");
        assert!(small.uv.iter().all(|&c| c == 128));
    }

    #[test]
    fn odd_sizes_and_strides_are_handled() {
        let (w, h) = (33usize, 21usize);
        let stride = 40;
        let mut bytes = vec![7u8; stride * (h + h.div_ceil(2))];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let (y, uv) = bytes.split_at(stride * h);
        let src = Planes {
            width: w,
            height: h,
            y,
            y_stride: stride,
            uv,
            uv_stride: stride,
        };
        let mut small = SmallImage::default();
        blur_background(&src, &mut small, &mut BlurScratch::default());
        let mut out = Vec::new();
        let mask = vec![128u8; MASK_SIZE * MASK_SIZE];
        let out_stride = composite(
            &src,
            &mask,
            &small,
            &mut CompositeScratch::default(),
            &mut out,
        );
        assert_eq!(out_stride, 34);
        assert_eq!(out.len(), 34 * (21 + 11));
        let mut mirrored = Vec::new();
        assert_eq!(mirror(&src, &mut mirrored), 34);
        assert_eq!(mirrored.len(), out.len());
    }

    #[test]
    fn mirroring_reverses_luma_and_swaps_chroma_pairs_whole() {
        let (w, h) = (4, 2);
        let bytes = [
            1, 2, 3, 4, //
            5, 6, 7, 8, //
            10, 11, 20, 21, // one chroma row: pairs (10,11) and (20,21)
        ];
        let mut out = Vec::new();
        mirror(&planes(w, h, &bytes), &mut out);
        assert_eq!(out, [4, 3, 2, 1, 8, 7, 6, 5, 20, 21, 10, 11]);
        // Mirroring twice restores the picture.
        let mut back = Vec::new();
        mirror(&planes(w, h, &out), &mut back);
        assert_eq!(back, bytes);
    }
}
