//! The test-only fake mode: `fastcord --fake-discord <port>` talks to the
//! loopback `fastcord-fake` server instead of Discord (debug builds only).
//!
//! It signs in at once with [`TOKEN`], memory-only, so no account, credential
//! store, tray, autostart or single-instance hand-off is involved, and it keeps
//! its settings and caches under `fastcord-fake` (see `audio_devices::app_dir_name`).
//!
//! `--script <file>` runs a UI script for layout checks without a person at
//! the screen: one step per line, `wait <seconds>`, `open <link>` (as a second
//! launch would), `size <width> <height>`, `toggle <panel>` (open or close one
//! of [`PANELS`] as its button would), `join <guild> <channel>` (join a voice
//! channel; the fake answers no voice, so it stays connecting), `snap <name>` (the window's own pixels
//! to `<name>.png` beside the script; the player window's while one is open),
//! `stream <seconds>` (a synthetic watched stream, see [`Step::Stream`]) and `quit`.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use iced::widget::{container, text};
use iced::{Element, Length};

/// The only token the fake server accepts.
pub const TOKEN: &str = "fake-token";

static PORT: OnceLock<u16> = OnceLock::new();
static SCRIPT: OnceLock<PathBuf> = OnceLock::new();

/// Panels a script can open or close.
pub const PANELS: &[&str] = &[
    "search",
    "pins",
    "inbox",
    "friends",
    "emoji",
    "audio",
    "app",
    "network",
    "chat",
    "account",
    "login-qr",
    "login-password",
    "login-token",
    "login-accounts",
    "server-menu",
    "conversation-menu",
    "account-menu",
    "mic-menu",
    "new-dm",
    "requests",
    "settings",
    "close-menus",
    "escape",
    "gif",
    "sticker",
    "profile",
    "accounts",
    "server-settings",
    "roles",
    "group",
    "long-draft",
    "create-channel",
    "network-details",
    "friend-menu",
];

/// One step of a `--script`.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Open(String),
    Size(f32, f32),
    /// One of [`PANELS`].
    Toggle(&'static str),
    /// A voice channel of a server.
    Join(u64, u64),
    /// Where the window's pixels go (the player window's while one is open).
    Snap(PathBuf),
    /// Watch a synthetic stream for this many seconds: a player window opens
    /// and 1280x720 pictures arrive at 30 fps (no voice exists offline).
    Stream(f32),
    Quit,
}

/// The script's steps with the pause before each; a bad line is skipped
/// with a message on standard error.
fn parse_script(text: &str, dir: &std::path::Path) -> Vec<(Duration, Step)> {
    let mut steps = Vec::new();
    let mut pause = Duration::ZERO;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        let step = match verb {
            "wait" => {
                match rest.parse::<f32>() {
                    Ok(seconds) if seconds >= 0.0 => pause += Duration::from_secs_f32(seconds),
                    _ => eprintln!("script: bad wait: {line}"),
                }
                continue;
            }
            "open" => Step::Open(rest.to_owned()),
            "size" => match rest
                .split_once(' ')
                .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            {
                Some((width, height)) => Step::Size(width, height),
                None => {
                    eprintln!("script: bad size: {line}");
                    continue;
                }
            },
            "snap" if !rest.is_empty() && !rest.contains(['/', '\\']) => {
                Step::Snap(dir.join(format!("{rest}.png")))
            }
            "toggle" => match PANELS.iter().find(|panel| **panel == rest) {
                Some(panel) => Step::Toggle(panel),
                None => {
                    eprintln!("script: unknown panel: {line}");
                    continue;
                }
            },
            "join" => match rest
                .split_once(' ')
                .and_then(|(g, c)| Some((g.trim().parse().ok()?, c.trim().parse().ok()?)))
            {
                Some((guild, channel)) => Step::Join(guild, channel),
                None => {
                    eprintln!("script: bad join: {line}");
                    continue;
                }
            },
            "stream" => match rest.parse::<f32>() {
                Ok(seconds) if (0.0..=600.0).contains(&seconds) => Step::Stream(seconds),
                _ => {
                    eprintln!("script: bad stream: {line}");
                    continue;
                }
            },
            "quit" => Step::Quit,
            _ => {
                eprintln!("script: unknown step: {line}");
                continue;
            }
        };
        steps.push((std::mem::take(&mut pause), step));
    }
    steps
}

/// The steps of `--script`, each after its pause; nothing without one.
pub fn script() -> impl iced::futures::Stream<Item = Step> {
    iced::stream::channel(1, async move |mut output| {
        use iced::futures::SinkExt;
        let Some(path) = SCRIPT.get() else {
            std::future::pending::<()>().await;
            return;
        };
        let text = std::fs::read_to_string(path).unwrap_or_else(|_| {
            eprintln!("script: cannot read {}", path.display());
            String::new()
        });
        let dir = path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_owned();
        for (pause, step) in parse_script(&text, &dir) {
            tokio::time::sleep(pause).await;
            let _ = output.send(step).await;
        }
        std::future::pending::<()>().await;
    })
}

/// Writes a window screenshot as an RGBA PNG.
pub fn save(path: &std::path::Path, shot: &iced::window::Screenshot) {
    let saved = image::save_buffer(
        path,
        &shot.rgba,
        shot.size.width,
        shot.size.height,
        image::ExtendedColorType::Rgba8,
    );
    match saved {
        Ok(()) => eprintln!("script: saved {}", path.display()),
        Err(error) => eprintln!("script: {}: {error}", path.display()),
    }
}

pub fn active() -> bool {
    PORT.get().is_some()
}

/// `--login-screen`: start on the sign-in screen instead of signing in.
pub fn login_screen() -> bool {
    std::env::args().any(|arg| arg == "--login-screen")
}

/// Reads `--fake-discord <port>` (a bare port, nothing else) and points the
/// Discord client at that loopback server. A malformed value ends the process.
pub fn init() {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--script" {
            if let Some(path) = args.next() {
                let _ = SCRIPT.set(PathBuf::from(path));
            }
            continue;
        }
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

/// Feeds a synthetic 1280x720 NV12 picture at 30 fps into `session` for
/// `seconds` on a thread of its own, the way the stream decoder's sink does.
pub fn feed(session: std::sync::Arc<crate::playback::Session>, seconds: f32) {
    use fastcord_video::DecodedFrame;
    use fastcord_video::frame::{ColorDescription, Nv12};
    std::thread::spawn(move || {
        let (width, height) = (1280usize, 720usize);
        let mut sink = session.sink();
        let mut picture = vec![128u8; width * height * 3 / 2];
        let started = std::time::Instant::now();
        let mut n = 0u32;
        while started.elapsed().as_secs_f32() < seconds
            && session.active.load(std::sync::atomic::Ordering::Acquire)
        {
            for row in 0..height {
                for x in 0..width {
                    picture[row * width + x] = ((x as u32 + n * 6) / 5 % 256) as u8;
                }
            }
            for pair in picture[width * height..].as_chunks_mut::<2>().0 {
                pair[0] = (n * 3 % 256) as u8;
                pair[1] = 255 - (n * 2 % 256) as u8;
            }
            if let Ok(image) = Nv12::packed(width as u32, height as u32, &picture) {
                sink(
                    0,
                    DecodedFrame {
                        image,
                        color: ColorDescription::default(),
                        timestamp: started.elapsed(),
                    },
                );
            }
            n += 1;
            std::thread::sleep(std::time::Duration::from_millis(33));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_parse_steps_with_their_pauses() {
        let dir = std::path::Path::new("shots");
        let steps = parse_script(
            "# comment\nwait 2\nopen discord://-/channels/@me\nsize 1080 672\nwait 0.5\nwait 0.5\nsnap home\nsnap ../x\nbogus\ntoggle pins\ntoggle nothing\njoin 5001 6009\nquit\n",
            dir,
        );
        assert_eq!(
            steps,
            [
                (
                    Duration::from_secs(2),
                    Step::Open("discord://-/channels/@me".into())
                ),
                (Duration::ZERO, Step::Size(1080.0, 672.0)),
                (Duration::from_secs(1), Step::Snap(dir.join("home.png"))),
                (Duration::ZERO, Step::Toggle("pins")),
                (Duration::ZERO, Step::Join(5001, 6009)),
                (Duration::ZERO, Step::Quit),
            ]
        );
    }

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
