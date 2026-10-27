//! Group DM membership and conversation management (SPEC §16, issue #78).
//!
//! A panel over the open DM or group DM: the member list with the owner marked,
//! add a friend, remove a member or hand over ownership (owner only), rename,
//! set or clear the icon, leave a group, close a one-to-one DM. Every change is
//! one explicit click and one request; at most one is in flight, none is ever
//! retried, and the member list, name, owner and icon change only when Discord's
//! `CHANNEL_*` events arrive, which is also how another client's changes show.
//! Leaving, removing a member and handing over ownership ask for a second click
//! first. Nothing here creates groups or invites anyone by itself: only friends
//! the user picks one by one can be added.

use crate::ui::button;
use fastcord_discord::state::navigation::{NavigationSnapshot, PrivateChannelSelection};
use fastcord_discord::{
    GroupEdit, GroupError, GroupRequestError, IconImage, MAX_GROUP_NAME_CHARS, MAX_ICON_BYTES,
    RestClient, RestError,
};
use fastcord_model::{RelationshipKind, Snowflake};
use iced::widget::{column, container, row, scrollable, space, text, text_input};
use iced::{Element, Length, Task};

use crate::i18n::{tr, trf};
use crate::panel_kit::{Confirm as Pending, Gate, WAIT, auth_rejected, pick_image};

/// Friends listed as candidates to add.
const MAX_CANDIDATES: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Add(Snowflake),
    Remove(Snowflake),
    Owner(Snowflake),
    Rename,
    Icon,
    /// Leave a group or close a DM.
    Leave,
}

/// A destructive action waiting for its second click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    Remove(Snowflake),
    Owner(Snowflake),
    Leave,
}

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    Close,
    NameInput(String),
    Rename,
    PickIcon,
    /// `Ok(None)`: the dialog was cancelled.
    IconPicked(u64, Result<Option<IconImage>, &'static str>),
    ClearIcon,
    Add(Snowflake),
    AskRemove(Snowflake),
    AskOwner(Snowflake),
    /// The `⋯` menu of one member (make owner, remove), or none.
    Menu(Option<Snowflake>),
    /// Leave a group, or close a DM (which needs no confirmation).
    Leave,
    Confirm,
    Cancel,
    Done(u64, Action, Result<(), RestError>),
}

#[derive(Default)]
pub struct State {
    /// The conversation the panel is open for.
    channel: Option<Snowflake>,
    name: String,
    confirm: Pending<Confirm>,
    /// The member whose `⋯` menu is open.
    menu: Option<Snowflake>,
    life: Gate<Action>,
    /// Dropped (aborting the request) only on logout or account change.
    task: Option<iced::task::Handle>,
}

fn describe(error: RestError) -> String {
    use crate::rest_text::Wording;
    crate::rest_text::describe(
        error,
        &Wording {
            forbidden: "Discord refused: you may lack permission, or the person cannot be added (for example because of blocking or privacy settings).",
            gone: "That conversation or person no longer exists.",
            unconfirmed: "Discord did not confirm. The change may have been applied; check the list before trying again.",
            rejected: "Discord rejected the request (the group may be full, or the value is not allowed).",
        },
    )
}

/// Reads at most one byte more than the image limit, so an oversized file is
/// detected without being loaded whole. Blocking: call from a worker.
pub(crate) fn read_image_bytes(path: &std::path::Path) -> Result<Vec<u8>, &'static str> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| "The image could not be read.")?;
    let mut bytes = Vec::new();
    file.take(MAX_ICON_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "The image could not be read.")?;
    Ok(bytes)
}

fn read_icon(path: &std::path::Path) -> Result<IconImage, &'static str> {
    IconImage::new(&read_image_bytes(path)?).map_err(|error| match error {
        GroupError::ImageTooLarge => "The image is larger than 512 KiB.",
        GroupError::UnsupportedImage => "Choose a PNG, JPEG or GIF image.",
        GroupError::NameTooLong => "The name is too long.",
    })
}

impl State {
    #[cfg(test)]
    fn is_open_for(&self, channel: Snowflake) -> bool {
        self.channel == Some(channel)
    }

    #[cfg(test)]
    fn busy(&self) -> bool {
        self.life.is_busy()
    }

    /// Closes the panel when the open conversation changes or disappears,
    /// unless a request is in flight (its answer must still be handled).
    pub fn sync(&mut self, selected: Option<Snowflake>) {
        if !self.life.is_busy() && self.channel.is_some() && self.channel != selected {
            self.clear();
        }
    }

    /// Logout or account change: drops the panel and any request.
    pub fn clear(&mut self) {
        let mut life = std::mem::take(&mut self.life);
        life.reset();
        *self = Self {
            life,
            ..Self::default()
        };
    }

    fn start(&mut self, action: Action) {
        self.life.begin(action);
        self.confirm.cancel();
    }

    /// Runs one request for the open conversation.
    fn send(
        &mut self,
        action: Action,
        channel: Snowflake,
        client: &RestClient,
        edit: Option<GroupEdit>,
    ) -> Task<Event> {
        self.start(action);
        let serial = self.life.serial();
        let client = client.clone();
        let (task, handle) = Task::perform(
            async move {
                match (action, edit) {
                    (Action::Add(user), _) => client.add_group_recipient(channel, user).await,
                    (Action::Remove(user), _) => client.remove_group_recipient(channel, user).await,
                    (Action::Leave, _) => client.close_private_channel(channel, false).await,
                    (_, Some(edit)) => {
                        client
                            .edit_group(channel, &edit)
                            .await
                            .map_err(|error| match error {
                                GroupRequestError::Rest(error) => error,
                                GroupRequestError::Invalid(_) => RestError::InvalidJson,
                            })
                    }
                    (_, None) => Err(RestError::InvalidJson),
                }
            },
            move |result| Event::Done(serial, action, result),
        )
        .abortable();
        self.task = Some(handle.abort_on_drop());
        task
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        me: Snowflake,
        client: &RestClient,
    ) -> Task<Event> {
        let selection = navigation.private_selection.as_ref();
        let owner = selection.is_some_and(|s| s.owner == Some(me));
        match event {
            Event::Toggle => {
                if self.channel.is_some() {
                    if !self.life.is_busy() {
                        self.clear();
                    }
                } else if let Some(selection) = selection {
                    self.channel = Some(selection.channel_id);
                    self.name = selection.custom_name.clone();
                }
            }
            Event::Close if !self.life.is_busy() => self.clear(),
            Event::Close => {}
            Event::NameInput(value) if self.channel.is_some() && !self.life.is_busy() => {
                self.name = value.chars().take(MAX_GROUP_NAME_CHARS + 20).collect();
                self.life.notice = None;
            }
            Event::Cancel => self.confirm.cancel(),
            Event::Menu(user) => self.menu = user,
            _ if self.life.is_busy()
                && !matches!(event, Event::Done(..) | Event::IconPicked(..)) =>
            {
                // One request at a time; the answer of the first is awaited.
                if matches!(
                    event,
                    Event::Add(_)
                        | Event::AskRemove(_)
                        | Event::AskOwner(_)
                        | Event::Leave
                        | Event::Confirm
                        | Event::Rename
                        | Event::PickIcon
                        | Event::ClearIcon
                ) {
                    self.life.say(tr(WAIT));
                }
            }
            Event::Rename => {
                let (Some(channel), Some(selection)) = (self.channel, selection) else {
                    return Task::none();
                };
                if !selection.group || selection.channel_id != channel {
                    return Task::none();
                }
                let name = self.name.trim().to_owned();
                if name.chars().count() > MAX_GROUP_NAME_CHARS {
                    self.life.notice = Some(trf(
                        "A group name has at most {} characters.",
                        &[&MAX_GROUP_NAME_CHARS],
                    ));
                } else if name == selection.custom_name {
                    self.life.notice = Some(tr("The name is unchanged.").to_owned());
                } else {
                    return self.send(Action::Rename, channel, client, Some(GroupEdit::Name(name)));
                }
            }
            Event::PickIcon => {
                if self.channel.is_some() && selection.is_some_and(|s| s.group) {
                    self.start(Action::Icon);
                    let serial = self.life.serial();
                    return Task::perform(
                        pick_image(tr("Group icon"), &["png", "jpg", "jpeg", "gif"], read_icon),
                        move |result| Event::IconPicked(serial, result),
                    );
                }
            }
            Event::IconPicked(serial, result) if self.life.accept(serial) => {
                let channel = self
                    .channel
                    .filter(|c| selection.is_some_and(|s| s.channel_id == *c));
                match (result, channel) {
                    (Ok(Some(icon)), Some(channel)) => {
                        return self.send(
                            Action::Icon,
                            channel,
                            client,
                            Some(GroupEdit::Icon(Some(icon))),
                        );
                    }
                    (Err(reason), _) => self.life.notice = Some(tr(reason).to_owned()),
                    _ => {}
                }
            }
            Event::ClearIcon => {
                if let (Some(channel), Some(s)) = (self.channel, selection)
                    && s.group
                    && s.has_icon
                {
                    return self.send(Action::Icon, channel, client, Some(GroupEdit::Icon(None)));
                }
            }
            Event::Add(user) => {
                let (Some(channel), Some(s)) = (self.channel, selection) else {
                    return Task::none();
                };
                let friend = navigation
                    .relationships
                    .iter()
                    .any(|r| r.user.id == user && r.kind == RelationshipKind::Friend);
                if !s.group {
                    self.life.notice = Some(tr("Only a group can have people added.").to_owned());
                } else if !friend || user == me || s.members.iter().any(|m| m.id == user) {
                    self.life.notice =
                        Some(tr("Pick a friend who is not in the group yet.").to_owned());
                } else {
                    return self.send(Action::Add(user), channel, client, None);
                }
            }
            Event::AskRemove(user) => {
                self.menu = None;
                if owner && selection.is_some_and(|s| s.members.iter().any(|m| m.id == user)) {
                    self.confirm.ask(Confirm::Remove(user));
                    self.life.notice = None;
                }
            }
            Event::AskOwner(user) => {
                self.menu = None;
                if owner && selection.is_some_and(|s| s.members.iter().any(|m| m.id == user)) {
                    self.confirm.ask(Confirm::Owner(user));
                    self.life.notice = None;
                }
            }
            Event::Leave => {
                let (Some(channel), Some(s)) = (self.channel, selection) else {
                    return Task::none();
                };
                if s.group {
                    self.confirm.ask(Confirm::Leave);
                    self.life.notice = None;
                } else {
                    return self.send(Action::Leave, channel, client, None);
                }
            }
            Event::Confirm => {
                let (Some(channel), Some(s), Some(confirm)) =
                    (self.channel, selection, self.confirm.take())
                else {
                    return Task::none();
                };
                let still_member = |user: Snowflake| s.members.iter().any(|m| m.id == user);
                match confirm {
                    Confirm::Leave if s.group => {
                        return self.send(Action::Leave, channel, client, None);
                    }
                    Confirm::Remove(user) if owner && still_member(user) => {
                        return self.send(Action::Remove(user), channel, client, None);
                    }
                    Confirm::Owner(user) if owner && still_member(user) => {
                        return self.send(
                            Action::Owner(user),
                            channel,
                            client,
                            Some(GroupEdit::Owner(user)),
                        );
                    }
                    _ => {
                        self.life.notice =
                            Some(tr("That person is no longer in the group.").to_owned())
                    }
                }
            }
            Event::Done(serial, action, result) if self.life.accept(serial) => {
                self.task = None;
                self.life.notice = Some(match result {
                    Ok(()) if action == Action::Leave => tr("Done.").to_owned(),
                    Ok(()) => tr("Sent. The panel updates when Discord confirms.").to_owned(),
                    Err(error) => describe(error),
                });
            }
            _ => {}
        }
        Task::none()
    }

    /// A 401 from this panel's request: the caller stops the account.
    pub fn is_rejected(event: &Event) -> bool {
        matches!(event, Event::Done(_, _, r) if auth_rejected(r))
    }
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
    me: Snowflake,
) -> Option<Element<'a, Event>> {
    let selection: &PrivateChannelSelection = navigation.private_selection.as_ref()?;
    if state.channel != Some(selection.channel_id) {
        return None;
    }
    let idle = !state.life.is_busy();
    let owner = selection.owner == Some(me);
    let title = if selection.group {
        tr("Group")
    } else {
        tr("Conversation")
    };
    let mut panel = column![
        row![
            text(title).size(13),
            space::horizontal(),
            crate::icons::close(idle.then_some(Event::Close)),
        ]
        .align_y(iced::Alignment::Center),
    ]
    .spacing(6);

    if selection.group {
        panel = panel.push(
            row![
                text_input(tr("Group name (empty restores the default)"), &state.name)
                    .on_input_maybe(idle.then_some(Event::NameInput))
                    .on_submit(Event::Rename),
                button(text(tr("Rename")).size(13)).on_press_maybe(idle.then_some(Event::Rename)),
            ]
            .spacing(6),
        );
        panel = panel.push(
            row![
                text(if selection.has_icon {
                    tr("Icon: set")
                } else {
                    tr("Icon: none")
                })
                .size(12)
                .color(crate::style::muted()),
                button(text(tr("Choose icon")).size(13))
                    .on_press_maybe(idle.then_some(Event::PickIcon)),
                button(text(tr("Remove icon")).size(13))
                    .on_press_maybe((idle && selection.has_icon).then_some(Event::ClearIcon)),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );
    }

    let members = selection.members.iter().map(|member| {
        let mut line = row![
            text(if selection.owner == Some(member.id) {
                trf("{} (owner)", &[&member.name])
            } else {
                member.name.clone()
            })
            .size(13)
            .width(Length::Fill),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        // Handing over the group and removing someone are behind `⋯`;
        // both still ask before anything is sent.
        let open = state.menu == Some(member.id);
        if selection.group && owner {
            line = line.push(crate::icons::action(crate::icons::Action {
                icon: crate::icons::Icon::More,
                on_press: idle.then_some(Event::Menu((!open).then_some(member.id))),
                active: open,
                danger: false,
                tip: tr("More").to_owned(),
                badge: 0,
            }));
        }
        let mut entry = column![line].spacing(4);
        if open && selection.group && owner {
            entry = entry.push(
                container(crate::ui::menu_card(
                    column![
                        crate::ui::item(
                            None,
                            tr("Make owner"),
                            idle.then_some(Event::AskOwner(member.id))
                        ),
                        crate::ui::danger_item(
                            None,
                            tr("Remove from group"),
                            idle.then_some(Event::AskRemove(member.id))
                        ),
                    ]
                    .spacing(1),
                    200.0,
                ))
                .width(Length::Fill)
                .align_x(iced::alignment::Horizontal::Right),
            );
        }
        entry.into()
    });
    panel = panel.push(text(trf("Members ({})", &[&(selection.members.len() + 1)])).size(12));
    panel = panel.push(column(members).spacing(2).push(text(tr("You")).size(13)));

    if selection.group {
        let friends: Vec<Element<'a, Event>> = navigation
            .relationships
            .iter()
            .filter(|r| {
                r.kind == RelationshipKind::Friend
                    && r.user.id != me
                    && !selection.members.iter().any(|m| m.id == r.user.id)
            })
            .take(MAX_CANDIDATES)
            .map(|r| {
                button(text(trf("Add {}", &[&r.user.display_name()])).size(12))
                    .on_press_maybe(idle.then_some(Event::Add(r.user.id)))
                    .style(crate::style::secondary_button(false))
                    .width(Length::Fill)
                    .into()
            })
            .collect();
        if !friends.is_empty() {
            panel = panel
                .push(text(tr("Add a friend")).size(12))
                .push(scrollable(column(friends).spacing(2)).height(Length::Fixed(90.0)));
        }
    }

    let leave_label = if selection.group {
        tr("Leave group")
    } else {
        tr("Close conversation")
    };
    match state.confirm.pending() {
        Some(confirm) => {
            let question = match confirm {
                Confirm::Leave => {
                    tr("Leave this group? You will not get back in unless someone adds you.")
                        .to_owned()
                }
                Confirm::Remove(user) => {
                    trf("Remove {} from the group?", &[&name_of(selection, user)])
                }
                Confirm::Owner(user) => trf(
                    "Make {} the owner? You will no longer be able to remove people.",
                    &[&name_of(selection, user)],
                ),
            };
            panel = panel.push(
                row![
                    text(question)
                        .size(13)
                        .style(text::danger)
                        .width(Length::Fill),
                    button(text(tr("Confirm")).size(13))
                        .on_press_maybe(idle.then_some(Event::Confirm))
                        .style(button::danger),
                    button(text(tr("Cancel")).size(13)).on_press(Event::Cancel),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center),
            );
        }
        None => {
            panel = panel.push(
                button(text(leave_label).size(13))
                    .on_press_maybe(idle.then_some(Event::Leave))
                    .style(if selection.group {
                        button::danger
                    } else {
                        button::secondary
                    }),
            );
        }
    }
    if state.life.is_busy() {
        panel = panel.push(
            text(tr("Waiting for Discord…"))
                .size(12)
                .color(crate::style::muted()),
        );
    }
    if let Some(notice) = &state.life.notice {
        panel = panel.push(text(notice.as_str()).size(12).color(crate::style::muted()));
    }
    // Shown as a dialog, which draws the card.
    Some(panel.into())
}

fn name_of(selection: &PrivateChannelSelection, user: Snowflake) -> String {
    selection
        .members
        .iter()
        .find(|m| m.id == user)
        .map_or_else(|| tr("this person").to_owned(), |m| m.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::state::navigation::PrivateMember;
    use fastcord_discord::{NetworkFailure, RetryableFailure};
    use fastcord_model::{Relationship, User};
    use std::sync::Arc;

    const ME: Snowflake = Snowflake(1);

    fn client() -> RestClient {
        use fastcord_discord::UserToken;
        use fastcord_discord::gateway::{ClientProperties, HostOs};
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-group-credential".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn user(id: u64) -> User {
        User {
            id: Snowflake(id),
            username: format!("u{id}"),
            global_name: None,
            avatar: None,
            bot: false,
        }
    }

    fn navigation(group: bool, owner: u64) -> NavigationSnapshot {
        NavigationSnapshot {
            private_selection: Some(PrivateChannelSelection {
                channel_id: Snowflake(50),
                name: "Team".to_owned(),
                group,
                custom_name: "Team".to_owned(),
                owner: Some(Snowflake(owner)),
                has_icon: true,
                members: vec![
                    PrivateMember {
                        id: Snowflake(2),
                        name: "two".to_owned(),
                    },
                    PrivateMember {
                        id: Snowflake(3),
                        name: "three".to_owned(),
                    },
                ],
                acked: None,
                unread: false,
                mentions: 0,
            }),
            relationships: Arc::from(vec![
                Relationship {
                    user: user(4),
                    kind: RelationshipKind::Friend,
                    ignored: false,
                },
                Relationship {
                    user: user(5),
                    kind: RelationshipKind::Blocked,
                    ignored: false,
                },
                Relationship {
                    user: user(2),
                    kind: RelationshipKind::Friend,
                    ignored: false,
                },
            ]),
            ..NavigationSnapshot::default()
        }
    }

    fn open(state: &mut State, nav: &NavigationSnapshot) {
        let _ = state.update(Event::Toggle, nav, ME, &client());
        assert!(state.is_open_for(Snowflake(50)));
    }

    #[test]
    fn destructive_actions_send_nothing_until_confirmed() {
        let nav = navigation(true, 1);
        let mut state = State::default();
        open(&mut state, &nav);
        for ask in [
            Event::Leave,
            Event::AskRemove(Snowflake(2)),
            Event::AskOwner(Snowflake(3)),
        ] {
            let _ = state.update(ask, &nav, ME, &client());
            assert!(!state.busy(), "asking is not a request");
            assert!(state.confirm.pending().is_some());
        }
        // Cancel clears it; Confirm without a question does nothing.
        let _ = state.update(Event::Cancel, &nav, ME, &client());
        let _ = state.update(Event::Confirm, &nav, ME, &client());
        assert!(!state.busy());
        let _ = state.update(Event::AskRemove(Snowflake(2)), &nav, ME, &client());
        let _ = state.update(Event::Confirm, &nav, ME, &client());
        assert_eq!(state.life.busy(), Some(Action::Remove(Snowflake(2))));
    }

    #[test]
    fn one_request_at_a_time_and_a_stale_answer_is_ignored() {
        let nav = navigation(true, 1);
        let mut state = State::default();
        open(&mut state, &nav);
        let _ = state.update(Event::Add(Snowflake(4)), &nav, ME, &client());
        assert_eq!(state.life.busy(), Some(Action::Add(Snowflake(4))));
        let serial = state.life.serial();
        // A second click while waiting is refused, not queued.
        let _ = state.update(Event::Add(Snowflake(4)), &nav, ME, &client());
        assert_eq!(state.life.busy(), Some(Action::Add(Snowflake(4))));
        assert!(state.life.notice.is_some());
        // The panel cannot be closed or moved away from under a request.
        let _ = state.update(Event::Close, &nav, ME, &client());
        state.sync(None);
        assert!(state.is_open_for(Snowflake(50)));
        // An answer of an earlier session is dropped.
        let _ = state.update(
            Event::Done(serial.wrapping_sub(1), Action::Add(Snowflake(4)), Ok(())),
            &nav,
            ME,
            &client(),
        );
        assert!(state.busy());
        let _ = state.update(
            Event::Done(
                serial,
                Action::Add(Snowflake(4)),
                Err(RestError::PermissionDenied),
            ),
            &nav,
            ME,
            &client(),
        );
        assert!(!state.busy());
        assert!(state.life.notice.as_deref().unwrap().contains("refused"));
        state.sync(None);
        assert!(state.channel.is_none(), "closes once idle and deselected");
    }

    #[test]
    fn only_the_owner_removes_or_hands_over_and_only_new_friends_are_added() {
        let nav = navigation(true, 2);
        let mut state = State::default();
        open(&mut state, &nav);
        let _ = state.update(Event::AskRemove(Snowflake(3)), &nav, ME, &client());
        let _ = state.update(Event::AskOwner(Snowflake(3)), &nav, ME, &client());
        assert!(state.confirm.pending().is_none(), "not the owner");
        for rejected in [2, 5, 1, 99] {
            let _ = state.update(Event::Add(Snowflake(rejected)), &nav, ME, &client());
            assert!(
                !state.busy(),
                "{rejected}: member, blocked, self or stranger"
            );
        }
        let _ = state.update(Event::Add(Snowflake(4)), &nav, ME, &client());
        assert!(state.busy());
    }

    #[test]
    fn a_dm_can_only_be_closed_and_a_group_name_is_validated() {
        let dm = navigation(false, 1);
        let mut state = State::default();
        open(&mut state, &dm);
        let _ = state.update(Event::Add(Snowflake(4)), &dm, ME, &client());
        let _ = state.update(Event::Rename, &dm, ME, &client());
        let _ = state.update(Event::PickIcon, &dm, ME, &client());
        assert!(!state.busy());
        let _ = state.update(Event::Leave, &dm, ME, &client());
        assert_eq!(
            state.life.busy(),
            Some(Action::Leave),
            "closing a DM is immediate"
        );

        let group = navigation(true, 1);
        let mut state = State::default();
        open(&mut state, &group);
        let _ = state.update(Event::Rename, &group, ME, &client());
        assert!(!state.busy(), "unchanged name sends nothing");
        let _ = state.update(Event::NameInput("x".repeat(101)), &group, ME, &client());
        let _ = state.update(Event::Rename, &group, ME, &client());
        assert!(!state.busy());
        let _ = state.update(Event::NameInput("New".into()), &group, ME, &client());
        let _ = state.update(Event::Rename, &group, ME, &client());
        assert_eq!(state.life.busy(), Some(Action::Rename));
    }

    #[test]
    fn failures_are_explained_and_a_401_is_flagged_for_the_account() {
        for (error, word) in [
            (RestError::PermissionDenied, "refused"),
            (RestError::ResourceGone, "no longer exists"),
            (RestError::InvalidRateLimit, "rate limiting"),
            (
                RestError::Http(fastcord_discord::StatusCode::TOO_MANY_REQUESTS),
                "rate limiting",
            ),
            (
                RestError::Http(fastcord_discord::StatusCode::BAD_REQUEST),
                "full",
            ),
            (
                RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout)),
                "may have been applied",
            ),
        ] {
            assert!(describe(error).contains(word), "{error:?}");
        }
        assert!(State::is_rejected(&Event::Done(
            0,
            Action::Leave,
            Err(RestError::AuthenticationRequired)
        )));
        assert!(!State::is_rejected(&Event::Done(0, Action::Leave, Ok(()))));
    }

    #[test]
    fn icon_files_are_read_with_a_bound() {
        let dir = std::env::temp_dir().join(format!("fastcord-icon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nbody").unwrap();
        assert!(read_icon(&png).is_ok());
        let big = dir.join("big.gif");
        let mut bytes = b"GIF89a".to_vec();
        bytes.resize(MAX_ICON_BYTES + 10, 0);
        std::fs::write(&big, bytes).unwrap();
        assert_eq!(read_icon(&big), Err("The image is larger than 512 KiB."));
        let text = dir.join("t.png");
        std::fs::write(&text, b"hello").unwrap();
        assert_eq!(read_icon(&text), Err("Choose a PNG, JPEG or GIF image."));
        assert!(read_icon(&dir.join("missing.png")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
