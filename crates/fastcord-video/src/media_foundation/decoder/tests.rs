use super::*;
use crate::codec::BackendPreference;
use crate::test_vectors;

fn session() -> Option<Session> {
    match Session::start() {
        Ok(session) => Some(session),
        Err(CodecError::PlatformUnavailable(what)) => {
            eprintln!("{what} is not installed on this Windows; skipped");
            None
        }
        Err(error) => panic!("{error}"),
    }
}

#[test]
fn native_output_attributes_override_only_the_fields_they_supply() {
    let Some(session) = session() else {
        return;
    };
    let media_type = session.media_type().unwrap();
    let fallback = ColorDescription {
        matrix: ColorMatrix::Bt2020,
        range: ColorRange::Full,
        pixel_aspect_ratio: (64, 45),
    };
    assert_eq!(
        output_format(&media_type).unwrap().color.resolve(fallback),
        fallback
    );
    for (native, matrix) in [
        (1, ColorMatrix::Bt709),
        (2, ColorMatrix::Bt601),
        (4, ColorMatrix::Bt2020),
        (5, ColorMatrix::Bt2020),
    ] {
        for (nominal, range) in [(1, ColorRange::Full), (2, ColorRange::Limited)] {
            // SAFETY: these are supported scalar attributes on a live media type.
            unsafe {
                media_type.SetUINT32(&MF_MT_YUV_MATRIX, native).unwrap();
                media_type
                    .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, nominal)
                    .unwrap();
                media_type
                    .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (4u64 << 32) | 3)
                    .unwrap();
            }
            assert_eq!(
                output_format(&media_type).unwrap().color.resolve(fallback),
                ColorDescription {
                    matrix,
                    range,
                    pixel_aspect_ratio: (4, 3),
                }
            );
        }
    }
    // SAFETY: changing a scalar media-type attribute needs no buffer access.
    unsafe {
        media_type.SetUINT32(&MF_MT_YUV_MATRIX, 3).unwrap();
    }
    assert!(matches!(
        output_format(&media_type),
        Err(CodecError::Unsupported(_))
    ));
}

#[test]
fn reordered_output_finds_its_own_sps_description_without_allocating_a_queue() {
    let mut state = DecoderState {
        backend: Backend {
            kind: BackendKind::Software,
            api: super::super::API,
            name: String::new(),
        },
        preference: BackendPreference::Software,
        format: OutputFormat::default(),
        colors: ColorState::default(),
        submitted: [None; 32],
        next_color: 0,
    };
    let old = ColorDescription {
        matrix: ColorMatrix::Bt601,
        ..ColorDescription::default()
    };
    let new = ColorDescription {
        matrix: ColorMatrix::Bt2020,
        range: ColorRange::Full,
        pixel_aspect_ratio: (3, 2),
    };
    state.remember_color(300, old);
    state.remember_color(100, new);
    assert_eq!(state.color_at(100), new);
    assert_eq!(state.color_at(300), old);
    for timestamp in 400..464 {
        state.remember_color(timestamp, new);
    }
    assert_eq!(state.color_at(463), new);
    assert_eq!(state.submitted.len(), 32);
}

#[test]
fn software_fixture_output_preserves_explicit_matrix_range_and_aspect() {
    for matrix in [ColorMatrix::Bt601, ColorMatrix::Bt709, ColorMatrix::Bt2020] {
        for range in [ColorRange::Limited, ColorRange::Full] {
            let color = ColorDescription {
                matrix,
                range,
                pixel_aspect_ratio: (4, 3),
            };
            let stream = test_vectors::ipcm_stream_with_color(color);
            let mut decoder = match super::super::open_decoder(BackendPreference::Software) {
                Ok(selected) => selected.codec,
                Err(crate::OpenError::Discovery(CodecError::PlatformUnavailable(what))) => {
                    eprintln!("{what} is not installed on this Windows; skipped");
                    return;
                }
                Err(error) => panic!("{error}"),
            };
            let mut count = 0;
            let mut collect = |frame: DecodedFrame<'_>| {
                assert_eq!(frame.color, color);
                assert_eq!((frame.image.width(), frame.image.height()), (128, 96));
                count += 1;
            };
            for (index, unit) in test_vectors::ipcm_access_units(&stream)
                .into_iter()
                .enumerate()
            {
                decoder
                    .decode(unit, Duration::from_millis(index as u64 * 33), &mut collect)
                    .unwrap();
            }
            decoder.flush(&mut collect).unwrap();
            assert_eq!(count, test_vectors::IPCM_FRAMES);
        }
    }
}
