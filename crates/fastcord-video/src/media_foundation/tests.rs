//! Media Foundation round trips on the real transforms of this machine.
//!
//! Software tests run wherever Media Foundation is installed; on Windows
//! editions without it (N editions without the Media Feature Pack, Server
//! without the feature) they report the missing platform and stop. Hardware
//! tests need a GPU with H.264 encode/decode and are run manually:
//! `cargo test -p fastcord-video --locked -- --ignored --nocapture`.

use std::collections::BTreeMap;
use std::time::Duration;

use super::*;
use crate::codec::{DecodedFrame, EncodedFrame, VideoDecoder, VideoEncoder};
use crate::h264::{NAL_SPS, inspect, nal_unit_type, nal_units, parse_sps};
use crate::test_vectors::{self, IPCM_FRAMES, IPCM_HEIGHT, IPCM_WIDTH, moving_pattern, psnr};

const FIXTURE: &[u8] = include_bytes!("../../../../fixtures/video/ipcm-128x96.h264");

const FIXTURE_ENCODER: EncoderConfig = EncoderConfig {
    width: IPCM_WIDTH,
    height: IPCM_HEIGHT,
    frame_rate: 30,
    bitrate: 500_000,
    keyframe_interval: 30,
};

/// The codec, or `None` with a notice when this Windows has no Media
/// Foundation at all. Every other failure fails the test.
fn opened<T>(result: Result<Selected<T>, OpenError>) -> Option<Selected<T>> {
    match result {
        Err(OpenError::Discovery(CodecError::PlatformUnavailable(what))) => {
            eprintln!("{what} is not installed on this Windows; skipped");
            None
        }
        Err(error) => panic!("{error}"),
        Ok(selected) => Some(selected),
    }
}

fn frame_time(index: usize) -> Duration {
    Duration::from_nanos(33_333_300) * index as u32
}

/// Decodes `access_units` (with `frame_time` timestamps) and flushes; returns
/// packed pictures by timestamp.
fn decode_all<'a>(
    decoder: &mut MfDecoder,
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
            .unwrap();
    }
    decoder.flush(&mut collect).unwrap();
    pictures
}

fn decode_ipcm_fixture(decoder: &mut MfDecoder) -> Vec<Vec<u8>> {
    let pictures = decode_all(decoder, test_vectors::ipcm_access_units(FIXTURE));
    assert_eq!(pictures.len(), IPCM_FRAMES);
    let mut frames = Vec::with_capacity(IPCM_FRAMES);
    for (index, (timestamp, picture)) in pictures.into_iter().enumerate() {
        assert_eq!(timestamp, frame_time(index));
        assert_eq!(picture.len(), (IPCM_WIDTH * IPCM_HEIGHT * 3 / 2) as usize);
        assert!(
            picture == test_vectors::ipcm_frame(index),
            "picture {index} differs from its I_PCM samples"
        );
        frames.push(picture);
    }
    frames
}

#[test]
fn software_decoder_reproduces_the_ipcm_fixture_bit_exactly() {
    let Some(selected) = opened(open_decoder(BackendPreference::Software)) else {
        return;
    };
    let mut decoder = selected.codec;
    assert_eq!(decoder.backend().kind, BackendKind::Software);
    assert_eq!(decoder.backend().api, API);
    drop(decode_ipcm_fixture(&mut decoder));
    // The decoder stays usable after a flush: the stream decodes again.
    drop(decode_ipcm_fixture(&mut decoder));
    assert_eq!(decoder.backend().kind, BackendKind::Software);
}

#[test]
#[ignore = "needs a GPU with D3D11VA H.264 decoding"]
fn hardware_decoder_reproduces_the_ipcm_fixture_bit_exactly() {
    let selected = open_decoder(BackendPreference::Hardware).unwrap();
    let mut decoder = selected.codec;
    drop(decode_ipcm_fixture(&mut decoder));
    println!("hardware decoder: {}", decoder.backend());
    // Output arrived on D3D11 surfaces, not a silent software fallback.
    assert_eq!(decoder.backend().kind, BackendKind::Hardware);
    drop(decode_ipcm_fixture(&mut decoder));
}

const ROUND_TRIP: EncoderConfig = EncoderConfig {
    width: 640,
    // Not a multiple of 16: the bitstream must crop 368 coded lines to 360.
    height: 360,
    frame_rate: 30,
    bitrate: 2_000_000,
    keyframe_interval: 30,
};
const FRAMES: usize = 75;
/// A receiver PLI arrives for this frame.
const FORCED_KEYFRAME: usize = 40;
/// The encoder is flushed (and must stay usable) after this frame.
const MID_FLUSH: usize = 20;

fn source(index: usize) -> Vec<u8> {
    moving_pattern(ROUND_TRIP.width, ROUND_TRIP.height, index as u32)
}

/// Encodes the moving pattern, checks the bitstream contract, and returns the
/// access units.
fn encode_pattern(encoder: &mut MfEncoder) -> Vec<EncodedFrame> {
    let mut encoded = Vec::new();
    for index in 0..FRAMES {
        let picture = source(index);
        let frame = Nv12::packed(ROUND_TRIP.width, ROUND_TRIP.height, &picture).unwrap();
        encoder
            .encode(
                &frame,
                frame_time(index),
                index == FORCED_KEYFRAME,
                &mut encoded,
            )
            .unwrap_or_else(|error| panic!("frame {index}: {error}"));
        if index == MID_FLUSH {
            encoder.flush(&mut encoded).unwrap();
            assert_eq!(
                encoded.len(),
                MID_FLUSH + 1,
                "flush returns every pending frame"
            );
        }
    }
    encoder.flush(&mut encoded).unwrap();
    assert_eq!(encoded.len(), FRAMES);

    // No reordering (no B-frames): decoding order is presentation order.
    let times: Vec<Duration> = encoded.iter().map(|frame| frame.timestamp).collect();
    let expected: Vec<Duration> = (0..FRAMES).map(frame_time).collect();
    assert_eq!(times, expected);

    let mut since_keyframe = 0;
    for (index, frame) in encoded.iter().enumerate() {
        let summary = inspect(&frame.data).unwrap();
        assert!(summary.vcl);
        assert_eq!(frame.keyframe, summary.idr, "frame {index}");
        if frame.keyframe {
            // Every keyframe is self-contained for a joining receiver.
            assert!(summary.sps && summary.pps, "keyframe {index} lacks SPS/PPS");
            let sps = nal_units(&frame.data)
                .find(|nal| nal_unit_type(nal) == NAL_SPS)
                .unwrap();
            let sps = parse_sps(sps).unwrap();
            // Constrained Baseline: profile_idc 66 with constraint_set1_flag.
            assert_eq!(sps.profile_idc, 66);
            assert_ne!(sps.constraint_flags & 0x40, 0, "constraint_set1_flag");
            assert_eq!(
                (sps.width, sps.height),
                (ROUND_TRIP.width, ROUND_TRIP.height)
            );
            since_keyframe = 0;
        } else {
            since_keyframe += 1;
            assert!(
                since_keyframe < ROUND_TRIP.keyframe_interval,
                "frame {index}: no keyframe for {since_keyframe} frames"
            );
        }
    }
    assert!(encoded[0].keyframe, "the stream starts with a keyframe");
    assert!(
        encoded[FORCED_KEYFRAME].keyframe,
        "a requested keyframe is produced"
    );

    // Constant bitrate within a generous bound (rate control settles over
    // the first frames).
    let bits: usize = encoded.iter().map(|frame| frame.data.len() * 8).sum();
    let seconds = FRAMES as f64 / f64::from(ROUND_TRIP.frame_rate);
    let rate = bits as f64 / seconds;
    assert!(
        rate < f64::from(ROUND_TRIP.bitrate) * 2.0,
        "{rate:.0} bit/s for a {} bit/s target",
        ROUND_TRIP.bitrate
    );
    encoded
}

/// Decodes `encoded` and compares each picture with its source.
fn check_decoded(decoder: &mut MfDecoder, encoded: &[EncodedFrame]) {
    let pictures = decode_all(decoder, encoded.iter().map(|frame| frame.data.as_slice()));
    assert_eq!(pictures.len(), FRAMES);
    let luma = (ROUND_TRIP.width * ROUND_TRIP.height) as usize;
    for (index, (timestamp, picture)) in pictures.into_iter().enumerate() {
        assert_eq!(timestamp, frame_time(index));
        let source = source(index);
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

fn open_encoder_for_test(preference: BackendPreference) -> Option<MfEncoder> {
    let selected = opened(open_encoder(ROUND_TRIP, preference))?;
    for attempt in &selected.rejected {
        println!("  skipped {attempt}");
    }
    let encoder = selected.codec;
    println!("encoder: {}", encoder.backend());
    assert_eq!(encoder.config(), &ROUND_TRIP);
    Some(encoder)
}

/// Decodes the committed H.264 fixture, re-encodes those exact pictures, and
/// decodes the result through the requested Media Foundation backend.
fn round_trip_fixture(preference: BackendPreference) {
    let Some(selected) = opened(open_decoder(preference)) else {
        return;
    };
    let expected_kind = match preference {
        BackendPreference::Hardware => BackendKind::Hardware,
        BackendPreference::Software => BackendKind::Software,
        BackendPreference::Auto => unreachable!(),
    };
    for attempt in &selected.rejected {
        println!("  skipped fixture source decoder: {attempt}");
    }
    let mut source_decoder = selected.codec;
    let source = decode_ipcm_fixture(&mut source_decoder);
    assert_eq!(source_decoder.backend().kind, expected_kind);
    println!("fixture source decoder: {}", source_decoder.backend());

    let Some(selected) = opened(open_encoder(FIXTURE_ENCODER, preference)) else {
        return;
    };
    for attempt in &selected.rejected {
        println!("  skipped fixture encoder: {attempt}");
    }
    let mut encoder = selected.codec;
    assert_eq!(encoder.backend().kind, expected_kind);
    println!("fixture encoder: {}", encoder.backend());
    let mut encoded = Vec::new();
    for (index, picture) in source.iter().enumerate() {
        let frame = Nv12::packed(IPCM_WIDTH, IPCM_HEIGHT, picture).unwrap();
        encoder
            .encode(&frame, frame_time(index), index == 0, &mut encoded)
            .unwrap();
    }
    encoder.flush(&mut encoded).unwrap();
    assert_eq!(encoded.len(), source.len());
    assert!(encoded[0].keyframe);

    let Some(selected) = opened(open_decoder(preference)) else {
        return;
    };
    for attempt in &selected.rejected {
        println!("  skipped fixture destination decoder: {attempt}");
    }
    let mut destination_decoder = selected.codec;
    let decoded = decode_all(
        &mut destination_decoder,
        encoded.iter().map(|frame| frame.data.as_slice()),
    );
    assert_eq!(destination_decoder.backend().kind, expected_kind);
    println!(
        "fixture destination decoder: {}",
        destination_decoder.backend()
    );
    assert_eq!(decoded.len(), source.len());
    let luma = (IPCM_WIDTH * IPCM_HEIGHT) as usize;
    for (index, (timestamp, picture)) in decoded.into_iter().enumerate() {
        assert_eq!(timestamp, frame_time(index));
        let (y, uv) = (
            psnr(&picture[..luma], &source[index][..luma]),
            psnr(&picture[luma..], &source[index][luma..]),
        );
        assert!(
            y > 32.0 && uv > 32.0,
            "fixture frame {index}: PSNR Y {y:.1} dB, UV {uv:.1} dB"
        );
    }
}

#[test]
fn software_encoder_round_trips_through_the_software_decoder() {
    round_trip_fixture(BackendPreference::Software);
    let Some(mut encoder) = open_encoder_for_test(BackendPreference::Software) else {
        return;
    };
    assert_eq!(encoder.backend().kind, BackendKind::Software);
    let encoded = encode_pattern(&mut encoder);
    let mut decoder = opened(open_decoder(BackendPreference::Software))
        .unwrap()
        .codec;
    check_decoded(&mut decoder, &encoded);
}

#[test]
#[ignore = "needs a GPU with a Media Foundation H.264 encoder and D3D11VA decoding"]
fn hardware_encoder_round_trips_through_hardware_and_software_decoders() {
    round_trip_fixture(BackendPreference::Hardware);
    let mut encoder = open_encoder_for_test(BackendPreference::Hardware).unwrap();
    assert_eq!(encoder.backend().kind, BackendKind::Hardware);
    let encoded = encode_pattern(&mut encoder);
    let mut hardware = open_decoder(BackendPreference::Hardware).unwrap().codec;
    check_decoded(&mut hardware, &encoded);
    println!("decoder: {}", hardware.backend());
    assert_eq!(hardware.backend().kind, BackendKind::Hardware);
    let mut software = open_decoder(BackendPreference::Software).unwrap().codec;
    check_decoded(&mut software, &encoded);
}

#[test]
fn automatic_selection_reports_the_backend_in_use() {
    let Some(encoder) = opened(open_encoder(ROUND_TRIP, BackendPreference::Auto)) else {
        return;
    };
    let Some(decoder) = opened(open_decoder(BackendPreference::Auto)) else {
        return;
    };
    for (role, backend, rejected) in [
        ("encoder", encoder.codec.backend(), &encoder.rejected),
        ("decoder", decoder.codec.backend(), &decoder.rejected),
    ] {
        println!("automatic {role}: {backend}");
        for attempt in rejected {
            println!("  skipped {attempt}");
        }
        assert_eq!(backend.api, API);
        assert!(!backend.name.is_empty());
        // Automatic mode only reaches software after hardware candidates
        // failed, and says which.
        if backend.kind == BackendKind::Hardware {
            assert!(
                rejected
                    .iter()
                    .all(|attempt| attempt.kind == BackendKind::Hardware)
            );
        }
    }
}

#[test]
fn invalid_input_is_rejected_before_the_transform() {
    assert!(matches!(
        open_encoder(
            EncoderConfig {
                width: 641,
                ..ROUND_TRIP
            },
            BackendPreference::Auto
        ),
        Err(OpenError::Invalid(CodecError::InvalidConfig(_)))
    ));
    let Some(selected) = opened(open_encoder(ROUND_TRIP, BackendPreference::Software)) else {
        return;
    };
    let mut encoder = selected.codec;
    let small = vec![128; 320 * 180 * 3 / 2];
    let frame = Nv12::packed(320, 180, &small).unwrap();
    assert_eq!(
        encoder.encode(&frame, Duration::ZERO, false, &mut Vec::new()),
        Err(CodecError::FrameSize {
            expected: (640, 360),
            actual: (320, 180)
        })
    );

    let mut decoder = opened(open_decoder(BackendPreference::Software))
        .unwrap()
        .codec;
    let mut never = |_: DecodedFrame<'_>| panic!("no picture expected");
    assert!(matches!(
        decoder.decode(&[0xFF, 0x00, 0x00, 0x01, 0x65], Duration::ZERO, &mut never),
        Err(CodecError::InvalidBitstream(_))
    ));
    // A picture larger than the supported limits (an 8192x1088 SPS) never
    // reaches the decoder.
    let mut oversized = Vec::new();
    let mut bits = test_vectors::BitWriter::default();
    bits.bits(66, 8);
    bits.bits(0xC0, 8);
    bits.bits(52, 8);
    bits.ue(0);
    bits.ue(0);
    bits.ue(2);
    bits.ue(1);
    bits.flag(false);
    bits.ue(8192 / 16 - 1);
    bits.ue(1080 / 16);
    bits.flag(true);
    bits.flag(true);
    bits.flag(false);
    bits.flag(false);
    bits.trailing();
    oversized.extend_from_slice(&[0, 0, 0, 1, 0x67]);
    oversized.extend(test_vectors::escape(&bits.finish()));
    oversized.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x80]);
    assert!(matches!(
        decoder.decode(&oversized, Duration::ZERO, &mut never),
        Err(CodecError::Unsupported(_))
    ));
    // The decoder is unaffected and still decodes the fixture.
    decode_ipcm_fixture(&mut decoder);
}

#[test]
fn helpers_parse_vendor_ids_convert_times_and_pack_strided_frames() {
    assert_eq!(parse_vendor_id("VEN_1002"), Some(0x1002));
    assert_eq!(parse_vendor_id("VEN_10DE"), Some(0x10DE));
    assert_eq!(parse_vendor_id("1002"), None);
    assert_eq!(parse_vendor_id("VEN_10022"), None);
    assert_eq!(to_hns(Duration::from_millis(1)), 10_000);
    assert_eq!(from_hns(10_000), Duration::from_millis(1));
    assert_eq!(from_hns(-5), Duration::ZERO);
    // A 4x2 frame with 6-byte luma rows packs to its visible bytes.
    let y = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8];
    let uv = [9, 10, 11, 12];
    let frame = Nv12::new(4, 2, &y, 6, &uv, 4).unwrap();
    let mut buffer = vec![0; frame.packed_len()];
    pack(&frame, &mut buffer);
    assert_eq!(buffer, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
}
