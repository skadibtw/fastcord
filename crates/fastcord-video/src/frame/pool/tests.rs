use super::*;
use crate::frame::{ColorMatrix, ColorRange};

fn picture<'a>(width: u32, height: u32, pixels: &'a [u8]) -> DecodedFrame<'a> {
    DecodedFrame {
        image: Nv12::packed(width, height, pixels).unwrap(),
        color: ColorDescription::default(),
        timestamp: Duration::from_millis(17),
    }
}

fn packed_len(width: u32, height: u32) -> usize {
    width as usize * height as usize + width.div_ceil(2) as usize * 2 * height.div_ceil(2) as usize
}

#[test]
fn lease_is_immutable_after_the_borrowed_callback_and_keeps_metadata() {
    let mut pool = FramePool::new(FrameFormat::Nv12);
    let mut source = vec![37; packed_len(3, 3)];
    let mut frame = picture(3, 3, &source);
    frame.color = ColorDescription {
        matrix: ColorMatrix::Bt601,
        range: ColorRange::Full,
        pixel_aspect_ratio: (4, 3),
    };
    let first = pool.lease(frame).unwrap();
    let expected = first.pixels().to_vec();
    source.fill(92);
    let second = pool.lease(picture(3, 3, &source)).unwrap();
    assert_eq!(first.pixels(), expected);
    assert_eq!(second.pixels(), source);
    assert_eq!((first.width(), first.height()), (3, 3));
    assert_eq!(first.timestamp(), Duration::from_millis(17));
    assert_eq!(first.color().pixel_aspect_ratio, (4, 3));
    assert_eq!(first.color().matrix, ColorMatrix::Bt601);
    assert_eq!(first.color().range, ColorRange::Full);
    assert_ne!(first.serial(), second.serial());
    assert_eq!(first.format(), FrameFormat::Nv12);
    assert_eq!(first.nv12().unwrap().packed_len(), 17);
}

#[test]
fn max_three_slots_and_clones_hold_a_slot_until_the_last_drop() {
    let source = [16, 128, 128];
    let mut pool = FramePool::new(FrameFormat::Nv12);
    assert_eq!((pool.buffers(), pool.capacity_bytes()), (0, 0));
    let first = pool.lease(picture(1, 1, &source)).unwrap();
    let clone = first.clone();
    let first_slot = Arc::as_ptr(&first.0);
    let first_pixels = first.pixels().as_ptr();
    let second = pool.lease(picture(1, 1, &source)).unwrap();
    let third = pool.lease(picture(1, 1, &source)).unwrap();
    assert_eq!((pool.buffers(), pool.capacity_bytes()), (3, 9));
    assert_eq!(
        pool.lease(picture(1, 1, &source)).unwrap_err(),
        LeaseError::Exhausted
    );
    drop(first);
    assert_eq!(
        pool.lease(picture(1, 1, &source)).unwrap_err(),
        LeaseError::Exhausted
    );
    drop(clone);
    let reused = pool.lease(picture(1, 1, &source)).unwrap();
    assert_eq!(Arc::as_ptr(&reused.0), first_slot);
    assert_eq!(reused.pixels().as_ptr(), first_pixels);
    assert_eq!((pool.buffers(), pool.capacity_bytes()), (3, 9));
    drop((second, third, reused));
    assert!(
        pool.slots
            .iter()
            .flatten()
            .all(|slot| Arc::strong_count(slot) == 1)
    );
}

#[test]
fn same_size_warmup_reuses_both_arc_and_pixel_allocations() {
    for format in [FrameFormat::Nv12, FrameFormat::Bgra] {
        let source = vec![128; packed_len(35, 27)];
        let mut pool = FramePool::new(format);
        let first = pool.lease(picture(35, 27, &source)).unwrap();
        let slot = Arc::as_ptr(&first.0);
        let pixels = first.pixels().as_ptr();
        let capacity = pool.capacity_bytes();
        drop(first);
        for serial in 2..130 {
            let frame = pool.lease(picture(35, 27, &source)).unwrap();
            assert_eq!(frame.serial(), serial);
            assert_eq!(Arc::as_ptr(&frame.0), slot);
            assert_eq!(frame.pixels().as_ptr(), pixels);
            assert_eq!(pool.capacity_bytes(), capacity);
            assert_eq!(pool.buffers(), 1);
        }
    }
}

#[test]
fn leases_survive_pool_drop_and_last_drop_releases_the_slot() {
    let source = [16, 128, 128];
    let mut pool = FramePool::new(FrameFormat::Nv12);
    let lease = pool.lease(picture(1, 1, &source)).unwrap();
    let weak = Arc::downgrade(&lease.0);
    drop(pool);
    assert_eq!(lease.pixels(), source);
    assert!(weak.upgrade().is_some());
    drop(lease);
    assert!(weak.upgrade().is_none());
}

#[test]
fn packing_preserves_odd_display_dimensions_and_strided_crop_views() {
    // A 3x3 display crop at (2,2) in a padded 6x6 coded image.
    let y: Vec<u8> = (0..46).collect();
    let uv: Vec<u8> = (100..122).collect();
    let image = Nv12::new(3, 3, &y[18..], 8, &uv[10..], 8).unwrap();
    let mut pool = FramePool::new(FrameFormat::Nv12);
    let frame = pool
        .lease(DecodedFrame {
            image,
            color: ColorDescription::default(),
            timestamp: Duration::ZERO,
        })
        .unwrap();
    assert_eq!(
        frame.pixels(),
        [
            18, 19, 20, 26, 27, 28, 34, 35, 36, 110, 111, 112, 113, 118, 119, 120, 121
        ]
    );
    let view = frame.nv12().unwrap();
    assert_eq!((view.luma_stride(), view.chroma_stride()), (3, 4));
}

#[test]
fn capacity_counts_retained_spares_and_churn_reclaims_only_unleased_buffers() {
    let small = vec![128; packed_len(2048, 2048)];
    let large = vec![128; packed_len(4096, 2304)];
    let mut pool = FramePool::new(FrameFormat::Bgra);
    let first = pool.lease(picture(2048, 2048, &small)).unwrap();
    let second = pool.lease(picture(2048, 2048, &small)).unwrap();
    let third = pool.lease(picture(2048, 2048, &small)).unwrap();
    assert_eq!(pool.capacity_bytes(), MAX_FRAME_POOL_BYTES);
    let first_pixels = first.pixels().as_ptr();
    drop(third);
    // A free slot exists, but the two leased 16 MiB buffers cannot coexist
    // with the new 36 MiB picture. This is a byte limit, not slot exhaustion.
    assert_eq!(
        pool.lease(picture(4096, 2304, &large)).unwrap_err(),
        LeaseError::CapacityLimit
    );
    assert_eq!(pool.capacity_bytes(), MAX_FRAME_POOL_BYTES);
    assert_eq!(first.pixels().as_ptr(), first_pixels);
    drop((first, second));
    let large_frame = pool.lease(picture(4096, 2304, &large)).unwrap();
    assert!(pool.capacity_bytes() <= MAX_FRAME_POOL_BYTES);
    let large_pixels = large_frame.pixels().as_ptr();
    drop(large_frame);
    for _ in 0..2 {
        let tiny = pool.lease(picture(1, 1, &[16, 128, 128])).unwrap();
        drop(tiny);
        let large_frame = pool.lease(picture(4096, 2304, &large)).unwrap();
        assert_eq!(large_frame.pixels().as_ptr(), large_pixels);
        assert!(pool.capacity_bytes() <= MAX_FRAME_POOL_BYTES);
        assert!(pool.buffers() <= MAX_FRAME_BUFFERS);
    }
}

#[test]
fn dimensions_are_rejected_separately_from_exhaustion() {
    let source = vec![128; packed_len(4097, 1)];
    let mut pool = FramePool::new(FrameFormat::Nv12);
    assert_eq!(
        pool.lease(picture(4097, 1, &source)).unwrap_err(),
        LeaseError::InvalidDimensions
    );
    assert_eq!((pool.buffers(), pool.capacity_bytes()), (0, 0));
}

fn shader_rgb(color: ColorDescription, samples: [u8; 3]) -> [u8; 3] {
    let samples = samples.map(|sample| f32::from(sample) / 255.0);
    color.transform_rows().map(|row| {
        ((row[0] * samples[0] + row[1] * samples[1] + row[2] * samples[2] + row[3]) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    })
}

fn converted_rgb(color: ColorDescription, samples: [u8; 3]) -> [u8; 3] {
    let mut pool = FramePool::new(FrameFormat::Bgra);
    let frame = pool
        .lease(DecodedFrame {
            image: Nv12::packed(1, 1, &samples).unwrap(),
            color,
            timestamp: Duration::ZERO,
        })
        .unwrap();
    assert!(frame.nv12().is_none());
    assert_eq!(frame.format(), FrameFormat::Bgra);
    assert_eq!(frame.pixels()[3], 255);
    [frame.pixels()[2], frame.pixels()[1], frame.pixels()[0]]
}

#[test]
fn all_matrices_and_ranges_have_exact_neutral_black_and_white() {
    for matrix in [ColorMatrix::Bt601, ColorMatrix::Bt709, ColorMatrix::Bt2020] {
        for range in [ColorRange::Limited, ColorRange::Full] {
            let color = ColorDescription {
                matrix,
                range,
                pixel_aspect_ratio: (1, 1),
            };
            let (black, white) = match range {
                ColorRange::Limited => (16, 235),
                ColorRange::Full => (0, 255),
            };
            for (y, expected) in [(black, [0; 3]), (white, [255; 3])] {
                assert_eq!(
                    shader_rgb(color, [y, 128, 128]),
                    expected,
                    "{matrix:?}/{range:?}"
                );
                assert_eq!(
                    converted_rgb(color, [y, 128, 128]),
                    expected,
                    "{matrix:?}/{range:?}"
                );
            }
        }
    }
}

#[test]
fn primary_vectors_use_the_selected_matrix_and_match_shader_rows() {
    // Rounded digital YCbCr encodings of unit RGB primaries, independently
    // tabulated from the standard Kr/Kb equations, not from transform_rows.
    let vectors = [
        (
            ColorMatrix::Bt601,
            ColorRange::Limited,
            [[81, 90, 240], [145, 54, 34], [41, 240, 110]],
        ),
        (
            ColorMatrix::Bt709,
            ColorRange::Limited,
            [[63, 102, 240], [173, 42, 26], [32, 240, 118]],
        ),
        (
            ColorMatrix::Bt2020,
            ColorRange::Limited,
            [[74, 97, 240], [164, 47, 25], [29, 240, 119]],
        ),
        (
            ColorMatrix::Bt601,
            ColorRange::Full,
            [[76, 85, 255], [150, 44, 21], [29, 255, 107]],
        ),
        (
            ColorMatrix::Bt709,
            ColorRange::Full,
            [[54, 99, 255], [182, 30, 12], [18, 255, 116]],
        ),
        (
            ColorMatrix::Bt2020,
            ColorRange::Full,
            [[67, 92, 255], [173, 36, 11], [15, 255, 118]],
        ),
    ];
    for (matrix, range, primaries) in vectors {
        let color = ColorDescription {
            matrix,
            range,
            pixel_aspect_ratio: (1, 1),
        };
        for (primary, samples) in primaries.into_iter().enumerate() {
            let rgb = converted_rgb(color, samples);
            let shader = shader_rgb(color, samples);
            for channel in 0..3 {
                let expected = if channel == primary { 255 } else { 0 };
                assert!(
                    rgb[channel].abs_diff(expected) <= 2,
                    "{matrix:?}/{range:?} {samples:?}: {rgb:?}"
                );
                assert!(rgb[channel].abs_diff(shader[channel]) <= 1);
            }
        }
    }
}

#[test]
fn bgra_conversion_samples_odd_strided_planes_without_padding_or_copying_nv12() {
    let y = [16, 235, 16, 0xee, 235, 16, 235, 0xee, 16, 235, 16];
    let uv = [128, 128, 128, 128, 0xee, 0xee, 128, 128, 128, 128];
    let image = Nv12::new(3, 3, &y, 4, &uv, 6).unwrap();
    let mut pool = FramePool::new(FrameFormat::Bgra);
    let frame = pool
        .lease(DecodedFrame {
            image,
            color: ColorDescription::default(),
            timestamp: Duration::ZERO,
        })
        .unwrap();
    assert_eq!(pool.capacity_bytes(), 3 * 3 * 4);
    for (index, pixel) in frame.pixels().as_chunks::<4>().0.iter().enumerate() {
        let value = if index % 2 == 0 { 0 } else { 255 };
        assert_eq!(*pixel, [value, value, value, 255]);
    }
}
