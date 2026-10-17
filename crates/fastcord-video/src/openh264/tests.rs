//! Selection and end-to-end tests for the OpenH264 backends.
//!
//! The bundled source is compiled wherever OpenH264's own build script can
//! run, so these tests also run on Windows and macOS, where the module is
//! inert for selection: the factories below are the only users of the
//! candidates, and the tests exercise them directly.

use std::collections::BTreeMap;
use std::time::Duration;

use super::{API, NAME, SoftwareDecoder, SoftwareEncoder, open_decoder, open_encoder};
use crate::codec::{
    BackendKind, BackendPreference, CodecError, DecodedFrame, EncodedFrame, EncoderConfig,
    VideoDecoder, VideoEncoder,
};
use crate::frame::Nv12;
use crate::select::OpenError;
use crate::test_vectors::{moving_pattern, psnr};

const ROUND_TRIP: EncoderConfig = EncoderConfig {
    width: 128,
    height: 96,
    frame_rate: 30,
    bitrate: 500_000,
    keyframe_interval: 30,
};

const FRAMES: usize = 12;
/// A receiver PLI arrives for this frame.
const FORCED_KEYFRAME: usize = 7;

fn frame_time(index: usize) -> Duration {
    Duration::from_millis(33) * index as u32
}

#[test]
fn software_backends_open_whenever_the_preference_allows_them() {
    for preference in [BackendPreference::Auto, BackendPreference::Software] {
        let selected = open_encoder(EncoderConfig::SCREEN_SHARE_720P30, preference).unwrap();
        // The only candidate is software, so automatic mode opens it without
        // any earlier failure to report.
        assert!(selected.rejected.is_empty(), "{preference:?}");
        assert_eq!(selected.codec.backend().kind, BackendKind::Software);
        assert_eq!(selected.codec.backend().api, API);
        assert_eq!(selected.codec.backend().name, NAME);
        assert_eq!(selected.codec.config(), &EncoderConfig::SCREEN_SHARE_720P30);

        let selected = open_decoder(preference).unwrap();
        assert!(selected.rejected.is_empty(), "{preference:?}");
        assert_eq!(selected.codec.backend().kind, BackendKind::Software);
        assert_eq!(selected.codec.backend().api, API);
        assert_eq!(selected.codec.backend().name, NAME);
    }
}

#[test]
fn a_hardware_preference_finds_no_openh264_backend() {
    // The bundled source is software; the platform glue must never present it
    // as a hardware candidate.
    assert!(matches!(
        open_encoder(
            EncoderConfig::SCREEN_SHARE_720P30,
            BackendPreference::Hardware
        ),
        Err(OpenError::NoBackend(BackendPreference::Hardware))
    ));
    assert!(matches!(
        open_decoder(BackendPreference::Hardware),
        Err(OpenError::NoBackend(BackendPreference::Hardware))
    ));
}

#[test]
fn invalid_settings_reject_without_an_attempt() {
    let config = EncoderConfig {
        width: 127,
        ..ROUND_TRIP
    };
    assert!(matches!(
        open_encoder(config, BackendPreference::Auto),
        Err(OpenError::Invalid(CodecError::InvalidConfig(_)))
    ));
    // The configured width is odd, which OpenH264 could not use either: the
    // backends are never built for a rejected configuration.
    assert!(matches!(
        open_encoder(config, BackendPreference::Software),
        Err(OpenError::Invalid(CodecError::InvalidConfig(_)))
    ));
}

#[test]
fn a_running_encoder_cannot_change_its_bitrate() {
    let mut encoder = open_encoder(ROUND_TRIP, BackendPreference::Software)
        .map(|selected| selected.codec)
        .unwrap();
    assert!(matches!(
        encoder.set_bitrate(ROUND_TRIP.bitrate * 2),
        Err(CodecError::Unsupported(_))
    ));
}

/// Encodes the moving pattern, checks the keyframe contract, and returns the
/// access units.
fn encode_pattern(encoder: &mut SoftwareEncoder) -> Vec<EncodedFrame> {
    let mut encoded = Vec::new();
    for index in 0..FRAMES {
        let picture = moving_pattern(ROUND_TRIP.width, ROUND_TRIP.height, index as u32);
        let frame = Nv12::packed(ROUND_TRIP.width, ROUND_TRIP.height, &picture).unwrap();
        encoder
            .encode(
                &frame,
                frame_time(index),
                index == FORCED_KEYFRAME,
                &mut encoded,
            )
            .unwrap_or_else(|error| panic!("frame {index}: {error}"));
    }
    encoder.flush(&mut encoded).unwrap();

    // No reordering (no B-frames): decoding order is presentation order.
    let times: Vec<Duration> = encoded.iter().map(|frame| frame.timestamp).collect();
    let expected: Vec<Duration> = (0..FRAMES).map(frame_time).collect();
    assert_eq!(times, expected);
    assert!(encoded[0].keyframe, "the stream starts with a keyframe");
    assert!(
        encoded[FORCED_KEYFRAME].keyframe,
        "a requested keyframe is produced"
    );
    assert_eq!(
        encoded.iter().filter(|frame| frame.keyframe).count(),
        2,
        "no other keyframe than the start and the forced one"
    );
    encoded
}

/// Decodes `access_units` (with `frame_time` timestamps) and flushes; returns
/// packed pictures by timestamp.
fn decode_all<'a>(
    decoder: &mut SoftwareDecoder,
    access_units: impl IntoIterator<Item = &'a [u8]>,
) -> BTreeMap<Duration, Vec<u8>> {
    let mut pictures = BTreeMap::new();
    let mut collect = |frame: DecodedFrame<'_>| {
        let mut packed = Vec::new();
        frame.image.copy_packed_into(&mut packed);
        assert!(
            pictures.insert(frame.timestamp, packed).is_none(),
            "two pictures at {:?}",
            frame.timestamp
        );
    };
    for (index, access_unit) in access_units.into_iter().enumerate() {
        decoder
            .decode(access_unit, frame_time(index), &mut collect)
            .unwrap_or_else(|error| panic!("unit {index}: {error}"));
    }
    decoder.flush(&mut collect).unwrap();
    pictures
}

#[test]
fn the_software_backend_round_trips_the_moving_pattern() {
    let mut encoder = open_encoder(ROUND_TRIP, BackendPreference::Software)
        .map(|selected| selected.codec)
        .unwrap();
    let encoded = encode_pattern(&mut encoder);

    let mut decoder = open_decoder(BackendPreference::Software)
        .map(|selected| selected.codec)
        .unwrap();
    let decoded = decode_all(
        &mut decoder,
        encoded.iter().map(|frame| frame.data.as_slice()),
    );
    assert_eq!(decoded.len(), FRAMES);
    let luma = (ROUND_TRIP.width * ROUND_TRIP.height) as usize;
    for (index, (timestamp, picture)) in decoded.into_iter().enumerate() {
        assert_eq!(timestamp, frame_time(index));
        let source = moving_pattern(ROUND_TRIP.width, ROUND_TRIP.height, index as u32);
        assert_eq!(picture.len(), source.len(), "decoded picture size");
        let (y, uv) = (
            psnr(&picture[..luma], &source[..luma]),
            psnr(&picture[luma..], &source[luma..]),
        );
        assert!(
            y > 32.0 && uv > 32.0,
            "frame {index}: PSNR Y {y:.1} dB, UV {uv:.1} dB"
        );
    }
}
