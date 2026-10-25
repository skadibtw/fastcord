//! Server-settings "Bans" and "Audit log" panels (SPEC §16, issue #95).
//!
//! Both are read when the user opens them (nothing is polled or kept after the
//! panel closes) and need the permission the reducer's `GuildAdmin` read model
//! reports: Ban Members for the ban list, View Audit Log for the log. Bans are
//! listed 50 at a time up to [`MAX_BANS`] entries and [`MAX_BANS_BYTES`] of
//! text; the log keeps at most [`MAX_AUDIT_PAGES`] pages of 50 entries, so
//! reading further back drops the oldest page held. Reasons and names are
//! shown only while the panel is open and never reach a log.
//!
//! Unban is the only write: one explicit click on one user, then an explicit
//! Confirm, checked again against the current read model, sent once and never
//! retried. An ambiguous failure says the unban may have been applied; the list
//! changes only when Discord answers. Nothing here moderates automatically.

use crate::i18n::{tr, trf};
use fastcord_discord::state::navigation::{GuildAdmin, NavigationSnapshot};
use fastcord_discord::{
    AUDIT_FILTERS, AuditEntry, AuditPage, AuditQuery, BanEntry, BansPage, RestClient, RestError,
    action_name,
};
use fastcord_model::markup::TimeStyle;
use fastcord_model::{Permissions, Snowflake};
use iced::widget::{
    button, column, container, pick_list, row, scrollable, space, text, text_input,
};
use iced::{Element, Length, Task};
use std::fmt;

/// Bans held at once, and the text they may hold.
pub const MAX_BANS: usize = 500;
pub const MAX_BANS_BYTES: usize = 256 * 1024;
/// Audit pages held at once; older pages are fetched again on demand.
pub const MAX_AUDIT_PAGES: usize = 4;

#[derive(Clone, Debug)]
pub enum Event {
    Bans(Snowflake),
    Audit(Snowflake),
    Close,
    /// An answer to a ban-list request (serial, page).
    BansDone(u64, Result<BansPage, RestError>),
    MoreBans,
    AskUnban(Snowflake),
    CancelUnban,
    ConfirmUnban,
    Unbanned(u64, Snowflake, Result<(), RestError>),
    AuditDone(u64, Result<AuditPage, RestError>),
    Older,
    Newer,
    Reload,
    Action(Option<u32>),
    UserFilter(String),
}

/// What a failed moderation request tells the user: fixed, secret-free text.
pub fn failure(error: RestError) -> String {
    match error {
        RestError::PermissionDenied => tr("Discord refused: you lack the permission, or the member has a role at or above yours.").to_owned(),
        RestError::ResourceGone => tr("That member, message or channel no longer exists.").to_owned(),
        RestError::Retryable(_) => tr("Discord did not confirm. The action may have been applied; check before trying again.").to_owned(),
        RestError::AuthenticationRequired => tr("Your session has expired. Log in again.").to_owned(),
        RestError::InvalidRateLimit | RestError::RateLimited | RestError::SchedulerCapacityExceeded => {
            tr("Discord is rate limiting requests. Wait a moment and try again.").to_owned()
        }
        RestError::Http(status) if status.as_u16() == 429 => {
            tr("Discord is rate limiting requests. Wait a moment and try again.").to_owned()
        }
        RestError::Refused(refusal) if refusal.code == Some(60003) => tr("Discord asks for two-factor verification for this action. fastcord does not answer that challenge: complete it in the official Discord client. Nothing was changed here.").to_owned(),
        RestError::Refused(refusal) if refusal.status.as_u16() == 403 => failure(RestError::PermissionDenied),
        RestError::Refused(refusal) if refusal.status.as_u16() == 404 => failure(RestError::ResourceGone),
        RestError::Refused(refusal) if refusal.status.as_u16() == 429 => {
            tr("Discord is rate limiting requests. Wait a moment and try again.").to_owned()
        }
        other => crate::invite::describe(other),
    }
}

#[derive(Debug)]
struct BanList {
    guild: Snowflake,
    bans: Vec<BanEntry>,
    bytes: usize,
    has_more: bool,
    loading: bool,
    error: Option<String>,
    confirm: Option<Snowflake>,
    /// The unban in flight.
    busy: Option<Snowflake>,
    notice: Option<String>,
}

#[derive(Debug)]
struct AuditLog {
    guild: Snowflake,
    pages: Vec<Vec<AuditEntry>>,
    /// The last page held may have older entries after it.
    has_more: bool,
    current: usize,
    action: Option<u32>,
    user_text: String,
    loading: bool,
    error: Option<String>,
}

impl AuditLog {
    /// The filters as the request carries them; an unreadable user ID filters
    /// nothing and is said so.
    fn query(&self, before: Option<Snowflake>) -> AuditQuery {
        AuditQuery {
            action_type: self.action,
            user: self.user_text.trim().parse().ok(),
            before,
        }
    }
}

#[derive(Debug)]
enum Panel {
    Bans(BanList),
    Audit(AuditLog),
}

#[derive(Default)]
pub struct State {
    panel: Option<Panel>,
    serial: u64,
}

fn entry_bytes(ban: &BanEntry) -> usize {
    64 + ban.name.len() + ban.reason.as_ref().map_or(0, String::len)
}

/// Whether the read model of `guild` grants `permission` right now.
fn allowed(admin: Option<&GuildAdmin>, guild: Snowflake, permission: Permissions) -> bool {
    admin.is_some_and(|a| a.id == guild && a.mine.contains(permission))
}

impl State {
    pub fn clear(&mut self) {
        self.panel = None;
        self.serial = self.serial.wrapping_add(1);
    }

    /// The server the panel needs the permission read model of.
    pub fn wanted(&self) -> Option<Snowflake> {
        match self.panel.as_ref()? {
            Panel::Bans(list) => Some(list.guild),
            Panel::Audit(log) => Some(log.guild),
        }
    }

    /// Whether Discord rejected the login (the account must stop).
    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::BansDone(_, Err(RestError::AuthenticationRequired))
                | Event::AuditDone(_, Err(RestError::AuthenticationRequired))
                | Event::Unbanned(_, _, Err(RestError::AuthenticationRequired))
        )
    }

    fn load_bans(&mut self, client: &RestClient, after: Option<Snowflake>) -> Task<Event> {
        let Some(Panel::Bans(list)) = &mut self.panel else {
            return Task::none();
        };
        list.loading = true;
        list.error = None;
        self.serial = self.serial.wrapping_add(1);
        let (serial, guild, client) = (self.serial, list.guild, client.clone());
        Task::perform(
            async move { client.list_bans(guild, after).await },
            move |r| Event::BansDone(serial, r),
        )
    }

    fn load_audit(&mut self, client: &RestClient, before: Option<Snowflake>) -> Task<Event> {
        let Some(Panel::Audit(log)) = &mut self.panel else {
            return Task::none();
        };
        log.loading = true;
        log.error = None;
        let query = log.query(before);
        self.serial = self.serial.wrapping_add(1);
        let (serial, guild, client) = (self.serial, log.guild, client.clone());
        Task::perform(
            async move { client.audit_log(guild, query).await },
            move |r| Event::AuditDone(serial, r),
        )
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        client: &RestClient,
    ) -> Task<Event> {
        let admin = navigation.admin.as_ref();
        match event {
            Event::Bans(guild) => {
                if !allowed(admin, guild, Permissions::BAN_MEMBERS) {
                    return Task::none();
                }
                self.panel = Some(Panel::Bans(BanList {
                    guild,
                    bans: Vec::new(),
                    bytes: 0,
                    has_more: false,
                    loading: false,
                    error: None,
                    confirm: None,
                    busy: None,
                    notice: None,
                }));
                return self.load_bans(client, None);
            }
            Event::Audit(guild) => {
                if !allowed(admin, guild, Permissions::VIEW_AUDIT_LOG) {
                    return Task::none();
                }
                self.panel = Some(Panel::Audit(AuditLog {
                    guild,
                    pages: Vec::new(),
                    has_more: false,
                    current: 0,
                    action: None,
                    user_text: String::new(),
                    loading: false,
                    error: None,
                }));
                return self.load_audit(client, None);
            }
            Event::Close => self.clear(),
            Event::MoreBans => {
                let after = match &self.panel {
                    Some(Panel::Bans(list))
                        if list.has_more && !list.loading && list.busy.is_none() =>
                    {
                        list.bans.last().map(|b| b.user)
                    }
                    _ => None,
                };
                if after.is_some() {
                    return self.load_bans(client, after);
                }
            }
            Event::BansDone(serial, result) if serial == self.serial => {
                let Some(Panel::Bans(list)) = &mut self.panel else {
                    return Task::none();
                };
                list.loading = false;
                match result {
                    Ok(page) => {
                        list.has_more = page.has_more;
                        for ban in page.bans {
                            let cost = entry_bytes(&ban);
                            if list.bans.len() >= MAX_BANS || list.bytes + cost > MAX_BANS_BYTES {
                                list.has_more = false;
                                list.notice = Some(trf(
                                    "Showing the first {} bans; the rest are not listed.",
                                    &[&list.bans.len()],
                                ));
                                break;
                            }
                            if !list.bans.iter().any(|held| held.user == ban.user) {
                                list.bytes += cost;
                                list.bans.push(ban);
                            }
                        }
                    }
                    Err(error) => list.error = Some(failure(error)),
                }
            }
            Event::AskUnban(user) => {
                if let Some(Panel::Bans(list)) = &mut self.panel
                    && list.busy.is_none()
                    && list.bans.iter().any(|b| b.user == user)
                {
                    list.confirm = Some(user);
                    list.notice = None;
                }
            }
            Event::CancelUnban => {
                if let Some(Panel::Bans(list)) = &mut self.panel {
                    list.confirm = None;
                }
            }
            Event::ConfirmUnban => {
                let Some(Panel::Bans(list)) = &mut self.panel else {
                    return Task::none();
                };
                let Some(user) = list.confirm.take() else {
                    return Task::none();
                };
                // Checked again against the current read model, then sent once.
                if list.busy.is_some() || !allowed(admin, list.guild, Permissions::BAN_MEMBERS) {
                    list.notice = Some(failure(RestError::PermissionDenied));
                    return Task::none();
                }
                list.busy = Some(user);
                list.notice = None;
                let (serial, guild, client) = (self.serial, list.guild, client.clone());
                return Task::perform(async move { client.unban(guild, user).await }, move |r| {
                    Event::Unbanned(serial, user, r)
                });
            }
            Event::Unbanned(serial, user, result) if serial == self.serial => {
                let Some(Panel::Bans(list)) = &mut self.panel else {
                    return Task::none();
                };
                if list.busy == Some(user) {
                    list.busy = None;
                }
                match result {
                    Ok(()) => {
                        if let Some(at) = list.bans.iter().position(|b| b.user == user) {
                            list.bytes -= entry_bytes(&list.bans.remove(at));
                        }
                        list.notice = Some(tr("Unbanned.").to_owned());
                    }
                    Err(RestError::ResourceGone) => {
                        // Not banned any more: what the click asked for.
                        if let Some(at) = list.bans.iter().position(|b| b.user == user) {
                            list.bytes -= entry_bytes(&list.bans.remove(at));
                        }
                        list.notice = Some(tr("That user was not banned.").to_owned());
                    }
                    Err(error) => list.notice = Some(failure(error)),
                }
            }
            Event::AuditDone(serial, result) if serial == self.serial => {
                let Some(Panel::Audit(log)) = &mut self.panel else {
                    return Task::none();
                };
                log.loading = false;
                match result {
                    Ok(page) => {
                        log.has_more = page.has_more;
                        log.pages.push(page.entries);
                        if log.pages.len() > MAX_AUDIT_PAGES {
                            log.pages.remove(0);
                        }
                        log.current = log.pages.len() - 1;
                    }
                    Err(error) => log.error = Some(failure(error)),
                }
            }
            Event::Older => {
                let Some(Panel::Audit(log)) = &mut self.panel else {
                    return Task::none();
                };
                if log.loading {
                    return Task::none();
                }
                if log.current + 1 < log.pages.len() {
                    log.current += 1;
                } else if log.has_more
                    && let Some(last) = log.pages.last().and_then(|p| p.last()).map(|e| e.id)
                {
                    return self.load_audit(client, Some(last));
                }
            }
            Event::Newer => {
                if let Some(Panel::Audit(log)) = &mut self.panel {
                    log.current = log.current.saturating_sub(1);
                }
            }
            Event::Reload => {
                if let Some(Panel::Audit(log)) = &mut self.panel
                    && !log.loading
                {
                    log.pages.clear();
                    log.current = 0;
                    return self.load_audit(client, None);
                }
            }
            Event::Action(action) => {
                if let Some(Panel::Audit(log)) = &mut self.panel
                    && !log.loading
                {
                    log.action = action;
                    log.pages.clear();
                    log.current = 0;
                    return self.load_audit(client, None);
                }
            }
            Event::UserFilter(text) => {
                if let Some(Panel::Audit(log)) = &mut self.panel {
                    log.user_text = text.chars().filter(char::is_ascii_digit).take(20).collect();
                    // The filter is applied by Reload, not on every keystroke.
                }
            }
            // Stale answers (a closed or replaced panel) change nothing.
            Event::BansDone(..) | Event::Unbanned(..) | Event::AuditDone(..) => {}
        }
        Task::none()
    }
}

fn small<'a>(label: impl Into<String>) -> iced::widget::Text<'a> {
    text(label.into()).size(12).style(text::secondary)
}

fn header<'a>(title: &str) -> Element<'a, Event> {
    row![
        text(tr(title).to_owned()).size(14),
        space::horizontal(),
        crate::icons::close(Some(Event::Close)),
    ]
    .align_y(iced::Alignment::Center)
    .into()
}

fn bans_view<'a>(list: &'a BanList, admin: Option<&GuildAdmin>) -> Element<'a, Event> {
    let can = allowed(admin, list.guild, Permissions::BAN_MEMBERS);
    let mut rows = column![].spacing(4);
    for ban in &list.bans {
        let name = if ban.name.is_empty() {
            ban.user.to_string()
        } else {
            ban.name.clone()
        };
        let mut line = column![text(name).size(13)].spacing(1);
        line = line.push(small(ban.reason.clone().map_or_else(
            || tr("No reason given").to_owned(),
            |reason| trf("Reason: {}", &[&reason]),
        )));
        let control: Element<'_, Event> = if list.confirm == Some(ban.user) {
            row![
                text(tr("Unban this user?")).size(12).style(text::danger),
                button(text(tr("Confirm")).size(12))
                    .on_press_maybe((can && list.busy.is_none()).then_some(Event::ConfirmUnban))
                    .style(button::danger),
                button(text(tr("Cancel")).size(12)).on_press(Event::CancelUnban),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into()
        } else {
            button(text(tr("Unban")).size(12))
                .on_press_maybe(
                    (can && list.busy.is_none() && !list.loading)
                        .then_some(Event::AskUnban(ban.user)),
                )
                .into()
        };
        rows = rows.push(
            row![line.width(Length::Fill), control]
                .spacing(8)
                .align_y(iced::Alignment::Center),
        );
    }
    let mut panel = column![header("Bans")].spacing(6);
    if !can {
        panel = panel.push(small(tr("You need Ban Members to see the ban list.")));
    }
    if list.bans.is_empty() && !list.loading && list.error.is_none() {
        panel = panel.push(small(tr("No one is banned.")));
    }
    panel = panel.push(scrollable(rows).height(Length::Shrink));
    if list.loading {
        panel = panel.push(small(tr("Loading\u{2026}")));
    }
    if list.busy.is_some() {
        panel = panel.push(small(tr("Waiting for Discord\u{2026}")));
    }
    if list.has_more && !list.loading {
        panel = panel.push(
            button(text(tr("Load more")).size(12))
                .on_press_maybe((list.busy.is_none()).then_some(Event::MoreBans)),
        );
    }
    for line in [&list.error, &list.notice].into_iter().flatten() {
        panel = panel.push(text(line.clone()).size(12));
    }
    panel.into()
}

#[derive(Clone, PartialEq, Eq)]
struct ActionChoice(Option<u32>, &'static str);

impl fmt::Display for ActionChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(tr(self.1))
    }
}

/// `Name` or the ID of someone who is not in the answer's user list.
fn who(name: Option<&String>, id: Option<Snowflake>) -> String {
    match (name, id) {
        (Some(name), _) => name.clone(),
        (None, Some(id)) => id.to_string(),
        (None, None) => tr("unknown").to_owned(),
    }
}

fn audit_line(entry: &AuditEntry, offset: Option<time::UtcOffset>, now: i64) -> Element<'_, Event> {
    let when = i64::try_from(entry.id.timestamp_ms() / 1000)
        .ok()
        .and_then(|unix| crate::localtime::markup(unix, TimeStyle::ShortDateTime, offset, now))
        .unwrap_or_default();
    let mut head = format!(
        "{} \u{2014} {}",
        who(entry.actor_name.as_ref(), entry.actor),
        tr(&action_name(entry.action))
    );
    if entry.target.is_some() {
        head.push_str(&format!(
            " \u{2192} {}",
            who(entry.target_name.as_ref(), entry.target)
        ));
    }
    let mut line = column![text(head).size(13), small(when)].spacing(1);
    if let Some(reason) = &entry.reason {
        line = line.push(small(trf("Reason: {}", &[reason])));
    }
    if !entry.summary.is_empty() {
        line = line.push(small(entry.summary.clone()));
    }
    line.into()
}

fn audit_view<'a>(log: &'a AuditLog, admin: Option<&GuildAdmin>) -> Element<'a, Event> {
    let can = allowed(admin, log.guild, Permissions::VIEW_AUDIT_LOG);
    let mut choices = vec![ActionChoice(None, "All actions")];
    choices.extend(
        AUDIT_FILTERS
            .iter()
            .map(|(n, label)| ActionChoice(Some(*n), label)),
    );
    let selected = choices.iter().find(|c| c.0 == log.action).cloned();
    let filters = row![
        pick_list(choices, selected, |choice| Event::Action(choice.0)).text_size(12),
        text_input(tr("User ID"), &log.user_text)
            .on_input(Event::UserFilter)
            .on_submit(Event::Reload)
            .size(12)
            .width(160),
        button(text(tr("Apply")).size(12)).on_press_maybe((!log.loading).then_some(Event::Reload)),
    ]
    .spacing(6)
    .align_y(iced::Alignment::Center);
    let (offset, now) = (crate::localtime::offset(), crate::localtime::now());
    let mut entries = column![].spacing(6);
    if let Some(page) = log.pages.get(log.current) {
        for entry in page {
            entries = entries.push(audit_line(entry, offset, now));
        }
    }
    let mut panel = column![header("Audit log"), filters].spacing(6);
    if !can {
        panel = panel.push(small(tr("You need View Audit Log to see this.")));
    }
    if log.pages.get(log.current).is_some_and(Vec::is_empty) && !log.loading {
        panel = panel.push(small(tr("No entries.")));
    }
    panel = panel.push(scrollable(entries).height(Length::Shrink));
    let at_end = log.current + 1 >= log.pages.len();
    panel = panel.push(
        row![
            button(text(tr("Newer")).size(12))
                .on_press_maybe((log.current > 0 && !log.loading).then_some(Event::Newer)),
            small(trf(
                "Page {} of {} held",
                &[&(log.current + 1), &log.pages.len().max(1)],
            )),
            button(text(tr("Older")).size(12)).on_press_maybe(
                (!log.loading && (!at_end || log.has_more)).then_some(Event::Older)
            ),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    );
    if log.loading {
        panel = panel.push(small(tr("Loading\u{2026}")));
    }
    if let Some(error) = &log.error {
        panel = panel.push(text(error.clone()).size(12));
    }
    panel.into()
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
) -> Option<Element<'a, Event>> {
    let admin = navigation.admin.as_ref();
    let panel = match state.panel.as_ref()? {
        Panel::Bans(list) => bans_view(list, admin),
        Panel::Audit(log) => audit_view(log, admin),
    };
    Some(container(panel).padding([6, 0]).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::gateway::{ClientProperties, HostOs};
    use fastcord_discord::{StatusCode, UserToken};

    fn client() -> RestClient {
        RestClient::new(
            UserToken::new("offline-test-credential".to_owned()),
            ClientProperties::web(HostOs::Linux, "en-US", 1),
        )
        .unwrap()
    }

    const G: Snowflake = Snowflake(100);

    fn navigation(permission: Permissions) -> NavigationSnapshot {
        NavigationSnapshot {
            admin: Some(GuildAdmin {
                id: G,
                name: "Home".into(),
                has_icon: false,
                owner: false,
                community: false,
                description: String::new(),
                system_channel: None,
                afk_channel: None,
                afk_timeout: 0,
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
                channels: Vec::new(),
                people: Vec::new(),
                can_manage_roles: false,
                roles: Vec::new(),
                mine: permission,
                grant_all: false,
                top_position: 0,
            }),
            ..NavigationSnapshot::default()
        }
    }

    fn ban(id: u64) -> BanEntry {
        BanEntry {
            user: Snowflake(id),
            name: format!("user{id}"),
            reason: Some("spam".to_owned()),
        }
    }

    fn page(range: std::ops::Range<u64>, has_more: bool) -> BansPage {
        BansPage {
            bans: range.map(ban).collect(),
            has_more,
        }
    }

    fn entry(id: u64) -> AuditEntry {
        AuditEntry {
            id: Snowflake(id),
            actor: None,
            actor_name: None,
            target: None,
            target_name: None,
            action: 22,
            reason: None,
            summary: String::new(),
        }
    }

    fn bans(state: &State) -> &BanList {
        match state.panel.as_ref().unwrap() {
            Panel::Bans(list) => list,
            Panel::Audit(_) => panic!("not the ban list"),
        }
    }

    fn audit(state: &State) -> &AuditLog {
        match state.panel.as_ref().unwrap() {
            Panel::Audit(log) => log,
            Panel::Bans(_) => panic!("not the audit log"),
        }
    }

    #[tokio::test]
    async fn the_panels_need_their_permission() {
        let mut state = State::default();
        let none = navigation(Permissions::NONE);
        let _ = state.update(Event::Bans(G), &none, &client());
        let _ = state.update(Event::Audit(G), &none, &client());
        assert!(state.panel.is_none());
        // Ban Members does not show the audit log, nor the reverse.
        let _ = state.update(
            Event::Audit(G),
            &navigation(Permissions::BAN_MEMBERS),
            &client(),
        );
        assert!(state.panel.is_none());
        let _ = state.update(
            Event::Bans(G),
            &navigation(Permissions::BAN_MEMBERS),
            &client(),
        );
        assert_eq!(state.wanted(), Some(G));
    }

    #[tokio::test]
    async fn ban_pages_are_deduplicated_bounded_and_stale_answers_dropped() {
        let (mut state, nav) = (State::default(), navigation(Permissions::BAN_MEMBERS));
        let _ = state.update(Event::Bans(G), &nav, &client());
        let serial = state.serial;
        let _ = state.update(
            Event::BansDone(serial, Ok(page(1..4, true))),
            &nav,
            &client(),
        );
        assert_eq!((bans(&state).bans.len(), bans(&state).has_more), (3, true));
        // A replayed page adds nothing.
        let _ = state.update(Event::MoreBans, &nav, &client());
        let serial = state.serial;
        let _ = state.update(
            Event::BansDone(serial, Ok(page(2..6, false))),
            &nav,
            &client(),
        );
        assert_eq!(bans(&state).bans.len(), 5);
        // An answer to a request since replaced changes nothing.
        let _ = state.update(
            Event::BansDone(serial - 1, Ok(page(50..60, false))),
            &nav,
            &client(),
        );
        assert_eq!(bans(&state).bans.len(), 5);
        // The list stops at its cap.
        let _ = state.update(Event::Bans(G), &nav, &client());
        let serial = state.serial;
        let big = page(1..(MAX_BANS as u64 + 20), true);
        let _ = state.update(Event::BansDone(serial, Ok(big)), &nav, &client());
        assert_eq!(bans(&state).bans.len(), MAX_BANS);
        assert!(!bans(&state).has_more && bans(&state).notice.is_some());
    }

    #[tokio::test]
    async fn an_unban_needs_confirm_is_sent_once_and_updates_only_on_the_answer() {
        let (mut state, nav) = (State::default(), navigation(Permissions::BAN_MEMBERS));
        let _ = state.update(Event::Bans(G), &nav, &client());
        let serial = state.serial;
        let _ = state.update(
            Event::BansDone(serial, Ok(page(1..3, false))),
            &nav,
            &client(),
        );
        // No Confirm, no request.
        let _ = state.update(Event::ConfirmUnban, &nav, &client());
        assert!(bans(&state).busy.is_none());
        let _ = state.update(Event::AskUnban(Snowflake(9)), &nav, &client());
        assert!(bans(&state).confirm.is_none(), "not a listed user");
        let _ = state.update(Event::AskUnban(Snowflake(1)), &nav, &client());
        // The permission is checked again when Confirm is pressed.
        let _ = state.update(
            Event::ConfirmUnban,
            &navigation(Permissions::NONE),
            &client(),
        );
        assert!(bans(&state).busy.is_none() && bans(&state).notice.is_some());
        let _ = state.update(Event::AskUnban(Snowflake(1)), &nav, &client());
        let _ = state.update(Event::ConfirmUnban, &nav, &client());
        assert_eq!(bans(&state).busy, Some(Snowflake(1)));
        // A second ask is refused while one is in flight; the row stays.
        let _ = state.update(Event::AskUnban(Snowflake(2)), &nav, &client());
        assert!(bans(&state).confirm.is_none() && bans(&state).bans.len() == 2);
        let serial = state.serial;
        let retryable = RestError::Retryable(fastcord_discord::RetryableFailure::Network(
            fastcord_discord::NetworkFailure::Timeout,
        ));
        let _ = state.update(
            Event::Unbanned(serial, Snowflake(1), Err(retryable)),
            &nav,
            &client(),
        );
        assert_eq!(bans(&state).bans.len(), 2);
        assert!(
            bans(&state)
                .notice
                .as_ref()
                .unwrap()
                .contains("may have been applied")
        );
        let _ = state.update(
            Event::Unbanned(serial, Snowflake(1), Ok(())),
            &nav,
            &client(),
        );
        assert_eq!(bans(&state).bans.len(), 1);
    }

    #[tokio::test]
    async fn failures_are_described_for_403_404_429_and_two_factor() {
        let refused = |status: u16, code| {
            RestError::Refused(fastcord_discord::Refusal::new(
                StatusCode::from_u16(status).unwrap(),
                code,
                "x",
            ))
        };
        assert!(failure(refused(403, None)).contains("lack the permission"));
        assert!(failure(refused(404, None)).contains("no longer exists"));
        assert!(failure(refused(429, None)).contains("rate limiting"));
        assert!(failure(refused(403, Some(60003))).contains("two-factor"));
        assert!(failure(RestError::PermissionDenied).contains("lack the permission"));
    }

    #[tokio::test]
    async fn the_audit_log_keeps_four_pages_and_navigates_without_new_requests() {
        let (mut state, nav) = (State::default(), navigation(Permissions::VIEW_AUDIT_LOG));
        let _ = state.update(Event::Audit(G), &nav, &client());
        for n in 0..5u64 {
            let serial = state.serial;
            let page = AuditPage {
                entries: vec![entry(1_000 - n * 10)],
                has_more: true,
            };
            let _ = state.update(Event::AuditDone(serial, Ok(page)), &nav, &client());
            if n < 4 {
                let _ = state.update(Event::Older, &nav, &client());
            }
        }
        assert_eq!(audit(&state).pages.len(), MAX_AUDIT_PAGES);
        // The oldest page held is the one that was dropped from the front.
        assert_eq!(audit(&state).pages[0][0].id, Snowflake(990));
        let _ = state.update(Event::Newer, &nav, &client());
        assert_eq!(audit(&state).current, MAX_AUDIT_PAGES - 2);
        let loading_before = audit(&state).loading;
        let _ = state.update(Event::Older, &nav, &client());
        assert!(
            !loading_before && !audit(&state).loading,
            "a held page needs no request"
        );
        // A stale answer is dropped.
        let serial = state.serial;
        let _ = state.update(
            Event::AuditDone(
                serial - 1,
                Ok(AuditPage {
                    entries: vec![],
                    has_more: false,
                }),
            ),
            &nav,
            &client(),
        );
        assert_eq!(audit(&state).pages.len(), MAX_AUDIT_PAGES);
    }

    #[tokio::test]
    async fn filters_restart_the_log_and_the_user_filter_keeps_digits() {
        let (mut state, nav) = (State::default(), navigation(Permissions::VIEW_AUDIT_LOG));
        let _ = state.update(Event::Audit(G), &nav, &client());
        let serial = state.serial;
        let _ = state.update(
            Event::AuditDone(
                serial,
                Ok(AuditPage {
                    entries: vec![entry(5)],
                    has_more: false,
                }),
            ),
            &nav,
            &client(),
        );
        let _ = state.update(Event::UserFilter("12a3 4".into()), &nav, &client());
        assert_eq!(audit(&state).user_text, "1234");
        assert_eq!(
            audit(&state).query(Some(Snowflake(7))),
            AuditQuery {
                action_type: None,
                user: Some(Snowflake(1234)),
                before: Some(Snowflake(7))
            }
        );
        let _ = state.update(Event::Action(Some(22)), &nav, &client());
        assert!(audit(&state).pages.is_empty() && audit(&state).loading);
        assert_eq!(audit(&state).query(None).action_type, Some(22));
        let _ = state.update(Event::Close, &nav, &client());
        assert!(state.panel.is_none());
    }

    #[test]
    fn a_rejected_login_is_detected_on_every_answer() {
        assert!(State::is_rejected(&Event::BansDone(
            1,
            Err(RestError::AuthenticationRequired)
        )));
        assert!(State::is_rejected(&Event::AuditDone(
            1,
            Err(RestError::AuthenticationRequired)
        )));
        assert!(!State::is_rejected(&Event::BansDone(
            1,
            Err(RestError::PermissionDenied)
        )));
    }
}
