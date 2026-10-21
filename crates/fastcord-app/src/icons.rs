//! Compact original vector icons for the account footer, drawn with iced's
//! canvas (both the wgpu and the tiny-skia renderer), and the footer layout
//! that keeps identity and all six actions inside the fixed sidebar width.

use std::f32::consts::PI;

use iced::mouse;
use iced::widget::canvas::{self, Canvas, Path, Stroke, Style};
use iced::widget::{button, container, row, text, tooltip};
use iced::{Color, Element, Length, Point, Radians, Renderer, Size, Theme};

use crate::style;

const SIZE: f32 = 20.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    /// The slash is the muted state.
    Mic {
        muted: bool,
    },
    /// The slash is the deafened state.
    Headphones {
        deafened: bool,
    },
    Profile,
    Safety,
    Audio,
    Exit,
    /// Filled body while the camera is on.
    Camera {
        on: bool,
    },
    /// Filled screen while sharing.
    Screen {
        sharing: bool,
    },
    Soundboard,
    HangUp,
    Signal,
    Settings,
}

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
        let pen = |color: Color| Stroke {
            style: Style::Solid(color),
            width: 1.7,
            line_cap: canvas::LineCap::Round,
            line_join: canvas::LineJoin::Round,
            ..Stroke::default()
        };
        let line = pen(self.color);
        let pen_wide = |color: Color| Stroke {
            width: 3.0,
            ..pen(color)
        };
        let p = |x: f32, y: f32| Point::new(x, y);
        let arc = |cx: f32, cy: f32, r: f32, from: f32, to: f32| {
            Path::new(|b| {
                b.arc(canvas::path::Arc {
                    center: p(cx, cy),
                    radius: r,
                    start_angle: Radians(from),
                    end_angle: Radians(to),
                });
            })
        };
        let poly = |points: &[(f32, f32)], close: bool| {
            Path::new(|b| {
                b.move_to(p(points[0].0, points[0].1));
                for q in &points[1..] {
                    b.line_to(p(q.0, q.1));
                }
                if close {
                    b.close();
                }
            })
        };
        let mut slash = false;
        match self.icon {
            Icon::Mic { muted } => {
                frame.stroke(
                    &Path::rounded_rectangle(p(7.0, 2.0), Size::new(6.0, 9.5), 3.0.into()),
                    line,
                );
                frame.stroke(&arc(10.0, 9.0, 5.5, 0.0, PI), line);
                frame.stroke(&poly(&[(10.0, 14.5), (10.0, 18.0)], false), line);
                slash = muted;
            }
            Icon::Headphones { deafened } => {
                frame.stroke(&arc(10.0, 11.0, 6.5, PI, 2.0 * PI), line);
                for x in [3.0, 13.0] {
                    frame.fill_rectangle(p(x, 11.0), Size::new(4.0, 6.0), self.color);
                }
                slash = deafened;
            }
            Icon::Profile => {
                frame.stroke(&Path::circle(p(10.0, 6.5), 3.3), line);
                frame.stroke(&arc(10.0, 18.0, 6.5, PI, 2.0 * PI), line);
            }
            Icon::Safety => {
                frame.stroke(
                    &poly(
                        &[
                            (10.0, 2.0),
                            (16.0, 4.5),
                            (16.0, 10.0),
                            (10.0, 18.0),
                            (4.0, 10.0),
                            (4.0, 4.5),
                        ],
                        true,
                    ),
                    line,
                );
            }
            Icon::Audio => {
                frame.fill(
                    &poly(
                        &[
                            (2.5, 8.0),
                            (6.5, 8.0),
                            (10.5, 4.5),
                            (10.5, 15.5),
                            (6.5, 12.0),
                            (2.5, 12.0),
                        ],
                        true,
                    ),
                    self.color,
                );
                frame.stroke(&arc(10.5, 10.0, 4.0, -0.9, 0.9), line);
                frame.stroke(&arc(10.5, 10.0, 7.0, -0.9, 0.9), line);
            }
            Icon::Exit => {
                frame.stroke(
                    &poly(&[(9.0, 3.0), (3.5, 3.0), (3.5, 17.0), (9.0, 17.0)], false),
                    line,
                );
                frame.stroke(&poly(&[(7.5, 10.0), (17.0, 10.0)], false), line);
                frame.stroke(
                    &poly(&[(13.5, 6.5), (17.0, 10.0), (13.5, 13.5)], false),
                    line,
                );
            }
            Icon::Camera { on } => {
                let body = Path::rounded_rectangle(p(2.0, 5.0), Size::new(11.0, 10.0), 2.0.into());
                if on {
                    frame.fill(&body, self.color);
                } else {
                    frame.stroke(&body, line);
                }
                frame.fill(
                    &poly(
                        &[(13.5, 8.5), (18.0, 5.5), (18.0, 14.5), (13.5, 11.5)],
                        true,
                    ),
                    self.color,
                );
            }
            Icon::Screen { sharing } => {
                let screen =
                    Path::rounded_rectangle(p(2.0, 3.0), Size::new(16.0, 10.5), 1.5.into());
                frame.stroke(&screen, line);
                if sharing {
                    frame.fill_rectangle(p(5.0, 6.0), Size::new(10.0, 4.5), self.color);
                }
                frame.stroke(&poly(&[(10.0, 13.5), (10.0, 17.0)], false), line);
                frame.stroke(&poly(&[(6.0, 17.0), (14.0, 17.0)], false), line);
            }
            Icon::Soundboard => {
                for (x, top, bottom) in [
                    (3.0, 8.0, 12.0),
                    (6.5, 4.0, 16.0),
                    (10.0, 6.5, 13.5),
                    (13.5, 3.0, 17.0),
                    (17.0, 8.0, 12.0),
                ] {
                    frame.stroke(&poly(&[(x, top), (x, bottom)], false), line);
                }
            }
            Icon::HangUp => {
                // A handset laid on its back: the arc and its two earpieces.
                frame.stroke(
                    &arc(10.0, 15.0, 8.0, PI * 1.15, PI * 1.85),
                    pen_wide(self.color),
                );
                frame.stroke(
                    &poly(&[(2.5, 9.5), (2.5, 13.0)], false),
                    pen_wide(self.color),
                );
                frame.stroke(
                    &poly(&[(17.5, 9.5), (17.5, 13.0)], false),
                    pen_wide(self.color),
                );
            }
            Icon::Signal => {
                for (x, height) in [(3.0, 4.0), (7.0, 8.0), (11.0, 12.0), (15.0, 16.0)] {
                    frame.fill_rectangle(p(x, 18.0 - height), Size::new(3.0, height), self.color);
                }
            }
            Icon::Settings => {
                frame.stroke(&Path::circle(p(10.0, 10.0), 3.0), line);
                for step in 0..8 {
                    let angle = step as f32 * PI / 4.0;
                    let (sin, cos) = angle.sin_cos();
                    frame.stroke(
                        &poly(
                            &[
                                (10.0 + 6.0 * cos, 10.0 + 6.0 * sin),
                                (10.0 + 8.5 * cos, 10.0 + 8.5 * sin),
                            ],
                            false,
                        ),
                        line,
                    );
                }
            }
        }
        if slash {
            // A diagonal bar is the muted/deafened state, whatever the colors.
            frame.stroke(&poly(&[(3.0, 3.0), (17.0, 17.0)], false), pen(style::RED));
        }
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
        for kind in [
            Icon::Mic { muted: true },
            Icon::Headphones { deafened: false },
            Icon::Profile,
            Icon::Safety,
            Icon::Audio,
            Icon::Exit,
            Icon::Camera { on: true },
            Icon::Screen { sharing: true },
            Icon::Soundboard,
            Icon::HangUp,
            Icon::Signal,
            Icon::Settings,
        ] {
            let mut ui = Ui::new(SIZE, SIZE, icon::<()>(kind, style::BRIGHT));
            ui.draw();
            let mut small = Ui::new(14.0, 14.0, sized::<()>(kind, style::BRIGHT, 14.0));
            small.draw();
        }
    }
}
