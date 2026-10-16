//! The dark palette and widget styles of the main screen, after the web
//! client's: a near-black server rail, a dark-grey channel sidebar, a lighter
//! chat pane, blurple actions, red mentions and green "speaking".

use iced::theme::Palette;
use iced::widget::{button, container};
use iced::{Background, Border, Color, Theme};

pub const RAIL: Color = Color::from_rgb(0.118, 0.122, 0.133); // #1e1f22
pub const SIDEBAR: Color = Color::from_rgb(0.169, 0.176, 0.192); // #2b2d31
pub const CHAT: Color = Color::from_rgb(0.192, 0.200, 0.220); // #313338
pub const FOOTER: Color = Color::from_rgb(0.137, 0.141, 0.157); // #232428
pub const HOVER: Color = Color::from_rgb(0.231, 0.239, 0.263); // #3b3d43
pub const SELECTED: Color = Color::from_rgb(0.251, 0.259, 0.282); // #404048
pub const TEXT: Color = Color::from_rgb(0.859, 0.871, 0.882); // #dbdee1
pub const MUTED: Color = Color::from_rgb(0.580, 0.608, 0.643); // #949ba4
pub const BRIGHT: Color = Color::WHITE;
pub const BLURPLE: Color = Color::from_rgb(0.345, 0.396, 0.949); // #5865f2
pub const GREEN: Color = Color::from_rgb(0.137, 0.647, 0.349); // #23a559
pub const RED: Color = Color::from_rgb(0.949, 0.247, 0.263); // #f23f43
pub const YELLOW: Color = Color::from_rgb(0.941, 0.698, 0.196); // #f0b232

pub fn theme() -> Theme {
    Theme::custom(
        "Discord".to_owned(),
        Palette {
            background: CHAT,
            text: TEXT,
            primary: BLURPLE,
            success: GREEN,
            warning: YELLOW,
            danger: RED,
        },
    )
}

fn fill(color: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(color)),
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

pub fn rail(_: &Theme) -> container::Style {
    fill(RAIL)
}

pub fn sidebar(_: &Theme) -> container::Style {
    fill(SIDEBAR)
}

pub fn chat(_: &Theme) -> container::Style {
    fill(CHAT)
}

pub fn footer(_: &Theme) -> container::Style {
    fill(FOOTER)
}

/// A rounded card on the chat pane (embeds, code, popups).
pub fn card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SIDEBAR)),
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

/// The circle behind a name's first letter.
pub fn avatar(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        border: Border {
            radius: 20.0.into(),
            ..Border::default()
        },
        text_color: Some(BRIGHT),
        ..container::Style::default()
    }
}

/// The ring of a speaking participant.
pub fn speaking(on: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(SELECTED)),
        border: Border {
            radius: 12.0.into(),
            width: if on { 2.0 } else { 0.0 },
            color: GREEN,
        },
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

/// A red pill for mention counts.
pub fn badge(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(RED)),
        border: Border {
            radius: 9.0.into(),
            ..Border::default()
        },
        text_color: Some(BRIGHT),
        ..container::Style::default()
    }
}

fn tint(base: Color, status: button::Status, hover: Color) -> Option<Background> {
    match status {
        button::Status::Hovered | button::Status::Pressed => Some(Background::Color(hover)),
        _ => Some(Background::Color(base)),
    }
}

/// A channel or conversation row.
pub fn row_button(
    selected: bool,
    unread: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: if selected {
            Some(Background::Color(SELECTED))
        } else {
            match status {
                button::Status::Hovered | button::Status::Pressed => Some(Background::Color(HOVER)),
                _ => None,
            }
        },
        text_color: if selected || unread { BRIGHT } else { MUTED },
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// A server icon: round, square-ish when hovered or selected, blurple when selected.
pub fn server_button(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hot = selected || matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: tint(if selected { BLURPLE } else { CHAT }, status, BLURPLE),
            text_color: BRIGHT,
            border: Border {
                radius: if hot { 16.0 } else { 24.0 }.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// A small square icon-like action (mute, deafen, settings).
pub fn tool_button(active: bool, danger: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => Some(Background::Color(HOVER)),
            _ => None,
        },
        text_color: if danger {
            RED
        } else if active {
            BRIGHT
        } else {
            MUTED
        },
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// A button that looks like its content (a name or avatar that opens a profile).
pub fn plain_button(_: &Theme, _: button::Status) -> button::Style {
    button::Style {
        text_color: TEXT,
        ..button::Style::default()
    }
}

/// A filled action (blurple), or a danger action (red).
pub fn action_button(danger: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let base = if danger { RED } else { BLURPLE };
        let faded = Color { a: 0.4, ..base };
        button::Style {
            background: Some(Background::Color(if status == button::Status::Disabled {
                faded
            } else {
                base
            })),
            text_color: BRIGHT,
            border: Border {
                radius: 4.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// A stable avatar color from a name or id, from the web client's palette.
pub fn avatar_color(seed: u64) -> Color {
    const COLORS: [Color; 6] = [
        BLURPLE,
        GREEN,
        Color::from_rgb(0.929, 0.259, 0.204),
        YELLOW,
        Color::from_rgb(0.322, 0.631, 0.784),
        Color::from_rgb(0.737, 0.298, 0.667),
    ];
    COLORS[(seed % COLORS.len() as u64) as usize]
}

/// The letter shown in an avatar.
pub fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect())
}
