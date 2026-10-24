//! Invitations and membership (SPEC §16, issue #90): invite cards under
//! messages, joining from one, creating an invite for the open channel
//! (lifetime and use limit chosen first), listing and revoking the invites
//! the account may manage, and leaving a server.
//!
//! A card is looked up (`GET /invites/{code}`) only for a message the timeline
//! has built, from a cache of at most [`MAX_ENTRIES`] codes that lives
//! [`TTL`], with at most [`MAX_IN_FLIGHT`] lookups in flight. Joining,
//! creating and leaving are each one explicit click and one request, never
//! retried; the server list changes only when Discord's `GUILD_CREATE` or
//! `GUILD_DELETE` arrives. A listed invite is revoked only after an explicit
//! Confirm, with one request that is never retried; the row leaves the list
//! when Discord answers success or "unknown invite", and stays with a note
//! after any other failure. A CAPTCHA or verification answer pauses every write
//! (`RestError::AccountVerificationRequired`) and is shown as such.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use fastcord_discord::state::navigation::NavigationSnapshot;
use fastcord_discord::{
    CREATE_AGES, CREATE_USES, Expiry, Invite, InviteScope, ManagedInvite, RestClient, RestError,
    extract_codes,
};
use fastcord_model::Snowflake;
use iced::widget::{button, column, container, image, pick_list, row, scrollable, space, text};
use iced::{Element, Length, Task};

use crate::avatars::{self, AvatarKey};
use crate::timeline;

/// Cached invite codes.
pub const MAX_ENTRIES: usize = 64;
/// How long a resolved (or invalid) invite is believed.
pub const TTL: Duration = Duration::from_secs(600);
/// A lookup that failed is not asked again for this long.
const FAILED_TTL: Duration = Duration::from_secs(30);
/// Lookups running at once.
pub const MAX_IN_FLIGHT: usize = 3;
/// Newest codes considered per pass, so cards never evict one another.
const MAX_WANTED: usize = MAX_ENTRIES / 4;
/// Height of a card; fixed so a row never changes size when it resolves.
pub const CARD_HEIGHT: f32 = 76.0;
const CARD_WIDTH: f32 = 420.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Card {
    Loading,
    Ready(Invite),
    /// Expired, revoked, unknown, or not a server invite.
    Invalid,
    /// Discord did not answer; asked again after a pause.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Join {
    Idle,
    Joining,
    Joined,
    /// Discord's reason, or the verification pause.
    Refused(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub card: Card,
    pub join: Join,
    at: Instant,
}

#[derive(Debug, Default)]
pub struct Cache {
    entries: HashMap<String, Entry>,
    /// Insertion order, oldest first.
    order: VecDeque<String>,
    in_flight: usize,
    /// Servers the account is in (the rail window), for "Joined".
    member: Vec<Snowflake>,
}

impl Cache {
    pub fn get(&self, code: &str) -> Option<&Entry> {
        self.entries.get(code)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn sync_guilds(&mut self, navigation: &NavigationSnapshot) {
        self.member = navigation.guilds.rows.iter().map(|g| g.id).collect();
    }

    fn is_member(&self, entry: &Entry) -> bool {
        entry.join == Join::Joined
            || matches!(&entry.card, Card::Ready(invite) if self.member.contains(&invite.guild))
    }

    fn expired(entry: &Entry, now: Instant) -> bool {
        let ttl = if entry.card == Card::Failed {
            FAILED_TTL
        } else {
            TTL
        };
        entry.card != Card::Loading
            && entry.join != Join::Joining
            && now.saturating_duration_since(entry.at) >= ttl
    }

    fn evict_one(&mut self) -> bool {
        let Some(at) = self.order.iter().position(|code| {
            self.entries
                .get(code)
                .is_none_or(|e| e.card != Card::Loading && e.join != Join::Joining)
        }) else {
            return false;
        };
        if let Some(code) = self.order.remove(at) {
            self.entries.remove(&code);
        }
        true
    }

    /// The codes to look up now, marked `Loading`: unknown or expired ones, at
    /// most [`MAX_IN_FLIGHT`] in flight, newest message first.
    pub fn want(&mut self, codes: &[String], now: Instant) -> Vec<String> {
        let mut start = Vec::new();
        for code in codes.iter().take(MAX_WANTED) {
            if self.in_flight >= MAX_IN_FLIGHT {
                break;
            }
            if let Some(entry) = self.entries.get(code) {
                if !Self::expired(entry, now) {
                    continue;
                }
                self.entries.remove(code);
                self.order.retain(|known| known != code);
            }
            if self.entries.len() >= MAX_ENTRIES && !self.evict_one() {
                break;
            }
            self.entries.insert(
                code.clone(),
                Entry {
                    card: Card::Loading,
                    join: Join::Idle,
                    at: now,
                },
            );
            self.order.push_back(code.clone());
            self.in_flight += 1;
            start.push(code.clone());
        }
        start
    }

    /// Stores a lookup's answer.
    pub fn finish(&mut self, code: &str, result: Result<Option<Invite>, RestError>, now: Instant) {
        self.in_flight = self.in_flight.saturating_sub(1);
        let Some(entry) = self.entries.get_mut(code) else {
            return;
        };
        entry.at = now;
        entry.card = match result {
            Ok(Some(invite)) => Card::Ready(invite),
            Ok(None) | Err(RestError::ResourceGone) => Card::Invalid,
            Err(RestError::Refused(refusal)) if refusal.status.as_u16() == 404 => Card::Invalid,
            Err(_) => Card::Failed,
        };
    }

    /// Marks a join as running; false unless the card is ready to join.
    fn begin_join(&mut self, code: &str) -> bool {
        match self.entries.get_mut(code) {
            Some(entry)
                if matches!(entry.card, Card::Ready(_))
                    && matches!(entry.join, Join::Idle | Join::Refused(_)) =>
            {
                entry.join = Join::Joining;
                true
            }
            _ => false,
        }
    }

    fn finish_join(&mut self, code: &str, result: Result<(), RestError>, now: Instant) {
        let Some(entry) = self.entries.get_mut(code) else {
            return;
        };
        entry.at = now;
        match result {
            Ok(()) => entry.join = Join::Joined,
            Err(RestError::ResourceGone) => {
                entry.join = Join::Idle;
                entry.card = Card::Invalid;
            }
            Err(RestError::Refused(refusal)) if refusal.status.as_u16() == 404 => {
                entry.join = Join::Idle;
                entry.card = Card::Invalid;
            }
            Err(error) => entry.join = Join::Refused(describe(error)),
        }
    }

    /// Icons of the servers the given visible codes lead to.
    pub fn icon_keys(&self, codes: &[String]) -> Vec<AvatarKey> {
        codes
            .iter()
            .filter_map(|code| match &self.entries.get(code)?.card {
                Card::Ready(invite) => AvatarKey::guild(invite.guild, invite.icon.as_deref()),
                _ => None,
            })
            .collect()
    }
}

/// Invite codes of the built timeline rows, newest message first.
pub fn visible_codes(rows: &[timeline::Row]) -> Vec<String> {
    let mut codes: Vec<String> = Vec::new();
    for row in rows.iter().rev() {
        if row.hidden.is_some() {
            continue;
        }
        for code in extract_codes(&row.message.content) {
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
        if codes.len() >= MAX_WANTED {
            break;
        }
    }
    codes
}

/// What a failed membership request tells the user.
pub fn describe(error: RestError) -> String {
    match error {
        RestError::ResourceGone => "That invite or server no longer exists.".to_owned(),
        RestError::PermissionDenied => {
            "Discord refused: you may lack permission, or may not join this server.".to_owned()
        }
        RestError::Retryable(_) => {
            "Discord did not confirm. The change may have been applied; check your server list before trying again.".to_owned()
        }
        RestError::Refused(refusal) if refusal.message().is_empty() => {
            crate::friends::failure(RestError::Refused(refusal))
        }
        RestError::Refused(refusal) => format!("Discord refused: {}", refusal.message()),
        other => crate::friends::failure(other),
    }
}

pub(crate) fn count(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn counts_line(invite: &Invite) -> String {
    let mut parts = Vec::new();
    if !invite.channel_name.is_empty() {
        parts.push(format!("#{}", invite.channel_name));
    }
    if let Some(online) = invite.online {
        parts.push(format!("{} online", count(online)));
    }
    if let Some(members) = invite.members {
        parts.push(format!("{} members", count(members)));
    }
    if let Some(expires) = invite.expires_at.as_deref().and_then(|e| e.get(..10)) {
        parts.push(format!("expires {expires}"));
    }
    parts.join(" \u{b7} ")
}

fn initials(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect())
}

/// The card under a message that holds `code`, at the fixed [`CARD_HEIGHT`].
pub fn card<'a>(
    cache: &'a Cache,
    code: &str,
    pictures: &HashMap<AvatarKey, image::Handle>,
) -> Element<'a, timeline::Event> {
    let (title, detail, face, action): (String, String, Element<'a, timeline::Event>, _) =
        match cache.get(code) {
            None
            | Some(Entry {
                card: Card::Loading,
                ..
            }) => (
                "Invite".to_owned(),
                "Loading\u{2026}".to_owned(),
                space().width(48).into(),
                None,
            ),
            Some(Entry {
                card: Card::Failed, ..
            }) => (
                "Invite".to_owned(),
                "Discord did not answer. It is asked again later.".to_owned(),
                space().width(48).into(),
                None,
            ),
            Some(Entry {
                card: Card::Invalid,
                ..
            }) => (
                "Invite invalid".to_owned(),
                "This invite has expired or does not exist.".to_owned(),
                space().width(48).into(),
                None,
            ),
            Some(
                entry @ Entry {
                    card: Card::Ready(invite),
                    join,
                    ..
                },
            ) => {
                let face = {
                    let name = invite.guild_name.clone();
                    match AvatarKey::guild(invite.guild, invite.icon.as_deref()) {
                        Some(key) => avatars::view(pictures, &key, 48.0, move || {
                            container(text(initials(&name)).size(18))
                                .center(48)
                                .style(container::rounded_box)
                                .into()
                        }),
                        None => container(text(initials(&name)).size(18))
                            .center(48)
                            .style(container::rounded_box)
                            .into(),
                    }
                };
                let detail = match join {
                    Join::Refused(reason) => reason.clone(),
                    _ => counts_line(invite),
                };
                let action = if cache.is_member(entry) {
                    Some(("Joined", Some(timeline::Event::OpenGuild(invite.guild))))
                } else if *join == Join::Joining {
                    Some(("Joining\u{2026}", None))
                } else {
                    Some(("Join", Some(timeline::Event::JoinInvite(code.to_owned()))))
                };
                (invite.guild_name.clone(), detail, face, action)
            }
        };
    let refused = matches!(
        cache.get(code),
        Some(Entry {
            join: Join::Refused(_),
            ..
        })
    );
    let detail = text(detail).size(12).wrapping(text::Wrapping::None);
    let mut line = row![
        face,
        column![
            text(title).size(15).wrapping(text::Wrapping::None),
            if refused {
                detail.style(text::danger)
            } else {
                detail.style(text::secondary)
            },
        ]
        .spacing(2)
        .width(Length::Fill),
    ]
    .spacing(12)
    .align_y(iced::Alignment::Center);
    if let Some((label, press)) = action {
        line = line.push(button(text(label).size(13)).on_press_maybe(press).style(
            if label == "Join" {
                button::success
            } else {
                button::secondary
            },
        ));
    }
    container(line)
        .padding(12)
        .width(CARD_WIDTH)
        .height(CARD_HEIGHT)
        .clip(true)
        .style(container::rounded_box)
        .into()
}

#[derive(Clone, Debug)]
pub enum Event {
    /// A lookup answered (generation, code).
    Resolved(u64, String, Result<Option<Invite>, RestError>),
    Join(String),
    Joined(u64, String, Result<Option<Invite>, RestError>),
    /// Right click on a server in the rail.
    AskLeave(Snowflake),
    ConfirmLeave,
    CancelLeave,
    Left(u64, Snowflake, Result<(), RestError>),
    /// Open the invite panel for this channel (`guild`, `channel`).
    Create(Snowflake, Snowflake),
    /// Choose a lifetime / use limit (index into the option lists).
    Age(usize),
    Uses(usize),
    /// Send the create request with the chosen options.
    Make,
    Created(u64, Result<String, RestError>),
    CopyLink,
    DismissCreated,
    /// List the invites of a server or channel the account may manage.
    OpenManage(InviteScope),
    Listed(u64, InviteScope, Result<Vec<ManagedInvite>, RestError>),
    CopyCode(String),
    AskRevoke(String),
    CancelRevoke(String),
    ConfirmRevoke(String),
    Revoked(u64, String, Result<(), RestError>),
    CloseManage,
    /// A pasted or launched invite link: look it up and show it, never join it.
    Open(String),
    CloseOpened,
}

#[derive(Debug)]
struct Leave {
    guild: Snowflake,
    name: String,
    owner: bool,
    busy: bool,
}

#[derive(Debug)]
enum Creating {
    Choose(Choose),
    Busy,
    /// The created code and what it allows.
    Link(String, String),
    Failed(String),
}

#[derive(Debug)]
struct Choose {
    guild: Snowflake,
    channel: Snowflake,
    can_create: bool,
    /// Where the account may manage invites from this channel, if anywhere.
    manage: Option<InviteScope>,
    age: usize,
    uses: usize,
}

/// The seven-day, unlimited default of the options.
const DEFAULT_AGE: usize = 5;

fn uses_name(uses: u8) -> String {
    match uses {
        0 => "no limit".to_owned(),
        1 => "1 use".to_owned(),
        n => format!("{n} uses"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RowState {
    Idle,
    /// Waiting for the explicit Confirm.
    Confirm,
    Revoking,
    Failed(String),
}

#[derive(Debug)]
struct Manage {
    scope: InviteScope,
    /// None while loading.
    rows: Option<Vec<(ManagedInvite, RowState)>>,
    error: Option<String>,
}

fn is_gone(error: &RestError) -> bool {
    match error {
        RestError::ResourceGone => true,
        RestError::Refused(refusal) => refusal.status.as_u16() == 404,
        _ => false,
    }
}

/// What a failed list or revoke request tells the user.
fn describe_manage(error: RestError) -> String {
    match error {
        RestError::PermissionDenied => "Discord refused: managing invites needs Manage Server (all invites) or Manage Channel (this channel only).".to_owned(),
        RestError::Refused(refusal) if refusal.status.as_u16() == 403 => describe_manage(RestError::PermissionDenied),
        RestError::Retryable(_) => "Discord did not confirm. The change may have been applied; reopen the list before trying again.".to_owned(),
        other => describe(other),
    }
}

impl Manage {
    fn new(scope: InviteScope) -> Self {
        Self {
            scope,
            rows: None,
            error: None,
        }
    }

    fn finish_list(&mut self, result: Result<Vec<ManagedInvite>, RestError>) {
        match result {
            Ok(list) => {
                self.rows = Some(list.into_iter().map(|i| (i, RowState::Idle)).collect());
            }
            Err(error) => self.error = Some(describe_manage(error)),
        }
    }

    fn row(&mut self, code: &str) -> Option<&mut RowState> {
        let rows = self.rows.as_mut()?;
        rows.iter_mut()
            .find(|(invite, _)| invite.code == code)
            .map(|(_, state)| state)
    }

    fn ask(&mut self, code: &str) {
        if let Some(rows) = &mut self.rows {
            for (invite, state) in rows {
                if invite.code == code && *state != RowState::Revoking {
                    *state = RowState::Confirm;
                } else if *state == RowState::Confirm {
                    *state = RowState::Idle;
                }
            }
        }
    }

    fn cancel(&mut self, code: &str) {
        if let Some(state) = self.row(code).filter(|s| **s == RowState::Confirm) {
            *state = RowState::Idle;
        }
    }

    /// Marks the revoke as running; false unless it was confirmed and is not
    /// already in flight.
    fn begin_revoke(&mut self, code: &str) -> bool {
        match self.row(code) {
            Some(state) if *state == RowState::Confirm => {
                *state = RowState::Revoking;
                true
            }
            _ => false,
        }
    }

    fn finish_revoke(&mut self, code: &str, result: Result<(), RestError>) {
        match result {
            Ok(()) => self.remove(code),
            Err(error) if is_gone(&error) => self.remove(code),
            Err(error) => {
                if let Some(state) = self.row(code) {
                    *state = RowState::Failed(describe_manage(error));
                }
            }
        }
    }

    fn remove(&mut self, code: &str) {
        if let Some(rows) = &mut self.rows {
            rows.retain(|(invite, _)| invite.code != code);
        }
    }
}

/// How long until an invite stops working, at `now` (unix seconds).
pub fn remaining(expiry: Expiry, now: i64) -> String {
    match expiry {
        Expiry::Never => "never expires".to_owned(),
        Expiry::Unknown => "expiry unknown".to_owned(),
        Expiry::At(at) if at <= now => "expired".to_owned(),
        Expiry::At(at) => {
            let secs = at - now;
            let (days, hours, minutes) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60);
            if days > 0 {
                format!("expires in {days}d {hours}h")
            } else if hours > 0 {
                format!("expires in {hours}h {minutes}m")
            } else {
                format!("expires in {}m", minutes.max(1))
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct State {
    pub cache: Cache,
    leave: Option<Leave>,
    creating: Option<Creating>,
    manage: Option<Manage>,
    /// The invite of a link the user opened (deep link or switcher paste).
    opened: Option<String>,
    /// What the create request in flight allows, shown with its link.
    made: Option<String>,
    notice: Option<String>,
    generation: u64,
}

fn link(code: &str) -> String {
    format!("https://discord.gg/{code}")
}

impl State {
    /// Logout or account change: drops every card, panel and request.
    pub fn clear(&mut self) {
        *self = Self {
            generation: self.generation.wrapping_add(1),
            ..Self::default()
        };
    }

    /// Looks up the visible codes that need it. Called when the timeline or
    /// the open channel changes and after each answer.
    pub fn pump(&mut self, rows: &[timeline::Row], client: &RestClient) -> Task<Event> {
        let codes = visible_codes(rows);
        let start = self.cache.want(&codes, Instant::now());
        let tasks: Vec<_> = start
            .into_iter()
            .map(|code| {
                let generation = self.generation;
                let client = client.clone();
                let ask = code.clone();
                Task::perform(
                    async move { client.lookup_invite(&ask).await },
                    move |result| Event::Resolved(generation, code.clone(), result),
                )
            })
            .collect();
        Task::batch(tasks)
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        rows: &[timeline::Row],
        client: &RestClient,
    ) -> Task<Event> {
        match event {
            Event::Resolved(generation, code, result) if generation == self.generation => {
                self.cache.finish(&code, result, Instant::now());
                return self.pump(rows, client);
            }
            Event::Joined(generation, code, result) if generation == self.generation => {
                let done = result.map(drop);
                self.cache.finish_join(&code, done, Instant::now());
            }
            Event::Left(generation, guild, result) if generation == self.generation => {
                if self.leave.as_ref().is_some_and(|l| l.guild == guild) {
                    let name = self.leave.take().map(|l| l.name).unwrap_or_default();
                    self.notice = Some(match result {
                        Ok(()) => format!("You left {name}."),
                        Err(error) => describe(error),
                    });
                }
            }
            Event::Created(generation, result) if generation == self.generation => {
                self.creating = Some(match result {
                    Ok(code) => Creating::Link(code, self.made.take().unwrap_or_default()),
                    Err(error) => Creating::Failed(describe(error)),
                });
            }
            Event::Listed(generation, scope, result) if generation == self.generation => {
                if let Some(manage) = self.manage.as_mut().filter(|m| m.scope == scope) {
                    manage.finish_list(result);
                }
            }
            Event::Revoked(generation, code, result) if generation == self.generation => {
                if let Some(manage) = &mut self.manage {
                    manage.finish_revoke(&code, result);
                }
            }
            Event::Resolved(..)
            | Event::Joined(..)
            | Event::Left(..)
            | Event::Created(..)
            | Event::Listed(..)
            | Event::Revoked(..) => {}
            Event::Open(code) => {
                let start = self.cache.want(std::slice::from_ref(&code), Instant::now());
                self.opened = Some(code);
                let generation = self.generation;
                return Task::batch(start.into_iter().map(|code| {
                    let client = client.clone();
                    let ask = code.clone();
                    Task::perform(
                        async move { client.lookup_invite(&ask).await },
                        move |result| Event::Resolved(generation, code.clone(), result),
                    )
                }));
            }
            Event::CloseOpened => self.opened = None,
            Event::Join(code) => {
                if self.cache.begin_join(&code) {
                    let generation = self.generation;
                    let client = client.clone();
                    let ask = code.clone();
                    return Task::perform(
                        async move { client.join_invite(&ask).await },
                        move |result| Event::Joined(generation, code.clone(), result),
                    );
                }
            }
            Event::AskLeave(guild) => {
                if self.leave.as_ref().is_some_and(|l| l.busy) {
                    return Task::none();
                }
                let row = navigation.guilds.rows.iter().find(|g| g.id == guild);
                self.notice = None;
                self.leave = row.map(|row| Leave {
                    guild,
                    name: row.name.clone(),
                    owner: row.owned,
                    busy: false,
                });
            }
            Event::CancelLeave => {
                if self.leave.as_ref().is_some_and(|l| !l.busy) {
                    self.leave = None;
                }
            }
            Event::ConfirmLeave => {
                let Some(leave) = self.leave.as_mut().filter(|l| !l.busy) else {
                    return Task::none();
                };
                // The owner check is made again at the click, against the
                // reducer's current view of the server.
                let owner = navigation
                    .guilds
                    .rows
                    .iter()
                    .any(|g| g.id == leave.guild && g.owned);
                if leave.owner || owner {
                    leave.owner = true;
                    return Task::none();
                }
                leave.busy = true;
                let (generation, guild) = (self.generation, leave.guild);
                let client = client.clone();
                return Task::perform(
                    async move { client.leave_guild(guild).await },
                    move |result| Event::Left(generation, guild, result),
                );
            }
            Event::Create(guild, channel) => {
                let permissions = navigation
                    .selection
                    .as_ref()
                    .filter(|s| s.guild_id == guild && s.channel_id == channel)
                    .map(|s| s.permissions);
                let Some(permissions) = permissions else {
                    return Task::none();
                };
                let manage = if permissions.manage_guild {
                    Some(InviteScope::Guild(guild))
                } else {
                    permissions
                        .manage_channel
                        .then_some(InviteScope::Channel(channel))
                };
                if !(permissions.create_invite || manage.is_some())
                    || matches!(self.creating, Some(Creating::Busy))
                {
                    return Task::none();
                }
                self.creating = Some(Creating::Choose(Choose {
                    guild,
                    channel,
                    can_create: permissions.create_invite,
                    manage,
                    age: DEFAULT_AGE,
                    uses: 0,
                }));
            }
            Event::Age(n) => {
                if let Some(Creating::Choose(choose)) = &mut self.creating {
                    choose.age = n.min(CREATE_AGES.len() - 1);
                }
            }
            Event::Uses(n) => {
                if let Some(Creating::Choose(choose)) = &mut self.creating {
                    choose.uses = n.min(CREATE_USES.len() - 1);
                }
            }
            Event::Make => {
                let Some(Creating::Choose(choose)) = &self.creating else {
                    return Task::none();
                };
                // The permission is checked again against the reducer's view.
                let allowed = choose.can_create
                    && navigation.selection.as_ref().is_some_and(|s| {
                        s.guild_id == choose.guild
                            && s.channel_id == choose.channel
                            && s.permissions.create_invite
                    });
                if !allowed {
                    return Task::none();
                }
                let (channel, (max_age, age_name), max_uses) = (
                    choose.channel,
                    CREATE_AGES[choose.age],
                    CREATE_USES[choose.uses],
                );
                self.made = Some(format!(
                    "valid {}, {}",
                    if max_age == 0 {
                        "forever".to_owned()
                    } else {
                        format!("for {age_name}")
                    },
                    uses_name(max_uses)
                ));
                self.creating = Some(Creating::Busy);
                let generation = self.generation;
                let client = client.clone();
                return Task::perform(
                    async move { client.create_invite(channel, max_age, max_uses).await },
                    move |result| Event::Created(generation, result),
                );
            }
            Event::CopyLink => {
                if let Some(Creating::Link(code, _)) = &self.creating {
                    return iced::clipboard::write(link(code));
                }
            }
            Event::OpenManage(scope) => {
                let allowed = match &self.creating {
                    Some(Creating::Choose(choose)) => choose.manage == Some(scope),
                    _ => false,
                };
                if !allowed {
                    return Task::none();
                }
                self.creating = None;
                self.manage = Some(Manage::new(scope));
                let generation = self.generation;
                let client = client.clone();
                return Task::perform(
                    async move { client.list_invites(scope).await },
                    move |result| Event::Listed(generation, scope, result),
                );
            }
            Event::CopyCode(code) => return iced::clipboard::write(link(&code)),
            Event::AskRevoke(code) => {
                if let Some(manage) = &mut self.manage {
                    manage.ask(&code);
                }
            }
            Event::CancelRevoke(code) => {
                if let Some(manage) = &mut self.manage {
                    manage.cancel(&code);
                }
            }
            Event::ConfirmRevoke(code) => {
                if self.manage.as_mut().is_some_and(|m| m.begin_revoke(&code)) {
                    let generation = self.generation;
                    let client = client.clone();
                    let ask = code.clone();
                    return Task::perform(
                        async move { client.revoke_invite(&ask).await },
                        move |result| Event::Revoked(generation, code.clone(), result),
                    );
                }
            }
            Event::CloseManage => self.manage = None,
            Event::DismissCreated => {
                if !matches!(self.creating, Some(Creating::Busy)) {
                    self.creating = None;
                }
            }
        }
        Task::none()
    }

    /// A 401 on any membership request stops the account like everywhere else.
    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Resolved(_, _, Err(RestError::AuthenticationRequired))
                | Event::Joined(_, _, Err(RestError::AuthenticationRequired))
                | Event::Left(_, _, Err(RestError::AuthenticationRequired))
                | Event::Created(_, Err(RestError::AuthenticationRequired))
                | Event::Listed(_, _, Err(RestError::AuthenticationRequired))
                | Event::Revoked(_, _, Err(RestError::AuthenticationRequired))
        )
    }
}

/// One line per listed invite: facts, Copy, and Revoke with its Confirm.
fn manage_view(manage: &Manage, now: i64) -> Element<'_, Event> {
    let scope = match manage.scope {
        InviteScope::Guild(_) => "Invites of this server",
        InviteScope::Channel(_) => "Invites of this channel",
    };
    let mut list = column![].spacing(6);
    match (&manage.rows, &manage.error) {
        (_, Some(error)) => list = list.push(text(error.clone()).size(12).style(text::danger)),
        (None, None) => list = list.push(text("Loading\u{2026}").size(12)),
        (Some(rows), None) if rows.is_empty() => {
            list = list.push(text("No invites.").size(12));
        }
        (Some(rows), None) => {
            for (invite, state) in rows {
                let mut facts = vec![format!("#{}", invite.channel_name)];
                if !invite.inviter.is_empty() {
                    facts.push(format!("by {}", invite.inviter));
                }
                facts.push(if invite.max_uses == 0 {
                    format!("{} uses", invite.uses)
                } else {
                    format!("{}/{} uses", invite.uses, invite.max_uses)
                });
                facts.push(remaining(invite.expiry, now));
                if invite.temporary {
                    facts.push("temporary".to_owned());
                }
                let code = invite.code.clone();
                let actions = match state {
                    RowState::Confirm => row![
                        button(text("Confirm revoke").size(12))
                            .on_press(Event::ConfirmRevoke(code.clone()))
                            .style(button::danger),
                        button(text("Cancel").size(12)).on_press(Event::CancelRevoke(code)),
                    ],
                    RowState::Revoking => row![button(text("Revoking\u{2026}").size(12))],
                    _ => row![
                        button(text("Copy").size(12)).on_press(Event::CopyCode(code.clone())),
                        button(text("Revoke").size(12)).on_press(Event::AskRevoke(code)),
                    ],
                }
                .spacing(6);
                let mut item = column![
                    row![
                        column![
                            text(invite.code.clone()).size(13),
                            text(facts.join(" \u{b7} "))
                                .size(12)
                                .style(text::secondary)
                                .wrapping(text::Wrapping::None),
                        ]
                        .width(Length::Fill),
                        actions,
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                ];
                if let RowState::Failed(reason) = state {
                    item = item.push(text(reason.clone()).size(12).style(text::danger));
                }
                list = list.push(item);
            }
            if rows.len() >= fastcord_discord::MAX_LISTED {
                list = list.push(
                    text(format!(
                        "Showing the first {}.",
                        fastcord_discord::MAX_LISTED
                    ))
                    .size(12)
                    .style(text::secondary),
                );
            }
        }
    }
    column![
        row![
            text(scope).size(13).width(Length::Fill),
            crate::icons::close(Some(Event::CloseManage)),
        ],
        scrollable(list).height(Length::Fixed(240.0)),
    ]
    .spacing(6)
    .max_width(720)
    .into()
}

/// The invite of an opened link: what it is and an explicit Join.
fn opened_view<'a>(cache: &'a Cache, code: &str) -> Element<'a, Event> {
    let (title, detail, join) = match cache.get(code) {
        Some(
            entry @ Entry {
                card: Card::Ready(invite),
                join,
                ..
            },
        ) => {
            let detail = match join {
                Join::Refused(reason) => reason.clone(),
                _ => counts_line(invite),
            };
            let action = if cache.is_member(entry) {
                ("Joined", None)
            } else if *join == Join::Joining {
                ("Joining\u{2026}", None)
            } else {
                ("Join", Some(Event::Join(code.to_owned())))
            };
            (invite.guild_name.clone(), detail, Some(action))
        }
        Some(Entry {
            card: Card::Invalid,
            ..
        }) => (
            "Invite invalid".to_owned(),
            "This invite has expired or does not exist.".to_owned(),
            None,
        ),
        Some(Entry {
            card: Card::Failed, ..
        }) => (
            "Invite".to_owned(),
            "Discord did not answer. Open the link again to retry.".to_owned(),
            None,
        ),
        _ => ("Invite".to_owned(), "Loading\u{2026}".to_owned(), None),
    };
    let mut line = row![
        column![
            text(title).size(14),
            text(detail).size(12).style(text::secondary)
        ]
        .width(Length::Fill),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);
    if let Some((label, press)) = join {
        line = line.push(
            button(text(label).size(13))
                .on_press_maybe(press)
                .style(button::success),
        );
    }
    line.push(crate::icons::close(Some(Event::CloseOpened)))
        .into()
}

/// The leave confirmation, the created link and the last result, as panels
/// above the conversation.
pub fn view(state: &State, now: i64) -> Option<Element<'_, Event>> {
    let mut panel = column![].spacing(6);
    let mut any = false;
    if let Some(leave) = &state.leave {
        any = true;
        panel = panel.push(if leave.owner {
            column![
                text(format!("You own {}, so you cannot leave it.", leave.name)).size(13),
                text(
                    "Transfer ownership or delete the server in the official Discord client first."
                )
                .size(12)
                .style(text::secondary),
                crate::icons::close(Some(Event::CancelLeave)),
            ]
            .spacing(6)
        } else {
            column![
                text(format!(
                    "Leave {}? You will need a new invite to join again.",
                    leave.name
                ))
                .size(13),
                row![
                    button(
                        text(if leave.busy {
                            "Leaving\u{2026}"
                        } else {
                            "Leave server"
                        })
                        .size(13)
                    )
                    .on_press_maybe((!leave.busy).then_some(Event::ConfirmLeave))
                    .style(button::danger),
                    button(text("Cancel").size(13))
                        .on_press_maybe((!leave.busy).then_some(Event::CancelLeave)),
                ]
                .spacing(6),
            ]
            .spacing(6)
        });
    }
    match &state.creating {
        Some(Creating::Choose(choose)) => {
            any = true;
            let ages: Vec<&str> = CREATE_AGES.iter().map(|(_, name)| *name).collect();
            let uses: Vec<String> = CREATE_USES.iter().map(|n| uses_name(*n)).collect();
            let mut line = row![].spacing(6).align_y(iced::Alignment::Center);
            if choose.can_create {
                line = line
                    .push(text("Expires after").size(12))
                    .push(
                        pick_list(ages.clone(), Some(ages[choose.age]), move |name| {
                            Event::Age(ages.iter().position(|a| *a == name).unwrap_or(0))
                        })
                        .text_size(13),
                    )
                    .push(text("Max uses").size(12))
                    .push(
                        pick_list(uses.clone(), Some(uses[choose.uses].clone()), move |name| {
                            Event::Uses(uses.iter().position(|u| *u == name).unwrap_or(0))
                        })
                        .text_size(13),
                    )
                    .push(button(text("Create link").size(13)).on_press(Event::Make));
            }
            if let Some(scope) = choose.manage {
                line = line.push(
                    button(text("Manage invites").size(13)).on_press(Event::OpenManage(scope)),
                );
            }
            panel = panel.push(
                row![
                    line.width(Length::Fill),
                    crate::icons::close(Some(Event::DismissCreated)),
                ]
                .spacing(6),
            );
        }
        Some(Creating::Busy) => {
            any = true;
            panel = panel.push(text("Creating an invite\u{2026}").size(13));
        }
        Some(Creating::Link(code, detail)) => {
            any = true;
            panel = panel.push(
                column![
                    text(format!("Invite link ({detail})")).size(12),
                    row![
                        text(link(code)).size(14).width(Length::Fill),
                        button(text("Copy").size(13)).on_press(Event::CopyLink),
                        crate::icons::close(Some(Event::DismissCreated)),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                ]
                .spacing(4),
            );
        }
        Some(Creating::Failed(reason)) => {
            any = true;
            panel = panel.push(
                row![
                    text(reason.clone())
                        .size(12)
                        .style(text::danger)
                        .width(Length::Fill),
                    crate::icons::close(Some(Event::DismissCreated)),
                ]
                .spacing(6),
            );
        }
        None => {}
    }
    if let Some(code) = &state.opened {
        any = true;
        panel = panel.push(opened_view(&state.cache, code));
    }
    if let Some(manage) = &state.manage {
        any = true;
        panel = panel.push(manage_view(manage, now));
    }
    if let Some(notice) = &state.notice
        && state.leave.is_none()
    {
        any = true;
        panel = panel.push(text(notice.clone()).size(12).style(text::secondary));
    }
    any.then(|| container(panel).padding([6, 0]).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::Refusal;
    use fastcord_discord::StatusCode;

    fn invite(guild: u64) -> Invite {
        Invite {
            code: "x".into(),
            guild: Snowflake(guild),
            guild_name: "Test".into(),
            icon: None,
            channel_name: "general".into(),
            members: Some(1200),
            online: Some(34),
            expires_at: None,
        }
    }

    fn codes(range: std::ops::Range<u32>) -> Vec<String> {
        range.map(|n| format!("code{n:03}")).collect()
    }

    #[test]
    fn lookups_are_bounded_in_flight_and_never_repeated() {
        let now = Instant::now();
        let mut cache = Cache::default();
        let wanted = codes(0..10);
        assert_eq!(cache.want(&wanted, now).len(), MAX_IN_FLIGHT);
        // Nothing more starts while three are in flight, and none twice.
        assert!(cache.want(&wanted, now).is_empty());
        cache.finish("code000", Ok(Some(invite(1))), now);
        assert_eq!(cache.want(&wanted, now), ["code003"]);
        assert!(matches!(cache.get("code000").unwrap().card, Card::Ready(_)));
    }

    #[test]
    fn the_cache_is_bounded_and_entries_expire() {
        let now = Instant::now();
        let mut cache = Cache::default();
        // Many passes over many distinct codes never exceed the bound.
        for pass in 0..40u32 {
            let wanted = codes(pass * MAX_WANTED as u32..(pass + 1) * MAX_WANTED as u32);
            loop {
                let started = cache.want(&wanted, now);
                if started.is_empty() {
                    break;
                }
                for code in started {
                    cache.finish(&code, Err(RestError::ResourceGone), now);
                }
            }
            assert!(cache.len() <= MAX_ENTRIES);
        }
        // An answer is believed for the TTL, then asked for again.
        let mut cache = Cache::default();
        let one = vec!["abcd".to_owned()];
        assert_eq!(cache.want(&one, now), ["abcd"]);
        cache.finish("abcd", Ok(Some(invite(1))), now);
        assert!(
            cache
                .want(&one, now + TTL - Duration::from_secs(1))
                .is_empty()
        );
        assert_eq!(cache.want(&one, now + TTL), ["abcd"]);
        // A failure is retried sooner, but not at once.
        cache.finish(
            "abcd",
            Err(RestError::Retryable(
                fastcord_discord::RetryableFailure::Server(StatusCode::BAD_GATEWAY),
            )),
            now,
        );
        assert_eq!(cache.get("abcd").unwrap().card, Card::Failed);
        assert!(cache.want(&one, now + Duration::from_secs(5)).is_empty());
        assert_eq!(cache.want(&one, now + FAILED_TTL), ["abcd"]);
    }

    #[test]
    fn expired_and_unknown_invites_are_invalid_not_failed() {
        let now = Instant::now();
        let mut cache = Cache::default();
        let wanted = codes(0..3);
        cache.want(&wanted, now);
        cache.finish("code000", Err(RestError::ResourceGone), now);
        cache.finish(
            "code001",
            Err(RestError::Refused(Refusal::new(
                StatusCode::NOT_FOUND,
                Some(10006),
                "Unknown Invite",
            ))),
            now,
        );
        cache.finish("code002", Ok(None), now);
        for code in &wanted {
            assert_eq!(cache.get(code).unwrap().card, Card::Invalid);
        }
    }

    #[test]
    fn joining_is_one_click_one_request_and_refusals_keep_their_reason() {
        let now = Instant::now();
        let mut cache = Cache::default();
        let one = vec!["abcd".to_owned()];
        assert!(!cache.begin_join("abcd"));
        cache.want(&one, now);
        assert!(!cache.begin_join("abcd"), "still loading");
        cache.finish("abcd", Ok(Some(invite(7))), now);
        assert!(cache.begin_join("abcd"));
        assert!(!cache.begin_join("abcd"), "a second click while joining");
        cache.finish_join(
            "abcd",
            Err(RestError::Refused(Refusal::new(
                StatusCode::FORBIDDEN,
                Some(40007),
                "You are banned from this guild.",
            ))),
            now,
        );
        assert_eq!(
            cache.get("abcd").unwrap().join,
            Join::Refused("Discord refused: You are banned from this guild.".to_owned())
        );
        // An explicit new click may try again; success becomes Joined.
        assert!(cache.begin_join("abcd"));
        cache.finish_join("abcd", Ok(()), now);
        assert!(cache.is_member(cache.get("abcd").unwrap()));
        assert!(!cache.begin_join("abcd"));
    }

    #[test]
    fn a_verification_pause_is_shown_as_such_and_is_not_retried() {
        let now = Instant::now();
        let mut cache = Cache::default();
        let one = vec!["abcd".to_owned()];
        cache.want(&one, now);
        cache.finish("abcd", Ok(Some(invite(7))), now);
        assert!(cache.begin_join("abcd"));
        cache.finish_join("abcd", Err(RestError::AccountVerificationRequired), now);
        let Join::Refused(reason) = &cache.get("abcd").unwrap().join else {
            panic!("not refused");
        };
        assert!(reason.contains("official Discord client"), "{reason}");
        assert!(reason.contains("paused"));
        assert!(!reason.is_empty());
        // The card stays; no lookup is repeated by the pause.
        assert!(cache.want(&one, now).is_empty());
    }

    #[test]
    fn a_server_the_account_is_in_reads_as_joined() {
        let now = Instant::now();
        let mut cache = Cache::default();
        let one = vec!["abcd".to_owned()];
        cache.want(&one, now);
        cache.finish("abcd", Ok(Some(invite(9))), now);
        assert!(!cache.is_member(cache.get("abcd").unwrap()));
        cache.member = vec![Snowflake(9)];
        assert!(cache.is_member(cache.get("abcd").unwrap()));
    }

    #[test]
    fn counts_read_like_the_official_card() {
        assert_eq!(count(7), "7");
        assert_eq!(count(1200), "1,200");
        assert_eq!(count(1_234_567), "1,234,567");
        let mut i = invite(1);
        assert_eq!(
            counts_line(&i),
            "#general \u{b7} 34 online \u{b7} 1,200 members"
        );
        i.online = None;
        i.members = None;
        i.expires_at = Some("2030-01-02T03:04:05+00:00".into());
        assert_eq!(counts_line(&i), "#general \u{b7} expires 2030-01-02");
    }

    fn managed(code: &str) -> ManagedInvite {
        ManagedInvite {
            code: code.to_owned(),
            channel_name: "general".into(),
            inviter: "Alice".into(),
            uses: 3,
            max_uses: 10,
            expiry: Expiry::Never,
            temporary: false,
        }
    }

    fn listed(codes: &[&str]) -> Manage {
        let mut manage = Manage::new(InviteScope::Guild(Snowflake(1)));
        manage.finish_list(Ok(codes.iter().map(|c| managed(c)).collect()));
        manage
    }

    fn codes_of(manage: &Manage) -> Vec<String> {
        let rows = manage.rows.as_ref().unwrap();
        rows.iter().map(|(i, _)| i.code.clone()).collect()
    }

    #[test]
    fn revoking_needs_a_confirm_and_is_sent_once() {
        let mut manage = listed(&["aaaa", "bbbb"]);
        assert!(!manage.begin_revoke("aaaa"), "no request before Confirm");
        manage.ask("aaaa");
        manage.ask("bbbb");
        assert_eq!(*manage.row("aaaa").unwrap(), RowState::Idle, "one prompt");
        manage.cancel("bbbb");
        assert!(!manage.begin_revoke("bbbb"));
        manage.ask("aaaa");
        assert!(manage.begin_revoke("aaaa"));
        assert!(!manage.begin_revoke("aaaa"), "not twice while in flight");
        manage.ask("aaaa");
        assert_eq!(*manage.row("aaaa").unwrap(), RowState::Revoking);
        assert!(!manage.begin_revoke("zzzz"));
    }

    #[test]
    fn a_revoke_removes_the_row_on_success_or_unknown_invite_only() {
        for gone in [
            Ok(()),
            Err(RestError::ResourceGone),
            Err(RestError::Refused(Refusal::new(
                StatusCode::NOT_FOUND,
                Some(10006),
                "Unknown Invite",
            ))),
        ] {
            let mut manage = listed(&["aaaa", "bbbb"]);
            manage.ask("aaaa");
            assert!(manage.begin_revoke("aaaa"));
            manage.finish_revoke("aaaa", gone);
            assert_eq!(codes_of(&manage), ["bbbb"]);
        }
        let failures = [
            Err(RestError::PermissionDenied),
            Err(RestError::Refused(Refusal::new(
                StatusCode::FORBIDDEN,
                Some(50013),
                "Missing Permissions",
            ))),
            Err(RestError::Retryable(
                fastcord_discord::RetryableFailure::Server(StatusCode::BAD_GATEWAY),
            )),
            Err(RestError::Http(StatusCode::TOO_MANY_REQUESTS)),
        ];
        for failure in failures {
            let mut manage = listed(&["aaaa"]);
            manage.ask("aaaa");
            assert!(manage.begin_revoke("aaaa"));
            manage.finish_revoke("aaaa", failure);
            // The row stays with a note, and nothing retries by itself: a new
            // request needs a new Confirm.
            assert_eq!(codes_of(&manage), ["aaaa"]);
            assert!(matches!(manage.row("aaaa"), Some(RowState::Failed(note)) if !note.is_empty()));
            assert!(!manage.begin_revoke("aaaa"));
            manage.ask("aaaa");
            assert!(manage.begin_revoke("aaaa"));
        }
    }

    #[test]
    fn a_refused_list_shows_why_and_a_missing_permission_names_it() {
        let mut manage = Manage::new(InviteScope::Channel(Snowflake(2)));
        manage.finish_list(Err(RestError::PermissionDenied));
        assert!(manage.rows.is_none());
        assert!(manage.error.as_deref().unwrap().contains("Manage Server"));
        let mut manage = Manage::new(InviteScope::Channel(Snowflake(2)));
        manage.finish_list(Ok(Vec::new()));
        assert_eq!(manage.rows.as_ref().map(Vec::len), Some(0));
    }

    #[test]
    fn expiry_reads_from_a_fixed_clock() {
        let now = 1_000_000;
        assert_eq!(remaining(Expiry::Never, now), "never expires");
        assert_eq!(remaining(Expiry::Unknown, now), "expiry unknown");
        assert_eq!(remaining(Expiry::At(now), now), "expired");
        assert_eq!(remaining(Expiry::At(now - 5), now), "expired");
        assert_eq!(remaining(Expiry::At(now + 20), now), "expires in 1m");
        assert_eq!(remaining(Expiry::At(now + 59 * 60), now), "expires in 59m");
        assert_eq!(
            remaining(Expiry::At(now + 3_600 + 120), now),
            "expires in 1h 2m"
        );
        assert_eq!(
            remaining(Expiry::At(now + 2 * 86_400 + 3 * 3_600 + 59), now),
            "expires in 2d 3h"
        );
    }

    #[test]
    fn creating_offers_the_listed_options_and_gates_on_permissions() {
        use fastcord_discord::state::navigation::{
            ChannelKind, ChannelSelection, PermissionSummary,
        };
        assert_eq!(CREATE_AGES[DEFAULT_AGE].0, 604_800);
        assert_eq!(CREATE_AGES.last().unwrap().0, 0, "never");
        assert_eq!(uses_name(0), "no limit");
        assert_eq!(uses_name(1), "1 use");
        assert_eq!(uses_name(25), "25 uses");
        let selection = |permissions| NavigationSnapshot {
            selection: Some(ChannelSelection {
                guild_id: Snowflake(1),
                channel_id: Snowflake(2),
                name: "general".into(),
                kind: ChannelKind::Text,
                permissions,
                acked: None,
                unread: false,
                mentions: 0,
                thread: None,
            }),
            ..NavigationSnapshot::default()
        };
        let client = fastcord_discord::RestClient::new(
            fastcord_net::NetworkContext::off(),
            fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
            fastcord_discord::gateway::ClientProperties::web(
                fastcord_discord::gateway::HostOs::Linux,
                "en-US",
                1,
            ),
        )
        .unwrap();
        let ask = |permissions| {
            let mut state = State::default();
            let _ = state.update(
                Event::Create(Snowflake(1), Snowflake(2)),
                &selection(permissions),
                &[],
                &client,
            );
            state
        };
        // No permission: no panel, and the manage entry cannot be forced.
        let mut state = ask(PermissionSummary::default());
        assert!(state.creating.is_none());
        let nav = selection(PermissionSummary::default());
        let _ = state.update(
            Event::OpenManage(InviteScope::Guild(Snowflake(1))),
            &nav,
            &[],
            &client,
        );
        assert!(state.manage.is_none());
        // Create only: no manage entry.
        let state = ask(PermissionSummary {
            create_invite: true,
            ..PermissionSummary::default()
        });
        assert!(matches!(&state.creating, Some(Creating::Choose(c)) if c.manage.is_none()));
        // Manage Channel alone lists that channel; Manage Server lists the server.
        let state = ask(PermissionSummary {
            manage_channel: true,
            ..PermissionSummary::default()
        });
        assert!(matches!(&state.creating,
            Some(Creating::Choose(c)) if c.manage == Some(InviteScope::Channel(Snowflake(2))) && !c.can_create));
        let state = ask(PermissionSummary {
            manage_guild: true,
            manage_channel: true,
            create_invite: true,
            ..PermissionSummary::default()
        });
        assert!(matches!(&state.creating,
            Some(Creating::Choose(c)) if c.manage == Some(InviteScope::Guild(Snowflake(1))) && c.can_create));
        // A stale answer from an earlier session changes nothing.
        let mut state = ask(PermissionSummary {
            manage_guild: true,
            ..PermissionSummary::default()
        });
        state.manage = Some(Manage::new(InviteScope::Guild(Snowflake(1))));
        let before = state.generation;
        state.clear();
        let _ = state.update(
            Event::Listed(before, InviteScope::Guild(Snowflake(1)), Ok(Vec::new())),
            &nav,
            &[],
            &client,
        );
        assert!(state.manage.is_none());
    }

    #[test]
    fn the_leave_owner_check_uses_the_servers_owned_flag() {
        use fastcord_discord::state::navigation::{GuildRow, Window};
        let row = |id, owned| GuildRow {
            id: Snowflake(id),
            name: format!("S{id}"),
            icon: None,
            available: true,
            owned,
            selected: false,
            unread: false,
            mentions: 0,
            folder: None,
        };
        let navigation = NavigationSnapshot {
            guilds: Window {
                offset: 0,
                total: 2,
                rows: vec![row(1, true), row(2, false)],
            },
            ..NavigationSnapshot::default()
        };
        let client = fastcord_discord::RestClient::new(
            fastcord_net::NetworkContext::off(),
            fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
            fastcord_discord::gateway::ClientProperties::web(
                fastcord_discord::gateway::HostOs::Linux,
                "en-US",
                1,
            ),
        )
        .unwrap();
        let mut state = State::default();
        let _ = state.update(Event::AskLeave(Snowflake(1)), &navigation, &[], &client);
        let _ = state.update(Event::ConfirmLeave, &navigation, &[], &client);
        let leave = state.leave.as_ref().unwrap();
        assert!(
            leave.owner && !leave.busy,
            "the owner is told why, nothing is sent"
        );
        assert!(fastcord_discord::owner_cannot_leave(
            Some(Snowflake(5)),
            Snowflake(5)
        ));
        let _ = state.update(Event::AskLeave(Snowflake(2)), &navigation, &[], &client);
        assert!(!state.leave.as_ref().unwrap().owner);
        // A stale answer from an earlier session changes nothing.
        let before = state.generation;
        state.clear();
        let _ = state.update(
            Event::Left(before, Snowflake(2), Ok(())),
            &navigation,
            &[],
            &client,
        );
        assert!(state.notice.is_none());
    }

    #[test]
    fn an_opened_invite_is_looked_up_but_never_joined() {
        let client = fastcord_discord::RestClient::new(
            fastcord_net::NetworkContext::off(),
            fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
            fastcord_discord::gateway::ClientProperties::web(
                fastcord_discord::gateway::HostOs::Linux,
                "en-US",
                1,
            ),
        )
        .unwrap();
        let navigation = NavigationSnapshot::default();
        let mut state = State::default();
        let _ = state.update(Event::Open("abcd".into()), &navigation, &[], &client);
        assert_eq!(state.opened.as_deref(), Some("abcd"));
        let entry = state.cache.get("abcd").unwrap();
        assert_eq!((&entry.card, &entry.join), (&Card::Loading, &Join::Idle));
        let _ = state.update(Event::CloseOpened, &navigation, &[], &client);
        assert_eq!(state.opened, None);
    }
}
