//! Fixed-height list windows: widgets exist only for the viewport and one
//! viewport of overscan on either side. Omitted rows occupy exact spacers.
use iced::widget::{column, scrollable, space};
use iced::{Element, Length};

pub const ROW_HEIGHT: f32 = 32.0;
pub const LIST_HEIGHT: f32 = 400.0;
pub const MAX_ROWS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub start: usize,
    pub count: usize,
}

impl Default for Window {
    fn default() -> Self {
        Self::viewport(0.0, LIST_HEIGHT)
    }
}

/// The tallest list the window arithmetic plans for.
pub const MAX_VIEW_HEIGHT: f32 = 1_200.0;

impl Window {
    pub fn viewport(offset: f32, height: f32) -> Self {
        Self::viewport_rows(offset, height, ROW_HEIGHT)
    }

    /// The window for rows of `row_height`.
    pub fn viewport_rows(offset: f32, height: f32, row_height: f32) -> Self {
        let visible = (height.clamp(row_height, MAX_VIEW_HEIGHT) / row_height).ceil() as usize;
        let first = (offset.max(0.0) / row_height).floor() as usize;
        Self {
            start: first.saturating_sub(visible),
            count: (visible * 3).min(MAX_ROWS),
        }
    }
}

pub fn view<'a, Message: Clone + 'a>(
    id: &'static str,
    start: usize,
    total: usize,
    rows: impl IntoIterator<Item = Element<'a, Message>>,
    on_scroll: impl Fn(Window) -> Message + 'a,
) -> Element<'a, Message> {
    view_rows(id, start, total, rows, on_scroll, ROW_HEIGHT)
}

/// [`view`] for rows of `row_height`, filling the height it is given.
pub fn view_rows<'a, Message: Clone + 'a>(
    id: &'static str,
    start: usize,
    total: usize,
    rows: impl IntoIterator<Item = Element<'a, Message>>,
    on_scroll: impl Fn(Window) -> Message + 'a,
    row_height: f32,
) -> Element<'a, Message> {
    let start = start.min(total);
    let mut content = column![space().height(start as f32 * row_height)];
    let mut built = 0;
    for item in rows.into_iter().take(MAX_ROWS) {
        content = content.push(item);
        built += 1;
    }
    let skipped = total.saturating_sub(start + built);
    content = content.push(space().height(skipped as f32 * row_height));
    scrollable(content.width(Length::Fill))
        .id(id)
        .height(Length::Fill)
        .on_scroll(move |viewport| {
            on_scroll(Window::viewport_rows(
                viewport.absolute_offset().y,
                viewport.bounds().height,
                row_height,
            ))
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_tracks_viewport_not_account_size() {
        assert_eq!(
            Window::default(),
            Window {
                start: 0,
                count: 39
            }
        );
        let middle = Window::viewport(32.0 * 10_000.0, 320.0);
        assert_eq!(
            middle,
            Window {
                start: 9_990,
                count: 30
            }
        );
        let bottom = Window::viewport(32.0 * 99_990.0, 320.0);
        assert_eq!(
            bottom,
            Window {
                start: 99_980,
                count: 30
            }
        );
        assert!(Window::viewport(f32::MAX, f32::MAX).count <= MAX_ROWS);
        assert_eq!(Window::viewport(-1.0, 0.0), Window { start: 0, count: 3 });
    }
}
