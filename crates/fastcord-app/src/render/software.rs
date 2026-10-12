//! tiny-skia fallback with borrowed video pixmaps, not per-frame image handles.
//!
//! iced exposes the software renderer's draw API but no dynamic-pixel primitive.
//! The root bridge collects at most one leased video layer; this compositor
//! blends its already-converted BGRA pixels after ordinary UI painting. The
//! watch pane never overlaps controls and is visibility-gated under other views.

use std::num::NonZeroU32;

use iced_renderer::core::{Color, Rectangle};
use iced_renderer::graphics::{self, Viewport, compositor};

use super::SoftwareVideo;

pub(super) struct Compositor {
    context: softbuffer::Context<Box<dyn compositor::Display>>,
    settings: graphics::Settings,
}

pub(super) struct Surface {
    window: softbuffer::Surface<Box<dyn compositor::Display>, Box<dyn compositor::Window>>,
    mask: tiny_skia::Mask,
}

impl Compositor {
    pub(super) fn new(
        settings: graphics::Settings,
        display: impl compositor::Display,
    ) -> Result<Self, graphics::Error> {
        let context = softbuffer::Context::new(Box::new(display) as Box<dyn compositor::Display>)
            .map_err(|error| graphics::Error::BackendError(error.to_string()))?;
        Ok(Self { context, settings })
    }

    pub(super) fn renderer(&self) -> iced_tiny_skia::Renderer {
        iced_tiny_skia::Renderer::new(self.settings.default_font, self.settings.default_text_size)
    }

    pub(super) fn surface(
        &mut self,
        window: impl compositor::Window + Clone,
        width: u32,
        height: u32,
    ) -> Surface {
        let window = softbuffer::Surface::new(
            &self.context,
            Box::new(window) as Box<dyn compositor::Window>,
        )
        .expect("create tiny-skia window surface");
        let mut surface = Surface {
            window,
            mask: tiny_skia::Mask::new(1, 1).expect("one-pixel mask"),
        };
        self.configure(&mut surface, width, height);
        surface
    }

    pub(super) fn configure(&mut self, surface: &mut Surface, width: u32, height: u32) {
        let (Some(width), Some(height)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return;
        };
        surface
            .window
            .resize(width, height)
            .expect("resize tiny-skia window surface");
        surface.mask = tiny_skia::Mask::new(width.get(), height.get()).expect("window clip mask");
    }

    pub(super) fn present(
        &mut self,
        renderer: &mut iced_tiny_skia::Renderer,
        videos: &[SoftwareVideo],
        surface: &mut Surface,
        viewport: &Viewport,
        background: Color,
        on_pre_present: impl FnOnce(),
    ) -> Result<(), compositor::SurfaceError> {
        let size = viewport.physical_size();
        let mut buffer = surface
            .window
            .buffer_mut()
            .map_err(|_| compositor::SurfaceError::Lost)?;
        let mut pixels = tiny_skia::PixmapMut::from_bytes(
            bytemuck::cast_slice_mut(&mut buffer),
            size.width,
            size.height,
        )
        .expect("window pixel map");
        draw(
            renderer,
            videos,
            &mut pixels,
            &mut surface.mask,
            viewport,
            background,
        );
        on_pre_present();
        buffer.present().map_err(|_| compositor::SurfaceError::Lost)
    }

    pub(super) fn screenshot(
        &mut self,
        renderer: &mut iced_tiny_skia::Renderer,
        videos: &[SoftwareVideo],
        viewport: &Viewport,
        background: Color,
    ) -> Vec<u8> {
        let size = viewport.physical_size();
        let mut pixels =
            tiny_skia::Pixmap::new(size.width, size.height).expect("screenshot pixels");
        let mut mask = tiny_skia::Mask::new(size.width, size.height).expect("screenshot clip mask");
        draw(
            renderer,
            videos,
            &mut pixels.as_mut(),
            &mut mask,
            viewport,
            background,
        );
        let mut result = pixels.take();
        for pixel in result.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        result
    }
}

fn draw(
    renderer: &mut iced_tiny_skia::Renderer,
    videos: &[SoftwareVideo],
    pixels: &mut tiny_skia::PixmapMut<'_>,
    mask: &mut tiny_skia::Mask,
    viewport: &Viewport,
    background: Color,
) {
    renderer.draw(
        pixels,
        mask,
        viewport,
        &[Rectangle::with_size(viewport.logical_size())],
        background,
    );
    let scale = viewport.scale_factor();
    for video in videos {
        if !video.active.load(std::sync::atomic::Ordering::Acquire) {
            continue;
        }
        let Some(image) = tiny_skia::PixmapRef::from_bytes(
            video.frame.pixels(),
            video.frame.width(),
            video.frame.height(),
        ) else {
            continue;
        };
        let bounds = video.bounds * scale;
        let clip = video.clip * scale;
        let Some(rect) = tiny_skia::Rect::from_xywh(clip.x, clip.y, clip.width, clip.height) else {
            continue;
        };
        mask.clear();
        mask.fill_path(
            &tiny_skia::PathBuilder::from_rect(rect),
            tiny_skia::FillRule::Winding,
            false,
            tiny_skia::Transform::identity(),
        );
        let transform = tiny_skia::Transform::from_scale(
            bounds.width / image.width() as f32,
            bounds.height / image.height() as f32,
        )
        .post_translate(bounds.x, bounds.y);
        pixels.draw_pixmap(
            0,
            0,
            image,
            &tiny_skia::PixmapPaint {
                quality: tiny_skia::FilterQuality::Bilinear,
                ..Default::default()
            },
            transform,
            Some(mask),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_video::DecodedFrame;
    use fastcord_video::frame::{ColorDescription, FrameFormat, FramePool, Nv12};
    use iced_renderer::core::{Font, Pixels, Size};
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::Duration;

    #[test]
    fn software_video_blends_borrowed_bgra_with_clipping_and_no_handle() {
        let mut pool = FramePool::new(FrameFormat::Bgra);
        let source = [235, 235, 235, 235, 128, 128];
        let frame = pool
            .lease(DecodedFrame {
                image: Nv12::packed(2, 2, &source).unwrap(),
                timestamp: Duration::ZERO,
                color: ColorDescription::default(),
            })
            .unwrap();
        let bounds = Rectangle {
            x: 2.0,
            y: 2.0,
            width: 4.0,
            height: 4.0,
        };
        let video = SoftwareVideo {
            frame,
            bounds,
            clip: Rectangle {
                width: 2.0,
                ..bounds
            },
            active: Arc::new(AtomicBool::new(true)),
        };
        let mut renderer = iced_tiny_skia::Renderer::new(Font::DEFAULT, Pixels(16.0));
        let mut pixels = tiny_skia::Pixmap::new(8, 8).unwrap();
        let mut mask = tiny_skia::Mask::new(8, 8).unwrap();
        draw(
            &mut renderer,
            &[video],
            &mut pixels.as_mut(),
            &mut mask,
            &Viewport::with_physical_size(Size::new(8, 8), 1.0),
            Color::BLACK,
        );
        assert_eq!(pixels.pixel(2, 2).unwrap().red(), 255);
        assert_eq!(pixels.pixel(4, 2).unwrap().red(), 0);
        assert_eq!(pixels.pixel(1, 1).unwrap().red(), 0);
    }
}
