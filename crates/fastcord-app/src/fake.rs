//! The test-only fake mode: `fastcord --fake-discord <port>` talks to the
//! loopback `fastcord-fake` server instead of Discord (debug builds only).
//!
//! It signs in at once with [`TOKEN`], memory-only, so no account, credential
//! store, tray, autostart or single-instance hand-off is involved, and it keeps
//! its settings and caches under `fastcord-fake` (see `audio_devices::app_dir_name`).

use std::sync::OnceLock;

use iced::widget::{container, text};
use iced::{Element, Length};

/// The only token the fake server accepts.
pub const TOKEN: &str = "fake-token";

static PORT: OnceLock<u16> = OnceLock::new();

pub fn active() -> bool {
    PORT.get().is_some()
}

/// Reads `--fake-discord <port>` (a bare port, nothing else) and points the
/// Discord client at that loopback server. A malformed value ends the process.
pub fn init() {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg != "--fake-discord" {
            continue;
        }
        let port = args.next().as_deref().and_then(parse_port);
        let Some(port) = port else {
            eprintln!("--fake-discord needs a bare port number, for example --fake-discord 8765");
            std::process::exit(2);
        };
        if fastcord_discord::fake_origin::set_origin(&format!("127.0.0.1:{port}")).is_err() {
            eprintln!("--fake-discord: the origin could not be set");
            std::process::exit(2);
        }
        let _ = PORT.set(port);
        return;
    }
}

fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|port| *port != 0)
}

/// The thin strip across the top of the window.
pub fn banner<'a, Message: 'a>() -> Option<Element<'a, Message>> {
    let port = *PORT.get()?;
    Some(
        container(
            text(format!(
                "Fake Discord on 127.0.0.1:{port}: nothing reaches discord.com"
            ))
            .size(12),
        )
        .width(Length::Fill)
        .padding([2, 8])
        .style(|theme: &iced::Theme| {
            let pair = theme.extended_palette().danger.strong;
            container::Style {
                background: Some(pair.color.into()),
                text_color: Some(pair.text),
                ..container::Style::default()
            }
        })
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bare_nonzero_port_is_accepted() {
        assert_eq!(parse_port("8765"), Some(8765));
        for bad in [
            "",
            "0",
            "-1",
            "+80",
            "65536",
            "127.0.0.1:80",
            "80 ",
            "0x50",
            "discord.com",
        ] {
            assert_eq!(parse_port(bad), None, "{bad}");
        }
    }
}
