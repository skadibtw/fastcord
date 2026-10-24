//! Announcement channels (SPEC §16, issue #98): publish a message to the
//! servers following its channel, follow an announcement channel into a text
//! channel, and list or remove what a text channel follows.
//!
//! Every write is one explicit click and one request: it is checked again
//! against the current read model (the reducer's permissions, the loaded
//! message) immediately before it is sent, at most one request is in flight,
//! and none is ever retried. Publishing needs a Confirm and never changes the
//! row: "Published" appears when Discord's `MESSAGE_UPDATE` sets the flag.
//! Following offers only the text channels the reducer says the account has
//! Manage Webhooks in. Unfollowing deletes the follower webhook after a
//! Confirm; webhook tokens are never read. No scheduler and no automatic
//! posting exist here.

use fastcord_discord::state::navigation::{
    AdminKind, ChannelKind, ChannelSelection, NavigationSnapshot,
};
use fastcord_discord::{Follower, RestClient, RestError};
use fastcord_model::{Permissions, Snowflake};
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Element, Length, Task};

use crate::timeline::{self, Publish};

/// What the account may publish in `channel`, from the reducer's selection.
pub fn right(selection: Option<&ChannelSelection>, channel: Snowflake) -> Publish {
    match selection {
        Some(s) if s.channel_id == channel && s.kind == ChannelKind::Announcement => {
            if s.permissions.manage_messages {
                Publish::Any
            } else if s.permissions.send_messages {
                Publish::Own
            } else {
                Publish::No
            }
        }
        _ => Publish::No,
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    /// "Publish" of a message row.
    AskPublish(Snowflake, Snowflake),
    ConfirmPublish(Snowflake, Snowflake),
    CancelPublish,
    /// "Follow" of an announcement channel's header: server and channel.
    OpenFollow(Snowflake, Snowflake),
    Pick(Snowflake),
    Follow,
    /// "Followed announcements" of a text channel's settings.
    OpenFollowers(Snowflake, Snowflake),
    AskRemove(Snowflake),
    ConfirmRemove,
    Cancel,
    Close,
    Published(u64, Result<(), RestError>),
    Followed(u64, Result<(), RestError>),
    Loaded(u64, Snowflake, Result<Vec<Follower>, RestError>),
    Removed(u64, Snowflake, Result<(), RestError>),
}

#[derive(Debug)]
enum Panel {
    Follow {
        source: (Snowflake, Snowflake),
        picked: Option<Snowflake>,
    },
    Followers {
        guild: Snowflake,
        channel: Snowflake,
        /// `None` until Discord answered.
        list: Option<Vec<Follower>>,
        confirm: Option<Snowflake>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Busy {
    Publish,
    Follow,
    Load,
    Remove,
}

#[derive(Default)]
pub struct State {
    /// The message whose publishing waits for the Confirm: channel, message.
    publishing: Option<(Snowflake, Snowflake)>,
    panel: Option<Panel>,
    busy: Option<Busy>,
    notice: Option<String>,
    serial: u64,
}

const SENT: &str = "Sent. The message shows Published when Discord confirms.";

pub(crate) fn describe(error: RestError, forbidden: &str, gone: &str) -> String {
    match error {
        RestError::PermissionDenied => forbidden.to_owned(),
        RestError::Refused(r) if r.status.as_u16() == 403 => forbidden.to_owned(),
        RestError::ResourceGone => gone.to_owned(),
        RestError::Refused(r) if r.status.as_u16() == 404 => gone.to_owned(),
        RestError::Retryable(_) => {
            "Discord did not confirm. It may have been applied; check before trying again."
                .to_owned()
        }
        other => crate::friends::failure(other),
    }
}

/// Why the loaded message cannot be published right now, or `None`.
fn publish_block(
    navigation: &NavigationSnapshot,
    snapshot: &timeline::Snapshot,
    channel: Snowflake,
    message: Snowflake,
) -> Option<&'static str> {
    let right = right(navigation.selection.as_ref(), channel);
    let Some(row) = snapshot
        .rows
        .iter()
        .find(|r| snapshot.channel_id == Some(channel) && r.message.id == message)
    else {
        return Some("That message is no longer loaded.");
    };
    match right {
        Publish::No => Some("You cannot publish in this channel."),
        Publish::Own if !row.own => Some("You can publish only your own messages here."),
        _ if row.message.kind != 0 => Some("Only ordinary messages can be published."),
        _ if row.message.is_crossposted() => Some("It is already published."),
        _ => None,
    }
}

impl State {
    pub fn clear(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            ..Self::default()
        };
    }

    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Published(_, Err(RestError::AuthenticationRequired))
                | Event::Followed(_, Err(RestError::AuthenticationRequired))
                | Event::Loaded(_, _, Err(RestError::AuthenticationRequired))
                | Event::Removed(_, _, Err(RestError::AuthenticationRequired))
        )
    }

    /// The row whose publishing waits for the Confirm.
    pub fn publishing(&self) -> Option<Snowflake> {
        self.publishing.map(|(_, message)| message)
    }

    /// The announcement channel the follow picker names the targets of.
    pub fn wanted_follow(&self) -> Option<(Snowflake, Snowflake)> {
        match self.panel {
            Some(Panel::Follow { source, .. }) => Some(source),
            _ => None,
        }
    }

    /// The server whose read model the followed-destinations list needs.
    pub fn wanted_guild(&self) -> Option<Snowflake> {
        match self.panel {
            Some(Panel::Followers { guild, .. }) => Some(guild),
            _ => None,
        }
    }

    /// Drops a publish confirmation when the user leaves its channel.
    pub fn sync(&mut self, open: Option<Snowflake>) {
        if self
            .publishing
            .is_some_and(|(channel, _)| Some(channel) != open)
        {
            self.publishing = None;
        }
    }

    fn start<F, Fut>(&mut self, busy: Busy, run: F) -> Task<Event>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Event> + Send + 'static,
    {
        self.busy = Some(busy);
        self.notice = None;
        Task::perform(run(), |event| event)
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        snapshot: &timeline::Snapshot,
        client: &RestClient,
    ) -> Task<Event> {
        let serial = self.serial;
        let client = client.clone();
        match event {
            Event::Published(s, result) if s == serial => {
                self.busy = None;
                self.notice = Some(match result {
                    Ok(()) => SENT.to_owned(),
                    Err(error) => describe(
                        error,
                        "Discord refused: you cannot publish here, or this message cannot be published.",
                        "That message or channel no longer exists.",
                    ),
                });
            }
            Event::Followed(s, result) if s == serial => {
                self.busy = None;
                match result {
                    Ok(()) => {
                        self.panel = None;
                        self.notice = Some("Sent. It shows under Followed announcements in that channel's settings when Discord confirms.".to_owned());
                    }
                    Err(error) => {
                        self.notice = Some(describe(
                            error,
                            "Discord refused: you lack Manage Webhooks there, or the channel cannot be followed.",
                            "The channel you follow or the destination no longer exists.",
                        ));
                    }
                }
            }
            Event::Loaded(s, channel, result) if s == serial => {
                self.busy = None;
                if let Some(Panel::Followers {
                    channel: open,
                    list,
                    ..
                }) = &mut self.panel
                    && *open == channel
                {
                    match result {
                        Ok(found) => *list = Some(found),
                        Err(error) => {
                            self.notice = Some(describe(
                                error,
                                "Discord refused: you need Manage Webhooks in this channel.",
                                "That channel no longer exists.",
                            ));
                        }
                    }
                }
            }
            Event::Removed(s, webhook, result) if s == serial => {
                self.busy = None;
                let gone = matches!(result, Err(RestError::ResourceGone));
                match result {
                    Ok(()) | Err(RestError::ResourceGone) => {
                        if let Some(Panel::Followers { list, confirm, .. }) = &mut self.panel {
                            *confirm = None;
                            if let Some(list) = list {
                                list.retain(|f| f.id != webhook);
                            }
                        }
                        self.notice = Some(
                            if gone {
                                "It was already removed."
                            } else {
                                "Unfollowed."
                            }
                            .to_owned(),
                        );
                    }
                    Err(error) => {
                        self.notice = Some(describe(
                            error,
                            "Discord refused: you need Manage Webhooks in this channel.",
                            "That follower no longer exists.",
                        ));
                    }
                }
            }
            Event::Published(..) | Event::Followed(..) | Event::Loaded(..) | Event::Removed(..) => {
            }
            Event::Cancel => {
                if let Some(Panel::Followers { confirm, .. }) = &mut self.panel {
                    *confirm = None;
                }
            }
            Event::CancelPublish => self.publishing = None,
            Event::Close if self.busy.is_none() => {
                self.panel = None;
                self.notice = None;
                self.publishing = None;
            }
            Event::Close => {}
            _ if self.busy.is_some() => {
                self.notice = Some("Wait for the previous request to finish.".to_owned());
            }
            Event::AskPublish(channel, message) => {
                match publish_block(navigation, snapshot, channel, message) {
                    None => {
                        self.publishing = Some((channel, message));
                        self.notice = None;
                    }
                    Some(reason) => self.notice = Some(reason.to_owned()),
                }
            }
            Event::ConfirmPublish(channel, message) => {
                if self.publishing != Some((channel, message)) {
                    return Task::none();
                }
                self.publishing = None;
                if let Some(reason) = publish_block(navigation, snapshot, channel, message) {
                    self.notice = Some(reason.to_owned());
                    return Task::none();
                }
                return self.start(Busy::Publish, move || async move {
                    Event::Published(serial, client.crosspost(channel, message).await)
                });
            }
            Event::OpenFollow(guild, channel) => {
                let here = navigation
                    .selection
                    .as_ref()
                    .is_some_and(|s| s.channel_id == channel && s.guild_id == guild);
                if here && right_to_see(navigation, channel) {
                    self.panel = Some(Panel::Follow {
                        source: (guild, channel),
                        picked: None,
                    });
                    self.notice = None;
                }
            }
            Event::Pick(target) => {
                if let Some(Panel::Follow { picked, .. }) = &mut self.panel {
                    let offered = navigation
                        .follow
                        .as_ref()
                        .is_some_and(|f| f.targets.iter().any(|t| t.channel == target));
                    *picked = offered.then_some(target);
                }
            }
            Event::Follow => {
                let Some(Panel::Follow {
                    source,
                    picked: Some(target),
                }) = &self.panel
                else {
                    return Task::none();
                };
                let (source, target) = (source.1, *target);
                // Offered now, by the reducer's permissions, for this source.
                let offered = navigation.follow.as_ref().is_some_and(|f| {
                    f.source == source && f.targets.iter().any(|t| t.channel == target)
                });
                if !offered {
                    self.notice = Some(
                        "You can no longer follow into that channel; choose another.".to_owned(),
                    );
                    return Task::none();
                }
                return self.start(Busy::Follow, move || async move {
                    Event::Followed(serial, client.follow_channel(source, target).await)
                });
            }
            Event::OpenFollowers(guild, channel) => {
                if !can_manage_webhooks(navigation, guild, channel) {
                    self.notice =
                        Some("You need Manage Webhooks in a text channel for this.".to_owned());
                    return Task::none();
                }
                self.panel = Some(Panel::Followers {
                    guild,
                    channel,
                    list: None,
                    confirm: None,
                });
                return self.start(Busy::Load, move || async move {
                    Event::Loaded(serial, channel, client.followed_channels(channel).await)
                });
            }
            Event::AskRemove(webhook) => {
                if let Some(Panel::Followers { list, confirm, .. }) = &mut self.panel
                    && list
                        .as_ref()
                        .is_some_and(|l| l.iter().any(|f| f.id == webhook))
                {
                    *confirm = Some(webhook);
                }
            }
            Event::ConfirmRemove => {
                let Some(Panel::Followers {
                    guild,
                    channel,
                    confirm: Some(webhook),
                    ..
                }) = &self.panel
                else {
                    return Task::none();
                };
                let webhook = *webhook;
                if !can_manage_webhooks(navigation, *guild, *channel) {
                    self.notice = Some("You need Manage Webhooks in this channel.".to_owned());
                    return Task::none();
                }
                return self.start(Busy::Remove, move || async move {
                    Event::Removed(serial, webhook, client.delete_follower(webhook).await)
                });
            }
        }
        Task::none()
    }
}

/// An announcement channel the account has open and can see.
fn right_to_see(navigation: &NavigationSnapshot, channel: Snowflake) -> bool {
    navigation.selection.as_ref().is_some_and(|s| {
        s.channel_id == channel && s.kind == ChannelKind::Announcement && s.permissions.view_channel
    })
}

/// Manage Webhooks in a text channel of the server the reducer describes.
pub(crate) fn can_manage_webhooks(
    navigation: &NavigationSnapshot,
    guild: Snowflake,
    channel: Snowflake,
) -> bool {
    navigation
        .admin
        .as_ref()
        .filter(|a| a.id == guild)
        .and_then(|a| a.channel(channel))
        .is_some_and(|c| c.kind == AdminKind::Text && c.mine.contains(Permissions::MANAGE_WEBHOOKS))
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
) -> Option<Element<'a, Event>> {
    if state.panel.is_none() && state.notice.is_none() {
        return None;
    }
    let idle = state.busy.is_none();
    let title = match &state.panel {
        Some(Panel::Follow { .. }) => "Follow this channel",
        Some(Panel::Followers { .. }) => "Followed announcements",
        None => "Announcements",
    };
    let mut panel = column![
        row![
            text(title).size(13),
            space::horizontal(),
            crate::icons::close(idle.then_some(Event::Close)),
        ]
        .align_y(iced::Alignment::Center)
    ]
    .spacing(6);
    match &state.panel {
        Some(Panel::Follow { picked, .. }) => {
            let targets = navigation
                .follow
                .as_ref()
                .map_or(&[][..], |f| &f.targets[..]);
            panel = panel.push(
                text("Messages published there appear in the text channel you pick. You need Manage Webhooks in it.")
                    .size(12)
                    .style(text::secondary),
            );
            if targets.is_empty() {
                panel = panel.push(
                    text("No text channel where you have Manage Webhooks.")
                        .size(12)
                        .style(text::secondary),
                );
            }
            let list = targets.iter().map(|t| {
                let ticked = *picked == Some(t.channel);
                button(
                    text(format!(
                        "{} {} #{}",
                        if ticked { "(o)" } else { "( )" },
                        t.guild_name,
                        t.channel_name
                    ))
                    .size(13),
                )
                .on_press_maybe(idle.then_some(Event::Pick(t.channel)))
                .style(button::secondary)
                .width(Length::Fill)
                .into()
            });
            panel = panel
                .push(scrollable(column(list).spacing(2)).height(Length::Fixed(180.0)))
                .push(
                    button(text("Follow").size(13))
                        .on_press_maybe((idle && picked.is_some()).then_some(Event::Follow)),
                );
        }
        Some(Panel::Followers { list, confirm, .. }) => match list {
            None => {
                panel = panel.push(text("Loading...").size(12).style(text::secondary));
            }
            Some(list) if list.is_empty() => {
                panel = panel.push(
                    text("This channel follows no announcement channel.")
                        .size(12)
                        .style(text::secondary),
                );
            }
            Some(list) => {
                for follower in list {
                    let mut line = row![text(follower.label()).size(13).width(Length::Fill),]
                        .spacing(8)
                        .align_y(iced::Alignment::Center);
                    if *confirm == Some(follower.id) {
                        line = line
                            .push(text("Stop following?").size(12).style(text::danger))
                            .push(
                                button(text("Unfollow").size(13))
                                    .style(button::danger)
                                    .on_press_maybe(idle.then_some(Event::ConfirmRemove)),
                            )
                            .push(button(text("Cancel").size(13)).on_press(Event::Cancel));
                    } else {
                        line = line.push(
                            button(text("Unfollow").size(13))
                                .on_press_maybe(idle.then_some(Event::AskRemove(follower.id))),
                        );
                    }
                    panel = panel.push(line);
                }
            }
        },
        None => {}
    }
    if let Some(notice) = &state.notice {
        panel = panel.push(text(notice.as_str()).size(12));
    }
    Some(container(panel).padding(8).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::Row;
    use fastcord_discord::state::navigation::{FollowTarget, FollowTargets, PermissionSummary};
    use fastcord_model::Message;
    use std::sync::Arc;

    fn client() -> RestClient {
        use fastcord_discord::UserToken;
        use fastcord_discord::gateway::{ClientProperties, HostOs};
        RestClient::new(
            UserToken::new("offline-announce-credential".into()),
            ClientProperties::web(HostOs::Linux, "en-US", 1),
        )
        .unwrap()
    }

    const GUILD: Snowflake = Snowflake(1);
    const NEWS: Snowflake = Snowflake(10);
    const MINE: Snowflake = Snowflake(100);
    const THEIRS: Snowflake = Snowflake(101);

    fn news_nav(manage: bool, send: bool) -> NavigationSnapshot {
        NavigationSnapshot {
            selection: Some(ChannelSelection {
                guild_id: GUILD,
                channel_id: NEWS,
                name: "news".into(),
                kind: ChannelKind::Announcement,
                permissions: PermissionSummary {
                    view_channel: true,
                    manage_messages: manage,
                    send_messages: send,
                    ..PermissionSummary::default()
                },
                acked: None,
                unread: false,
                mentions: 0,
                thread: None,
            }),
            ..NavigationSnapshot::default()
        }
    }

    fn message(id: Snowflake, flags: u64) -> Arc<Message> {
        let mut message: Message = serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "channel_id": NEWS.to_string(), "content": "news",
            "timestamp": "2026-10-07T12:00:00.000000+00:00",
            "author": {"id": "3", "username": "a"}, "flags": flags
        }))
        .unwrap();
        message.kind = 0;
        Arc::new(message)
    }

    fn rows(flags: u64) -> timeline::Snapshot {
        let mut mine = Row::new(message(MINE, flags), 1);
        mine.own = true;
        timeline::Snapshot {
            channel_id: Some(NEWS),
            rows: vec![mine, Row::new(message(THEIRS, 0), 1)],
            ..timeline::Snapshot::default()
        }
    }

    fn run(state: &mut State, event: Event, nav: &NavigationSnapshot, rows: &timeline::Snapshot) {
        let _ = state.update(event, nav, rows, &client());
    }

    #[test]
    fn the_right_follows_the_channel_kind_and_permissions() {
        let sel = |manage, send| news_nav(manage, send).selection.unwrap();
        assert_eq!(right(Some(&sel(true, true)), NEWS), Publish::Any);
        assert_eq!(right(Some(&sel(false, true)), NEWS), Publish::Own);
        assert_eq!(right(Some(&sel(false, false)), NEWS), Publish::No);
        assert_eq!(right(Some(&sel(true, true)), Snowflake(11)), Publish::No);
        let mut text = sel(true, true);
        text.kind = ChannelKind::Text;
        assert_eq!(right(Some(&text), NEWS), Publish::No);
        assert_eq!(right(None, NEWS), Publish::No);
    }

    #[test]
    fn publishing_asks_first_checks_again_and_is_sent_once() {
        let (nav, rows) = (news_nav(false, true), rows(0));
        let mut state = State::default();
        // Someone else's message needs Manage Messages.
        run(&mut state, Event::AskPublish(NEWS, THEIRS), &nav, &rows);
        assert!(state.publishing.is_none() && state.notice.is_some());
        run(&mut state, Event::AskPublish(NEWS, MINE), &nav, &rows);
        assert_eq!(state.publishing, Some((NEWS, MINE)));
        // A confirm for another message sends nothing.
        run(&mut state, Event::ConfirmPublish(NEWS, THEIRS), &nav, &rows);
        assert_eq!(state.busy, None);
        // Losing the permission before the click blocks the write.
        run(
            &mut state,
            Event::ConfirmPublish(NEWS, MINE),
            &news_nav(false, false),
            &rows,
        );
        assert_eq!(state.busy, None);
        assert!(state.publishing.is_none() && state.notice.is_some());
        run(&mut state, Event::AskPublish(NEWS, MINE), &nav, &rows);
        run(&mut state, Event::ConfirmPublish(NEWS, MINE), &nav, &rows);
        assert_eq!(state.busy, Some(Busy::Publish));
        // While the request is in flight nothing else is sent.
        run(&mut state, Event::AskPublish(NEWS, MINE), &nav, &rows);
        assert!(state.publishing.is_none());
        run(&mut state, Event::Published(1, Ok(())), &nav, &rows);
        assert_eq!(state.busy, Some(Busy::Publish), "a stale answer is ignored");
        run(&mut state, Event::Published(0, Ok(())), &nav, &rows);
        assert_eq!(state.busy, None);
        assert_eq!(state.notice.as_deref(), Some(SENT));
    }

    #[test]
    fn a_published_message_cannot_be_published_again() {
        let mut state = State::default();
        run(
            &mut state,
            Event::AskPublish(NEWS, MINE),
            &news_nav(true, true),
            &rows(1),
        );
        assert!(state.publishing.is_none());
        assert_eq!(state.notice.as_deref(), Some("It is already published."));
    }

    #[test]
    fn publish_failures_say_what_happened() {
        let nav = news_nav(true, true);
        let rows = rows(0);
        for (error, expect) in [
            (RestError::PermissionDenied, "cannot publish"),
            (RestError::ResourceGone, "no longer exists"),
            (RestError::InvalidRateLimit, "rate limiting"),
        ] {
            let mut state = State {
                busy: Some(Busy::Publish),
                ..State::default()
            };
            run(&mut state, Event::Published(0, Err(error)), &nav, &rows);
            assert!(state.notice.unwrap().contains(expect));
        }
        assert!(State::is_rejected(&Event::Published(
            0,
            Err(RestError::AuthenticationRequired)
        )));
    }

    fn targets(ids: &[u64]) -> Option<FollowTargets> {
        Some(FollowTargets {
            source: NEWS,
            targets: ids
                .iter()
                .map(|id| FollowTarget {
                    guild: Snowflake(2),
                    guild_name: "Mine".into(),
                    channel: Snowflake(*id),
                    channel_name: "general".into(),
                })
                .collect(),
        })
    }

    #[test]
    fn following_offers_only_reducer_targets_and_sends_once() {
        let mut nav = news_nav(false, true);
        let rows = rows(0);
        let mut state = State::default();
        run(&mut state, Event::OpenFollow(GUILD, NEWS), &nav, &rows);
        assert_eq!(state.wanted_follow(), Some((GUILD, NEWS)));
        nav.follow = targets(&[20, 21]);
        run(&mut state, Event::Pick(Snowflake(99)), &nav, &rows);
        run(&mut state, Event::Follow, &nav, &rows);
        assert_eq!(state.busy, None, "nothing picked, nothing sent");
        run(&mut state, Event::Pick(Snowflake(20)), &nav, &rows);
        // The target disappears from the reducer before the click.
        let mut gone = nav.clone();
        gone.follow = targets(&[21]);
        run(&mut state, Event::Follow, &gone, &rows);
        assert_eq!(state.busy, None);
        assert!(state.notice.is_some());
        run(&mut state, Event::Follow, &nav, &rows);
        assert_eq!(state.busy, Some(Busy::Follow));
        run(&mut state, Event::Follow, &nav, &rows);
        assert_eq!(
            state.busy,
            Some(Busy::Follow),
            "a second click sends nothing"
        );
        // An ambiguous failure keeps the picker and says it may have applied.
        run(
            &mut state,
            Event::Followed(
                0,
                Err(RestError::Retryable(
                    fastcord_discord::RetryableFailure::Server(reqwest::StatusCode::BAD_GATEWAY),
                )),
            ),
            &nav,
            &rows,
        );
        assert!(state.panel.is_some());
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("may have been applied")
        );
        run(&mut state, Event::Pick(Snowflake(20)), &nav, &rows);
        run(&mut state, Event::Follow, &nav, &rows);
        run(&mut state, Event::Followed(0, Ok(())), &nav, &rows);
        assert!(state.panel.is_none() && state.busy.is_none());
    }

    #[test]
    fn the_follow_picker_needs_an_announcement_channel_the_account_can_see() {
        let mut state = State::default();
        let mut hidden = news_nav(false, true);
        hidden.selection.as_mut().unwrap().permissions.view_channel = false;
        run(
            &mut state,
            Event::OpenFollow(GUILD, NEWS),
            &hidden,
            &rows(0),
        );
        assert!(state.panel.is_none());
        let mut text = news_nav(false, true);
        text.selection.as_mut().unwrap().kind = ChannelKind::Text;
        run(&mut state, Event::OpenFollow(GUILD, NEWS), &text, &rows(0));
        assert!(state.panel.is_none());
    }

    fn admin_nav(webhooks: bool) -> NavigationSnapshot {
        use fastcord_discord::state::navigation::{AdminChannel, GuildAdmin};
        let channel = AdminChannel {
            id: Snowflake(20),
            name: "general".into(),
            kind: AdminKind::Text,
            parent_id: None,
            position: 0,
            topic: String::new(),
            nsfw: false,
            slowmode: 0,
            user_limit: 0,
            bitrate: 0,
            rtc_region: None,
            video_quality: 1,
            can_manage: true,
            overwrites: Vec::new(),
            can_manage_permissions: false,
            mine: if webhooks {
                Permissions::MANAGE_WEBHOOKS
            } else {
                Permissions::NONE
            },
            synced: None,
            everyone_view: true,
        };
        NavigationSnapshot {
            admin: Some(GuildAdmin {
                id: Snowflake(2),
                name: "Mine".into(),
                has_icon: false,
                owner: false,
                community: true,
                description: String::new(),
                system_channel: None,
                afk_channel: None,
                afk_timeout: 300,
                notifications: 0,
                content_filter: 0,
                rules_channel: None,
                public_updates_channel: None,
                verification_level: 0,
                features: Vec::new(),
                mfa_required: false,
                safety_alerts_channel: None,
                raid_alerts_disabled: false,
                can_manage_guild: false,
                can_manage_channels: false,
                channels: vec![channel],
                people: Vec::new(),
                can_manage_roles: false,
                roles: Vec::new(),
                mine: Permissions::NONE,
                grant_all: false,
                top_position: 0,
            }),
            ..NavigationSnapshot::default()
        }
    }

    #[test]
    fn followed_destinations_are_listed_and_removed_after_a_confirm() {
        let rows = rows(0);
        let nav = admin_nav(true);
        let mut state = State::default();
        run(
            &mut state,
            Event::OpenFollowers(Snowflake(2), Snowflake(20)),
            &admin_nav(false),
            &rows,
        );
        assert!(state.panel.is_none(), "needs Manage Webhooks");
        run(
            &mut state,
            Event::OpenFollowers(Snowflake(2), Snowflake(20)),
            &nav,
            &rows,
        );
        assert_eq!(state.busy, Some(Busy::Load));
        assert_eq!(state.wanted_guild(), Some(Snowflake(2)));
        let list = fastcord_discord::parse_followers(
            include_str!("../../../fixtures/rest/channel-webhooks-synthetic.json").as_bytes(),
        )
        .unwrap();
        run(
            &mut state,
            Event::Loaded(0, Snowflake(20), Ok(list)),
            &nav,
            &rows,
        );
        run(&mut state, Event::AskRemove(Snowflake(9999)), &nav, &rows);
        assert!(matches!(
            state.panel,
            Some(Panel::Followers { confirm: None, .. })
        ));
        run(&mut state, Event::ConfirmRemove, &nav, &rows);
        assert_eq!(state.busy, None, "nothing is removed without asking");
        run(&mut state, Event::AskRemove(Snowflake(7001)), &nav, &rows);
        // Losing the permission before the Confirm blocks the delete.
        run(&mut state, Event::ConfirmRemove, &admin_nav(false), &rows);
        assert_eq!(state.busy, None);
        run(&mut state, Event::ConfirmRemove, &nav, &rows);
        assert_eq!(state.busy, Some(Busy::Remove));
        run(&mut state, Event::ConfirmRemove, &nav, &rows);
        assert_eq!(state.busy, Some(Busy::Remove));
        run(
            &mut state,
            Event::Removed(0, Snowflake(7001), Ok(())),
            &nav,
            &rows,
        );
        let Some(Panel::Followers {
            list: Some(list),
            confirm,
            ..
        }) = &state.panel
        else {
            panic!("the list stays");
        };
        assert!(confirm.is_none() && list.iter().all(|f| f.id != Snowflake(7001)));
        // The panel and the list build.
        assert!(view(&state, &nav).is_some());
    }

    #[test]
    fn a_forbidden_or_gone_removal_keeps_the_list_or_drops_the_stale_row() {
        let rows = rows(0);
        let nav = admin_nav(true);
        let list = fastcord_discord::parse_followers(
            include_str!("../../../fixtures/rest/channel-webhooks-synthetic.json").as_bytes(),
        )
        .unwrap();
        let panel = || Panel::Followers {
            guild: Snowflake(2),
            channel: Snowflake(20),
            list: Some(list.clone()),
            confirm: Some(Snowflake(7001)),
        };
        let mut state = State {
            panel: Some(panel()),
            busy: Some(Busy::Remove),
            ..State::default()
        };
        run(
            &mut state,
            Event::Removed(0, Snowflake(7001), Err(RestError::PermissionDenied)),
            &nav,
            &rows,
        );
        assert!(
            matches!(&state.panel, Some(Panel::Followers { list: Some(l), .. }) if l.len() == 2)
        );
        assert!(state.notice.as_deref().unwrap().contains("Manage Webhooks"));
        state.busy = Some(Busy::Remove);
        run(
            &mut state,
            Event::Removed(0, Snowflake(7001), Err(RestError::ResourceGone)),
            &nav,
            &rows,
        );
        assert!(
            matches!(&state.panel, Some(Panel::Followers { list: Some(l), .. }) if l.len() == 1)
        );
    }

    #[test]
    fn leaving_the_channel_drops_the_publish_confirmation_and_clear_resets() {
        let (nav, rows) = (news_nav(true, true), rows(0));
        let mut state = State::default();
        run(&mut state, Event::AskPublish(NEWS, MINE), &nav, &rows);
        state.sync(Some(NEWS));
        assert!(state.publishing.is_some());
        state.sync(Some(Snowflake(11)));
        assert!(state.publishing.is_none());
        run(&mut state, Event::OpenFollow(GUILD, NEWS), &nav, &rows);
        let serial = state.serial;
        state.clear();
        assert!(state.panel.is_none() && state.serial != serial);
        assert!(view(&state, &nav).is_none());
    }
}
