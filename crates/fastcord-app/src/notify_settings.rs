//! The alerts panel (issue #106): per-server and per-conversation notification
//! level, mute durations and mention suppression, plus the local "hide muted
//! channels" choice. Remote writes are quarantined pending validation; opening
//! the panel is local and the hide-muted choice stays available. When enabled,
//! every change is one explicit click, checked against what
//! is open right now, and sent once as `PATCH /users/@me/guilds/{g|@me}/settings`
//! (`RestClient::set_notification`); the panel shows what Discord's
//! `USER_GUILD_SETTINGS_UPDATE` echo says, never its own guess.

use fastcord_discord::gateway::NotifyLevel;
use fastcord_discord::state::navigation::NotificationView;
use fastcord_discord::{
    MuteFor, NOTIFICATION_WRITES_ENABLED, NotifySetting, RestClient, RestError,
};
use fastcord_model::Snowflake;
use iced::widget::{button, checkbox, column, container, pick_list, row, space, text};
use iced::{Color, Element, Length, Task};

const WARN: Color = crate::style::RED;
const PAUSED: &str = "Notification changes are paused pending validation. Use the official Discord client to change them. Opening Alerts sends no request.";

/// What a setting applies to: the server, or one conversation (`None` guild
/// for a DM or group DM).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Server(Snowflake),
    Channel(Option<Snowflake>, Snowflake),
}

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    Set(Target, NotifySetting),
    /// The local choice to hide a server's muted channels; handled by the app.
    HideMuted(Snowflake),
    Done(Result<(), RestError>),
}

/// A level as the pick list shows it; `Inherit` follows the next level up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Choice(Option<NotifyLevel>);

impl std::fmt::Display for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            None => "Default",
            Some(NotifyLevel::All) => "All messages",
            Some(NotifyLevel::Mentions) => "Only @mentions",
            Some(NotifyLevel::Nothing) => "Nothing",
        })
    }
}

const CHOICES: [Choice; 4] = [
    Choice(None),
    Choice(Some(NotifyLevel::All)),
    Choice(Some(NotifyLevel::Mentions)),
    Choice(Some(NotifyLevel::Nothing)),
];

#[derive(Debug, Default)]
pub struct State {
    open: bool,
    busy: bool,
    notice: Option<&'static str>,
}

fn describe(error: RestError) -> &'static str {
    match error {
        RestError::NotificationWritesPaused => PAUSED,
        RestError::AccountVerificationRequired => {
            "Discord requires an account check. All account requests have stopped; open the official Discord client."
        }
        RestError::PermissionDenied => "Discord does not allow this.",
        RestError::ResourceGone => "That server or conversation is gone.",
        RestError::AuthenticationRequired => "Discord no longer accepts this login.",
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            "Discord is rate limiting this. Try again shortly."
        }
        RestError::Retryable(_) | RestError::Http(_) => {
            "Discord did not answer; the change may have been applied."
        }
        _ => "The notification change failed.",
    }
}

impl State {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Whether the open server or conversation still is `target`.
    fn current(view: &NotificationView, target: Target) -> bool {
        match target {
            Target::Server(guild) => view.guild == Some(guild),
            Target::Channel(Some(guild), channel) => {
                view.guild == Some(guild) && view.channel == Some(channel)
            }
            Target::Channel(None, channel) => view.private == Some(channel),
        }
    }

    /// Whether `setting` would change nothing as the panel shows it.
    fn redundant(view: &NotificationView, target: Target, setting: NotifySetting) -> bool {
        let (level, muted) = match target {
            Target::Server(_) => (view.guild_level, view.guild_muted),
            Target::Channel(Some(_), _) => (view.channel_level, view.channel_muted),
            Target::Channel(None, _) => (view.private_level, view.private_muted),
        };
        match setting {
            NotifySetting::Level(next) => next == level,
            NotifySetting::Unmute => !muted,
            NotifySetting::SuppressEveryone(on) => on == view.suppress_everyone,
            NotifySetting::SuppressRoles(on) => on == view.suppress_roles,
            // A new duration replaces the old one, so it is never redundant.
            NotifySetting::Mute(_) => false,
        }
    }

    pub fn update(
        &mut self,
        event: Event,
        view: &NotificationView,
        client: &RestClient,
    ) -> Task<Event> {
        match event {
            Event::Toggle => {
                self.open = !self.open;
                self.notice = None;
            }
            Event::Set(target, setting) => {
                if !NOTIFICATION_WRITES_ENABLED {
                    self.notice = Some(PAUSED);
                    return Task::none();
                }
                if self.busy
                    || !Self::current(view, target)
                    || Self::redundant(view, target, setting)
                {
                    return Task::none();
                }
                self.busy = true;
                self.notice = None;
                let (guild, channel) = match target {
                    Target::Server(guild) => (Some(guild), None),
                    Target::Channel(guild, channel) => (guild, Some(channel)),
                };
                let client = client.clone();
                return Task::perform(
                    async move { client.set_notification(guild, channel, setting).await },
                    Event::Done,
                );
            }
            Event::Done(result) => {
                self.busy = false;
                self.notice = result.err().map(describe);
            }
            Event::HideMuted(_) => {}
        }
        Task::none()
    }
}

pub fn view<'a>(
    state: &State,
    notifications: &NotificationView,
    directs: bool,
) -> Element<'a, Event> {
    let busy = state.busy || !NOTIFICATION_WRITES_ENABLED;
    let mut body = column![row![
        text("Notifications").size(16),
        space::horizontal(),
        crate::icons::close(Some(Event::Toggle)),
    ]]
    .spacing(10);
    if !NOTIFICATION_WRITES_ENABLED {
        body = body.push(text(PAUSED).size(12).color(WARN));
    }
    if let Some(notice) = state.notice {
        body = body.push(text(notice).size(12).color(WARN));
    }
    let n = notifications;
    // One block per target: level, then mute.
    let block = |title: &'static str, target: Target, level: Option<NotifyLevel>, muted: bool| {
        if !NOTIFICATION_WRITES_ENABLED {
            return column![
                text(title).size(14),
                text(format!("Notify for: {}", Choice(level))).size(13),
                text(if muted { "Muted" } else { "Not muted" }).size(13),
            ]
            .spacing(6);
        }
        let pick = pick_list(CHOICES, Some(Choice(level)), move |c: Choice| {
            Event::Set(target, NotifySetting::Level(c.0))
        })
        .text_size(13);
        let mute = pick_list(MuteFor::ALL, None::<MuteFor>, move |length| {
            Event::Set(target, NotifySetting::Mute(length))
        })
        .placeholder(if muted { "Muted: change…" } else { "Mute…" })
        .text_size(13);
        let unmute = button(text("Unmute").size(13))
            .on_press_maybe((muted && !busy).then_some(Event::Set(target, NotifySetting::Unmute)));
        column![
            text(title).size(14),
            row![text("Notify for").size(13), pick].spacing(8),
            row![mute, unmute].spacing(8),
        ]
        .spacing(6)
    };
    if !directs && let Some(guild) = n.guild {
        let server = Target::Server(guild);
        let mut section =
            column![block("This server", server, n.guild_level, n.guild_muted)].spacing(6);
        section =
            section.push(
                checkbox(n.suppress_everyone)
                    .label("Suppress @everyone and @here")
                    .on_toggle_maybe((!busy).then_some(move |on| {
                        Event::Set(server, NotifySetting::SuppressEveryone(on))
                    })),
            );
        section = section.push(
            checkbox(n.suppress_roles)
                .label("Suppress all role @mentions")
                .on_toggle_maybe(
                    (!busy)
                        .then_some(move |on| Event::Set(server, NotifySetting::SuppressRoles(on))),
                ),
        );
        section = section.push(
            checkbox(n.hide_muted)
                .label("Hide muted channels in this server (this device only)")
                .on_toggle(move |_| Event::HideMuted(guild)),
        );
        body = body.push(section);
        if let Some(channel) = n.channel {
            body = body.push(block(
                "This channel",
                Target::Channel(Some(guild), channel),
                n.channel_level,
                n.channel_muted,
            ));
        }
    } else if directs && let Some(channel) = n.private {
        body = body.push(block(
            "This conversation",
            Target::Channel(None, channel),
            n.private_level,
            n.private_muted,
        ));
    } else {
        body = body.push(text("Open a server or conversation first.").size(13));
    }
    body = body.push(
        text("\"Default\" follows the server's setting. Muting stops badges and notifications; mentions still show in muted places.")
            .size(11),
    );
    container(body)
        .padding(16)
        .width(Length::Fixed(420.0))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> RestClient {
        use fastcord_discord::UserToken;
        use fastcord_discord::gateway::{ClientProperties, HostOs};
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-alerts-credential".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn view() -> NotificationView {
        NotificationView {
            guild: Some(Snowflake(1)),
            channel: Some(Snowflake(2)),
            private: Some(Snowflake(3)),
            guild_muted: true,
            ..NotificationView::default()
        }
    }

    #[test]
    fn alerts_and_quarantined_changes_never_start_network_tasks() {
        let (client, view) = (client(), view());
        let mut state = State::default();
        let server = Target::Server(Snowflake(1));
        let set = |target, setting| Event::Set(target, setting);
        assert_eq!(state.update(Event::Toggle, &view, &client).units(), 0);
        assert!(state.is_open());
        // This also covers stale events and callers bypassing the read-only UI.
        for event in [
            set(Target::Server(Snowflake(9)), NotifySetting::Unmute),
            set(
                Target::Channel(Some(Snowflake(1)), Snowflake(8)),
                NotifySetting::Unmute,
            ),
            set(server, NotifySetting::Level(None)),
            set(server, NotifySetting::SuppressRoles(false)),
            set(server, NotifySetting::Unmute),
            set(server, NotifySetting::Mute(MuteFor::Minutes15)),
            set(server, NotifySetting::Level(Some(NotifyLevel::Nothing))),
            set(server, NotifySetting::SuppressEveryone(true)),
            set(
                Target::Channel(Some(Snowflake(1)), Snowflake(2)),
                NotifySetting::Unmute,
            ),
        ] {
            assert_eq!(state.update(event, &view, &client).units(), 0);
            assert!(!state.busy);
            assert_eq!(state.notice, Some(PAUSED));
        }
        assert_eq!(state.update(Event::Toggle, &view, &client).units(), 0);
        assert!(!state.is_open());
    }

    #[test]
    fn ambiguous_and_rate_limited_failures_read_honestly() {
        assert!(describe(RestError::InvalidRateLimit).contains("rate limiting"));
        assert!(describe(RestError::ResourceGone).contains("gone"));
    }
}
