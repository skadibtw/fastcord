//! Original fastcord icon system, drawn with iced canvas on both renderers.
//! SVG sources, desktop exports and a review sheet live in `assets/icons`.

mod catalog;
pub use catalog::Icon;

use iced::mouse;
use iced::widget::canvas::{self, Canvas};
use iced::widget::{button, container, row, text, tooltip};
use iced::{Color, Element, Length, Renderer, Theme};

use crate::style;

const SIZE: f32 = 20.0;

struct Glyph {
    icon: Icon,
    color: Color,
    size: f32,
}

impl<M> canvas::Program<M> for Glyph {
    type State = ();

    fn draw(
        &self,
        _: &(),
        renderer: &Renderer,
        _: &Theme,
        bounds: iced::Rectangle,
        _: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        frame.scale(self.size / SIZE);
        catalog::draw(self.icon, &mut frame, self.color);
        vec![frame.into_geometry()]
    }
}

pub fn icon<'a, M: 'a>(icon: Icon, color: Color) -> Element<'a, M> {
    sized(icon, color, SIZE)
}

/// The same glyph at another square size (the 20 px drawing is scaled).
pub fn sized<'a, M: 'a>(icon: Icon, color: Color, size: f32) -> Element<'a, M> {
    Canvas::new(Glyph { icon, color, size })
        .width(size)
        .height(size)
        .into()
}

/// A glyph with a visible caption for actions that need a persistent name.
pub fn labeled<'a, M: 'a>(
    kind: Icon,
    label: impl text::IntoFragment<'a>,
    size: u32,
) -> Element<'a, M> {
    row![sized(kind, style::TEXT, 16.0), text(label).size(size)]
        .spacing(5)
        .align_y(iced::alignment::Vertical::Center)
        .into()
}

/// A compact tool keeps its explicit action name in a tooltip.
pub fn tool<'a, M: Clone + 'a>(kind: Icon, label: &'a str, message: M) -> Element<'a, M> {
    action(Action {
        icon: kind,
        on_press: Some(message),
        active: false,
        danger: false,
        tip: label.to_owned(),
        badge: 0,
    })
}

/// Decode the fixed, small window artwork once, before the event loop starts.
pub fn window_icon() -> Option<iced::window::Icon> {
    let image =
        image::load_from_memory(include_bytes!("../../../assets/icons/app/fastcord-64.png"))
            .ok()?
            .into_rgba8();
    iced::window::icon::from_rgba(image.into_raw(), 64, 64).ok()
}

/// Offline visual review of the actual native renderer, available in debug builds.
#[cfg(debug_assertions)]
pub fn gallery() -> iced::Result {
    fn view(_: &()) -> Element<'_, ()> {
        use iced::widget::{column, scrollable};
        let mut content = column![
            row![
                sized::<()>(Icon::Brand, style::BLURPLE, 48.0),
                column![
                    text("fastcord / icon system").size(28),
                    text("90 original glyphs · 14 / 20 / 32 px · native canvas")
                        .color(style::MUTED),
                ],
            ]
            .spacing(18)
            .align_y(iced::Alignment::Center),
        ]
        .spacing(12);
        for chunk in catalog::NAMED.chunks(6) {
            let cells = chunk.iter().map(|&(name, kind)| {
                container(
                    column![
                        row![
                            sized::<()>(kind, style::MUTED, 14.0),
                            icon::<()>(kind, style::TEXT),
                            sized::<()>(kind, style::BRIGHT, 32.0),
                        ]
                        .spacing(12)
                        .align_y(iced::Alignment::Center),
                        text(name).size(12).color(style::MUTED),
                    ]
                    .spacing(10),
                )
                .padding(12)
                .width(Length::Fill)
                .style(style::card)
                .into()
            });
            content = content.push(row(cells).spacing(10));
        }
        scrollable(container(content).padding(24)).into()
    }
    crate::render::application(|| (), |_: &mut (), _: ()| {}, view)
        .title("fastcord — icon gallery")
        .theme(|_: &()| style::theme())
        .window(iced::window::Settings {
            size: iced::Size::new(1200.0, 840.0),
            icon: window_icon(),
            ..iced::window::Settings::default()
        })
        .run()
}

/// Every footer action: its message (`None` while disabled), look and tip.
pub struct Action<M> {
    pub icon: Icon,
    pub on_press: Option<M>,
    pub active: bool,
    pub danger: bool,
    pub tip: String,
    /// Pending-request style count next to the icon, shown as `9+` beyond 9.
    pub badge: u32,
}

pub fn action<'a, M: Clone + 'a>(a: Action<M>) -> Element<'a, M> {
    let color = if a.danger {
        style::RED
    } else if a.active {
        style::BRIGHT
    } else {
        style::MUTED
    };
    let mut content = row![icon(a.icon, color)]
        .spacing(3)
        .align_y(iced::alignment::Vertical::Center);
    if a.badge > 0 {
        content = content.push(
            text(if a.badge > 9 {
                "9+".to_owned()
            } else {
                a.badge.to_string()
            })
            .size(11)
            .color(style::YELLOW),
        );
    }
    tooltip(
        button(content)
            .padding([4, 5])
            .on_press_maybe(a.on_press)
            .style(style::tool_button(a.active, a.danger)),
        container(text(a.tip).size(11))
            .padding([3, 6])
            .style(container::rounded_box),
        tooltip::Position::Top,
    )
    .into()
}

/// Identity on top (clipped to the sidebar), the six actions in a row below.
pub fn footer<'a, M: Clone + 'a>(
    identity: Element<'a, M>,
    actions: impl IntoIterator<Item = Action<M>>,
) -> Element<'a, M> {
    iced::widget::column![
        container(identity).width(Length::Fill).clip(true),
        row(actions.into_iter().map(action)).spacing(2),
    ]
    .spacing(6)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::variable_list::harness::Ui;
    use std::collections::BTreeSet;

    const WIDTH: f32 = crate::navigation::SIDEBAR_WIDTH;

    fn act(icon: Icon, name: &'static str, on: bool, badge: u32) -> Action<&'static str> {
        Action {
            icon,
            on_press: on.then_some(name),
            active: true,
            danger: false,
            tip: format!("{name} with a long explanation"),
            badge,
        }
    }

    fn element() -> Element<'static, &'static str> {
        let identity = row![
            container(text("A").size(14)).center(32),
            iced::widget::column![
                text("An extremely long display name that would never fit the sidebar")
                    .size(13)
                    .wrapping(text::Wrapping::None),
                button(
                    text("A very long custom status text that overflows too")
                        .wrapping(text::Wrapping::None)
                )
                .on_press("status"),
            ]
            .width(Length::Fill),
        ];
        // The footer sits inside the same 8 px padding as in the app.
        container(footer(
            identity.into(),
            [
                act(Icon::Mic { muted: true }, "mute", true, 0),
                act(Icon::Headphones { deafened: true }, "deafen", true, 0),
                act(Icon::Profile, "me", true, 0),
                act(Icon::Safety, "safety", true, 12_345),
                act(Icon::Audio, "audio", true, 0),
                act(Icon::Exit, "exit", true, 0),
            ],
        ))
        .padding(8)
        .width(WIDTH)
        .into()
    }

    #[test]
    fn long_identity_and_large_count_keep_every_action_inside_the_sidebar() {
        let mut ui = Ui::new(WIDTH, 80.0, element());
        let mut hit = BTreeSet::new();
        for y in (0..80).step_by(2) {
            for x in (0..WIDTH as i32).step_by(2) {
                hit.extend(ui.click(x as f32, y as f32));
            }
        }
        for name in ["mute", "deafen", "me", "safety", "audio", "exit", "status"] {
            assert!(
                hit.contains(name),
                "{name} is not reachable inside {WIDTH}px"
            );
        }
        // Nothing hangs out beyond the sidebar either: a wider surface finds no more.
        let mut wide = Ui::new(WIDTH + 200.0, 80.0, element());
        for y in (0..80).step_by(4) {
            for x in (WIDTH as i32..WIDTH as i32 + 200).step_by(4) {
                assert!(wide.click(x as f32, y as f32).is_empty(), "action at x={x}");
            }
        }
    }

    #[test]
    fn disabled_actions_publish_nothing() {
        let off = [
            Icon::Mic { muted: false },
            Icon::Headphones { deafened: false },
        ]
        .map(|i| act(i, "off", false, 0));
        let mut ui = Ui::new(WIDTH, 40.0, footer(text("me").into(), off));
        for y in (0..40).step_by(2) {
            for x in (0..WIDTH as i32).step_by(2) {
                assert!(ui.click(x as f32, y as f32).is_empty());
            }
        }
    }

    #[test]
    fn icons_draw_on_the_software_renderer() {
        for &kind in catalog::ALL {
            let mut ui = Ui::new(SIZE, SIZE, icon::<()>(kind, style::BRIGHT));
            ui.draw();
            let mut small = Ui::new(14.0, 14.0, sized::<()>(kind, style::BRIGHT, 14.0));
            small.draw();
        }
    }
}
