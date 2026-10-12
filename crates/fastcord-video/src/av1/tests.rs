use super::*;
use serde_json::Value;
use sha2::{Digest, Sha256};

const BASE: &[u8] = include_bytes!("../../../../fixtures/video/av1-lossless-420-8.ivf");
const BASE_PIXELS: &[u8] = include_bytes!("../../../../fixtures/video/av1-lossless-420-8.nv12");
const METADATA: &str = include_str!("../../../../fixtures/video/av1-fixture.json");
const FRAME_BYTES: usize = 1449;

struct Fixture {
    name: &'static str,
    encoded: &'static [u8],
    pixels: &'static [u8],
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "av1-lossless-420-8.ivf",
        encoded: BASE,
        pixels: BASE_PIXELS,
    },
    Fixture {
        name: "av1-lossless-420-10.ivf",
        encoded: include_bytes!("../../../../fixtures/video/av1-lossless-420-10.ivf"),
        pixels: include_bytes!("../../../../fixtures/video/av1-lossless-420-10.nv12"),
    },
    Fixture {
        name: "av1-lossless-420-12.ivf",
        encoded: include_bytes!("../../../../fixtures/video/av1-lossless-420-12.ivf"),
        pixels: include_bytes!("../../../../fixtures/video/av1-lossless-420-12.nv12"),
    },
    Fixture {
        name: "av1-lossless-mono-8.ivf",
        encoded: include_bytes!("../../../../fixtures/video/av1-lossless-mono-8.ivf"),
        pixels: include_bytes!("../../../../fixtures/video/av1-lossless-mono-8.nv12"),
    },
    Fixture {
        name: "av1-lossless-422-10.ivf",
        encoded: include_bytes!("../../../../fixtures/video/av1-lossless-422-10.ivf"),
        pixels: include_bytes!("../../../../fixtures/video/av1-lossless-422-10.nv12"),
    },
    Fixture {
        name: "av1-lossless-444-8.ivf",
        encoded: include_bytes!("../../../../fixtures/video/av1-lossless-444-8.ivf"),
        pixels: include_bytes!("../../../../fixtures/video/av1-lossless-444-8.nv12"),
    },
];

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn packets(ivf: &[u8]) -> Vec<(Duration, &[u8])> {
    packets_sized(ivf, 35, 27)
}

fn packets_sized(mut ivf: &[u8], width: u16, height: u16) -> Vec<(Duration, &[u8])> {
    assert_eq!(&ivf[..4], b"DKIF");
    assert_eq!(&ivf[8..12], b"AV01");
    assert_eq!(u16::from_le_bytes(ivf[12..14].try_into().unwrap()), width);
    assert_eq!(u16::from_le_bytes(ivf[14..16].try_into().unwrap()), height);
    let rate = u32::from_le_bytes(ivf[16..20].try_into().unwrap());
    let scale = u32::from_le_bytes(ivf[20..24].try_into().unwrap());
    let header = u16::from_le_bytes(ivf[6..8].try_into().unwrap());
    ivf = &ivf[usize::from(header)..];
    let mut result = Vec::new();
    while !ivf.is_empty() {
        let len = u32::from_le_bytes(ivf[..4].try_into().unwrap()) as usize;
        let timestamp = u64::from_le_bytes(ivf[4..12].try_into().unwrap());
        let nanos = timestamp * u64::from(scale) * 1_000_000_000 / u64::from(rate);
        result.push((Duration::from_nanos(nanos), &ivf[12..12 + len]));
        ivf = &ivf[12 + len..];
    }
    result
}

/// Lossy streams (130x66) with the encoder's in-loop filters, hidden and
/// show-existing frames, film grain and screen content. Their expected
/// pictures are libaom's reference decoder output (fixtures/video/README via
/// `gen_av1_fixture.py`), recorded as hashes in the manifest.
const LOSSY: &[(&str, &[u8])] = &[
    (
        "av1-lossy-420-8.ivf",
        include_bytes!("../../../../fixtures/video/av1-lossy-420-8.ivf"),
    ),
    (
        "av1-lagged-420-8.ivf",
        include_bytes!("../../../../fixtures/video/av1-lagged-420-8.ivf"),
    ),
    (
        "av1-grain-420-8.ivf",
        include_bytes!("../../../../fixtures/video/av1-grain-420-8.ivf"),
    ),
    (
        "av1-screen-420-8.ivf",
        include_bytes!("../../../../fixtures/video/av1-screen-420-8.ivf"),
    ),
];

fn pool_stats(decoder: &Decoder) -> (usize, usize) {
    let Some(handle) = &decoder.handle else {
        return (0, 0);
    };
    // SAFETY: the test observes counters on the uniquely-owned live handle;
    // no decoding or allocator callback runs concurrently.
    unsafe {
        (
            fc_av1_pool_bytes(handle.0.as_ptr()),
            fc_av1_pool_allocations(handle.0.as_ptr()),
        )
    }
}

fn assert_picture(frame: DecodedFrame<'_>, expected: &[u8], timestamp: Duration) {
    assert_eq!((frame.image.width(), frame.image.height()), (35, 27));
    assert_eq!(frame.timestamp, timestamp);
    let mut packed = Vec::with_capacity(FRAME_BYTES);
    frame.image.copy_packed_into(&mut packed);
    // A one-line failure beats a 1449-element array diff.
    assert_eq!(packed.len(), expected.len());
    if let Some(index) = packed.iter().zip(expected).position(|(a, b)| a != b) {
        let wrong = packed.iter().zip(expected).filter(|(a, b)| a != b).count();
        panic!(
            "{wrong} of {} bytes differ; first at {index}: decoded {} expected {}",
            expected.len(),
            packed[index],
            expected[index]
        );
    }
}

#[test]
fn genuine_libaom_streams_match_independent_pixels_and_hashes() {
    let metadata: Value = serde_json::from_str(METADATA).unwrap();
    assert_eq!(
        metadata["provenance"]["matches_pinned_windows_encoder"],
        true
    );
    for fixture in FIXTURES {
        let manifest = metadata["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|stream| stream["encoded"] == fixture.name)
            .unwrap();
        assert_eq!(hash(fixture.encoded), manifest["encoded_sha256"]);
        assert_eq!(hash(fixture.pixels), manifest["expected_nv12_sha256"]);
        let mut decoder = Decoder::open().unwrap();
        assert!(decoder.handle.is_none(), "native construction must be lazy");
        assert_eq!(decoder.backend().kind, BackendKind::Software);
        assert_eq!(decoder.backend().api, API);
        let mut pictures = 0;
        for (index, (timestamp, packet)) in packets(fixture.encoded).into_iter().enumerate() {
            let expected = &fixture.pixels[index * FRAME_BYTES..(index + 1) * FRAME_BYTES];
            let mut emitted = 0;
            decoder
                .decode(packet, timestamp, &mut |frame| {
                    assert_picture(frame, expected, timestamp);
                    assert_eq!(hash(expected), manifest["pictures"][index]["nv12_sha256"]);
                    assert_eq!(
                        frame.image.luma_row(0)[0],
                        manifest["pictures"][index]["pixels"]["y_0_0"]
                    );
                    emitted += 1;
                    pictures += 1;
                })
                .unwrap();
            assert_eq!(
                emitted, 1,
                "synchronous real decode must produce the picture"
            );
        }
        decoder.flush(&mut |_| panic!("already drained")).unwrap();
        assert_eq!(pictures, 3, "{}", fixture.name);
        let (bytes, _) = pool_stats(&decoder);
        assert!(bytes > 0 && bytes <= MAX_PICTURE_POOL_BYTES);
        assert!(decoder.luma.capacity() <= MAX_PIXELS as usize);
        assert!(decoder.chroma.capacity() <= MAX_PIXELS as usize);
    }
}

#[test]
fn input_timestamps_are_preserved_without_monotonicity_or_precision_loss() {
    let timestamps = [
        Duration::from_nanos(i64::MAX as u64 - 7),
        Duration::ZERO,
        Duration::from_nanos(91_234_567),
    ];
    let mut decoder = Decoder::open().unwrap();
    let mut count = 0;
    for (index, (_, packet)) in packets(BASE).into_iter().enumerate() {
        let expected = &BASE_PIXELS[index * FRAME_BYTES..(index + 1) * FRAME_BYTES];
        decoder
            .decode(packet, timestamps[index], &mut |frame| {
                assert_picture(frame, expected, timestamps[index]);
                count += 1;
            })
            .unwrap();
        // flush is drain, not dav1d_flush (which would discard references).
        decoder.flush(&mut |_| panic!("duplicate picture")).unwrap();
    }
    assert_eq!(count, 3);
    decoder.reset();
    assert!(matches!(
        decoder.decode(
            packets(BASE)[0].1,
            Duration::from_secs(u64::MAX),
            &mut |_| {}
        ),
        Err(CodecError::InvalidConfig(_))
    ));
    assert!(decoder.handle.is_none());
}

#[test]
fn delimiter_is_optional_and_eight_bit_luma_is_borrowed() {
    let mut decoder = Decoder::open().unwrap();
    let packet = packets(BASE)[0].1;
    assert_eq!(&packet[..2], &[0x12, 0]);
    let mut count = 0;
    decoder
        .decode(&packet[2..], Duration::ZERO, &mut |frame| {
            assert_picture(frame, &BASE_PIXELS[..FRAME_BYTES], Duration::ZERO);
            assert!(
                frame.image.luma_stride() >= 128,
                "borrow the padded native plane"
            );
            assert_eq!(frame.image.chroma_stride(), 36);
            count += 1;
        })
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        decoder.luma.capacity(),
        0,
        "8-bit luma must never be copied"
    );
}

#[test]
fn pools_and_scratch_are_reused_after_warmup() {
    let mut decoder = Decoder::open().unwrap();
    let input = packets(BASE);
    let mut warmed = None;
    for cycle in 0..12 {
        for &(timestamp, packet) in &input {
            decoder.decode(packet, timestamp, &mut |_| {}).unwrap();
        }
        let current = (
            pool_stats(&decoder),
            decoder.chroma.as_ptr(),
            decoder.chroma.capacity(),
        );
        if cycle == 2 {
            warmed = Some(current);
        } else if cycle > 2 {
            assert_eq!(
                Some(current),
                warmed,
                "no steady-state pool/scratch allocation"
            );
        }
        assert!(current.0.0 <= MAX_PICTURE_POOL_BYTES);
    }
}

#[test]
fn reset_and_drop_allow_independent_stream_restarts() {
    for _ in 0..20 {
        let mut decoder = Decoder::open().unwrap();
        for _ in 0..2 {
            let mut count = 0;
            for (index, (timestamp, packet)) in packets(BASE).into_iter().enumerate() {
                decoder
                    .decode(packet, timestamp, &mut |frame| {
                        assert_picture(
                            frame,
                            &BASE_PIXELS[index * FRAME_BYTES..(index + 1) * FRAME_BYTES],
                            timestamp,
                        );
                        count += 1;
                    })
                    .unwrap();
            }
            assert_eq!(count, 3);
            assert!(pool_stats(&decoder).0 > 0);
            decoder.reset();
            assert!(decoder.handle.is_none());
            assert_eq!(decoder.luma.capacity(), 0);
            assert_eq!(decoder.chroma.capacity(), 0);
        }
        // Drop a live native reference/pool as well, not only reset owners.
        decoder
            .decode(packets(BASE)[0].1, Duration::ZERO, &mut |_| {})
            .unwrap();
    }
}

#[test]
fn encoded_byte_cap_and_malformed_obu_framing_precede_native_allocation() {
    let mut decoder = Decoder::open().unwrap();
    let malformed: &[&[u8]] = &[
        &[],
        &[0x0a],
        &[0x0a, 0x80],
        &[0x0e],
        &[0x0e, 0x07, 0],
        &[0x8a, 0],
        &[0x0b, 0],
        &[0x08, 0],
        &[0x32, 3, 0],
        &[0x0a, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0],
    ];
    for bytes in malformed {
        assert!(matches!(
            decoder.decode(bytes, Duration::ZERO, &mut |_| panic!("invalid frame")),
            Err(CodecError::InvalidBitstream(_))
        ));
        assert!(decoder.handle.is_none());
    }
    let oversized = vec![0u8; MAX_ENCODED_BYTES + 1];
    assert!(matches!(
        decoder.decode(&oversized, Duration::ZERO, &mut |_| panic!(
            "oversized frame"
        )),
        Err(CodecError::Unsupported(_))
    ));
    assert!(decoder.handle.is_none());
    // One padding OBU exactly at the input byte cap: a three-byte LEB128
    // encodes MAX_ENCODED_BYTES - 4 bytes of payload.
    let mut at_limit = vec![0u8; MAX_ENCODED_BYTES];
    at_limit[..4].copy_from_slice(&[0x7a, 0xfc, 0xff, 0x7f]);
    assert_eq!(check_temporal_unit(&at_limit), Ok(false));
}

#[test]
fn native_parser_rejects_malformed_sequence_and_recovers_on_keyframe() {
    let mut decoder = Decoder::open().unwrap();
    // Sized OBU framing is valid, but profile 7 and the truncated sequence
    // header payload are invalid AV1 syntax. This reaches real dav1d parsing.
    assert!(matches!(
        decoder.decode(&[0x0a, 1, 0xff], Duration::ZERO, &mut |_| panic!(
            "invalid picture"
        )),
        Err(CodecError::InvalidBitstream(_))
    ));
    assert_eq!(pool_stats(&decoder).0, 0);
    let mut count = 0;
    decoder
        .decode(packets(BASE)[0].1, Duration::ZERO, &mut |frame| {
            assert_picture(frame, &BASE_PIXELS[..FRAME_BYTES], Duration::ZERO);
            count += 1;
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn genuine_oversized_headers_fail_before_any_native_picture_allocation() {
    let headers: &[&[u8]] = &[
        include_bytes!("../../../../fixtures/video/av1-too-wide.obu"),
        include_bytes!("../../../../fixtures/video/av1-too-tall.obu"),
        include_bytes!("../../../../fixtures/video/av1-too-large-area.obu"),
    ];
    for header in headers {
        let mut decoder = Decoder::open().unwrap();
        assert!(matches!(
            decoder.decode(header, Duration::ZERO, &mut |_| panic!("oversized picture")),
            Err(CodecError::Unsupported(_))
        ));
        assert_eq!(pool_stats(&decoder), (0, 0));
    }
}

#[test]
fn native_picture_allocator_enforces_its_byte_cap() {
    let mut decoder = Decoder::open().unwrap();
    decoder.pool_limit = 64; // Less than one aligned, padded 35x27 picture.
    assert_eq!(
        decoder.decode(packets(BASE)[0].1, Duration::ZERO, &mut |_| panic!(
            "over-budget picture"
        )),
        Err(CodecError::Unsupported(
            "AV1 native picture pool byte limit exceeded"
        ))
    );
    assert_eq!(pool_stats(&decoder), (0, 0));
    decoder.reset();
    decoder.pool_limit = MAX_PICTURE_POOL_BYTES;
    let mut count = 0;
    decoder
        .decode(packets(BASE)[0].1, Duration::ZERO, &mut |_| count += 1)
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn identity_rgb_is_explicitly_rejected_instead_of_mislabeled_nv12() {
    let encoded = include_bytes!("../../../../fixtures/video/av1-rgb-identity.ivf");
    let mut decoder = Decoder::open().unwrap();
    assert_eq!(
        decoder.decode(packets(encoded)[0].1, Duration::ZERO, &mut |_| panic!(
            "GBR is not NV12"
        )),
        Err(CodecError::Unsupported(
            "AV1 RGB/identity matrix cannot be output as NV12"
        ))
    );
}

#[test]
fn filtered_lossy_streams_match_the_reference_decoder_bit_for_bit() {
    let metadata: Value = serde_json::from_str(METADATA).unwrap();
    assert_eq!(
        metadata["lossy_streams"].as_array().unwrap().len(),
        LOSSY.len()
    );
    for &(name, encoded) in LOSSY {
        let manifest = lossy_manifest(&metadata, name);
        assert_eq!(hash(encoded), manifest["encoded_sha256"], "{name}");
        let width = u32::try_from(manifest["width"].as_u64().unwrap()).unwrap();
        let height = u32::try_from(manifest["height"].as_u64().unwrap()).unwrap();
        let pictures = manifest["pictures"].as_array().unwrap();
        let units = packets_sized(encoded, width as u16, height as u16);
        assert_eq!(units.len(), pictures.len(), "{name}");
        let mut decoder = Decoder::open().unwrap();
        let mut all = Vec::new();
        for (index, (timestamp, unit)) in units.into_iter().enumerate() {
            let mut emitted = 0;
            decoder
                .decode(unit, timestamp, &mut |frame| {
                    assert_eq!((frame.image.width(), frame.image.height()), (width, height));
                    assert_eq!(frame.timestamp, timestamp, "{name} unit {index}");
                    let mut packed = Vec::new();
                    frame.image.copy_packed_into(&mut packed);
                    assert_eq!(
                        hash(&packed),
                        pictures[index]["nv12_sha256"],
                        "{name} picture {index}"
                    );
                    all.extend_from_slice(&packed);
                    emitted += 1;
                })
                .unwrap();
            // Hidden frames and show-existing-frame units still produce exactly
            // one picture per temporal unit.
            assert_eq!(emitted, 1, "{name} unit {index}");
        }
        assert_eq!(hash(&all), manifest["expected_nv12_sha256"], "{name}");
        decoder.flush(&mut |_| panic!("already drained")).unwrap();
        assert!(pool_stats(&decoder).0 <= MAX_PICTURE_POOL_BYTES);
    }
}

fn lossy_manifest<'a>(metadata: &'a Value, name: &str) -> &'a Value {
    metadata["lossy_streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stream| stream["encoded"] == name)
        .unwrap()
}

/// Truncations and one flipped bit per sampled byte of `unit`: dense for a
/// keyframe (which carries every header and the whole intra syntax), sparse for
/// the other units.
fn damaged(unit: &[u8], dense: bool) -> Vec<Vec<u8>> {
    let step = if dense { 3 } else { (unit.len() / 12).max(1) };
    let flip_step = if dense { 5 } else { step };
    let mut cases: Vec<Vec<u8>> = (0..unit.len())
        .step_by(step)
        .map(|end| unit[..end].to_vec())
        .collect();
    for position in (0..unit.len()).step_by(flip_step) {
        let mut flipped = unit.to_vec();
        flipped[position] ^= 1 << (position % 8);
        cases.push(flipped);
    }
    cases
}

/// Damaged temporal units go through real dav1d. Any answer is acceptable
/// except a crash, an unbounded pool, or a decoder that no longer decodes: the
/// next keyframe must produce the exact first picture again.
#[test]
fn damaged_temporal_units_never_crash_the_decoder_and_a_keyframe_always_recovers() {
    let metadata: Value = serde_json::from_str(METADATA).unwrap();
    let mut streams = vec![(BASE, 35, 27, hash(&BASE_PIXELS[..FRAME_BYTES]))];
    for &(name, encoded) in LOSSY {
        let manifest = lossy_manifest(&metadata, name);
        streams.push((
            encoded,
            manifest["width"].as_u64().unwrap() as u16,
            manifest["height"].as_u64().unwrap() as u16,
            manifest["pictures"][0]["nv12_sha256"]
                .as_str()
                .unwrap()
                .to_owned(),
        ));
    }
    for (stream, (encoded, width, height, first)) in streams.into_iter().enumerate() {
        let units = packets_sized(encoded, width, height);
        let keyframe = units[0];
        let mut decoder = Decoder::open().unwrap();
        for (index, &(timestamp, unit)) in units.iter().enumerate() {
            for (number, case) in damaged(unit, index == 0).into_iter().enumerate() {
                // The reference chain first, so inter frames have references.
                for &(earlier, bytes) in &units[..index] {
                    let _ = decoder.decode(bytes, earlier, &mut |_| {});
                }
                let outcome = decoder.decode(&case, timestamp, &mut |_| {});
                let mut recovered = 0;
                decoder
                    .decode(keyframe.1, keyframe.0, &mut |frame| {
                        let mut packed = Vec::new();
                        frame.image.copy_packed_into(&mut packed);
                        assert_eq!(hash(&packed), first);
                        recovered += 1;
                    })
                    .unwrap_or_else(|error| {
                        panic!(
                            "stream {stream} unit {index} case {number} ({} bytes, first \
                             answered {outcome:?}): the keyframe after it failed: {error:?}",
                            case.len()
                        )
                    });
                assert_eq!(recovered, 1);
                assert!(pool_stats(&decoder).0 <= MAX_PICTURE_POOL_BYTES);
            }
        }
    }
}

/// A keyframe whose `show_frame` bit is flipped (byte 20: the frame header's first
/// byte) shifts every later header field, and dav1d takes the frame for one still
/// waiting for tile groups. It then keeps a reference to the input buffer, which a
/// bridge that only flushed could never get past: every later keyframe, of any
/// stream, was refused. A failed decode discards the context instead.
#[test]
fn a_keyframe_dav1d_keeps_waiting_on_does_not_wedge_the_decoder() {
    let metadata: Value = serde_json::from_str(METADATA).unwrap();
    let name = "av1-screen-420-8.ivf";
    let manifest = lossy_manifest(&metadata, name);
    let encoded = LOSSY.iter().find(|(stream, _)| *stream == name).unwrap().1;
    let units = packets_sized(
        encoded,
        manifest["width"].as_u64().unwrap() as u16,
        manifest["height"].as_u64().unwrap() as u16,
    );
    let (timestamp, keyframe) = units[0];
    let mut damaged = keyframe.to_vec();
    damaged[20] ^= 1 << 4;
    let mut decoder = Decoder::open().unwrap();
    assert_eq!(
        decoder.decode(&damaged, timestamp, &mut |_| panic!("not a picture")),
        Err(CodecError::InvalidBitstream(
            "dav1d rejected malformed or incomplete AV1"
        ))
    );
    assert!(decoder.handle.is_none(), "the failed context was discarded");
    // The same stream's keyframe, then another stream's, decode exactly.
    let mut count = 0;
    decoder
        .decode(keyframe, timestamp, &mut |frame| {
            let mut packed = Vec::new();
            frame.image.copy_packed_into(&mut packed);
            assert_eq!(hash(&packed), manifest["pictures"][0]["nv12_sha256"]);
            count += 1;
        })
        .unwrap();
    assert_eq!(count, 1);
    decoder
        .decode(packets(BASE)[0].1, Duration::ZERO, &mut |frame| {
            assert_picture(frame, &BASE_PIXELS[..FRAME_BYTES], Duration::ZERO);
            count += 1;
        })
        .unwrap();
    assert_eq!(count, 2);
}
