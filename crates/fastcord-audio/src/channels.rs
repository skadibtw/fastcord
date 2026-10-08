//! Channel-layout conversion between device layouts and the engine's
//! internal formats (mono capture, stereo playback).

/// Averages each interleaved `channels`-sample frame of `input` into `out`.
/// Returns the number of frames written (limited by both buffers).
pub(crate) fn downmix_to_mono(input: &[f32], channels: usize, out: &mut [f32]) -> usize {
    debug_assert!(channels > 0);
    if channels == 1 {
        let frames = input.len().min(out.len());
        out[..frames].copy_from_slice(&input[..frames]);
        return frames;
    }
    let scale = 1.0 / channels as f32;
    let mut frames = 0;
    for (frame, slot) in input.chunks_exact(channels).zip(out.iter_mut()) {
        *slot = frame.iter().sum::<f32>() * scale;
        frames += 1;
    }
    frames
}

/// Expands interleaved stereo into a device layout with `channels` channels.
///
/// A mono device receives the average of left and right; a stereo or wider
/// device receives left/right in its first two channels (front left/right in
/// every common surround order) and silence in the rest.
pub(crate) fn stereo_to_device(stereo: &[f32], channels: usize) -> impl Iterator<Item = f32> + '_ {
    debug_assert!(channels > 0);
    stereo.as_chunks::<2>().0.iter().flat_map(move |frame| {
        (0..channels).map(move |channel| match (channels, channel) {
            (1, _) => (frame[0] + frame[1]) * 0.5,
            (_, 0) => frame[0],
            (_, 1) => frame[1],
            _ => 0.0,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_input_is_copied() {
        let mut out = [0.0; 4];
        assert_eq!(downmix_to_mono(&[0.1, 0.2, 0.3], 1, &mut out), 3);
        assert_eq!(&out[..3], &[0.1, 0.2, 0.3]);
    }

    #[test]
    fn multichannel_frames_are_averaged_and_partial_frames_ignored() {
        let mut out = [0.0; 4];
        let input = [1.0, 0.0, 0.5, 0.5, -1.0, -0.5, 0.25];
        assert_eq!(downmix_to_mono(&input, 2, &mut out), 3);
        assert_eq!(&out[..3], &[0.5, 0.5, -0.75]);
        let quad = [0.4, 0.4, 0.0, 0.0];
        assert_eq!(downmix_to_mono(&quad, 4, &mut out), 1);
        assert!((out[0] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn downmix_stops_at_output_capacity() {
        let mut out = [0.0; 1];
        assert_eq!(downmix_to_mono(&[1.0, 1.0, 0.0, 0.0], 2, &mut out), 1);
    }

    #[test]
    fn stereo_maps_to_mono_stereo_and_surround_devices() {
        let stereo = [0.2, 0.6, -1.0, 1.0];
        let mono: Vec<f32> = stereo_to_device(&stereo, 1).collect();
        assert_eq!(mono, [0.4, 0.0]);
        let same: Vec<f32> = stereo_to_device(&stereo, 2).collect();
        assert_eq!(same, stereo);
        let surround: Vec<f32> = stereo_to_device(&stereo, 6).collect();
        assert_eq!(
            surround,
            [0.2, 0.6, 0.0, 0.0, 0.0, 0.0, -1.0, 1.0, 0.0, 0.0, 0.0, 0.0]
        );
    }
}
