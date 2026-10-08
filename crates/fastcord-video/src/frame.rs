//! Borrowed NV12 images, the pixel format exchanged with every codec backend.

use std::fmt;

/// A borrowed 8-bit 4:2:0 NV12 image: a full-resolution Y plane and a
/// half-resolution plane of interleaved U/V pairs, each with its own stride.
///
/// Odd dimensions are allowed (a decoder's display crop may be odd); chroma
/// then covers `ceil(width / 2)` pairs and `ceil(height / 2)` rows.
#[derive(Clone, Copy)]
pub struct Nv12<'a> {
    width: u32,
    height: u32,
    y: &'a [u8],
    y_stride: usize,
    uv: &'a [u8],
    uv_stride: usize,
}

/// Why an [`Nv12`] view was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Width or height is zero.
    Empty,
    /// A stride is shorter than one row of its plane.
    StrideTooSmall,
    /// A plane slice ends before its last row.
    PlaneTooShort,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "the image has no pixels",
            Self::StrideTooSmall => "an NV12 stride is shorter than its row",
            Self::PlaneTooShort => "an NV12 plane is shorter than its rows",
        })
    }
}

impl std::error::Error for FrameError {}

/// Bytes needed for `rows` rows of `row_bytes` at `stride`; the last row need
/// not be padded to the full stride.
fn plane_len(rows: usize, row_bytes: usize, stride: usize) -> Option<usize> {
    stride.checked_mul(rows - 1)?.checked_add(row_bytes)
}

impl<'a> Nv12<'a> {
    /// Validates plane sizes against the dimensions and strides.
    pub fn new(
        width: u32,
        height: u32,
        y: &'a [u8],
        y_stride: usize,
        uv: &'a [u8],
        uv_stride: usize,
    ) -> Result<Self, FrameError> {
        if width == 0 || height == 0 {
            return Err(FrameError::Empty);
        }
        let image = Self {
            width,
            height,
            y,
            y_stride,
            uv,
            uv_stride,
        };
        let (luma_row, chroma_row) = (image.luma_row_bytes(), image.chroma_row_bytes());
        if y_stride < luma_row || uv_stride < chroma_row {
            return Err(FrameError::StrideTooSmall);
        }
        let luma = plane_len(height as usize, luma_row, y_stride);
        let chroma = plane_len(image.chroma_rows(), chroma_row, uv_stride);
        match (luma, chroma) {
            (Some(luma), Some(chroma)) if y.len() >= luma && uv.len() >= chroma => Ok(image),
            _ => Err(FrameError::PlaneTooShort),
        }
    }

    /// A tightly packed image: `width`-byte rows, the UV plane directly after
    /// the Y plane.
    pub fn packed(width: u32, height: u32, data: &'a [u8]) -> Result<Self, FrameError> {
        let luma = (width as usize)
            .checked_mul(height as usize)
            .ok_or(FrameError::PlaneTooShort)?;
        let stride = packed_chroma_row_bytes(width);
        let (y, uv) = data
            .split_at_checked(luma)
            .ok_or(FrameError::PlaneTooShort)?;
        Self::new(width, height, y, width as usize, uv, stride)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Visible bytes of a Y row.
    pub fn luma_row_bytes(&self) -> usize {
        self.width as usize
    }

    /// Visible bytes of a UV row (two bytes per chroma sample pair).
    pub fn chroma_row_bytes(&self) -> usize {
        packed_chroma_row_bytes(self.width)
    }

    /// Number of UV rows.
    pub fn chroma_rows(&self) -> usize {
        self.height.div_ceil(2) as usize
    }

    /// Visible bytes of Y row `row`. Panics if `row >= height`.
    pub fn luma_row(&self, row: usize) -> &'a [u8] {
        assert!(row < self.height as usize, "luma row out of range");
        let start = row * self.y_stride;
        &self.y[start..start + self.luma_row_bytes()]
    }

    /// Visible bytes of UV row `row`. Panics if `row >= chroma_rows()`.
    pub fn chroma_row(&self, row: usize) -> &'a [u8] {
        assert!(row < self.chroma_rows(), "chroma row out of range");
        let start = row * self.uv_stride;
        &self.uv[start..start + self.chroma_row_bytes()]
    }
    /// Byte distance between successive rows of the luma plane.
    pub fn luma_stride(&self) -> usize {
        self.y_stride
    }

    /// Byte distance between successive rows of the interleaved chroma plane.
    pub fn chroma_stride(&self) -> usize {
        self.uv_stride
    }

    /// Bytes of a tightly packed copy (see [`Nv12::packed`]).
    pub fn packed_len(&self) -> usize {
        self.luma_row_bytes() * self.height as usize + self.chroma_row_bytes() * self.chroma_rows()
    }

    /// Appends a tightly packed copy of the visible pixels to `out`.
    pub fn copy_packed_into(&self, out: &mut Vec<u8>) {
        out.reserve(self.packed_len());
        for row in 0..self.height as usize {
            out.extend_from_slice(self.luma_row(row));
        }
        for row in 0..self.chroma_rows() {
            out.extend_from_slice(self.chroma_row(row));
        }
    }
}

fn packed_chroma_row_bytes(width: u32) -> usize {
    width.div_ceil(2) as usize * 2
}

impl fmt::Debug for Nv12<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Nv12")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("y_stride", &self.y_stride)
            .field("uv_stride", &self.uv_stride)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_layout_splits_planes_at_the_luma_size() {
        // 4x2 luma, then one row of two UV pairs.
        let data: Vec<u8> = (0..12).collect();
        let image = Nv12::packed(4, 2, &data).unwrap();
        assert_eq!(image.luma_row(0), &[0, 1, 2, 3]);
        assert_eq!(image.luma_row(1), &[4, 5, 6, 7]);
        assert_eq!(image.chroma_row(0), &[8, 9, 10, 11]);
        assert_eq!(image.packed_len(), 12);
        assert!(Nv12::packed(4, 2, &data[..11]).is_err());
    }

    #[test]
    fn strided_planes_skip_padding_and_need_no_padding_after_the_last_row() {
        // 2x2 image, stride 4: rows at 0 and 4; the last row ends at 6.
        let y = [10, 11, 0xEE, 0xEE, 12, 13];
        let uv = [20, 21];
        let image = Nv12::new(2, 2, &y, 4, &uv, 2).unwrap();
        let mut packed = Vec::new();
        image.copy_packed_into(&mut packed);
        assert_eq!(packed, [10, 11, 12, 13, 20, 21]);
        assert_eq!(
            Nv12::new(2, 2, &y[..5], 4, &uv, 2).unwrap_err(),
            FrameError::PlaneTooShort
        );
    }

    #[test]
    fn odd_dimensions_round_chroma_up() {
        // 3x3: chroma is 2 pairs (4 bytes) by 2 rows.
        let y = [0u8; 9];
        let uv = [0u8; 8];
        let image = Nv12::new(3, 3, &y, 3, &uv, 4).unwrap();
        assert_eq!(image.chroma_rows(), 2);
        assert_eq!(image.chroma_row_bytes(), 4);
        assert_eq!(
            Nv12::new(3, 3, &y, 3, &uv, 3).unwrap_err(),
            FrameError::StrideTooSmall
        );
        assert_eq!(
            Nv12::new(3, 3, &y, 3, &uv[..7], 4).unwrap_err(),
            FrameError::PlaneTooShort
        );
    }

    #[test]
    fn empty_and_overflowing_images_are_rejected() {
        assert_eq!(Nv12::packed(0, 2, &[]).unwrap_err(), FrameError::Empty);
        assert_eq!(
            Nv12::new(2, 2, &[0; 4], usize::MAX, &[0; 2], 2).unwrap_err(),
            FrameError::PlaneTooShort
        );
    }
}
