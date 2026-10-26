use std::io::Cursor;
use std::time::Duration;

use super::*;
use crate::h264::{nal_unit_type, nal_units};
use crate::{BackendPreference, VideoDecoder};

const MP4: &[u8] = include_bytes!("../../../../fixtures/video/container-h264-opus.mp4");
const MP4_AAC_LC: &[u8] = include_bytes!("../../../../fixtures/video/container-h264-aac-lc.mp4");
const MP4_AAC: &[u8] = include_bytes!("../../../../fixtures/video/container-h264-aac.mp4");
const WEBM: &[u8] = include_bytes!("../../../../fixtures/video/container-av1-opus.webm");
const H264: &[u8] = include_bytes!("../../../../fixtures/video/ipcm-128x96.h264");
const AV1_PIXELS: &[u8] = include_bytes!("../../../../fixtures/video/av1-lossless-420-8.nv12");

fn index(bytes: &[u8]) -> Result<Media, ContainerError> {
    open(&mut Cursor::new(bytes.to_vec()))
}

fn video_of(bytes: &[u8]) -> VideoTrack {
    index(bytes).unwrap().video.unwrap()
}

fn pts(samples: &[Sample]) -> Vec<u32> {
    samples.iter().map(|s| s.pts_ms).collect()
}

/// Replaces the first occurrence of `from` (same length as `to`) and returns the bytes.
fn patched(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    assert_eq!(from.len(), to.len());
    let at = bytes
        .windows(from.len())
        .position(|w| w == from)
        .expect("pattern present");
    let mut out = bytes.to_vec();
    out[at..at + to.len()].copy_from_slice(to);
    out
}

#[test]
fn sniffing_goes_by_content_not_by_name() {
    assert_eq!(sniff(MP4), Some(Format::Mp4));
    assert_eq!(sniff(WEBM), Some(Format::Matroska));
    assert_eq!(sniff(b"OggS\0\x02\0\0"), None);
    assert_eq!(sniff(b""), None);
    assert_eq!(
        index(b"plain text, not a video").unwrap_err(),
        ContainerError::UnknownFormat
    );
}

#[test]
fn mp4_with_h264_and_opus_is_indexed_with_composition_offsets_and_edit_list() {
    let media = index(MP4).unwrap();
    let video = media.video.unwrap();
    assert_eq!(video.codec, VideoCodecId::H264);
    assert_eq!((video.width, video.height), (128, 96));
    // ctts delays every picture by 6000 ticks and the edit list takes it back.
    assert_eq!(pts(&video.samples), [0, 33, 66]);
    assert_eq!(
        video.samples.iter().map(|s| s.key).collect::<Vec<_>>(),
        [true, false, true]
    );
    let audio = media.audio.unwrap();
    assert_eq!(audio.channels, 2);
    assert_eq!(pts(&audio.samples), [0, 20, 40, 60, 80, 100]);
    assert_eq!(media.duration_ms, 100);
    assert_eq!(media.audio_skipped, None);
}

#[test]
fn mp4_samples_come_back_as_annex_b_with_parameter_sets_before_keyframes() {
    let video = video_of(MP4);
    let mut reader = Cursor::new(MP4);
    let (mut stored, mut unit) = (Vec::new(), Vec::new());
    let expected: Vec<&[u8]> = nal_units(H264).collect();
    assert_eq!(
        expected
            .iter()
            .map(|n| nal_unit_type(n))
            .collect::<Vec<_>>(),
        [7, 8, 5, 1, 5]
    );

    read_sample(&mut reader, &video.samples[0], &mut stored).unwrap();
    video.access_unit(&stored, true, &mut unit).unwrap();
    assert_eq!(nal_units(&unit).collect::<Vec<_>>(), expected[..3]);

    read_sample(&mut reader, &video.samples[1], &mut stored).unwrap();
    video.access_unit(&stored, false, &mut unit).unwrap();
    assert_eq!(nal_units(&unit).collect::<Vec<_>>(), expected[3..4]);

    read_sample(&mut reader, &video.samples[2], &mut stored).unwrap();
    video.access_unit(&stored, true, &mut unit).unwrap();
    let units: Vec<_> = nal_units(&unit).map(nal_unit_type).collect();
    assert_eq!(units, [7, 8, 5]);
}

#[test]
fn the_h264_samples_decode_with_the_software_decoder() {
    let video = video_of(MP4);
    let mut decoder = crate::openh264::open_decoder(BackendPreference::Software)
        .unwrap()
        .codec;
    let mut reader = Cursor::new(MP4);
    let (mut stored, mut unit) = (Vec::new(), Vec::new());
    let mut sizes = Vec::new();
    for sample in &video.samples {
        read_sample(&mut reader, sample, &mut stored).unwrap();
        video.access_unit(&stored, sample.key, &mut unit).unwrap();
        decoder
            .decode(
                &unit,
                Duration::from_millis(sample.pts_ms.into()),
                &mut |frame| {
                    sizes.push((frame.image.width(), frame.image.height()));
                },
            )
            .unwrap();
    }
    decoder
        .flush(&mut |frame| sizes.push((frame.image.width(), frame.image.height())))
        .unwrap();
    assert_eq!(sizes, [(128, 96); 3]);
}

#[test]
fn aac_lc_is_an_audio_track_with_its_configuration() {
    let media = index(MP4_AAC_LC).unwrap();
    assert!(media.video.is_some());
    assert_eq!(media.audio_skipped, None);
    let audio = media.audio.unwrap();
    assert_eq!(audio.codec, AudioCodecId::Aac);
    assert_eq!((audio.rate, audio.channels), (44_100, 2));
    assert_eq!(audio.config, [0x12, 0x10]);
    assert_eq!(audio.samples.len(), 22);
    // 1024 samples at 44.1 kHz: 0, 23, 46, 69 ... ms.
    assert_eq!(pts(&audio.samples)[..4], [0, 23, 46, 69]);
    assert_eq!(audio.index_at(100), 5);
}

#[test]
fn aac_other_than_lc_is_reported_skipped_and_the_video_stays() {
    // Object type 5 (SBR) instead of 2 (LC) in the AudioSpecificConfig.
    let mut bytes = MP4_AAC.to_vec();
    let at = bytes
        .windows(4)
        .position(|w| w == [5, 2, 0x12, 0x10])
        .unwrap();
    bytes[at + 2] = 0x2A;
    let media = index(&bytes).unwrap();
    assert!(media.video.is_some());
    assert!(media.audio.is_none());
    assert_eq!(media.audio_skipped, Some(AudioSkipped::Aac));
}

#[test]
fn the_aac_configuration_is_checked() {
    assert_eq!(aac_lc_info(&[0x12, 0x10]), Some((44_100, 2)));
    assert_eq!(aac_lc_info(&[0x11, 0x88]), Some((48_000, 1)));
    // SBR object type, 7 channels, 960-sample frames, explicit rate of 0, truncated.
    assert_eq!(aac_lc_info(&[0x2A, 0x10]), None);
    assert_eq!(aac_lc_info(&[0x12, 0x38]), None);
    assert_eq!(aac_lc_info(&[0x12, 0x14]), None);
    assert_eq!(aac_lc_info(&[0x17, 0x80, 0x00, 0x00, 0x10]), None);
    assert_eq!(aac_lc_info(&[0x12]), None);
    assert_eq!(aac_lc_info(&[]), None);
}

#[test]
fn webm_with_av1_and_opus_is_indexed_across_clusters_and_block_groups() {
    let media = index(WEBM).unwrap();
    let video = media.video.unwrap();
    assert_eq!(video.codec, VideoCodecId::Av1);
    assert_eq!((video.width, video.height), (35, 27));
    assert_eq!(pts(&video.samples), [0, 33, 66]);
    // The BlockGroup with a ReferenceBlock is a delta frame, like the unflagged SimpleBlock.
    assert_eq!(
        video.samples.iter().map(|s| s.key).collect::<Vec<_>>(),
        [true, false, false]
    );
    let audio = media.audio.unwrap();
    assert_eq!(pts(&audio.samples), [0, 20, 40, 60, 80, 100]);
    assert_eq!(media.duration_ms, 120);
}

#[test]
fn webm_av1_samples_decode_to_the_reference_pictures() {
    let video = video_of(WEBM);
    let mut decoder = crate::av1::Decoder::open().unwrap();
    let mut reader = Cursor::new(WEBM);
    let (mut stored, mut unit) = (Vec::new(), Vec::new());
    let mut pictures: Vec<Vec<u8>> = Vec::new();
    for sample in &video.samples {
        read_sample(&mut reader, sample, &mut stored).unwrap();
        video.access_unit(&stored, sample.key, &mut unit).unwrap();
        decoder
            .decode(
                &unit,
                Duration::from_millis(sample.pts_ms.into()),
                &mut |frame| {
                    let mut bytes = Vec::new();
                    frame.image.copy_packed_into(&mut bytes);
                    pictures.push(bytes);
                },
            )
            .unwrap();
    }
    let frame_len = AV1_PIXELS.len() / 3;
    assert_eq!(pictures.len(), 3);
    for (picture, expected) in pictures.iter().zip(AV1_PIXELS.chunks(frame_len)) {
        assert_eq!(picture, expected);
    }
}

#[test]
fn seeking_picks_the_keyframe_before_the_time_and_the_packet_after_it() {
    let media = index(MP4).unwrap();
    let video = media.video.unwrap();
    assert_eq!(video.keyframe_for(0), 0);
    assert_eq!(video.keyframe_for(50), 0);
    assert_eq!(video.keyframe_for(66), 2);
    assert_eq!(video.keyframe_for(5_000), 2);
    let audio = media.audio.unwrap();
    assert_eq!(audio.index_at(0), 0);
    assert_eq!(audio.index_at(45), 3);
    assert_eq!(audio.index_at(100), 5);
    assert_eq!(audio.index_at(101), 6);
}

#[test]
fn unsupported_codecs_and_layouts_are_named() {
    // HEVC and VP9 video: Download only.
    assert_eq!(
        index(&patched(MP4, b"avc1      ", b"hvc1      ")).unwrap_err(),
        ContainerError::UnsupportedVideo
    );
    assert_eq!(
        index(&patched(WEBM, b"V_AV1", b"V_VP9")).unwrap_err(),
        ContainerError::UnsupportedVideo
    );
    // Another audio codec: the video plays without sound.
    let flac = index(&patched(WEBM, b"A_OPUS", b"A_FLAC")).unwrap();
    assert!(flac.audio.is_none() && flac.video.is_some());
    assert_eq!(flac.audio_skipped, Some(AudioSkipped::Other));
    // Fragmented MP4.
    let mut fragmented = MP4.to_vec();
    fragmented.extend_from_slice(&[0, 0, 0, 8, b'm', b'o', b'o', b'f']);
    assert_eq!(index(&fragmented).unwrap_err(), ContainerError::Fragmented);
    // No tracks at all.
    let mut empty = Vec::new();
    empty.extend_from_slice(&[0, 0, 0, 8, b'f', b't', b'y', b'p']);
    assert!(index(&empty).is_err());
}

#[test]
fn an_unknown_size_cluster_ends_at_the_next_cluster() {
    let mut bytes = WEBM.to_vec();
    let id = [0x1F, 0x43, 0xB6, 0x75];
    let at = bytes.windows(4).position(|w| w == id).unwrap() + 4;
    // The fixture writes the first cluster's size in two bytes; make it "unknown".
    let length = bytes[at].leading_zeros() as usize + 1;
    bytes[at] = 0xFF >> (length - 1);
    for byte in &mut bytes[at + 1..at + length] {
        *byte = 0xFF;
    }
    let media = index(&bytes).unwrap();
    assert_eq!(media.video.unwrap().samples.len(), 3);
    assert_eq!(media.audio.unwrap().samples.len(), 6);
}

#[test]
fn hostile_sample_counts_are_refused_before_allocating() {
    let at = MP4.windows(4).position(|w| w == b"stsz").unwrap() + 4 + 8;
    for count in [u32::MAX, MAX_SAMPLES as u32 + 1, 100_000] {
        let mut bytes = MP4.to_vec();
        bytes[at..at + 4].copy_from_slice(&count.to_be_bytes());
        assert!(
            matches!(
                index(&bytes),
                Err(ContainerError::TooLarge | ContainerError::Malformed)
            ),
            "{count}"
        );
    }
    // A sample table that points past the end of the file.
    let at = MP4.windows(4).position(|w| w == b"stco").unwrap() + 4 + 8;
    let mut bytes = MP4.to_vec();
    bytes[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(index(&bytes).unwrap_err(), ContainerError::Malformed);
}

#[test]
fn truncated_files_never_panic_and_truncated_mp4_is_refused() {
    for len in (0..MP4.len()).step_by(7) {
        assert!(index(&MP4[..len]).is_err(), "mp4 prefix {len}");
    }
    for len in 0..WEBM.len() {
        // A cut between clusters is a shorter, valid file; every other cut must be refused
        // or yield only samples that lie inside the cut.
        if let Ok(media) = index(&WEBM[..len]) {
            let end = len as u64;
            for samples in [
                media.video.as_ref().map(|v| &v.samples),
                media.audio.as_ref().map(|a| &a.samples),
            ]
            .into_iter()
            .flatten()
            {
                assert!(samples.iter().all(|s| s.offset + u64::from(s.size) <= end));
            }
        }
    }
}

#[test]
fn corrupted_files_never_panic() {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for file in [MP4, MP4_AAC, MP4_AAC_LC, WEBM] {
        // Only the leading metadata matters for indexing; flip bytes through all of it.
        let metadata = file.len().min(if file.len() > 10_000 {
            1_800
        } else {
            file.len()
        });
        for _ in 0..4_000 {
            let mut bytes = file.to_vec();
            for _ in 0..1 + next() % 3 {
                let at = (next() as usize) % metadata;
                bytes[at] = next() as u8;
            }
            let _ = index(&bytes);
        }
    }
}

#[test]
fn a_webm_of_millions_of_void_elements_is_refused_quickly() {
    // EBML header (DocType "webm"), a Segment of unknown size, then 2 M
    // empty Void elements: only the element cap stops the walk (#124).
    let mut file = vec![0x1A, 0x45, 0xDF, 0xA3, 0x87, 0x42, 0x82, 0x84];
    file.extend_from_slice(b"webm");
    file.extend_from_slice(&[
        0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    ]);
    for _ in 0..2_000_000 {
        file.extend_from_slice(&[0xEC, 0x80]);
    }
    let started = std::time::Instant::now();
    assert_eq!(index(&file).unwrap_err(), ContainerError::TooLarge);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}
