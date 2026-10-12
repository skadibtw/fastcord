//! Color conversion shared by the GPU shader and the CPU fallback.

/// Non-constant-luminance YCbCr matrix coefficients (ITU-T H.273).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMatrix {
    /// BT.470BG/SMPTE 170M use the BT.601 coefficients.
    Bt601,
    #[default]
    Bt709,
    /// BT.2020 non-constant-luminance; constant-luminance is not an affine matrix.
    Bt2020,
}

impl ColorMatrix {
    /// Supported H.273 matrix identifiers. Unspecified uses our documented
    /// BT.709 default; other matrices must not be silently mislabeled.
    pub(crate) fn from_h273(id: u32) -> Option<Self> {
        match id {
            1 | 2 => Some(Self::Bt709),
            5 | 6 => Some(Self::Bt601),
            9 => Some(Self::Bt2020),
            _ => None,
        }
    }
}

/// Quantization range of the 8-bit samples delivered by a decoder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorRange {
    /// Y=16..235 and Cb/Cr=16..240, with neutral chroma at 128.
    #[default]
    Limited,
    /// Y=0..255 and Cb/Cr=0..255, with neutral chroma at 128.
    Full,
}

/// Color and display geometry of a decoded picture.
///
/// Missing/unspecified matrix metadata uses BT.709. Missing sample-range
/// metadata follows H.264/AV1's limited-range default, and missing aspect
/// metadata uses square pixels. An explicit unsupported matrix is rejected
/// by the decoder rather than replaced by these defaults. BT.2020 here is
/// non-constant-luminance only; these coefficients do not perform HDR tone
/// mapping, transfer-function conversion or gamut mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorDescription {
    pub matrix: ColorMatrix,
    pub range: ColorRange,
    /// Sample width : sample height, with nonzero numerator and denominator.
    pub pixel_aspect_ratio: (u32, u32),
}

impl Default for ColorDescription {
    fn default() -> Self {
        Self {
            matrix: ColorMatrix::Bt709,
            range: ColorRange::Limited,
            pixel_aspect_ratio: (1, 1),
        }
    }
}

impl ColorDescription {
    /// RGB transform rows multiplying normalized sampled `[Y, U, V, 1]`.
    ///
    /// Y/UV textures are `unorm`: byte samples are divided by 255. The bias
    /// therefore uses the exact digital neutral chroma value `128/255`, not
    /// `0.5`. Clamp the three resulting RGB channels to `[0, 1]`; the same
    /// rows drive the opaque BGRA fallback on the decoding thread.
    pub fn transform_rows(self) -> [[f32; 4]; 3] {
        let (kr, kb) = match self.matrix {
            ColorMatrix::Bt601 => (0.299, 0.114),
            ColorMatrix::Bt709 => (0.2126, 0.0722),
            ColorMatrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1.0 - kr - kb;
        let (y_scale, y_offset, c_scale) = match self.range {
            ColorRange::Limited => (255.0 / 219.0, 16.0 / 255.0, 255.0 / 224.0),
            ColorRange::Full => (1.0, 0.0, 1.0),
        };
        let c_offset = 128.0 / 255.0;
        let r_v = 2.0 * (1.0 - kr) * c_scale;
        let g_u = -2.0 * kb * (1.0 - kb) / kg * c_scale;
        let g_v = -2.0 * kr * (1.0 - kr) / kg * c_scale;
        let b_u = 2.0 * (1.0 - kb) * c_scale;
        [
            [y_scale, 0.0, r_v, -y_scale * y_offset - r_v * c_offset],
            [
                y_scale,
                g_u,
                g_v,
                -y_scale * y_offset - (g_u + g_v) * c_offset,
            ],
            [y_scale, b_u, 0.0, -y_scale * y_offset - b_u * c_offset],
        ]
    }
}
