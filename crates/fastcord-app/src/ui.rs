//! Shared building blocks of the redesigned screens: the one primary button,
//! quiet links, labelled fields, segmented tabs, menus and popups. One accent
//! (blurple) marks the main action or the current choice; red is only for
//! errors and destructive actions.

use iced::widget::{button, column, container, mouse_area, opaque, row, space, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Theme};

use crate::icons::{self, Icon};
use crate::style;

pub const BOLD: iced::Font = iced::Font {
    weight: iced::font::Weight::Bold,
    ..iced::Font::DEFAULT
};

/// The main action of a form: full width, 44 px tall.
pub fn primary<'a, M: Clone + 'a>(label: impl text::IntoFragment<'a>, on: Option<M>) -> Element<'a, M> {
    button(container(text(label).size(15).font(BOLD)).center_x(Length::Fill))
        .on_press_maybe(on)
        .width(Length::Fill)
        .height(44)
        .padding([12, 16])
        .style(style::action_button(false))
        .into()
}

/// A neutral button: grey, never competing with the primary one.
pub fn neutral<'a, M: Clone + 'a>(label: impl text::IntoFragment<'a>, on: Option<M>) -> Element<'a, M> {
    button(text(label).size(14))
        .on_press_maybe(on)
        .padding([8, 14])
        .style(style::secondary_button(false))
        .into()
}

/// Text that acts: blurple-light, no fill.
pub fn link<'a, M: Clone + 'a>(label: impl text::IntoFragment<'a>, on: M) -> Element<'a, M> {
    button(text(label).size(14))
        .on_press(on)
        .padding([4, 2])
        .style(|_: &Theme, status| button::Style {
            text_color: match status {
                button::Status::Hovered | button::Status::Pressed => style::BRIGHT,
                _ => style::MENTION,
            },
            ..button::Style::default()
        })
        .into()
}

/// A field's permanent label above it.
pub fn label<'a, M: 'a>(label: impl text::IntoFragment<'a>) -> Element<'a, M> {
    text(label).size(12).font(BOLD).color(style::SUBTLE).into()
}

/// Secondary explanatory text.
pub fn note<'a, M: 'a>(note: impl text::IntoFragment<'a>) -> Element<'a, M> {
    text(note).size(13).color(style::MUTED).into()
}

/// An error next to what it is about.
pub fn error<'a, M: 'a>(error: impl text::IntoFragment<'a>) -> Element<'a, M> {
    text(error).size(13).color(style::RED).into()
}

/// Segmented tabs: the chosen one filled, the others plain.
pub fn tabs<'a, M: Clone + 'a, T: Copy + PartialEq + 'a>(
    choices: &[(T, &'a str)],
    current: T,
    on: impl Fn(T) -> M,
) -> Element<'a, M> {
    let mut line = row![].spacing(4);
    for &(value, title) in choices {
        let selected = value == current;
        line = line.push(
            button(container(text(title).size(14)).center_x(Length::Fill))
                .on_press_maybe((!selected).then(|| on(value)))
                .width(Length::Fill)
                .padding([7, 10])
                .style(move |_: &Theme, status| button::Style {
                    background: Some(Background::Color(if selected {
                        style::SELECTED
                    } else if matches!(status, button::Status::Hovered) {
                        style::HOVER
                    } else {
                        Color::TRANSPARENT
                    })),
                    text_color: if selected { style::BRIGHT } else { style::SUBTLE },
                    border: Border {
                        radius: 4.0.into(),
                        ..Border::default()
                    },
                    ..button::Style::default()
                }),
        );
    }
    container(line)
        .padding(3)
        .style(|_| container::Style {
            background: Some(Background::Color(style::RAIL)),
            border: Border {
                radius: 6.0.into(),
                ..Border::default()
            },
            ..container::Style::default()
        })
        .into()
}

/// A short highlighted note (warning or information) with a yellow edge.
pub fn callout<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> Element<'a, M> {
    container(content)
        .padding([10, 12])
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(Color::from_rgba(0.941, 0.698, 0.196, 0.12))),
            border: Border {
                radius: 6.0.into(),
                width: 1.0,
                color: Color::from_rgba(0.941, 0.698, 0.196, 0.5),
            },
            text_color: Some(style::TEXT),
            ..container::Style::default()
        })
        .into()
}

/// A round avatar with initials, drawn at once (no picture to wait for).
pub fn initials<'a, M: 'a>(name: &str, seed: u64, size: f32) -> Element<'a, M> {
    container(text(style::initial(name)).size(size * 0.42).font(BOLD))
        .center(size)
        .style(style::avatar(style::avatar_color(seed)))
        .into()
}

/// One row of a menu: glyph, label, and what a click does.
pub fn item<'a, M: Clone + 'a>(icon: Option<Icon>, label: impl text::IntoFragment<'a>, on: Option<M>) -> Element<'a, M> {
    menu_row(icon, label, on, false)
}

/// A destructive menu row, red.
pub fn danger_item<'a, M: Clone + 'a>(
    icon: Option<Icon>,
    label: impl text::IntoFragment<'a>,
    on: Option<M>,
) -> Element<'a, M> {
    menu_row(icon, label, on, true)
}

fn menu_row<'a, M: Clone + 'a>(
    icon: Option<Icon>,
    label: impl text::IntoFragment<'a>,
    on: Option<M>,
    danger: bool,
) -> Element<'a, M> {
    let enabled = on.is_some();
    let color = if !enabled {
        Color { a: 0.4, ..style::SUBTLE }
    } else if danger {
        style::RED
    } else {
        style::TEXT
    };
    let mut line = row![].spacing(10).align_y(Alignment::Center);
    if let Some(icon) = icon {
        line = line.push(icons::sized(icon, color, 16.0));
    }
    line = line.push(text(label).size(14).color(color).width(Length::Fill));
    button(line)
        .on_press_maybe(on)
        .width(Length::Fill)
        .padding([6, 8])
        .style(move |_: &Theme, status| button::Style {
            background: match status {
                button::Status::Hovered | button::Status::Pressed => Some(Background::Color(
                    if danger { style::RED } else { style::BLURPLE },
                )),
                _ => None,
            },
            text_color: color,
            border: Border {
                radius: 3.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        })
        .into()
}

/// A thin line between groups of a menu.
pub fn separator<'a, M: 'a>() -> Element<'a, M> {
    container(space().height(1).width(Length::Fill))
        .padding([4, 4])
        .style(|_| container::Style {
            background: Some(Background::Color(style::SELECTED)),
            ..container::Style::default()
        })
        .height(1)
        .into()
}

/// A small heading over a group of a menu or a settings page.
pub fn heading<'a, M: 'a>(title: impl text::IntoFragment<'a>) -> Element<'a, M> {
    container(text(title).size(11).font(BOLD).color(style::MUTED))
        .padding(Padding::new(0.0).top(6.0).left(8.0).bottom(2.0))
        .into()
}

/// The floating card of a menu or popover.
pub fn menu_card<'a, M: 'a>(content: impl Into<Element<'a, M>>, width: f32) -> Element<'a, M> {
    container(content)
        .padding(6)
        .width(width)
        .style(|_| container::Style {
            background: Some(Background::Color(style::RAIL)),
            border: Border {
                radius: 6.0.into(),
                width: 1.0,
                color: style::SELECTED,
            },
            text_color: Some(style::TEXT),
            shadow: iced::Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.45),
                offset: iced::Vector::new(0.0, 6.0),
                blur_radius: 18.0,
            },
            ..container::Style::default()
        })
        .into()
}

/// Where a popover sits in the window.
#[derive(Clone, Copy, Debug)]
pub enum Anchor {
    /// Distance from the left and top edges.
    TopLeft(f32, f32),
    /// Distance from the right and top edges.
    TopRight(f32, f32),
    /// Distance from the left and bottom edges.
    BottomLeft(f32, f32),
}

/// A popover over the whole window: a click anywhere outside it closes it.
/// The layer under it stays visible, so it reads as a temporary layer, not a
/// new screen.
pub fn popover<'a, M: Clone + 'a>(card: Element<'a, M>, anchor: Anchor, close: M) -> Element<'a, M> {
    let (x, y, padding) = match anchor {
        Anchor::TopLeft(left, top) => (
            iced::alignment::Horizontal::Left,
            iced::alignment::Vertical::Top,
            Padding::new(0.0).left(left).top(top),
        ),
        Anchor::TopRight(right, top) => (
            iced::alignment::Horizontal::Right,
            iced::alignment::Vertical::Top,
            Padding::new(0.0).right(right).top(top),
        ),
        Anchor::BottomLeft(left, bottom) => (
            iced::alignment::Horizontal::Left,
            iced::alignment::Vertical::Bottom,
            Padding::new(0.0).left(left).bottom(bottom),
        ),
    };
    iced::widget::stack![
        mouse_area(space().width(Length::Fill).height(Length::Fill))
            .on_press(close.clone())
            .on_right_press(close),
        container(opaque(card))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(padding)
            .align_x(x)
            .align_y(y),
    ]
    .into()
}

/// A dialog in the middle of a dimmed window; a click on the dim closes it.
pub fn modal<'a, M: Clone + 'a>(card: Element<'a, M>, close: M) -> Element<'a, M> {
    iced::widget::stack![
        mouse_area(
            container(space())
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_| container::Style {
                    background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.6))),
                    ..container::Style::default()
                })
        )
        .on_press(close),
        container(opaque(card))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(24)
            .center(Length::Fill),
    ]
    .into()
}

/// A dialog's card: title row with a close cross, then the body.
pub fn dialog<'a, M: Clone + 'a>(
    title: impl text::IntoFragment<'a>,
    body: impl Into<Element<'a, M>>,
    close: M,
    width: f32,
) -> Element<'a, M> {
    container(
        column![
            row![
                text(title).size(18).font(BOLD).width(Length::Fill),
                icons::close(Some(close)),
            ]
            .align_y(Alignment::Center),
            body.into(),
        ]
        .spacing(14),
    )
    .padding(20)
    .max_width(width)
    .style(|_| container::Style {
        background: Some(Background::Color(style::CHAT)),
        border: Border {
            radius: 8.0.into(),
            ..Border::default()
        },
        text_color: Some(style::TEXT),
        shadow: iced::Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.5),
            offset: iced::Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
        ..container::Style::default()
    })
    .into()
}
