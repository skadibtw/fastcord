//! Desktop integration (issue #105): deep links, the tray icon and second-launch
//! hand-off streams, the close-to-tray decision, the settings section with the
//! application preferences, and the
//! compact call window. The native parts are `fastcord_platform::{tray,
//! instance, autostart}`; nothing here polls: every stream blocks on a bounded
//! queue fed by an OS thread.

use std::sync::Mutex;

use fastcord_model::Snowflake;
use fastcord_platform::instance::Forwarded;
use fastcord_platform::tray::Command;
use iced::widget::{checkbox, column, container, row, scrollable, space, text};
use iced::{Element, Length};

use crate::gateway::{VoiceControls, VoiceParticipant};
use crate::i18n::{tr, trf};
use crate::icons::{self, Action, Icon};
use crate::{Message, message_link, style};

/// A place a link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeepLink {
    /// `guild` is `None` for a direct or group conversation (`@me`).
    Channel {
        guild: Option<Snowflake>,
        channel: Snowflake,
        message: Option<Snowflake>,
    },
    /// An invite code. Opening it only shows the invite; joining is a click.
    Invite(String),
}

/// What `text` (a command-line argument or a pasted link, nothing else in it)
/// names: `discord://-/channels/{guild|@me}/{channel}[/{message}]`,
/// `discord://-/invite/{code}`, `https://discord.com/channels/...` (also the
/// PTB and Canary hosts) or an invite link. Nothing is fetched to resolve it.
pub fn parse(text: &str) -> Option<DeepLink> {
    let text = text.trim();
    if text.is_empty() || text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let path = if let Some(rest) = text.strip_prefix("discord://") {
        let (host, path) = rest.split_once('/')?;
        if host != "-" {
            return None;
        }
        if let Some(code) = path.strip_prefix("invite/") {
            return invite(&format!("https://discord.gg/{code}"));
        }
        path
    } else if let Some(rest) = text.strip_prefix("https://") {
        let (host, path) = rest.split_once('/')?;
        if !message_link::HOSTS.contains(&host.to_ascii_lowercase().as_str()) {
            return invite(text);
        }
        if path.starts_with("invite/") {
            return invite(text);
        }
        path
    } else {
        return invite(text);
    };
    let path = path.split(['?', '#']).next()?;
    let mut parts = path.trim_end_matches('/').split('/');
    if parts.next()? != "channels" {
        return None;
    }
    let guild = match parts.next()? {
        "@me" => None,
        part => Some(message_link::id(part)?),
    };
    let channel = message_link::id(parts.next()?)?;
    let message = parts.next().map(message_link::id);
    if parts.next().is_some() || message == Some(None) {
        return None;
    }
    Some(DeepLink::Channel {
        guild,
        channel,
        message: message.flatten(),
    })
}

fn invite(text: &str) -> Option<DeepLink> {
    let mut codes = fastcord_discord::extract_codes(text);
    (codes.len() == 1).then(|| DeepLink::Invite(codes.remove(0)))
}

/// What a close request of the window does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    /// Hide the window; the process, its call and the tray icon stay.
    Hide,
    /// Leave any call and exit.
    Quit,
}

/// Closing hides the window only when the user chose the tray *and* the icon
/// is up: with no icon there would be no way back to a hidden window.
pub fn close_action(close_to_tray: bool, tray_running: bool) -> CloseAction {
    if close_to_tray && tray_running {
        CloseAction::Hide
    } else {
        CloseAction::Quit
    }
}

/// The link the process was started with, taken once by `App::boot`.
static STARTUP: Mutex<Option<String>> = Mutex::new(None);

pub fn set_startup(arg: Option<String>) {
    if let Ok(mut slot) = STARTUP.lock() {
        *slot = arg;
    }
}

pub fn take_startup() -> Option<DeepLink> {
    STARTUP.lock().ok()?.take().and_then(|arg| parse(&arg))
}

type Receiver = tokio::sync::mpsc::Receiver<Forwarded>;

/// Where `main` parks the hand-off queue for the subscription.
static FORWARDED: Mutex<Option<Receiver>> = Mutex::new(None);

pub fn set_forwarded(receiver: Receiver) {
    if let Ok(mut slot) = FORWARDED.lock() {
        *slot = Some(receiver);
    }
}

/// Second launches, as the guard in `main` received them.
pub fn forwarded_stream() -> impl iced::futures::Stream<Item = Forwarded> {
    iced::stream::channel(8, async move |mut output| {
        use iced::futures::SinkExt;
        let receiver = FORWARDED.lock().ok().and_then(|mut slot| slot.take());
        let Some(mut receiver) = receiver else {
            std::future::pending::<()>().await;
            return;
        };
        while let Some(request) = receiver.recv().await {
            let _ = output.send(request).await;
        }
    })
}

#[derive(Clone, Debug)]
pub enum TrayEvent {
    Started,
    Failed(String),
    Command(Command),
}

/// Queue between the tray thread and the application, in commands.
const TRAY_QUEUE: usize = 16;

/// The tray icon exists while this stream is subscribed.
pub fn tray_stream() -> impl iced::futures::Stream<Item = TrayEvent> {
    iced::stream::channel(8, async move |mut output| {
        use iced::futures::SinkExt;
        let (tx, mut rx) = tokio::sync::mpsc::channel(TRAY_QUEUE);
        let started = fastcord_platform::tray::Tray::start(move |command| {
            // A full queue drops a click; a held-down menu cannot flood it.
            let _ = tx.try_send(command);
        });
        let _tray = match started {
            Ok(tray) => tray,
            Err(error) => {
                let _ = output.send(TrayEvent::Failed(error.to_string())).await;
                std::future::pending().await
            }
        };
        let _ = output.send(TrayEvent::Started).await;
        while let Some(command) = rx.recv().await {
            let _ = output.send(TrayEvent::Command(command)).await;
        }
    })
}

/// The desktop section of the audio and general settings.
pub fn view<'a>(
    close_to_tray: bool,
    tray: &Result<(), String>,
    autostart: Option<bool>,
) -> Element<'a, Message> {
    let mut content = column![text(tr("Desktop")).size(18)].spacing(8);
    content = content.push(
        checkbox(close_to_tray)
            .label(tr(
                "Closing the window minimizes fastcord to the tray instead of quitting",
            ))
            .on_toggle_maybe(tray.is_ok().then_some(Message::CloseToTrayToggled)),
    );
    content = content.push(
        text(match tray {
            Ok(()) => {
                tr("Quit in the tray menu always exits fastcord and leaves any call.").to_owned()
            }
            Err(reason) => trf(
                "Tray icon unavailable: {} Closing the window quits.",
                &[&tr(reason)],
            ),
        })
        .size(12),
    );
    if let Some(on) = autostart {
        content = content.push(
            checkbox(on)
                .label(tr("Start fastcord when I sign in"))
                .on_toggle(Message::AutostartToggled),
        );
    }
    content.into()
}

/// Whether the window runs on Wayland, which gives applications no way to keep
/// a window above the others (winit picks Wayland whenever it is available).
static WAYLAND: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some()
});

/// The compact window's description; on Wayland it is not advertised as
/// staying on top, because the compositor decides that.
pub fn compact_tip() -> &'static str {
    if *WAYLAND {
        tr("Compact call window (small; Wayland does not let it stay on top)")
    } else {
        tr("Compact call window (small, always on top)")
    }
}

/// Window size of the compact call mode.
pub const COMPACT_SIZE: iced::Size = iced::Size::new(300.0, 260.0);

/// The compact call window: who is speaking, the account's own microphone and
/// deafen state, Disconnect, and the way back to the full window.
pub fn compact_view(voice: &VoiceControls) -> Element<'static, Message> {
    let ring = |participant: &VoiceParticipant| {
        let speaking = participant.speaking;
        let color = if speaking { style::GREEN } else { style::MUTED };
        container(space())
            .width(12)
            .height(12)
            .style(move |_| container::Style {
                border: iced::Border {
                    color,
                    width: if speaking { 3.0 } else { 1.0 },
                    radius: 6.0.into(),
                },
                ..container::Style::default()
            })
    };
    let mut people = column![].spacing(6);
    for participant in &voice.participants {
        people = people.push(
            row![
                ring(participant),
                text(voice.display_name(participant.user_id))
                    .size(14)
                    .wrapping(text::Wrapping::None),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
    }
    let tool = |icon, press, active, danger, tip: &str| {
        icons::action(Action {
            icon,
            on_press: Some(press),
            active,
            danger,
            tip: tip.to_owned(),
            badge: 0,
        })
    };
    let controls = row![
        tool(
            Icon::Mic {
                muted: voice.self_muted
            },
            Message::SetSelfMute(!voice.self_muted),
            !voice.self_muted,
            voice.self_muted,
            if voice.self_muted {
                tr("Microphone muted. Click to unmute")
            } else {
                tr("Microphone on. Click to mute")
            },
        ),
        tool(
            Icon::Headphones {
                deafened: voice.deafened
            },
            Message::SetDeafen(!voice.deafened),
            !voice.deafened,
            voice.deafened,
            if voice.deafened {
                tr("Deafened. Click to hear again")
            } else {
                tr("Hearing. Click to deafen")
            },
        ),
        tool(
            Icon::HangUp,
            Message::LeaveVoice,
            true,
            true,
            tr("Disconnect")
        ),
        space::horizontal(),
        tool(
            Icon::Pin,
            Message::ToggleCompact,
            true,
            false,
            tr("Back to the full window"),
        ),
    ]
    .spacing(6)
    .align_y(iced::Alignment::Center);
    container(
        column![
            text(tr("In call")).size(12).color(crate::style::muted()),
            scrollable(people).height(Length::Fill),
            controls,
        ]
        .spacing(8),
    )
    .padding(10)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// Reads (`None`) or writes the start-at-sign-in setting on a blocking thread
/// (registry, desktop entry or launch agent) and answers with the setting now.
pub(crate) fn autostart_task(set: Option<bool>) -> iced::Task<Message> {
    iced::Task::perform(
        async move {
            tokio::task::spawn_blocking(move || match set {
                Some(on) => fastcord_platform::autostart::set(on)
                    .map(|()| on)
                    .map_err(|error| error.to_string()),
                None => Ok(fastcord_platform::autostart::is_enabled()),
            })
            .await
            .unwrap_or_else(|_| Err("The autostart task stopped.".to_owned()))
        },
        Message::AutostartDone,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(guild: Option<u64>, channel: u64, message: Option<u64>) -> Option<DeepLink> {
        Some(DeepLink::Channel {
            guild: guild.map(Snowflake),
            channel: Snowflake(channel),
            message: message.map(Snowflake),
        })
    }

    #[test]
    fn channel_and_message_links_parse_in_every_form() {
        for link in [
            "discord://-/channels/1/2/3",
            "https://discord.com/channels/1/2/3",
            "https://PTB.discord.com/channels/1/2/3/?x=1#y",
            "  https://canary.discord.com/channels/1/2/3\n",
        ] {
            assert_eq!(parse(link), channel(Some(1), 2, Some(3)), "{link:?}");
        }
        assert_eq!(parse("discord://-/channels/1/2"), channel(Some(1), 2, None));
        assert_eq!(
            parse("https://discord.com/channels/@me/2"),
            channel(None, 2, None)
        );
        assert_eq!(
            parse("discord://-/channels/@me/2/3"),
            channel(None, 2, Some(3))
        );
    }

    #[test]
    fn invites_parse_and_other_text_does_not() {
        for link in [
            "https://discord.gg/AbC-123",
            "discord.gg/AbC-123",
            "https://discord.com/invite/AbC-123",
            "discord://-/invite/AbC-123",
        ] {
            assert_eq!(
                parse(link),
                Some(DeepLink::Invite("AbC-123".to_owned())),
                "{link:?}"
            );
        }
        for bad in [
            "",
            "hello",
            "join https://discord.gg/AbC-123 now",
            "discord://evil/channels/1/2",
            "discord://-/channels/1",
            "discord://-/channels/1/2/3/4",
            "discord://-/channels/x/2",
            "discord://-/channels/1/0",
            "discord://-/channels/1/2/x",
            "discord://-/users/1/2",
            "http://discord.com/channels/1/2/3",
            "https://evil.example/channels/1/2/3",
            "https://discord.com.evil.example/channels/1/2",
            "https://notdiscord.gg/AbC-123",
            "https://evil.example/discord.gg/AbC-123",
            "javascript:alert(1)",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn closing_hides_only_when_the_tray_was_chosen_and_is_up() {
        assert_eq!(close_action(false, true), CloseAction::Quit, "default");
        assert_eq!(close_action(false, false), CloseAction::Quit);
        assert_eq!(close_action(true, false), CloseAction::Quit, "no way back");
        assert_eq!(close_action(true, true), CloseAction::Hide);
    }
}
