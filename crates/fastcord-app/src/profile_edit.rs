//! Editing the account's own free identity (SPEC §16, issue #79).
//!
//! A panel opened from the user footer: display name, static avatar, bio and
//! pronouns, the nickname in the selected server, and which connections the
//! profile shows. Every change is one explicit click and one request; at most
//! one request is in flight, none is ever retried, and a failure keeps what was
//! typed. Bio and pronouns are editable only after the account's current
//! profile loaded, so a failed load cannot make a save overwrite them blindly.
//! A name or avatar change is shown from Discord's own answer (and from
//! `USER_UPDATE`); a nickname only when `GUILD_MEMBER_UPDATE` arrives. Username
//! changes need the password and are left to the official client; paid
//! cosmetics (banner, animated avatar, theme colors, effects, per-server
//! profiles) are not offered.

use crate::ui::{button, primary_button};
use fastcord_discord::state::navigation::NavigationSnapshot;
use fastcord_discord::{
    AccountEdit, AvatarImage, Connection, MAX_BIO_CHARS, MAX_DISPLAY_NAME_CHARS, MAX_NICK_CHARS,
    MAX_PRONOUNS_CHARS, NetworkFailure, Profile, ProfileError, ProfileRequestError, ProfileText,
    RestClient, RestError, RetryableFailure,
};
use fastcord_model::{Snowflake, User};
use iced::widget::{column, row, space, text, text_input};
use iced::{Element, Length, Task};

use crate::i18n::{tr, trf};
use crate::style;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Reading the account's current bio and pronouns.
    Load,
    Name,
    Avatar,
    Text,
    Nick,
    Connections,
    Visibility,
    Activity,
}

/// What a finished request hands back.
#[derive(Clone, Debug)]
pub enum Outcome {
    Done,
    /// Discord answered with the account; the display follows `USER_UPDATE`
    /// (the Gateway is the single writer of what is shown).
    Saved,
    Profile(Box<Profile>),
    Connections(Vec<Connection>),
    Visibility(usize, bool),
    Activity(usize, bool),
}

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    NameInput(String),
    SaveName,
    PickAvatar,
    /// `Ok(None)`: the dialog was cancelled.
    AvatarPicked(u64, Result<Option<AvatarImage>, &'static str>),
    RemoveAvatar,
    BioInput(String),
    PronounsInput(String),
    SaveText,
    NickInput(String),
    SaveNick,
    LoadConnections,
    ToggleConnection(usize),
    ToggleActivity(usize),
    Done(u64, Action, Result<Outcome, ProfileRequestError>),
}

#[derive(Default)]
pub struct State {
    open: bool,
    name: String,
    bio: String,
    pronouns: String,
    nick: String,
    /// Bio and pronouns as last read or saved; `None` until loaded.
    loaded: Option<(String, String)>,
    /// The server the nickname field belongs to.
    nick_guild: Option<Snowflake>,
    connections: Option<Vec<Connection>>,
    busy: Option<Action>,
    notice: Option<String>,
    serial: u64,
    /// Dropped (aborting the request) only on logout or account change.
    task: Option<iced::task::Handle>,
}

fn describe(error: ProfileRequestError, action: Action) -> String {
    let error = match error {
        ProfileRequestError::Invalid(ProfileError::TooLong) => {
            return tr("That text is longer than Discord allows.").to_owned();
        }
        ProfileRequestError::Invalid(ProfileError::UnsupportedImage) => {
            return tr("Choose a PNG or JPEG image (animated avatars need Nitro).").to_owned();
        }
        ProfileRequestError::Invalid(ProfileError::ImageTooLarge) => {
            return tr("The image is larger than 512 KiB.").to_owned();
        }
        ProfileRequestError::Invalid(ProfileError::UneditableConnection) => {
            return tr("That connection cannot be changed from here.").to_owned();
        }
        ProfileRequestError::Rest(error) => error,
    };
    match error {
        RestError::AuthenticationRequired => tr("Your session has expired. Log in again.").to_owned(),
        RestError::PermissionDenied => match action {
            Action::Nick => tr("Discord refused: you may not be allowed to change your nickname in this server.").to_owned(),
            _ => tr("Discord refused this change for this account.").to_owned(),
        },
        RestError::ResourceGone => tr("Discord could not find that server or connection.").to_owned(),
        RestError::InvalidRateLimit | RestError::RateLimited | RestError::SchedulerCapacityExceeded => {
            tr("Discord is rate limiting requests. Wait a moment and try again.").to_owned()
        }
        RestError::Http(status) if status.as_u16() == 429 => {
            tr("Discord is rate limiting requests. Wait a moment and try again.").to_owned()
        }
        RestError::Http(status) if status.as_u16() == 400 => {
            tr("Discord rejected the value. Names are limited in how often they change, and some changes need verification (password, two-factor or CAPTCHA) that only the official Discord client can complete. What you typed is kept.")
                .to_owned()
        }
        RestError::RequestBodyTooLarge => tr("That image is too large to send.").to_owned(),
        RestError::InvalidJson => {
            tr("Discord answered with something unexpected. The change may have been applied; reopen this panel to check.")
                .to_owned()
        }
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection))
        | RestError::Retryable(_) => {
            tr("Discord did not confirm. The change may have been applied; reopen this panel to check before trying again. What you typed is kept.")
                .to_owned()
        }
        RestError::WriteNotVerified(_) => tr(fastcord_discord::WRITE_NOT_VERIFIED).to_owned(),
        other => trf("The request failed: {}.", &[&tr(&other.to_string())]),
    }
}

fn avatar_from(path: &std::path::Path) -> Result<AvatarImage, &'static str> {
    let bytes = crate::group_dm::read_image_bytes(path)?;
    AvatarImage::new(&bytes).map_err(|error| match error {
        ProfileError::ImageTooLarge => "The image is larger than 512 KiB.",
        _ => "Choose a PNG or JPEG image (animated avatars need Nitro).",
    })
}

impl State {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Logout or account change: drops the panel, its fields and any request.
    pub fn clear(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            ..Self::default()
        };
    }

    /// Follows the selected server while the nickname field is untouched by a
    /// request, so a save always names the server shown.
    pub fn sync(&mut self, navigation: &NavigationSnapshot) {
        if self.open && self.busy.is_none() && self.nick_guild != navigation.selected_guild {
            self.nick_guild = navigation.selected_guild;
            self.nick = navigation.own_nick.clone().unwrap_or_default();
        }
    }

    fn send<F, Fut>(&mut self, action: Action, client: &RestClient, run: F) -> Task<Event>
    where
        F: FnOnce(RestClient) -> Fut,
        Fut: Future<Output = Result<Outcome, ProfileRequestError>> + Send + 'static,
    {
        self.busy = Some(action);
        self.notice = None;
        let serial = self.serial;
        let client = client.clone();
        let (task, handle) = Task::perform(run(client), move |result| {
            Event::Done(serial, action, result)
        })
        .abortable();
        self.task = Some(handle.abort_on_drop());
        task
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        me: &User,
        client: &RestClient,
    ) -> Task<Event> {
        match event {
            Event::Toggle if self.open => {
                if self.busy.is_none() {
                    self.clear();
                }
            }
            Event::Toggle => {
                self.open = true;
                self.name = me.global_name.clone().unwrap_or_default();
                self.nick_guild = navigation.selected_guild;
                self.nick = navigation.own_nick.clone().unwrap_or_default();
                let id = me.id;
                return self.send(Action::Load, client, move |client| async move {
                    client
                        .user_profile(id)
                        .await
                        .map(|profile| Outcome::Profile(Box::new(profile)))
                        .map_err(ProfileRequestError::Rest)
                });
            }
            Event::Done(serial, action, result) if serial == self.serial => {
                self.busy = None;
                self.task = None;
                match result {
                    Ok(outcome) => self.finished(outcome, action),
                    Err(error) => self.notice = Some(describe(error, action)),
                }
            }
            Event::AvatarPicked(serial, result) if serial == self.serial => {
                self.busy = None;
                match result {
                    Ok(Some(avatar)) => {
                        return self.send(Action::Avatar, client, move |client| async move {
                            client
                                .edit_account(&AccountEdit::Avatar(Some(avatar)))
                                .await
                                .map(|_| Outcome::Saved)
                        });
                    }
                    Ok(None) => {}
                    Err(reason) => self.notice = Some(tr(reason).to_owned()),
                }
            }
            _ if !self.open => {}
            _ if self.busy.is_some() => {
                // One request at a time; the answer of the first is awaited.
                if !matches!(
                    event,
                    Event::Done(..)
                        | Event::AvatarPicked(..)
                        | Event::NameInput(_)
                        | Event::BioInput(_)
                        | Event::PronounsInput(_)
                        | Event::NickInput(_)
                ) {
                    self.notice = Some(tr("Wait for the previous change to finish.").to_owned());
                } else {
                    self.edit_field(event);
                }
            }
            Event::NameInput(_)
            | Event::BioInput(_)
            | Event::PronounsInput(_)
            | Event::NickInput(_) => self.edit_field(event),
            Event::SaveName => {
                let name = self.name.trim().to_owned();
                if name == me.global_name.as_deref().unwrap_or_default() {
                    self.notice = Some(tr("The display name is unchanged.").to_owned());
                } else if name.chars().count() > MAX_DISPLAY_NAME_CHARS {
                    self.notice = Some(trf(
                        "A display name has at most {} characters.",
                        &[&MAX_DISPLAY_NAME_CHARS],
                    ));
                } else {
                    return self.send(Action::Name, client, move |client| async move {
                        client
                            .edit_account(&AccountEdit::DisplayName(name))
                            .await
                            .map(|_| Outcome::Saved)
                    });
                }
            }
            Event::PickAvatar => {
                self.busy = Some(Action::Avatar);
                self.notice = None;
                let serial = self.serial;
                return Task::perform(
                    async {
                        let Some(file) = rfd::AsyncFileDialog::new()
                            .set_title(tr("Avatar"))
                            .add_filter(tr("Image"), &["png", "jpg", "jpeg"])
                            .pick_file()
                            .await
                        else {
                            return Ok(None);
                        };
                        let path = file.path().to_owned();
                        tokio::task::spawn_blocking(move || avatar_from(&path).map(Some))
                            .await
                            .unwrap_or(Err("The image could not be read."))
                    },
                    move |result| Event::AvatarPicked(serial, result),
                );
            }
            Event::RemoveAvatar if me.avatar.is_some() => {
                return self.send(Action::Avatar, client, move |client| async move {
                    client
                        .edit_account(&AccountEdit::Avatar(None))
                        .await
                        .map(|_| Outcome::Saved)
                });
            }
            Event::SaveText => {
                let Some((bio, pronouns)) = self.loaded.clone() else {
                    self.notice = Some(
                        tr("Your current bio could not be read, so it is not edited.").to_owned(),
                    );
                    return Task::none();
                };
                let text = ProfileText {
                    bio: (self.bio.trim() != bio).then(|| self.bio.clone()),
                    pronouns: (self.pronouns.trim() != pronouns).then(|| self.pronouns.clone()),
                };
                if text == ProfileText::default() {
                    self.notice = Some(tr("Bio and pronouns are unchanged.").to_owned());
                } else {
                    return self.send(Action::Text, client, move |client| async move {
                        client
                            .edit_profile_text(&text)
                            .await
                            .map(|()| Outcome::Done)
                    });
                }
            }
            Event::SaveNick => {
                let Some(guild) = self
                    .nick_guild
                    .filter(|g| navigation.selected_guild == Some(*g))
                else {
                    self.notice = Some(tr("Select a server first.").to_owned());
                    return Task::none();
                };
                let nick = self.nick.trim().to_owned();
                if nick == navigation.own_nick.as_deref().unwrap_or_default() {
                    self.notice = Some(tr("The nickname is unchanged.").to_owned());
                } else {
                    return self.send(Action::Nick, client, move |client| async move {
                        client
                            .edit_own_nick(guild, &nick)
                            .await
                            .map(|()| Outcome::Done)
                    });
                }
            }
            Event::LoadConnections => {
                return self.send(Action::Connections, client, move |client| async move {
                    client
                        .connections()
                        .await
                        .map(Outcome::Connections)
                        .map_err(ProfileRequestError::Rest)
                });
            }
            Event::ToggleConnection(index) => {
                let Some(connection) = self
                    .connections
                    .as_ref()
                    .and_then(|list| list.get(index))
                    .cloned()
                else {
                    return Task::none();
                };
                let visible = !connection.visible;
                return self.send(Action::Visibility, client, move |client| async move {
                    client
                        .set_connection_visibility(&connection, visible)
                        .await
                        .map(|()| Outcome::Visibility(index, visible))
                });
            }
            Event::ToggleActivity(index) => {
                let Some(connection) = self
                    .connections
                    .as_ref()
                    .and_then(|list| list.get(index))
                    .cloned()
                else {
                    return Task::none();
                };
                let show = !connection.show_activity.unwrap_or(false);
                return self.send(Action::Activity, client, move |client| async move {
                    client
                        .set_connection_show_activity(&connection, show)
                        .await
                        .map(|()| Outcome::Activity(index, show))
                });
            }
            _ => {}
        }
        Task::none()
    }

    fn edit_field(&mut self, event: Event) {
        self.notice = None;
        match event {
            Event::NameInput(v) => self.name = v.chars().take(MAX_DISPLAY_NAME_CHARS + 8).collect(),
            Event::BioInput(v) if self.loaded.is_some() => {
                self.bio = v.chars().take(MAX_BIO_CHARS + 20).collect();
            }
            Event::PronounsInput(v) if self.loaded.is_some() => {
                self.pronouns = v.chars().take(MAX_PRONOUNS_CHARS + 8).collect();
            }
            Event::NickInput(v) => self.nick = v.chars().take(MAX_NICK_CHARS + 8).collect(),
            _ => {}
        }
    }

    fn finished(&mut self, outcome: Outcome, action: Action) {
        match outcome {
            Outcome::Profile(profile) => {
                self.bio = profile.bio.clone();
                self.pronouns = profile.pronouns.clone();
                self.loaded = Some((profile.bio, profile.pronouns));
            }
            Outcome::Saved => self.notice = Some(tr("Saved.").to_owned()),
            Outcome::Connections(list) => {
                self.notice = list.is_empty().then(|| tr("No connections.").to_owned());
                self.connections = Some(list);
            }
            Outcome::Visibility(index, visible) => {
                if let Some(connection) = self.connections.as_mut().and_then(|l| l.get_mut(index)) {
                    connection.visible = visible;
                }
                self.notice = Some(tr("Saved.").to_owned());
            }
            Outcome::Activity(index, show) => {
                if let Some(connection) = self.connections.as_mut().and_then(|l| l.get_mut(index)) {
                    connection.show_activity = Some(show);
                }
                self.notice = Some(tr("Saved.").to_owned());
            }
            Outcome::Done if action == Action::Text => {
                // What was sent is now what Discord has.
                self.loaded = Some((self.bio.trim().to_owned(), self.pronouns.trim().to_owned()));
                self.notice = Some(tr("Saved.").to_owned());
            }
            Outcome::Done => {
                self.notice =
                    Some(tr("Sent. It shows here when Discord confirms the change.").to_owned());
            }
        }
    }

    /// A 401 from this panel's request: the caller stops the account.
    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Done(
                _,
                _,
                Err(ProfileRequestError::Rest(RestError::AuthenticationRequired))
            )
        )
    }
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
    me: &'a User,
) -> Option<Element<'a, Event>> {
    if !state.open {
        return None;
    }
    let idle = state.busy.is_none();
    let editable_text = idle && state.loaded.is_some();
    // A page of the settings screen, which has the title and the cross.
    // Each field saves on its own: its Save is the one filled button there.
    let save = |on: Option<Event>| {
        primary_button(text(tr("Save")).size(13))
            .on_press_maybe(on)
            .padding([7, 14])
            .style(style::action_button(false))
    };
    let neutral = |label: &'a str, on: Option<Event>| {
        button(text(label).size(13))
            .on_press_maybe(on)
            .padding([7, 14])
            .style(style::secondary_button(false))
    };
    fn field<'a>(
        label: impl text::IntoFragment<'a>,
        body: Element<'a, Event>,
    ) -> Element<'a, Event> {
        column![crate::ui::label(label), body].spacing(6).into()
    }
    let mut panel = column![
        field(
            tr("Display name"),
            column![
                row![
                    text_input(tr("Empty shows your username"), &state.name)
                        .on_input(Event::NameInput)
                        .on_submit(Event::SaveName)
                        .padding(8),
                    save(idle.then_some(Event::SaveName)),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
                crate::ui::note(trf("Username: @{}", &[&me.username])),
                crate::ui::note(tr("Changing your username needs your password (and two-factor code), so do it in the official Discord client.")),
            ]
            .spacing(4)
            .into(),
        ),
        field(
            tr("Avatar"),
            row![
                neutral(tr("Choose avatar"), idle.then_some(Event::PickAvatar)),
                neutral(
                    tr("Remove avatar"),
                    (idle && me.avatar.is_some()).then_some(Event::RemoveAvatar)
                ),
                crate::ui::note(if me.avatar.is_some() {
                    tr("Avatar: set")
                } else {
                    tr("Avatar: none")
                }),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .wrap()
            .into(),
        ),
        field(
            tr("About me"),
            column![
                text_input(tr("Pronouns"), &state.pronouns)
                    .on_input_maybe(editable_text.then_some(Event::PronounsInput))
                    .padding(8),
                row![
                    text_input(tr("About me"), &state.bio)
                        .on_input_maybe(editable_text.then_some(Event::BioInput))
                        .on_submit(Event::SaveText)
                        .padding(8),
                    save(editable_text.then_some(Event::SaveText)),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            ]
            .spacing(6)
            .into(),
        ),
    ]
    .spacing(24);
    if state.loaded.is_none() && idle {
        panel = panel.push(crate::ui::note(tr(
            "Your current bio and pronouns could not be read; close and reopen to try again.",
        )));
    }
    panel = if navigation.selected_guild.is_some() {
        panel.push(field(
            trf("Nickname in {}", &[&navigation.selected_guild_name]),
            row![
                text_input(tr("Nickname (empty removes it)"), &state.nick)
                    .on_input(Event::NickInput)
                    .on_submit(Event::SaveNick)
                    .padding(8),
                save(idle.then_some(Event::SaveNick)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        ))
    } else {
        panel.push(crate::ui::note(tr(
            "Open a server to change your nickname there.",
        )))
    };
    let mut connections = column![
        row![
            crate::ui::note(tr("Shown on your profile when Discord allows it.")),
            space::horizontal(),
            neutral(
                if state.connections.is_some() {
                    tr("Reload")
                } else {
                    tr("Load")
                },
                idle.then_some(Event::LoadConnections)
            ),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
    ]
    .spacing(6);
    if let Some(list) = &state.connections {
        let rows = list.iter().enumerate().map(|(index, c)| {
            row![
                text(format!("{}: {}", c.kind, c.name))
                    .size(13)
                    .width(Length::Fill),
                button(text(if c.visible { tr("Shown") } else { tr("Hidden") }).size(12))
                    .on_press_maybe((idle && c.editable).then_some(Event::ToggleConnection(index)))
                    .style(style::secondary_button(c.visible)),
            ]
            .extend(c.show_activity.map(|shown| {
                button(
                    text(if shown {
                        tr("Activity shown")
                    } else {
                        tr("Activity hidden")
                    })
                    .size(12),
                )
                .on_press_maybe((idle && c.editable).then_some(Event::ToggleActivity(index)))
                .style(style::secondary_button(shown))
                .into()
            }))
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into()
        });
        connections = connections.push(column(rows).spacing(4));
    }
    panel = panel.push(field(tr("Connections"), connections.into()));
    if state.busy.is_some() {
        panel = panel.push(crate::ui::note(tr("Waiting for Discord…")));
    }
    if let Some(notice) = &state.notice {
        panel = panel.push(crate::ui::note(notice.as_str()));
    }
    Some(panel.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::gateway::{ClientProperties, HostOs};

    fn rest() -> RestClient {
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            fastcord_discord::UserToken::new("dummy".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn me() -> User {
        User {
            id: Snowflake(1),
            username: "alt".into(),
            global_name: Some("Alt".into()),
            avatar: None,
            bot: false,
        }
    }

    fn open(state: &mut State) -> NavigationSnapshot {
        let navigation = NavigationSnapshot::default();
        drop(state.update(Event::Toggle, &navigation, &me(), &rest()));
        navigation
    }

    fn profile(bio: &str) -> Outcome {
        Outcome::Profile(Box::new(Profile {
            user: me(),
            bio: bio.into(),
            pronouns: String::new(),
            mutual_guilds: 0,
            mutual_friends: 0,
        }))
    }

    #[tokio::test]
    async fn bio_is_not_editable_or_saved_until_the_profile_loaded() {
        let mut state = State::default();
        let navigation = open(&mut state);
        assert_eq!(state.busy, Some(Action::Load));
        let serial = state.serial;
        // The load failed: typing is ignored and a save is refused.
        drop(state.update(
            Event::Done(
                serial,
                Action::Load,
                Err(ProfileRequestError::Rest(RestError::ResourceGone)),
            ),
            &navigation,
            &me(),
            &rest(),
        ));
        assert!(state.busy.is_none());
        drop(state.update(Event::BioInput("x".into()), &navigation, &me(), &rest()));
        assert!(state.bio.is_empty());
        drop(state.update(Event::SaveText, &navigation, &me(), &rest()));
        assert!(state.busy.is_none());
        assert!(state.notice.as_deref().unwrap().contains("not edited"));
    }

    #[tokio::test]
    async fn one_request_at_a_time_failures_keep_the_typing_and_stale_answers_drop() {
        let mut state = State::default();
        let navigation = open(&mut state);
        let serial = state.serial;
        drop(state.update(
            Event::Done(serial, Action::Load, Ok(profile("old"))),
            &navigation,
            &me(),
            &rest(),
        ));
        assert_eq!(state.bio, "old");
        drop(state.update(Event::NameInput("New".into()), &navigation, &me(), &rest()));
        drop(state.update(Event::SaveName, &navigation, &me(), &rest()));
        assert_eq!(state.busy, Some(Action::Name));
        // A second save while one is in flight is refused, not queued.
        drop(state.update(Event::SaveText, &navigation, &me(), &rest()));
        assert_eq!(state.busy, Some(Action::Name));
        assert!(state.notice.as_deref().unwrap().contains("Wait"));
        // An ambiguous failure keeps the typed name and is never repeated.
        let failure = Err(ProfileRequestError::Rest(RestError::Retryable(
            RetryableFailure::Network(NetworkFailure::Timeout),
        )));
        drop(state.update(
            Event::Done(serial, Action::Name, failure),
            &navigation,
            &me(),
            &rest(),
        ));
        assert!(state.busy.is_none());
        assert_eq!(state.name, "New");
        assert!(state.notice.as_deref().unwrap().contains("may have been"));
        // An answer from before a logout is ignored.
        state.clear();
        let _ = open(&mut state);
        drop(state.update(
            Event::Done(serial, Action::Load, Ok(profile("stale"))),
            &navigation,
            &me(),
            &rest(),
        ));
        assert!(state.loaded.is_none());
    }

    #[tokio::test]
    async fn a_saved_answer_is_reported_and_a_rejection_stops_the_account() {
        let mut state = State::default();
        let navigation = open(&mut state);
        let serial = state.serial;
        drop(state.update(
            Event::Done(serial, Action::Name, Ok(Outcome::Saved)),
            &navigation,
            &me(),
            &rest(),
        ));
        assert!(state.busy.is_none());
        assert_eq!(state.notice.as_deref(), Some("Saved."));
        let rejected = Event::Done(
            serial,
            Action::Name,
            Err(ProfileRequestError::Rest(RestError::AuthenticationRequired)),
        );
        assert!(State::is_rejected(&rejected));
        assert!(!State::is_rejected(&Event::Toggle));
    }

    #[tokio::test]
    async fn unchanged_values_and_a_missing_server_send_nothing() {
        let mut state = State::default();
        let navigation = open(&mut state);
        let serial = state.serial;
        drop(state.update(
            Event::Done(serial, Action::Load, Ok(profile("same"))),
            &navigation,
            &me(),
            &rest(),
        ));
        drop(state.update(Event::SaveName, &navigation, &me(), &rest()));
        assert!(state.busy.is_none());
        drop(state.update(Event::SaveText, &navigation, &me(), &rest()));
        assert!(state.busy.is_none());
        drop(state.update(Event::SaveNick, &navigation, &me(), &rest()));
        assert!(state.busy.is_none());
        assert!(state.notice.as_deref().unwrap().contains("Select a server"));
    }

    #[test]
    fn failures_have_their_own_wording() {
        let say = |e| describe(ProfileRequestError::Rest(e), Action::Name);
        assert!(say(RestError::AuthenticationRequired).contains("expired"));
        assert!(say(RestError::PermissionDenied).contains("refused"));
        assert!(say(RestError::ResourceGone).contains("could not find"));
        assert!(
            say(RestError::Http(
                fastcord_discord::StatusCode::TOO_MANY_REQUESTS
            ))
            .contains("rate limiting")
        );
        assert!(
            say(RestError::Http(fastcord_discord::StatusCode::BAD_REQUEST))
                .contains("official Discord client")
        );
        assert!(
            describe(
                ProfileRequestError::Rest(RestError::PermissionDenied),
                Action::Nick
            )
            .contains("nickname")
        );
    }
}
