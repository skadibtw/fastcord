//! OpenH264 decoding through the codec contract's borrowed NV12 pictures.

use std::time::Duration;

use super::native;
use crate::codec::{Backend, CodecError, DecodedFrame, VideoDecoder, check_dimensions};
use crate::frame::{ColorDescription, Nv12};
use native::decoder::DecodedYUV;
use native::formats::YUVSource;

const UNUSABLE: CodecError = CodecError::Unsupported("OpenH264 returned an unusable picture");

/// An OpenH264 software H.264 decoder (see [`super::open_decoder`]).
pub struct SoftwareDecoder {
    backend: Backend,
    native: native::decoder::Decoder,
    colors: crate::h264::ColorState,
    /// The last decoded access unit's reporting timestamp and color. Pictures
    /// the decoder still holds from an earlier unit never learn their own
    /// attribution through the wrapper; with reporting order monotonic, the
    /// last unit's values are the honest stand-in for anything flushed later.
    last: (Duration, ColorDescription),
    scratch: Scratch,
}

impl SoftwareDecoder {
    pub(super) fn open() -> Result<Self, CodecError> {
        // The wrapper's default flush behavior surfaces every picture at the
        // first call that makes it ready: a call whose bitstream yields no
        // fresh picture flushes an earlier buffered one. A low-latency watch
        // path wants exactly that; whatever is left after the last access
        // unit drains in `flush`.
        let native = native::decoder::Decoder::new()
            .map_err(super::native_error("OpenH264 create decoder"))?;
        Ok(Self {
            backend: super::backend(),
            colors: crate::h264::ColorState::default(),
            last: (Duration::ZERO, ColorDescription::default()),
            scratch: Scratch::default(),
            native,
        })
    }
}

/// The NV12 staging area a decoded picture is written into before it is
/// reported, reallocated exactly when a picture's dimensions change. Every
/// allocation is bounded by the decoder's own SPS checks (at most 4096x2304,
/// about 14 MiB for the pair), so it never grows unboundedly, and it is
/// transient: the calling side's [`crate::frame::FramePool`] owns the
/// reported leases.
#[derive(Default)]
struct Scratch {
    y: Vec<u8>,
    uv: Vec<u8>,
}

impl Scratch {
    /// Interleaves the borrowed I420 picture (the wrapper's plane set) into
    /// NV12 and returns the borrowed validated view.
    fn picture<'a>(&'a mut self, source: &DecodedYUV<'_>) -> Result<Nv12<'a>, CodecError> {
        let (first, second) = source.dimensions();
        let (Ok(width), Ok(height)) = (u32::try_from(first), u32::try_from(second)) else {
            return Err(UNUSABLE);
        };
        check_dimensions(width, height)?;
        let luma_width = width as usize;
        let luma_height = height as usize;

        // Our own packed NV12 layout: full-resolution luma rows, then
        // ceil(w / 2) interleaved pairs over ceil(h / 2) rows.
        let luma = luma_width.checked_mul(luma_height).ok_or(UNUSABLE)?;
        let uv_stride = luma_width.div_ceil(2) * 2;
        let chroma = uv_stride
            .checked_mul(luma_height.div_ceil(2))
            .ok_or(UNUSABLE)?;
        if self.y.len() != luma || self.uv.len() != chroma {
            // One reallocation per size change: sizes alternate only with a
            // new SPS, which the decoder checks first (see decode).
            self.y = vec![0; luma];
            self.uv = vec![0; chroma];
        }

        let (y_stride, u_stride, v_stride) = source.strides();
        for row in 0..luma_height {
            let from = source
                .y()
                .get(row * y_stride..row * y_stride + luma_width)
                .ok_or(UNUSABLE)?;
            self.y[row * luma_width..(row + 1) * luma_width].copy_from_slice(from);
        }
        let pairs = luma_width.div_ceil(2);
        for row in 0..luma_height.div_ceil(2) {
            let u_row = source
                .u()
                .get(row * u_stride..row * u_stride + pairs)
                .ok_or(UNUSABLE)?;
            let v_row = source
                .v()
                .get(row * v_stride..row * v_stride + pairs)
                .ok_or(UNUSABLE)?;
            let target = &mut self.uv[row * uv_stride..];
            for (index, (&u, &v)) in u_row.iter().zip(v_row.iter()).enumerate() {
                target[2 * index] = u;
                target[2 * index + 1] = v;
            }
        }
        Ok(Nv12::new(
            width, height, &self.y, luma_width, &self.uv, uv_stride,
        )?)
    }
}

impl VideoDecoder for SoftwareDecoder {
    fn backend(&self) -> &Backend {
        &self.backend
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        timestamp: Duration,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError> {
        // Framing, the 8 MiB bound, SPS size/format limits and the access
        // unit's color all come from the repo's own inspection before the
        // native decoder sees anything.
        let color = self.colors.inspect(access_unit)?;
        let Some(source) = self
            .native
            .decode(access_unit)
            .map_err(super::native_error("OpenH264 decode frame"))?
        else {
            // The picture is buffered: the wrapper flushes it with the next
            // access unit (or the final flush); nothing ready to report now.
            return Ok(());
        };
        let image = self.scratch.picture(&source)?;
        (out)(DecodedFrame {
            image,
            color,
            timestamp,
        });
        self.last = (timestamp, color);
        Ok(())
    }

    fn flush(&mut self, out: &mut dyn FnMut(DecodedFrame<'_>)) -> Result<(), CodecError> {
        // Pictures the decoder still holds were input through earlier access
        // units; report them with the last seen time and color (screen-share
        // streams change neither without a new SPS in a later picture).
        let pictures = self
            .native
            .flush_remaining()
            .map_err(super::native_error("OpenH264 flush decoder"))?;
        let (timestamp, color) = self.last;
        for source in pictures {
            let image = self.scratch.picture(&source)?;
            (out)(DecodedFrame {
                image,
                color,
                timestamp,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::codec::{BackendKind, BackendPreference};
    use crate::frame::{ColorMatrix, ColorRange};
    use crate::h264::MAX_ACCESS_UNIT_BYTES;
    use crate::test_vectors::{self, IPCM_FRAMES};

    const FIXTURE: &[u8] = include_bytes!("../../../../fixtures/video/ipcm-128x96.h264");

    fn frame_time(index: usize) -> Duration {
        Duration::from_millis(33) * index as u32
    }

    fn opened() -> SoftwareDecoder {
        // Selection itself is covered by the integration tests; the codec
        // under test is what these build.
        let selected = super::super::open_decoder(BackendPreference::Software).unwrap();
        assert!(selected.rejected.is_empty());
        selected.codec
    }

    /// Decodes `access_units` (with `frame_time` timestamps), flushes, and
    /// returns the reported pictures in reporting order.
    fn decode_all<'a>(
        decoder: &mut SoftwareDecoder,
        access_units: impl IntoIterator<Item = &'a [u8]>,
    ) -> Vec<(Duration, ColorDescription, Vec<u8>)> {
        let mut pictures = Vec::new();
        let mut collect = |frame: DecodedFrame<'_>| {
            let mut packed = Vec::new();
            frame.image.copy_packed_into(&mut packed);
            pictures.push((frame.timestamp, frame.color, packed));
        };
        for (index, access_unit) in access_units.into_iter().enumerate() {
            decoder
                .decode(access_unit, frame_time(index), &mut collect)
                .unwrap_or_else(|error| panic!("unit {index}: {error}"));
        }
        decoder.flush(&mut collect).unwrap();
        pictures
    }

    /// Reports each picture against its expected `I_PCM` sample data and the
    /// stream's defaults (no VUI metadata anywhere in the fixture).
    fn assert_ipcm_pictures(pictures: Vec<(Duration, ColorDescription, Vec<u8>)>) {
        assert_eq!(pictures.len(), IPCM_FRAMES);
        for (index, (timestamp, color, picture)) in pictures.into_iter().enumerate() {
            // No reordering (pic_order_cnt_type 2): one picture per unit.
            assert_eq!(timestamp, frame_time(index), "picture {index}");
            assert_eq!(color, ColorDescription::default(), "picture {index}");
            let expected = test_vectors::ipcm_frame(index);
            assert_eq!(picture.len(), expected.len(), "picture {index} size");
            assert!(
                picture == expected,
                "picture {index} differs from its I_PCM samples"
            );
        }
    }

    fn fixture_units() -> Vec<&'static [u8]> {
        test_vectors::ipcm_access_units(FIXTURE)
    }

    #[test]
    fn decodes_the_committed_ipcm_fixture_bit_exactly() {
        let mut decoder = opened();
        assert_eq!(decoder.backend().kind, BackendKind::Software);
        assert_eq!(decoder.backend().api, super::super::API);
        let units = fixture_units();
        assert_ipcm_pictures(decode_all(&mut decoder, units.clone()));
        // The decoder stays usable after a flush: the stream decodes again.
        assert_ipcm_pictures(decode_all(&mut decoder, units));
    }

    #[test]
    fn color_metadata_follows_the_sps_declaration() {
        for color in [
            ColorDescription {
                matrix: ColorMatrix::Bt601,
                range: ColorRange::Full,
                pixel_aspect_ratio: (12, 11),
            },
            ColorDescription {
                matrix: ColorMatrix::Bt2020,
                range: ColorRange::Limited,
                pixel_aspect_ratio: (64, 99),
            },
        ] {
            let mut decoder = opened();
            let stream = test_vectors::ipcm_stream_with_color(color);
            let pictures = decode_all(&mut decoder, test_vectors::ipcm_access_units(&stream));
            assert_eq!(pictures.len(), IPCM_FRAMES);
            for (index, (timestamp, reported, _)) in pictures.into_iter().enumerate() {
                assert_eq!(timestamp, frame_time(index), "picture {index}");
                assert_eq!(reported, color, "picture {index}");
            }
        }
    }

    #[test]
    fn malformed_input_is_rejected_and_the_decoder_stays_usable() {
        let mut decoder = opened();
        let mut never = |_: DecodedFrame<'_>| panic!("no picture expected");

        // Bytes with no start code, and a start code with nothing after it,
        // are refused before the native decoder runs.
        assert!(matches!(
            decoder.decode(&[0x65, 0x88], Duration::ZERO, &mut never),
            Err(CodecError::InvalidBitstream(_))
        ));
        assert!(matches!(
            decoder.decode(&[0, 0, 0, 1], Duration::ZERO, &mut never),
            Err(CodecError::InvalidBitstream(_))
        ));
        // An access unit beyond the decoder's 8 MiB bound never reaches the
        // native decoder either (the vector stops exactly one byte too long).
        let oversized = vec![0u8; MAX_ACCESS_UNIT_BYTES + 1];
        assert!(matches!(
            decoder.decode(&oversized, Duration::ZERO, &mut never),
            Err(CodecError::Unsupported(_))
        ));

        // Nothing above touched the decoder, so the fixture decodes as if
        // nothing had happened.
        assert_ipcm_pictures(decode_all(&mut decoder, fixture_units()));
    }

    #[test]
    fn flush_without_any_input_reports_nothing() {
        let mut decoder = opened();
        let mut never = |_: DecodedFrame<'_>| panic!("no picture expected");
        decoder.flush(&mut never).unwrap();
    }
}
