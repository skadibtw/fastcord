//! The dark palette and widget styles of the main screen, after the web
//! client's: a near-black server rail, a dark-grey channel sidebar, a lighter
//! chat pane, blurple actions, red mentions and green "speaking".

use crate::ui::button;
use iced::theme::Palette;
use iced::widget::container;
use iced::{Background, Border, Color, Theme};

pub const RAIL: Color = Color::from_rgb(0.118, 0.122, 0.133); // #1e1f22
pub const SIDEBAR: Color = Color::from_rgb(0.169, 0.176, 0.192); // #2b2d31
pub const CHAT: Color = Color::from_rgb(0.192, 0.200, 0.220); // #313338
pub const FOOTER: Color = Color::from_rgb(0.137, 0.141, 0.157); // #232428
pub const HOVER: Color = Color::from_rgb(0.231, 0.239, 0.263); // #3b3d43
pub const SELECTED: Color = Color::from_rgb(0.251, 0.259, 0.282); // #404048
pub const TEXT: Color = Color::from_rgb(0.859, 0.871, 0.882); // #dbdee1
pub const MUTED: Color = Color::from_rgb(0.647, 0.671, 0.706); // #a5abb4
pub const BRIGHT: Color = Color::WHITE;
pub const BLURPLE: Color = Color::from_rgb(0.345, 0.396, 0.949); // #5865f2
pub const GREEN: Color = Color::from_rgb(0.137, 0.647, 0.349); // #23a559
pub const RED: Color = Color::from_rgb(0.949, 0.247, 0.263); // #f23f43
pub const YELLOW: Color = Color::from_rgb(0.941, 0.698, 0.196); // #f0b232
/// Secondary labels inside panels (dates, counts): lighter than [`MUTED`].
pub const SUBTLE: Color = Color::from_rgb(0.710, 0.729, 0.757); // #b5bac1
/// Mention and timestamp pills in messages: Discord's light text on a
/// translucent blurple, readable on the chat background.
pub const MENTION: Color = Color::from_rgb(0.788, 0.804, 0.984); // #c9cdfb
pub const MENTION_BG: Color = Color::from_rgba(0.345, 0.396, 0.949, 0.3);
/// Filled secondary buttons.
pub const SECONDARY: Color = Color::from_rgb(0.306, 0.314, 0.345); // #4e5058

/// The high-contrast variant (`chat_prefs`): near-black panes, white text.
fn high_contrast() -> bool {
    crate::chat_prefs::current().high_contrast
}

/// Panel background: `normal`, or black-based under high contrast.
fn pane(normal: Color, strong: Color) -> Color {
    if high_contrast() { strong } else { normal }
}

/// Body text.
pub fn text_color() -> Color {
    pane(TEXT, BRIGHT)
}

/// Secondary text, for the styles in this module.
pub fn muted() -> Color {
    pane(MUTED, Color::from_rgb(0.88, 0.89, 0.90))
}

pub fn theme() -> Theme {
    Theme::custom(
        "Discord".to_owned(),
        Palette {
            background: pane(CHAT, Color::BLACK),
            text: text_color(),
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
        text_color: Some(text_color()),
        ..container::Style::default()
    }
}

pub fn rail(_: &Theme) -> container::Style {
    fill(pane(RAIL, Color::BLACK))
}

pub fn sidebar(_: &Theme) -> container::Style {
    fill(pane(SIDEBAR, Color::from_rgb(0.05, 0.05, 0.06)))
}

pub fn chat(_: &Theme) -> container::Style {
    fill(pane(CHAT, Color::BLACK))
}

pub fn footer(_: &Theme) -> container::Style {
    fill(pane(FOOTER, Color::from_rgb(0.03, 0.03, 0.04)))
}

/// A rounded card on the chat pane (embeds, code, popups).
pub fn card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(pane(
            SIDEBAR,
            Color::from_rgb(0.05, 0.05, 0.06),
        ))),
        border: Border {
            radius: 6.0.into(),
            width: if high_contrast() { 1.0 } else { 0.0 },
            color: BRIGHT,
        },
        text_color: Some(text_color()),
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

/// Row of a message that mentions the account: gold tint.
pub fn mention_row(_: &Theme) -> container::Style {
    fill(Color {
        a: 1.0,
        ..Color::from_rgb(0.263, 0.243, 0.212) // #433e36
    })
}

/// The gold bar on the left edge of such a row.
pub fn mention_bar(_: &Theme) -> container::Style {
    fill(YELLOW)
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

/// The red "LIVE" tag of someone streaming.
pub fn live_badge(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(RED)),
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        text_color: Some(BRIGHT),
        ..container::Style::default()
    }
}

/// The green ring around the avatar of someone speaking (always the same
/// size, so rows never move).
pub fn avatar_ring(on: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        border: Border {
            radius: 14.0.into(),
            width: if on { 2.0 } else { 0.0 },
            color: GREEN,
        },
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
        text_color: if selected || unread { BRIGHT } else { muted() },
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
            muted()
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
        text_color: text_color(),
        ..button::Style::default()
    }
}

/// A filled action (blurple), or a danger action (red).
pub fn action_button(danger: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let base = if danger { RED } else { BLURPLE };
        let faded = Color { a: 0.4, ..base };
        button::Style {
            background: Some(Background::Color(match status {
                button::Status::Disabled => faded,
                button::Status::Hovered | button::Status::Pressed => Color {
                    r: base.r * 0.85,
                    g: base.g * 0.85,
                    b: base.b * 0.85,
                    a: 1.0,
                },
                button::Status::Active => base,
            })),
            // Unavailable reads as such: faded fill and faded text.
            text_color: if status == button::Status::Disabled {
                Color { a: 0.5, ..BRIGHT }
            } else {
                BRIGHT
            },
            border: Border {
                radius: 4.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// A filled grey button for secondary actions (Refresh, Clear, Mark read);
/// `selected` marks the current tab.
pub fn secondary_button(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let base = if selected { SELECTED } else { SECONDARY };
        button::Style {
            background: Some(Background::Color(match status {
                button::Status::Hovered | button::Status::Pressed => HOVER,
                button::Status::Disabled => Color { a: 0.35, ..base },
                button::Status::Active => base,
            })),
            text_color: match status {
                button::Status::Disabled => Color { a: 0.4, ..TEXT },
                _ if selected => BRIGHT,
                _ => TEXT,
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(c: Color) -> f32 {
        let lin = |v: f32| {
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
    }

    fn ratio(a: Color, b: Color) -> f32 {
        let (hi, lo) = (
            luminance(a).max(luminance(b)),
            luminance(a).min(luminance(b)),
        );
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn the_high_contrast_palette_is_stronger() {
        // Pure functions of the colors: the global preference is not touched.
        let normal = ratio(MUTED, SIDEBAR);
        let strong = ratio(
            Color::from_rgb(0.88, 0.89, 0.90),
            Color::from_rgb(0.05, 0.05, 0.06),
        );
        assert!(strong >= 7.0, "{strong}");
        assert!(strong > normal);
        assert!(ratio(BRIGHT, Color::BLACK) >= 7.0);
    }

    #[test]
    fn mention_pills_are_readable_on_the_chat_background() {
        let a = MENTION_BG.a;
        let pill = Color::from_rgb(
            CHAT.r * (1.0 - a) + MENTION_BG.r * a,
            CHAT.g * (1.0 - a) + MENTION_BG.g * a,
            CHAT.b * (1.0 - a) + MENTION_BG.b * a,
        );
        let contrast = ratio(MENTION, pill);
        assert!(contrast >= 4.5, "{contrast}");
    }
}
