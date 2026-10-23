//! Friends, relationship actions and user profiles (SPEC §16 milestone 45).
//!
//! The lists come from the reducer's shared `Relationship` slice (READY plus
//! RELATIONSHIP_* events); this module only holds what the user is doing with
//! them. Every mutation follows an explicit click and is one request. A user
//! with a request in flight cannot be acted on again, so nothing is ever sent
//! twice by the program; a failure is shown once and the user may click again.
//! The lists themselves change only when Discord's event arrives.
//!
//! Presence dots and the Online tab read the reducer's shared presence table
//! (milestone 46); an unknown presence draws no dot.

use std::collections::HashMap;

type Presences = HashMap<Snowflake, Presence>;

use fastcord_discord::{
    NetworkFailure, Profile, RelationshipAction, RestClient, RestError, RetryableFailure,
    UsernameError, parse_username,
};
use fastcord_model::{Presence, Relationship, RelationshipKind, Snowflake, Status, User};
use iced::font::Weight;
use iced::widget::{button, column, container, row, scrollable, space, text, text_input};
use iced::{Element, Font, Length, Task, alignment};

use crate::style;

/// Requests in flight at once; a user can only have one.
pub const MAX_PENDING: usize = 16;
/// Rows drawn per tab. The list is not virtualized, so it is capped.
// ponytail: cap instead of a virtual list; add one if 300 friends is not enough.
const MAX_SHOWN: usize = 300;
const MAX_ADD_INPUT_BYTES: usize = 64;

const BOLD: Font = Font {
    weight: Weight::Bold,
    ..Font::DEFAULT
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Online,
    All,
    Pending,
    Blocked,
}

impl Tab {
    fn includes(self, relationship: &Relationship, presences: &Presences) -> bool {
        let kind = relationship.kind;
        match self {
            Self::Online => {
                kind == RelationshipKind::Friend
                    && matches!(
                        presences.get(&relationship.user.id).map(|p| p.status),
                        Some(Status::Online | Status::Idle | Status::Dnd)
                    )
            }
            Self::All => kind == RelationshipKind::Friend,
            Self::Pending => matches!(
                kind,
                RelationshipKind::IncomingRequest | RelationshipKind::OutgoingRequest
            ),
            Self::Blocked => kind == RelationshipKind::Blocked,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Open,
    Close,
    Tab(Tab),
    AddInput(String),
    AddSubmit,
    Act(Snowflake, RelationshipAction),
    Done(u64, Snowflake, Result<(), RestError>),
    AddDone(u64, Result<(), RestError>),
    /// Show a user's profile (a message author or a friend-list row).
    Profile(User),
    ProfileLoaded(u64, Result<Profile, RestError>),
    CloseProfile,
    /// Open the direct conversation with this user; handled by the caller.
    OpenDm(Snowflake),
    /// Start reporting this user; handled by the caller (a review follows).
    Report(User),
}

#[derive(Debug)]
enum ProfileState {
    Loading,
    Loaded(Profile),
    Failed(String),
}

#[derive(Debug)]
struct ProfileView {
    user: User,
    serial: u64,
    state: ProfileState,
}

#[derive(Debug, Default)]
pub struct State {
    pub open: bool,
    tab: Option<Tab>,
    add_input: String,
    add_pending: bool,
    pending: HashMap<Snowflake, RelationshipAction>,
    notice: Option<String>,
    profile: Option<ProfileView>,
    /// Bumped on logout/new session: answers from an earlier one are dropped.
    epoch: u64,
    serial: u64,
}

/// What a failed relationship request tells the user to do next.
pub fn failure(error: RestError) -> String {
    match error {
        RestError::AccountVerificationRequired => "Discord requires an account check. Requests are paused. Open the official Discord client, complete its verification, then reconnect fastcord.".to_owned(),
        RestError::Refused(refusal) => format!("Discord refused ({}{}): {}", refusal.status.as_u16(), refusal.code.map_or_else(String::new, |code| format!(", code {code}")), refusal.message()),
        RestError::AuthenticationRequired => "Your session has expired. Log in again.".to_owned(),
        RestError::PermissionDenied => {
            "Discord refused: the user may have blocked you or may not accept friend requests."
                .to_owned()
        }
        RestError::ResourceGone => "Discord could not find that user.".to_owned(),
        RestError::InvalidRateLimit | RestError::SchedulerCapacityExceeded => {
            "Discord is rate limiting requests. Wait a moment and try again.".to_owned()
        }
        RestError::Http(status) if status.as_u16() == 429 => {
            "Discord is rate limiting requests. Wait a moment and try again.".to_owned()
        }
        RestError::Http(status) if status.as_u16() == 400 => {
            "Discord rejected the request. Check the username and try again.".to_owned()
        }
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)) => {
            "Could not reach Discord. Check the list before trying again.".to_owned()
        }
        RestError::Retryable(_) => {
            "Discord did not answer. Check the list before trying again.".to_owned()
        }
        RestError::WriteNotVerified(_) => fastcord_discord::WRITE_NOT_VERIFIED.to_owned(),
        other => format!("The request failed: {other}."),
    }
}

fn username_failure(error: UsernameError) -> &'static str {
    match error {
        UsernameError::Empty => "Enter a username to send a friend request.",
        UsernameError::Invalid => "That is not a valid username (2 to 32 characters, no spaces).",
    }
}

impl State {
    /// Whose profile card is open.
    pub fn profile_user(&self) -> Option<Snowflake> {
        self.profile.as_ref().map(|view| view.user.id)
    }

    /// Forgets everything about the previous account or session.
    pub fn reset(&mut self) {
        *self = Self {
            epoch: self.epoch + 1,
            serial: self.serial,
            ..Self::default()
        };
    }

    /// Closes the Friends screen and any profile.
    pub fn close(&mut self) {
        self.open = false;
        self.profile = None;
    }

    /// Logout purge: drops whatever a signed-out account left behind.
    pub fn purge(&mut self) {
        if self.open
            || self.profile.is_some()
            || self.add_pending
            || !self.pending.is_empty()
            || !self.add_input.is_empty()
            || self.notice.is_some()
        {
            self.reset();
        }
    }

    #[cfg(test)]
    fn profile_open(&self) -> bool {
        self.profile.is_some()
    }

    fn tab(&self) -> Tab {
        self.tab.unwrap_or(Tab::All)
    }

    /// Claims the one request a user may have in flight.
    fn begin(&mut self, user: Snowflake, action: RelationshipAction) -> bool {
        if self.pending.contains_key(&user) {
            return false;
        }
        if self.pending.len() >= MAX_PENDING {
            self.notice = Some("Too many requests are still being sent. Wait a moment.".into());
            return false;
        }
        self.pending.insert(user, action);
        self.notice = None;
        true
    }

    pub fn update(&mut self, event: Event, rest: &RestClient) -> Task<Event> {
        match event {
            Event::Open => self.open = true,
            Event::Close => self.close(),
            Event::Tab(tab) => {
                self.tab = Some(tab);
                self.notice = None;
            }
            Event::AddInput(value) => {
                if value.len() <= MAX_ADD_INPUT_BYTES {
                    self.add_input = value;
                    self.notice = None;
                }
            }
            Event::AddSubmit => {
                if self.add_pending {
                    return Task::none();
                }
                if let Err(error) = parse_username(&self.add_input) {
                    self.notice = Some(username_failure(error).to_owned());
                    return Task::none();
                }
                self.add_pending = true;
                self.notice = None;
                let (rest, name, epoch) = (rest.clone(), self.add_input.clone(), self.epoch);
                return Task::perform(
                    async move { rest.send_friend_request(&name).await },
                    move |result| Event::AddDone(epoch, result),
                );
            }
            Event::Act(user, action) => {
                if !self.begin(user, action) {
                    return Task::none();
                }
                let (rest, epoch) = (rest.clone(), self.epoch);
                return Task::perform(
                    async move { rest.relationship_action(action, user).await },
                    move |result| Event::Done(epoch, user, result),
                );
            }
            Event::Done(epoch, user, result) if epoch == self.epoch => {
                self.pending.remove(&user);
                // Success shows up as the list changing (the Gateway event).
                self.notice = result.err().map(failure);
            }
            Event::AddDone(epoch, result) if epoch == self.epoch => {
                self.add_pending = false;
                match result {
                    Ok(()) => {
                        self.add_input.clear();
                        self.notice = Some("Friend request sent.".to_owned());
                    }
                    Err(error) => self.notice = Some(failure(error)),
                }
            }
            Event::Profile(user) => {
                self.serial += 1;
                let serial = self.serial;
                let (rest, id) = (rest.clone(), user.id);
                self.profile = Some(ProfileView {
                    user,
                    serial,
                    state: ProfileState::Loading,
                });
                return Task::perform(async move { rest.user_profile(id).await }, move |result| {
                    Event::ProfileLoaded(serial, result)
                });
            }
            Event::ProfileLoaded(serial, result) => {
                if let Some(view) = self.profile.as_mut().filter(|view| view.serial == serial) {
                    view.state = match result {
                        Ok(profile) => ProfileState::Loaded(profile),
                        Err(error) => ProfileState::Failed(failure(error)),
                    };
                }
            }
            Event::CloseProfile => self.profile = None,
            // Stale answers from an earlier session, and the caller's own.
            Event::Done(..) | Event::AddDone(..) | Event::OpenDm(_) | Event::Report(_) => {}
        }
        Task::none()
    }
}

fn avatar<'a, M: 'a>(user: &User, size: f32) -> Element<'a, M> {
    container(
        text(style::initial(user.display_name()))
            .size(size * 0.42)
            .font(BOLD),
    )
    .center(size)
    .style(style::avatar(style::avatar_color(user.id.0)))
    .into()
}

fn small_button<'a>(
    label: &'static str,
    event: Option<Event>,
    danger: bool,
    filled: bool,
) -> Element<'a, Event> {
    let content = text(label).size(12);
    if filled {
        button(content)
            .on_press_maybe(event)
            .style(style::action_button(danger))
            .into()
    } else {
        button(content)
            .on_press_maybe(event)
            .style(style::tool_button(true, danger))
            .into()
    }
}

/// The buttons that apply to one relationship.
fn actions<'a>(
    kind: Option<RelationshipKind>,
    user: Snowflake,
    busy: bool,
) -> Vec<Element<'a, Event>> {
    let act = |action| (!busy).then_some(Event::Act(user, action));
    let message = (!busy).then_some(Event::OpenDm(user));
    match kind {
        Some(RelationshipKind::Friend) => vec![
            small_button("Message", message, false, true),
            small_button("Remove", act(RelationshipAction::Remove), true, false),
            small_button("Block", act(RelationshipAction::Block), true, false),
        ],
        Some(RelationshipKind::IncomingRequest) => vec![
            small_button("Accept", act(RelationshipAction::Befriend), false, true),
            small_button("Decline", act(RelationshipAction::Remove), true, false),
            small_button("Block", act(RelationshipAction::Block), true, false),
        ],
        Some(RelationshipKind::OutgoingRequest) => {
            vec![small_button(
                "Cancel request",
                act(RelationshipAction::Remove),
                true,
                false,
            )]
        }
        Some(RelationshipKind::Blocked) => vec![small_button(
            "Unblock",
            act(RelationshipAction::Remove),
            false,
            false,
        )],
        None => vec![
            small_button("Message", message, false, true),
            small_button(
                "Add friend",
                act(RelationshipAction::Befriend),
                false,
                false,
            ),
            small_button("Block", act(RelationshipAction::Block), true, false),
        ],
    }
}

fn kind_label(kind: RelationshipKind) -> &'static str {
    match kind {
        RelationshipKind::Friend => "Friend",
        RelationshipKind::Blocked => "Blocked",
        RelationshipKind::IncomingRequest => "Incoming friend request",
        RelationshipKind::OutgoingRequest => "Outgoing friend request",
    }
}

fn person_row<'a>(
    relationship: &'a Relationship,
    pending: Option<RelationshipAction>,
    status: Option<Status>,
) -> Element<'a, Event> {
    let user = &relationship.user;
    let mut buttons = row![].spacing(6).align_y(alignment::Vertical::Center);
    if pending.is_some() {
        buttons = buttons.push(text("Working…").size(12).style(text::secondary));
    }
    for button in actions(Some(relationship.kind), user.id, pending.is_some()) {
        buttons = buttons.push(button);
    }
    container(
        row![
            avatar(user, 36.0),
            button(
                column![
                    text(user.display_name()).font(BOLD).size(14),
                    row![
                        crate::status::dot(
                            status.filter(|_| relationship.kind == RelationshipKind::Friend)
                        ),
                        text(format!(
                            "{} · {}",
                            user.username,
                            kind_label(relationship.kind)
                        ))
                        .size(12)
                        .style(text::secondary),
                    ]
                    .spacing(5)
                    .align_y(alignment::Vertical::Center),
                ]
                .spacing(1),
            )
            .padding(0)
            .style(style::plain_button)
            .on_press(Event::Profile(user.clone())),
            space::horizontal(),
            buttons,
        ]
        .spacing(10)
        .align_y(alignment::Vertical::Center),
    )
    .padding([8, 12])
    .width(Length::Fill)
    .style(style::card)
    .into()
}

/// The Friends screen, in place of the conversation.
pub fn view<'a>(
    state: &'a State,
    relationships: &'a [Relationship],
    presences: &'a Presences,
) -> Element<'a, Event> {
    let count = |tab: Tab| {
        relationships
            .iter()
            .filter(|r| tab.includes(r, presences))
            .count()
    };
    let tab_button = |tab: Tab, label: &str| {
        button(text(format!("{label} ({})", count(tab))).size(13))
            .on_press(Event::Tab(tab))
            .style(style::tool_button(state.tab() == tab, false))
    };
    let tabs = row![
        text("Friends").size(16).font(BOLD),
        tab_button(Tab::Online, "Online"),
        tab_button(Tab::All, "All"),
        tab_button(Tab::Pending, "Pending"),
        tab_button(Tab::Blocked, "Blocked"),
        space::horizontal(),
        small_button("Close", Some(Event::Close), false, false),
    ]
    .spacing(10)
    .align_y(alignment::Vertical::Center);
    let add = row![
        text_input("Add a friend by username", &state.add_input)
            .on_input(Event::AddInput)
            .on_submit(Event::AddSubmit)
            .size(13)
            .padding(6),
        small_button(
            "Send friend request",
            (!state.add_pending).then_some(Event::AddSubmit),
            false,
            true,
        ),
    ]
    .spacing(8)
    .align_y(alignment::Vertical::Center);
    let mut list = column![].spacing(6);
    let mut shown = 0;
    let mut total = 0;
    for relationship in relationships
        .iter()
        .filter(|r| state.tab().includes(r, presences))
    {
        total += 1;
        if shown < MAX_SHOWN {
            shown += 1;
            list = list.push(person_row(
                relationship,
                state.pending.get(&relationship.user.id).copied(),
                presences.get(&relationship.user.id).map(|p| p.status),
            ));
        }
    }
    if total == 0 {
        list = list.push(
            text(match state.tab() {
                Tab::Online => "Nobody is online right now.",
                Tab::All => "No friends yet. Add one by username above.",
                Tab::Pending => "No pending friend requests.",
                Tab::Blocked => "You have not blocked anyone.",
            })
            .size(13)
            .style(text::secondary),
        );
    } else if total > shown {
        list = list.push(
            text(format!("{} more are not shown.", total - shown))
                .size(12)
                .style(text::secondary),
        );
    }
    let mut body = column![tabs, add].spacing(10);
    body = body.push(
        text(state.notice.as_deref().unwrap_or(""))
            .size(12)
            .style(text::secondary),
    );
    body = body.push(scrollable(list).height(Length::Fill));
    container(body.padding(16))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(style::chat)
        .into()
}

/// What the open server and the presence table add to a profile card.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemberContext<'a> {
    pub server: Option<&'a str>,
    pub nick: Option<&'a str>,
    /// The member's roles in the open server, highest first.
    pub roles: [Option<&'a str>; MAX_ROLES_SHOWN],
    pub more_roles: usize,
    /// The custom status already in the presence table (no game detection).
    pub custom: Option<&'a str>,
    /// The account ignores this user (distinct from blocking).
    pub ignored: bool,
}

/// Roles drawn on a card; the rest are counted.
pub const MAX_ROLES_SHOWN: usize = 12;

impl<'a> MemberContext<'a> {
    pub fn of(
        user: Option<Snowflake>,
        navigation: &'a fastcord_discord::state::navigation::NavigationSnapshot,
        directs: bool,
    ) -> Self {
        let Some(user) = user else {
            return Self::default();
        };
        let mut context = Self {
            custom: navigation
                .presences
                .get(&user)
                .and_then(|p| p.custom.as_deref()),
            ignored: navigation.suppressed.get(&user)
                == Some(&fastcord_discord::state::Suppression::Ignored),
            ..Self::default()
        };
        if directs || navigation.selected_guild.is_none() {
            return context;
        }
        // A member not held is simply not described: roles are never guessed.
        if let Some(member) = navigation.members.get(&user) {
            context.server = Some(navigation.selected_guild_name.as_str());
            context.nick = member.nick.as_deref();
            let mut names = navigation
                .guild_roles
                .iter()
                .filter(|(id, _)| member.roles.contains(id))
                .map(|(_, name)| name.as_str());
            for slot in &mut context.roles {
                *slot = names.next();
            }
            context.more_roles = names.count();
        }
        context
    }
}

/// The profile popup over the screen, while one is open.
pub fn profile_view<'a>(
    state: &'a State,
    relationships: &'a [Relationship],
    me: Snowflake,
    context: MemberContext<'a>,
) -> Option<Element<'a, Event>> {
    let view = state.profile.as_ref()?;
    let user = &view.user;
    let kind = relationships
        .iter()
        .find(|r| r.user.id == user.id)
        .map(|r| r.kind);
    let mut card = column![
        row![
            avatar(user, 56.0),
            column![
                text(user.display_name()).size(18).font(BOLD),
                text(format!("@{}", user.username))
                    .size(13)
                    .style(text::secondary),
            ]
            .spacing(2),
        ]
        .spacing(12)
        .align_y(alignment::Vertical::Center),
    ]
    .spacing(10);
    if user.bot {
        card = card.push(text("Bot").size(12).style(text::secondary));
    }
    if let Some(kind) = kind {
        card = card.push(text(kind_label(kind)).size(12).style(text::secondary));
    }
    if context.ignored {
        card = card.push(
            text("Ignored: their messages, notifications and calls are hidden.")
                .size(12)
                .style(text::secondary),
        );
    }
    if let Some(custom) = context.custom {
        card = card.push(text(custom).size(12));
    }
    if let (Some(server), true) = (
        context.server,
        context.roles[0].is_some() || context.nick.is_some(),
    ) {
        if let Some(nick) = context.nick {
            card = card.push(text(format!("Nickname in {server}: {nick}")).size(12));
        }
        if context.roles[0].is_some() {
            let mut line = context
                .roles
                .iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>()
                .join(", ");
            if context.more_roles > 0 {
                line.push_str(&format!(" and {} more", context.more_roles));
            }
            card = card.push(text(format!("Roles in {server}: {line}")).size(12));
        }
    }
    match &view.state {
        ProfileState::Loading => {
            card = card.push(text("Loading profile…").size(13).style(text::secondary));
        }
        ProfileState::Failed(reason) => {
            card = card.push(text(reason.as_str()).size(13).style(text::danger));
        }
        ProfileState::Loaded(profile) => {
            if !profile.pronouns.is_empty() {
                card = card.push(text(profile.pronouns.as_str()).size(12));
            }
            if !profile.bio.is_empty() {
                card = card.push(text(profile.bio.as_str()).size(13));
            }
            card = card.push(
                text(format!(
                    "{} mutual servers · {} mutual friends",
                    profile.mutual_guilds, profile.mutual_friends
                ))
                .size(12)
                .style(text::secondary),
            );
        }
    }
    let mut buttons = row![].spacing(6);
    if user.id != me && !user.bot {
        let busy = state.pending.contains_key(&user.id);
        if busy {
            buttons = buttons.push(text("Working…").size(12).style(text::secondary));
        }
        for button in actions(kind, user.id, busy) {
            buttons = buttons.push(button);
        }
        // Ignore never changes the relationship; it is its own request.
        let (label, action) = if context.ignored {
            ("Stop ignoring", RelationshipAction::Unignore)
        } else {
            ("Ignore", RelationshipAction::Ignore)
        };
        buttons = buttons.push(small_button(
            label,
            (!busy).then_some(Event::Act(user.id, action)),
            false,
            false,
        ));
        buttons = buttons.push(small_button(
            "Report",
            Some(Event::Report(user.clone())),
            false,
            false,
        ));
    }
    buttons = buttons.push(small_button(
        "Close",
        Some(Event::CloseProfile),
        false,
        false,
    ));
    card = card.push(buttons);
    if let Some(notice) = &state.notice {
        card = card.push(text(notice.as_str()).size(12).style(text::secondary));
    }
    Some(
        container(
            container(card.width(Length::Fill))
                .padding(20)
                .max_width(420)
                .style(style::card),
        )
        .center(Length::Fill)
        .style(|_| container::Style {
            background: Some(iced::Color::from_rgba(0.0, 0.0, 0.0, 0.6).into()),
            ..container::Style::default()
        })
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::StatusCode;
    use fastcord_discord::gateway::{ClientProperties, HostOs};

    fn rest() -> RestClient {
        RestClient::new(
            fastcord_discord::UserToken::new("dummy".into()),
            ClientProperties::web(HostOs::Linux, "en-US", 1),
        )
        .unwrap()
    }

    fn user(id: u64) -> User {
        User {
            id: Snowflake(id),
            username: format!("user{id}"),
            global_name: None,
            avatar: None,
            bot: false,
        }
    }

    #[test]
    fn a_user_has_at_most_one_request_in_flight_and_the_ledger_is_bounded() {
        let mut state = State::default();
        let bob = Snowflake(2);
        assert!(state.begin(bob, RelationshipAction::Befriend));
        // A second click on the same user, whatever the action, is refused.
        assert!(!state.begin(bob, RelationshipAction::Remove));
        assert!(!state.begin(bob, RelationshipAction::Befriend));
        for id in 10..10 + MAX_PENDING as u64 {
            state.begin(Snowflake(id), RelationshipAction::Block);
        }
        assert_eq!(state.pending.len(), MAX_PENDING);
        assert!(state.notice.is_some());
    }

    #[test]
    fn answers_clear_pending_show_actionable_failures_and_stale_ones_are_dropped() {
        let (client, mut state) = (rest(), State::default());
        let bob = Snowflake(2);
        let _ = state.update(Event::Act(bob, RelationshipAction::Remove), &client);
        assert!(state.pending.contains_key(&bob));
        let _ = state.update(Event::Act(bob, RelationshipAction::Block), &client);
        assert_eq!(state.pending[&bob], RelationshipAction::Remove);
        let epoch = state.epoch;
        let _ = state.update(
            Event::Done(epoch, bob, Err(RestError::PermissionDenied)),
            &client,
        );
        assert!(state.pending.is_empty());
        assert!(state.notice.as_deref().unwrap().contains("refused"));
        let _ = state.update(Event::Act(bob, RelationshipAction::Remove), &client);
        state.reset();
        assert!(state.pending.is_empty() && !state.open && state.notice.is_none());
        // An answer to a request of the purged session changes nothing.
        let _ = state.update(
            Event::Done(epoch, bob, Err(RestError::ResourceGone)),
            &client,
        );
        assert!(state.notice.is_none());
    }

    #[test]
    fn friend_requests_by_username_are_validated_and_sent_once() {
        let (client, mut state) = (rest(), State::default());
        let _ = state.update(Event::AddSubmit, &client);
        assert!(!state.add_pending && state.notice.is_some());
        let _ = state.update(Event::AddInput("two words".into()), &client);
        let _ = state.update(Event::AddSubmit, &client);
        assert!(!state.add_pending);
        let _ = state.update(Event::AddInput("alice".into()), &client);
        let _ = state.update(Event::AddSubmit, &client);
        assert!(state.add_pending);
        let _ = state.update(
            Event::AddInput("x".repeat(MAX_ADD_INPUT_BYTES + 1)),
            &client,
        );
        assert_eq!(state.add_input, "alice");
        let epoch = state.epoch;
        let _ = state.update(
            Event::AddDone(epoch, Err(RestError::Http(StatusCode::BAD_REQUEST))),
            &client,
        );
        assert!(!state.add_pending && state.notice.as_deref().unwrap().contains("username"));
        let _ = state.update(Event::AddDone(epoch, Ok(())), &client);
        assert!(state.add_input.is_empty());
    }

    #[test]
    fn profiles_ignore_answers_for_a_replaced_or_closed_popup() {
        let (client, mut state) = (rest(), State::default());
        let _ = state.update(Event::Profile(user(5)), &client);
        let first = state.serial;
        let _ = state.update(Event::Profile(user(6)), &client);
        let loaded = Profile {
            user: user(5),
            bio: "hi".into(),
            pronouns: String::new(),
            mutual_guilds: 1,
            mutual_friends: 0,
        };
        let _ = state.update(Event::ProfileLoaded(first, Ok(loaded)), &client);
        assert!(matches!(
            state.profile.as_ref().unwrap().state,
            ProfileState::Loading
        ));
        let _ = state.update(
            Event::ProfileLoaded(state.serial, Err(RestError::ResourceGone)),
            &client,
        );
        assert!(matches!(
            state.profile.as_ref().unwrap().state,
            ProfileState::Failed(_)
        ));
        let _ = state.update(Event::CloseProfile, &client);
        let _ = state.update(
            Event::ProfileLoaded(state.serial, Err(RestError::ResourceGone)),
            &client,
        );
        assert!(!state.profile_open());
    }

    #[test]
    fn failures_say_what_to_do_including_rate_limits() {
        let limited = failure(RestError::Http(StatusCode::TOO_MANY_REQUESTS));
        assert!(limited.contains("rate limiting"));
        assert!(failure(RestError::InvalidRateLimit).contains("rate limiting"));
        assert!(failure(RestError::PermissionDenied).contains("refused"));
        assert!(failure(RestError::ResourceGone).contains("find"));
        assert!(failure(RestError::AuthenticationRequired).contains("Log in"));
    }

    #[test]
    fn tabs_split_the_relationships_and_views_build_for_every_kind() {
        use RelationshipKind::*;
        let all = [Friend, IncomingRequest, OutgoingRequest, Blocked];
        let none = Presences::new();
        let mut state = State::default();
        let rows: Vec<Relationship> = all
            .iter()
            .enumerate()
            .map(|(i, kind)| Relationship {
                user: user(i as u64 + 1),
                kind: *kind,
                ignored: false,
            })
            .collect();
        assert_eq!(
            rows.iter().filter(|r| Tab::All.includes(r, &none)).count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|r| Tab::Pending.includes(r, &none))
                .count(),
            2
        );
        assert_eq!(
            rows.iter()
                .filter(|r| Tab::Blocked.includes(r, &none))
                .count(),
            1
        );
        let client = rest();
        let mut online = Presences::new();
        online.insert(
            Snowflake(1),
            Presence {
                status: Status::Idle,
                custom: None,
            },
        );
        assert_eq!(
            rows.iter()
                .filter(|r| Tab::Online.includes(r, &online))
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|r| Tab::Online.includes(r, &none))
                .count(),
            0
        );
        for tab in [Tab::Online, Tab::All, Tab::Pending, Tab::Blocked] {
            let _ = state.update(Event::Tab(tab), &client);
            let _ = view(&state, &rows, &online);
        }
        let _ = state.update(Event::Profile(user(1)), &client);
        assert!(profile_view(&state, &rows, Snowflake(1), MemberContext::default()).is_some());
        assert!(profile_view(&state, &rows, Snowflake(99), MemberContext::default()).is_some());
    }
}

#[cfg(test)]
mod member_context_tests {
    use super::*;
    use fastcord_discord::state::navigation::{MemberView, NavigationSnapshot};

    #[test]
    fn roles_and_nick_show_only_for_a_held_member_of_the_open_server() {
        let mut navigation = NavigationSnapshot {
            selected_guild: Some(Snowflake(100)),
            selected_guild_name: "Home".into(),
            guild_roles: vec![
                (Snowflake(3), "Mod".into()),
                (Snowflake(2), "Fan".into()),
                (Snowflake(1), "Other".into()),
            ],
            ..NavigationSnapshot::default()
        };
        navigation.members.insert(
            Snowflake(7),
            MemberView {
                nick: Some("Bee".into()),
                roles: vec![Snowflake(2), Snowflake(3)],
                ..MemberView::default()
            },
        );
        let held = MemberContext::of(Some(Snowflake(7)), &navigation, false);
        assert_eq!(held.server, Some("Home"));
        assert_eq!(held.nick, Some("Bee"));
        assert_eq!(held.roles[..3], [Some("Mod"), Some("Fan"), None]);
        assert_eq!(held.more_roles, 0);
        // Not held, in a DM, or no card: nothing is guessed.
        assert!(
            MemberContext::of(Some(Snowflake(8)), &navigation, false)
                .server
                .is_none()
        );
        assert!(
            MemberContext::of(Some(Snowflake(7)), &navigation, true)
                .server
                .is_none()
        );
        assert!(MemberContext::of(None, &navigation, false).roles[0].is_none());
        // Many roles are counted, not all drawn.
        navigation.guild_roles = (1..=20).map(|i| (Snowflake(i), format!("r{i}"))).collect();
        navigation.members.get_mut(&Snowflake(7)).unwrap().roles =
            (1..=20).map(Snowflake).collect();
        let many = MemberContext::of(Some(Snowflake(7)), &navigation, false);
        assert_eq!(many.roles.iter().flatten().count(), MAX_ROLES_SHOWN);
        assert_eq!(many.more_roles, 20 - MAX_ROLES_SHOWN);
    }

    #[test]
    fn ignore_is_separate_from_blocking_on_the_profile_card() {
        use fastcord_discord::state::Suppression;
        let navigation = NavigationSnapshot {
            suppressed: std::sync::Arc::new(std::collections::BTreeMap::from([
                (Snowflake(5), Suppression::Ignored),
                (Snowflake(6), Suppression::Blocked),
            ])),
            ..NavigationSnapshot::default()
        };
        let of = |id| MemberContext::of(Some(Snowflake(id)), &navigation, true).ignored;
        assert!(of(5), "ignored");
        assert!(!of(6), "blocked is not ignored");
        assert!(!of(7), "unknown");
        assert!(!MemberContext::of(None, &navigation, false).ignored);
    }
}
