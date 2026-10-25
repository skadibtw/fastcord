//! Desktop integration (issue #105): deep links, the tray icon and second-launch
//! hand-off streams, the close-to-tray decision, the settings section with the
//! blocking bypass (`fastcord_bypass`, started once at launch), and the
//! compact call window. The native parts are `fastcord_platform::{tray,
//! instance, autostart}`; nothing here polls: every stream blocks on a bounded
//! queue fed by an OS thread.

use std::sync::{Mutex, OnceLock};

use fastcord_model::Snowflake;
use fastcord_platform::instance::Forwarded;
use fastcord_platform::tray::Command;
use iced::widget::{
    checkbox, column, container, pick_list, row, scrollable, space, text, text_input,
};
use iced::{Element, Length};
use serde_json::{Value, json};

use crate::gateway::{VoiceControls, VoiceParticipant};
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

/// Which strategy the bypass runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Direct, then each built-in preset until a host answers.
    Auto,
    Preset(&'static fastcord_bypass::Preset),
    /// Only the advanced options.
    Custom,
}

impl Choice {
    fn all() -> Vec<Self> {
        std::iter::once(Self::Auto)
            .chain(fastcord_bypass::presets().iter().map(Self::Preset))
            .chain([Self::Custom])
            .collect()
    }

    fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Preset(preset) => preset.id,
            Self::Custom => "custom",
        }
    }

    /// An unknown id (a preset this OS lacks, an older file) means Automatic.
    fn from_id(id: &str) -> Self {
        Self::all()
            .into_iter()
            .find(|choice| choice.id() == id)
            .unwrap_or(Self::Auto)
    }
}

impl std::fmt::Display for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "Automatic: try each strategy until Discord answers",
            Self::Preset(preset) => preset.name,
            Self::Custom => "Only my options (Advanced)",
        })
    }
}

/// The blocking bypass setting: Discord through the embedded byedpi.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bypass {
    pub on: bool,
    pub choice: Choice,
    /// byedpi options put before the strategy (for Custom, the whole strategy).
    pub options: String,
}

pub const OPTIONS_LIMIT: usize = 512;

impl Default for Bypass {
    fn default() -> Self {
        Self {
            on: false,
            choice: Choice::Auto,
            options: String::new(),
        }
    }
}

impl Bypass {
    pub fn from_json(value: &Value) -> Self {
        Self {
            on: value.get("on").and_then(Value::as_bool) == Some(true),
            choice: Choice::from_id(value.get("choice").and_then(Value::as_str).unwrap_or("")),
            options: value
                .get("options")
                .and_then(Value::as_str)
                .filter(|options| options.len() <= OPTIONS_LIMIT)
                .unwrap_or_default()
                .to_owned(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"on": self.on, "choice": self.choice.id(), "options": self.options})
    }

    /// The byedpi arguments this setting runs.
    fn strategy(&self) -> String {
        let strategy = match self.choice {
            Choice::Auto => fastcord_bypass::auto_strategy(),
            Choice::Preset(preset) => preset.args.to_owned(),
            Choice::Custom => String::new(),
        };
        format!("{} {strategy}", self.options).trim().to_owned()
    }
}

/// What this process started with: the setting and, when on, the engine's result.
struct Launch {
    bypass: Bypass,
    started: Option<Result<(), String>>,
}

static LAUNCH: OnceLock<Launch> = OnceLock::new();

/// Once, before any network request: routes never switch while running.
pub fn start_bypass(bypass: Bypass) {
    let started = bypass.on.then(|| {
        fastcord_bypass::enable(&bypass.strategy())
            .map(|_| ())
            .map_err(|error| error.to_string())
    });
    let _ = LAUNCH.set(Launch { bypass, started });
}

fn bypass_status(bypass: &Bypass) -> String {
    let Some(launch) = LAUNCH.get() else {
        return "Restart fastcord to apply.".to_owned();
    };
    let unchanged = launch.bypass == *bypass || !(launch.bypass.on || bypass.on);
    match &launch.started {
        _ if !unchanged => "Restart fastcord to apply.".to_owned(),
        None => "Off: Discord is reached directly.".to_owned(),
        Some(Ok(())) => "On: Discord connections go through byedpi.".to_owned(),
        Some(Err(error)) => format!("Not running: {error} Discord is reached directly."),
    }
}

/// The desktop section of the audio and general settings.
pub fn view<'a>(
    close_to_tray: bool,
    tray: &Result<(), String>,
    autostart: Option<bool>,
    bypass: &'a Bypass,
    advanced: bool,
) -> Element<'a, Message> {
    let mut content = column![text("Desktop").size(18)].spacing(8);
    content = content.push(
        checkbox(close_to_tray)
            .label("Closing the window minimizes fastcord to the tray instead of quitting")
            .on_toggle_maybe(tray.is_ok().then_some(Message::CloseToTrayToggled)),
    );
    content = content.push(
        text(match tray {
            Ok(()) => "Quit in the tray menu always exits fastcord and leaves any call.".to_owned(),
            Err(reason) => format!("Tray icon unavailable: {reason} Closing the window quits."),
        })
        .size(12),
    );
    if let Some(on) = autostart {
        content = content.push(
            checkbox(on)
                .label("Start fastcord when I sign in")
                .on_toggle(Message::AutostartToggled),
        );
    }
    content = content
        .push(text("Blocking bypass").size(18))
        .push(
            checkbox(bypass.on)
                .label("Reach Discord through the built-in byedpi, for networks that block it")
                .on_toggle(Message::BypassToggled),
        )
        .push(
            pick_list(
                Choice::all(),
                Some(bypass.choice),
                Message::BypassChoiceSelected,
            )
            .width(Length::Fill),
        )
        .push(
            text(
                "Messages, the Gateway and media files use it; voice and video still connect \
                 directly. Automatic waits up to 2 seconds per strategy the first time it \
                 reaches each Discord server.",
            )
            .size(12),
        )
        .push(text(bypass_status(bypass)).size(12))
        .push(
            checkbox(advanced)
                .label("Advanced options")
                .on_toggle(Message::BypassAdvancedToggled),
        );
    if advanced {
        content = content
            .push(
                text_input(
                    "byedpi options, for example -I 192.168.1.10",
                    &bypass.options,
                )
                .on_input(Message::BypassOptionsEdited)
                .padding(8),
            )
            .push(
                text(
                    "Put before the chosen strategy; with \"Only my options\" they are the \
                     whole strategy. Behind a VPN that captures traffic with a TUN adapter, \
                     -I <this computer's network address> sends byedpi past it.",
                )
                .size(12),
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
        "Compact call window (small; Wayland does not let it stay on top)"
    } else {
        "Compact call window (small, always on top)"
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
                "Microphone muted. Click to unmute"
            } else {
                "Microphone on. Click to mute"
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
                "Deafened. Click to hear again"
            } else {
                "Hearing. Click to deafen"
            },
        ),
        tool(Icon::HangUp, Message::LeaveVoice, true, true, "Disconnect"),
        space::horizontal(),
        tool(
            Icon::Pin,
            Message::ToggleCompact,
            true,
            false,
            "Back to the full window",
        ),
    ]
    .spacing(6)
    .align_y(iced::Alignment::Center);
    container(
        column![
            text("In call").size(12).style(text::secondary),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bypass_strategy_puts_options_first_and_reads_unknown_choices_as_automatic() {
        let preset = &fastcord_bypass::presets()[0];
        let bypass = Bypass {
            on: true,
            choice: Choice::Preset(preset),
            options: "-I 192.0.2.1".to_owned(),
        };
        assert_eq!(bypass.strategy(), format!("-I 192.0.2.1 {}", preset.args));
        assert_eq!(Bypass::from_json(&bypass.to_json()), bypass);
        let custom = Bypass {
            choice: Choice::Custom,
            ..bypass
        };
        assert_eq!(custom.strategy(), "-I 192.0.2.1");
        let auto = Bypass::default();
        assert_eq!(auto.strategy(), fastcord_bypass::auto_strategy());
        let unknown =
            Bypass::from_json(&json!({"on": true, "choice": "gone", "options": "x".repeat(600)}));
        assert_eq!(unknown.choice, Choice::Auto);
        assert!(unknown.options.is_empty(), "oversized options are dropped");
    }

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
