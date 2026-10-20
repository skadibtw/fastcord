//! Camera devices and capture-mode selection (SPEC §8.8, issue #84).
//!
//! A camera is a capture source that is chosen by a stable device ID rather
//! than a window handle. A session delivers the same bounded NV12 pictures and
//! uses the same [`super::FrameHandler`] and [`super::CaptureSession`] as a
//! screen capture, so the encode pipeline is shared. Opening and listing are
//! native and Windows-only so far (Media Foundation source reader); elsewhere
//! every entry point reports [`super::CaptureError::Unsupported`].

use super::{CaptureConfig, MAX_TITLE_CHARS};

/// Cameras listed at once.
pub const MAX_CAMERAS: usize = 16;
/// Longest stable ID kept, in bytes (a Windows symbolic link is under 300).
pub const MAX_CAMERA_ID_BYTES: usize = 512;

/// A camera the user can turn on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraDevice {
    /// The operating system's stable name for the device (on Windows its
    /// symbolic link: it survives restarts and USB re-plugging into the same
    /// port). Never compared by label.
    pub id: String,
    /// The friendly name, at most [`MAX_TITLE_CHARS`] characters.
    pub name: String,
}

impl CameraDevice {
    /// A device with its ID and name bounded; `None` for an unusable ID.
    pub fn bounded(id: &str, name: &str) -> Option<Self> {
        if id.is_empty() || id.len() > MAX_CAMERA_ID_BYTES {
            return None;
        }
        let name: String = name.chars().take(MAX_TITLE_CHARS).collect();
        Some(Self {
            id: id.to_owned(),
            name: if name.trim().is_empty() {
                "Camera".to_owned()
            } else {
                name
            },
        })
    }
}

/// The pixel layouts a camera offers, in the order the converter prefers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraFormat {
    /// Already the output layout: no conversion.
    Nv12,
    /// Packed 4:2:2: a cheap conversion.
    Yuy2,
    /// Compressed: decoded by the system before conversion.
    Mjpeg,
    /// Anything else the system can convert.
    Other,
}

impl CameraFormat {
    fn rank(self) -> u8 {
        match self {
            Self::Nv12 => 3,
            Self::Yuy2 => 2,
            Self::Mjpeg => 1,
            Self::Other => 0,
        }
    }
}

/// One mode a camera can deliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraMode {
    pub width: u32,
    pub height: u32,
    /// The frame rate as a ratio (`30000/1001` is 29.97 fps).
    pub rate: (u32, u32),
    pub format: CameraFormat,
}

impl CameraMode {
    /// Frames per 1000 seconds, 0 for a malformed ratio.
    fn rate_milli(self) -> u64 {
        let (numerator, denominator) = self.rate;
        if denominator == 0 {
            return 0;
        }
        u64::from(numerator) * 1000 / u64::from(denominator)
    }

    fn fits(self, config: CaptureConfig) -> bool {
        let edge = |edge: u32, max: u32| {
            (super::convert::MIN_OUTPUT_EDGE..=max).contains(&edge) && edge.is_multiple_of(2)
        };
        edge(self.width, config.width) && edge(self.height, config.height) && self.rate_milli() > 0
    }
}

/// Below this a camera looks like a slideshow; any mode at or above it wins
/// over a larger but slower one.
const SMOOTH_MILLI: u64 = 24_000;

/// The mode to open: the largest picture within `config` that runs smoothly,
/// then the rate nearest to (and not above) the configured one, then the
/// cheapest pixel layout. `None` when the camera has no mode within the
/// bounds (the picture is never scaled up or cropped here).
pub fn choose_mode(modes: &[CameraMode], config: CaptureConfig) -> Option<usize> {
    let target = u64::from(config.frame_rate) * 1000;
    modes
        .iter()
        .enumerate()
        .filter(|(_, mode)| mode.fits(config))
        .max_by_key(|(index, mode)| {
            let milli = mode.rate_milli();
            (
                milli >= SMOOTH_MILLI.min(target),
                u64::from(mode.width) * u64::from(mode.height),
                milli.min(target),
                mode.format.rank(),
                // Equal modes: the first one listed.
                std::cmp::Reverse(*index),
            )
        })
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u32, height: u32, fps: u32, format: CameraFormat) -> CameraMode {
        CameraMode {
            width,
            height,
            rate: (fps, 1),
            format,
        }
    }

    const BASE: CaptureConfig = CaptureConfig::SCREEN_SHARE_720P30;

    #[test]
    fn the_largest_smooth_mode_within_the_bounds_wins() {
        let modes = [
            mode(640, 480, 30, CameraFormat::Yuy2),
            mode(1920, 1080, 30, CameraFormat::Mjpeg),
            mode(1280, 720, 30, CameraFormat::Mjpeg),
            mode(1280, 720, 15, CameraFormat::Nv12),
        ];
        assert_eq!(choose_mode(&modes, BASE), Some(2));
    }

    #[test]
    fn a_slideshow_loses_to_a_smaller_smooth_mode() {
        let modes = [
            mode(1280, 720, 10, CameraFormat::Yuy2),
            mode(640, 480, 30, CameraFormat::Yuy2),
        ];
        assert_eq!(choose_mode(&modes, BASE), Some(1));
        // With nothing smooth the biggest picture is the best available.
        let slow = [
            mode(640, 480, 10, CameraFormat::Yuy2),
            mode(1280, 720, 10, CameraFormat::Yuy2),
        ];
        assert_eq!(choose_mode(&slow, BASE), Some(1));
    }

    #[test]
    fn rate_then_layout_break_ties_and_ntsc_rates_count_as_smooth() {
        let modes = [
            mode(1280, 720, 60, CameraFormat::Mjpeg),
            CameraMode {
                rate: (30000, 1001),
                ..mode(1280, 720, 0, CameraFormat::Yuy2)
            },
            mode(1280, 720, 30, CameraFormat::Mjpeg),
            mode(1280, 720, 30, CameraFormat::Nv12),
        ];
        // 60 fps is capped at the 30 target, so it ties with the 30 fps modes
        // and the layout ranking picks NV12.
        assert_eq!(choose_mode(&modes, BASE), Some(3));
        let ntsc = [CameraMode {
            rate: (30000, 1001),
            ..mode(640, 480, 0, CameraFormat::Yuy2)
        }];
        assert_eq!(choose_mode(&ntsc, BASE), Some(0));
    }

    #[test]
    fn modes_outside_the_bounds_are_never_chosen() {
        assert_eq!(choose_mode(&[], BASE), None);
        let too_big = [mode(1920, 1080, 30, CameraFormat::Nv12)];
        assert_eq!(choose_mode(&too_big, BASE), None);
        let odd = [
            mode(1279, 719, 30, CameraFormat::Nv12),
            mode(8, 8, 30, CameraFormat::Nv12),
            CameraMode {
                rate: (30, 0),
                ..mode(640, 480, 30, CameraFormat::Nv12)
            },
        ];
        assert_eq!(choose_mode(&odd, BASE), None);
    }

    #[test]
    fn device_ids_and_names_are_bounded() {
        assert!(CameraDevice::bounded("", "x").is_none());
        assert!(CameraDevice::bounded(&"a".repeat(MAX_CAMERA_ID_BYTES + 1), "x").is_none());
        let device = CameraDevice::bounded("id", &"n".repeat(500)).unwrap();
        assert_eq!(device.name.chars().count(), MAX_TITLE_CHARS);
        assert_eq!(CameraDevice::bounded("id", "  ").unwrap().name, "Camera");
    }
}
