//! Roles, member roles, channel permission overwrites and the read-only
//! permission inspection (issue #94), as part of the management panel.
//!
//! What may be changed is decided by the reducer's `GuildAdmin` read model
//! (`edit_lock`, `move_lock`, `assign_lock`, `can_grant`, `AdminMember::lock`):
//! controls it forbids are shown disabled with the reason, and the same rules
//! are checked again at the click against the snapshot of that moment. Every
//! write is one request and none is retried; roles, member roles and
//! overwrites change on screen only when Discord's `GUILD_ROLE_*`,
//! `GUILD_MEMBER_UPDATE` and `CHANNEL_UPDATE` arrive.

use fastcord_discord::state::navigation::{AdminMember, AdminRole};
use fastcord_discord::{MAX_ROLE_COLOR, MAX_ROLE_NAME_CHARS, RoleEdit};
use fastcord_model::{
    OverwriteKind, PERMISSION_GROUPS, PermissionInfo, Permissions, permission_scope as scope,
};

use iced::widget::{button, checkbox, column, row, scrollable, space, text, text_input};

use super::*;

/// At most `max` characters of typed text.
fn cut(value: String, max: usize) -> String {
    value.chars().take(max).collect()
}

const PRESET_COLORS: [(&str, u32); 8] = [
    ("None", 0),
    ("Red", 0xE74C3C),
    ("Orange", 0xE67E22),
    ("Yellow", 0xF1C40F),
    ("Green", 0x2ECC71),
    ("Teal", 0x1ABC9C),
    ("Blue", 0x3498DB),
    ("Purple", 0x9B59B6),
];
const MAX_FILTER_CHARS: usize = 64;
const SYNC_REASON: &str = "Only a channel inside a category can be synced.";

/// A permission's state in an overwrite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tri {
    Allow,
    Neutral,
    Deny,
}

/// The server's roles panel: the list, and the role being edited.
#[derive(Debug)]
pub(super) struct RolesForm {
    pub guild: Snowflake,
    pub(super) selected: Option<Snowflake>,
    pub(super) name: String,
    /// Hex digits without `#`.
    color: String,
    hoist: bool,
    mentionable: bool,
    pub(super) perms: Permissions,
    confirm_delete: bool,
}

impl RolesForm {
    pub fn new(guild: Snowflake) -> Self {
        Self {
            guild,
            selected: None,
            name: String::new(),
            color: String::new(),
            hoist: false,
            mentionable: false,
            perms: Permissions::NONE,
            confirm_delete: false,
        }
    }

    fn load(&mut self, role: &AdminRole) {
        self.selected = Some(role.id);
        self.name = role.name.clone();
        self.color = if role.color == 0 {
            String::new()
        } else {
            format!("{:06X}", role.color)
        };
        self.hoist = role.hoist;
        self.mentionable = role.mentionable;
        self.perms = role.permissions;
        self.confirm_delete = false;
    }
}

/// The permissions part of a channel's edit form.
#[derive(Debug, Default)]
pub(super) struct Access {
    /// The overwrite being edited; it may not exist yet.
    target: Option<(Snowflake, OverwriteKind)>,
    pub(super) allow: Permissions,
    pub(super) deny: Permissions,
    pub filter: String,
    /// The member whose effective permissions are shown.
    pub inspect: Option<Snowflake>,
    pub inspect_filter: String,
    confirm_remove: bool,
}

/// `#RRGGBB` without the hash, or empty for no colour.
fn parse_color(text: &str) -> Option<u32> {
    let digits = text.trim().trim_start_matches('#');
    if digits.is_empty() {
        return Some(0);
    }
    if digits.len() > 6 {
        return None;
    }
    u32::from_str_radix(digits, 16)
        .ok()
        .filter(|c| *c <= MAX_ROLE_COLOR)
}

/// What a role form changes, checked against the hierarchy and the grant rule.
fn role_edit(
    form: &RolesForm,
    role: &AdminRole,
    admin: &GuildAdmin,
) -> Result<RoleEdit, &'static str> {
    if let Some(reason) = role.edit_lock {
        return Err(reason);
    }
    let mut edit = RoleEdit::default();
    if !role.everyone {
        let name = form.name.trim();
        if name != role.name {
            edit.name = Some(name.to_owned());
        }
        let color =
            parse_color(&form.color).ok_or("A colour is up to six hex digits, such as 3498DB.")?;
        if color != role.color {
            edit.color = Some(color);
        }
        if form.hoist != role.hoist {
            edit.hoist = Some(form.hoist);
        }
        if form.mentionable != role.mentionable {
            edit.mentionable = Some(form.mentionable);
        }
    }
    if form.perms != role.permissions {
        let changed = Permissions(form.perms.0 ^ role.permissions.0);
        if !admin.can_grant(admin.mine, changed) {
            return Err("You cannot grant or take permissions you do not have yourself.");
        }
        edit.permissions = Some(form.perms);
    }
    Ok(edit)
}

/// The permissions an overwrite can set in a channel of this kind.
fn relevant(kind: AdminKind) -> impl Iterator<Item = (&'static str, PermissionInfo)> {
    let (text, voice) = match kind {
        AdminKind::Voice | AdminKind::Stage => (false, true),
        AdminKind::Category => (true, true),
        _ => (true, false),
    };
    PERMISSION_GROUPS
        .into_iter()
        .flat_map(move |(group, list)| {
            list.iter()
                .copied()
                .filter(move |i| {
                    i.scope == scope::CHANNEL
                        || (i.scope == scope::TEXT && text)
                        || (i.scope == scope::VOICE && voice)
                })
                .map(move |i| (group, i))
        })
}

fn overwrite_name(admin: &GuildAdmin, id: Snowflake, kind: OverwriteKind) -> String {
    match kind {
        OverwriteKind::Role => admin
            .role(id)
            .map_or_else(|| format!("Role {id}"), |r| r.name.clone()),
        OverwriteKind::Member => admin
            .people
            .iter()
            .find(|(p, _)| *p == id)
            .map_or_else(|| format!("Member {id}"), |(_, n)| n.clone()),
    }
}

impl State {
    /// The member the reducer should describe for the open panel.
    pub fn wanted_member(&self) -> Option<Snowflake> {
        match self.panel.as_ref()? {
            Panel::MemberRoles(_, user) => Some(*user),
            Panel::Channel(form) => form.access.inspect,
            _ => None,
        }
    }

    /// Edits that only change what the form shows and send nothing.
    pub(super) fn access_local(&mut self, event: Event, admin: Option<&GuildAdmin>) {
        match (&mut self.panel, event) {
            (Some(Panel::Roles(form)), Event::PickRole(id)) => {
                match admin
                    .filter(|a| a.id == form.guild)
                    .and_then(|a| a.role(id))
                {
                    Some(role) => form.load(role),
                    None => form.selected = None,
                }
            }
            (Some(Panel::Roles(form)), Event::Bit(bit)) => {
                let flag = Permissions(1 << bit);
                let allowed = admin
                    .filter(|a| a.id == form.guild)
                    .zip(form.selected)
                    .and_then(|(a, id)| a.role(id).map(|r| (a, r)))
                    .is_some_and(|(a, r)| r.edit_lock.is_none() && a.can_grant(a.mine, flag));
                if allowed {
                    form.perms.0 ^= flag.0;
                }
            }
            (Some(Panel::Roles(form)), Event::Hoist(on)) => form.hoist = on,
            (Some(Panel::Roles(form)), Event::Mentionable(on)) => form.mentionable = on,
            (Some(Panel::Roles(form)), Event::Color(color)) => {
                form.color = if color == 0 {
                    String::new()
                } else {
                    format!("{color:06X}")
                };
            }
            (Some(Panel::Channel(form)), Event::PickTarget(target)) => {
                let access = &mut form.access;
                access.confirm_remove = false;
                access.target = target;
                (access.allow, access.deny) = target
                    .and_then(|(id, _)| {
                        admin
                            .filter(|a| a.id == form.guild)
                            .and_then(|a| a.channel(form.channel))
                            .and_then(|c| c.overwrites.iter().find(|o| o.id == id))
                            .map(|o| (o.allow, o.deny))
                    })
                    .unwrap_or_default();
            }
            (Some(Panel::Channel(form)), Event::Tri(bit, state)) => {
                let flag = Permissions(1 << bit);
                let access = &mut form.access;
                let may_allow = admin
                    .filter(|a| a.id == form.guild)
                    .and_then(|a| a.channel(form.channel).map(|c| (a, c)))
                    .is_some_and(|(a, c)| c.can_manage_permissions && a.can_grant(c.mine, flag));
                access.allow.0 &= !flag.0;
                access.deny.0 &= !flag.0;
                match state {
                    Tri::Allow if may_allow => access.allow.0 |= flag.0,
                    Tri::Deny => access.deny.0 |= flag.0,
                    // A refused Allow leaves the bit neutral.
                    _ => {}
                }
            }
            (Some(Panel::Channel(form)), Event::Inspect(user)) => {
                form.access.inspect = user;
                form.access.inspect_filter.clear();
            }
            _ => {}
        }
    }

    /// Requests that need the previous answer first.
    pub(super) fn access_write(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        client: &RestClient,
    ) -> Task<Event> {
        let admin = navigation.admin.as_ref();
        match (&self.panel, event) {
            (Some(Panel::Roles(form)), Event::NewRole) => {
                let Some(admin) = admin.filter(|a| a.id == form.guild && a.can_manage_roles) else {
                    self.notice =
                        Some(fastcord_discord::state::navigation::NEEDS_MANAGE_ROLES.to_owned());
                    return Task::none();
                };
                let (guild, new) = (
                    admin.id,
                    RoleEdit {
                        name: Some("new role".to_owned()),
                        ..RoleEdit::default()
                    },
                );
                self.send(Action::CreateRole, client, move |client| async move {
                    client.create_role(guild, &new).await
                })
            }
            (Some(Panel::Roles(form)), Event::MoveRole(id, down)) => {
                let Some(plan) = admin
                    .filter(|a| a.id == form.guild)
                    .and_then(|a| a.plan_role_move(id, down))
                else {
                    self.notice = Some("That role cannot be moved there.".to_owned());
                    return Task::none();
                };
                let guild = form.guild;
                self.send(Action::MoveRole, client, move |client| async move {
                    client.reorder_roles(guild, &plan).await
                })
            }
            (Some(Panel::MemberRoles(guild, user)), Event::ToggleMemberRole(role, give)) => {
                let (guild, user) = (*guild, *user);
                let member = navigation.admin_member.as_ref().filter(|m| m.user == user);
                let allowed = admin.filter(|a| a.id == guild).is_some_and(|a| {
                    a.can_manage_roles
                        && a.role(role).is_some_and(|r| r.assign_lock.is_none())
                        && member
                            .is_some_and(|m| m.lock.is_none() && m.roles.contains(&role) != give)
                });
                if !allowed {
                    self.notice =
                        Some("That role cannot be changed for this member now.".to_owned());
                    return Task::none();
                }
                self.send(Action::MemberRole, client, move |client| async move {
                    client
                        .set_member_role(guild, user, role, give)
                        .await
                        .map_err(Into::into)
                })
            }
            (Some(Panel::Channel(form)), Event::SaveOverwrite) => {
                let Some((admin, current)) = admin
                    .filter(|a| a.id == form.guild)
                    .and_then(|a| a.channel(form.channel).map(|c| (a, c)))
                else {
                    self.notice = Some("That channel no longer exists.".to_owned());
                    return Task::none();
                };
                let access = &form.access;
                let Some((target, kind)) = access.target else {
                    return Task::none();
                };
                if !current.can_manage_permissions {
                    self.notice = Some("You need Manage Roles in this channel.".to_owned());
                    return Task::none();
                }
                let existing = current.overwrites.iter().find(|o| o.id == target);
                if existing.is_some_and(|o| o.allow == access.allow && o.deny == access.deny)
                    || (existing.is_none() && access.allow.0 | access.deny.0 == 0)
                {
                    self.notice = Some("Nothing to change.".to_owned());
                    return Task::none();
                }
                // Only the bits that changed are checked: what the overwrite
                // already allowed stays, whoever set it.
                let newly = Permissions(access.allow.0 & !existing.map_or(0, |o| o.allow.0));
                if !admin.can_grant(current.mine, newly) {
                    self.notice = Some(
                        "You cannot allow permissions you do not have in this channel.".to_owned(),
                    );
                    return Task::none();
                }
                let (channel, allow, deny) = (form.channel, access.allow, access.deny);
                self.send(Action::PutOverwrite, client, move |client| async move {
                    client
                        .put_overwrite(channel, target, kind, allow, deny)
                        .await
                })
            }
            (Some(Panel::Channel(form)), Event::Sync) => {
                let Some(current) = admin
                    .filter(|a| a.id == form.guild)
                    .and_then(|a| a.channel(form.channel))
                else {
                    self.notice = Some("That channel no longer exists.".to_owned());
                    return Task::none();
                };
                let parent = current
                    .parent_id
                    .and_then(|p| admin.and_then(|a| a.channel(p)));
                let (true, Some(parent), Some(false)) =
                    (current.can_manage_permissions, parent, current.synced)
                else {
                    self.notice = Some(if current.synced == Some(true) {
                        "Already synced with the category.".to_owned()
                    } else if current.synced.is_none() {
                        SYNC_REASON.to_owned()
                    } else {
                        "You need Manage Roles in this channel.".to_owned()
                    });
                    return Task::none();
                };
                let (channel, overwrites) = (form.channel, parent.overwrites.clone());
                self.send(Action::SyncOverwrites, client, move |client| async move {
                    client.sync_overwrites(channel, &overwrites).await
                })
            }
            _ => Task::none(),
        }
    }

    /// Saves the role being edited.
    pub(super) fn save_role(
        &mut self,
        admin: Option<&GuildAdmin>,
        client: &RestClient,
    ) -> Task<Event> {
        let Some(Panel::Roles(form)) = &self.panel else {
            return Task::none();
        };
        let Some((admin, role)) = admin
            .filter(|a| a.id == form.guild)
            .zip(form.selected)
            .and_then(|(a, id)| a.role(id).map(|r| (a, r)))
        else {
            self.notice = Some("That role no longer exists.".to_owned());
            return Task::none();
        };
        let edit = match role_edit(form, role, admin) {
            Ok(edit) if edit == RoleEdit::default() => {
                self.notice = Some("Nothing to change.".to_owned());
                return Task::none();
            }
            Ok(edit) => edit,
            Err(reason) => {
                self.notice = Some(reason.to_owned());
                return Task::none();
            }
        };
        let (guild, id) = (form.guild, role.id);
        self.send(Action::SaveRole, client, move |client| async move {
            client.edit_role(guild, id, &edit).await
        })
    }

    /// The Confirm of a pending role deletion or overwrite removal.
    pub(super) fn confirm_access(
        &mut self,
        admin: Option<&GuildAdmin>,
        client: &RestClient,
    ) -> Option<Task<Event>> {
        match &self.panel {
            Some(Panel::Roles(form)) if form.confirm_delete => {
                let role = admin
                    .filter(|a| a.id == form.guild)
                    .zip(form.selected)
                    .and_then(|(a, id)| a.role(id));
                let Some(role) = role.filter(|r| r.move_lock.is_none()) else {
                    self.notice = Some(
                        role.and_then(|r| r.move_lock)
                            .unwrap_or("That role no longer exists.")
                            .to_owned(),
                    );
                    return Some(Task::none());
                };
                let (guild, id) = (form.guild, role.id);
                Some(
                    self.send(Action::DeleteRole, client, move |client| async move {
                        client.delete_role(guild, id).await.map_err(Into::into)
                    }),
                )
            }
            Some(Panel::Channel(form)) if form.access.confirm_remove => {
                let allowed = admin
                    .filter(|a| a.id == form.guild)
                    .and_then(|a| a.channel(form.channel))
                    .is_some_and(|c| c.can_manage_permissions);
                let Some((target, _)) = form.access.target.filter(|_| allowed) else {
                    self.notice = Some("You need Manage Roles in this channel.".to_owned());
                    return Some(Task::none());
                };
                let channel = form.channel;
                Some(
                    self.send(Action::DeleteOverwrite, client, move |client| async move {
                        client
                            .delete_overwrite(channel, target)
                            .await
                            .map_err(Into::into)
                    }),
                )
            }
            _ => None,
        }
    }

    pub(super) fn access_ask(&mut self, ask: Ask) -> bool {
        match (&mut self.panel, ask) {
            (Some(Panel::Roles(form)), Ask::DeleteRole) if form.selected.is_some() => {
                form.confirm_delete = true;
                true
            }
            (Some(Panel::Channel(form)), Ask::RemoveOverwrite) if form.access.target.is_some() => {
                form.access.confirm_remove = true;
                true
            }
            _ => false,
        }
    }

    pub(super) fn access_cancel(&mut self) {
        match &mut self.panel {
            Some(Panel::Roles(form)) => form.confirm_delete = false,
            Some(Panel::Channel(form)) => form.access.confirm_remove = false,
            _ => {}
        }
    }

    pub(super) fn access_input(&mut self, field: Field, value: String) {
        let value = cut(value, MAX_FILTER_CHARS.max(MAX_ROLE_NAME_CHARS));
        match (&mut self.panel, field) {
            (Some(Panel::Roles(form)), Field::RoleName) => form.name = value,
            (Some(Panel::Roles(form)), Field::RoleColor) => {
                form.color = cut(value, 7);
            }
            (Some(Panel::Channel(form)), Field::TargetFilter) => {
                form.access.filter = cut(value, MAX_FILTER_CHARS);
            }
            (Some(Panel::Channel(form)), Field::InspectFilter) => {
                form.access.inspect_filter = cut(value, MAX_FILTER_CHARS);
            }
            _ => {}
        }
    }

    pub(super) fn access_finished(&mut self, action: Action) {
        match (action, &mut self.panel) {
            (Action::DeleteRole, Some(Panel::Roles(form))) => {
                form.selected = None;
                form.confirm_delete = false;
            }
            (Action::DeleteOverwrite, Some(Panel::Channel(form))) => {
                form.access.target = None;
                form.access.confirm_remove = false;
            }
            _ => {}
        }
    }
}

// ----- views -----

fn swatch<'a>(color: u32) -> Element<'a, Event> {
    let c = iced::Color::from_rgb8((color >> 16) as u8, (color >> 8) as u8, color as u8);
    let dot = text("\u{25CF}").size(13);
    if color == 0 {
        dot.style(text::secondary).into()
    } else {
        dot.color(c).into()
    }
}

fn lock_note<'a>(reason: Option<&'static str>) -> Element<'a, Event> {
    match reason {
        Some(reason) => small(reason).into(),
        None => space().into(),
    }
}

pub(super) fn roles_view<'a>(
    form: &'a RolesForm,
    admin: Option<&'a GuildAdmin>,
    idle: bool,
) -> Element<'a, Event> {
    let Some(admin) = admin.filter(|a| a.id == form.guild) else {
        return column![
            header("Roles".to_owned(), idle),
            small("This server is not available, or your roles in it are not known yet."),
        ]
        .spacing(6)
        .into();
    };
    let mut panel = column![header(format!("Roles: {}", admin.name), idle)].spacing(6);
    if !admin.can_manage_roles {
        panel = panel.push(small(
            fastcord_discord::state::navigation::NEEDS_MANAGE_ROLES,
        ));
    }
    panel = panel.push(
        button(text("New role").size(12))
            .on_press_maybe((idle && admin.can_manage_roles).then_some(Event::NewRole)),
    );
    let list = column(admin.roles.iter().map(|role| {
        let up = idle && admin.plan_role_move(role.id, false).is_some();
        let down = idle && admin.plan_role_move(role.id, true).is_some();
        row![
            swatch(role.color),
            text(role.name.clone())
                .size(13)
                .width(Length::Fill)
                .wrapping(text::Wrapping::None),
            button(text("Up").size(11))
                .on_press_maybe(up.then_some(Event::MoveRole(role.id, false))),
            button(text("Down").size(11))
                .on_press_maybe(down.then_some(Event::MoveRole(role.id, true))),
            button(text("Edit").size(11)).on_press(Event::PickRole(role.id)),
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center)
        .into()
    }))
    .spacing(2)
    .padding([0, 8]);
    panel = panel.push(scrollable(list).height(Length::Fixed(160.0)));
    if let Some(role) = form.selected.and_then(|id| admin.role(id)) {
        panel = panel.push(role_editor(form, role, admin, idle));
    }
    panel.into()
}

fn role_editor<'a>(
    form: &'a RolesForm,
    role: &'a AdminRole,
    admin: &'a GuildAdmin,
    idle: bool,
) -> Element<'a, Event> {
    let enabled = idle && role.edit_lock.is_none();
    let plain = enabled && !role.everyone;
    let mut panel = column![
        text(format!("Edit {}", role.name)).size(13),
        lock_note(role.edit_lock)
    ]
    .spacing(6);
    if role.everyone {
        panel = panel.push(small("@everyone has no name, colour or badges to change."));
    } else {
        panel = panel.push(labelled(
            "Name",
            text_input("Role name", &form.name)
                .on_input_maybe(plain.then_some(|v| Event::Input(Field::RoleName, v)))
                .size(13)
                .into(),
        ));
        let presets = PRESET_COLORS.iter().map(|(label, color)| {
            button(text(*label).size(11))
                .on_press_maybe(plain.then_some(Event::Color(*color)))
                .into()
        });
        panel = panel.push(labelled(
            "Colour",
            column![
                row(presets).spacing(3).wrap(),
                text_input("Hex, such as 3498DB", &form.color)
                    .on_input_maybe(plain.then_some(|v| Event::Input(Field::RoleColor, v)))
                    .size(13),
            ]
            .spacing(4)
            .into(),
        ));
        panel = panel
            .push(
                checkbox(form.hoist)
                    .label("Show members of this role separately")
                    .on_toggle_maybe(plain.then_some(Event::Hoist)),
            )
            .push(
                checkbox(form.mentionable)
                    .label("Allow anyone to mention this role")
                    .on_toggle_maybe(plain.then_some(Event::Mentionable)),
            );
    }
    let mut perms = column![].spacing(2);
    for (group, list) in PERMISSION_GROUPS {
        perms = perms.push(text(group).size(12).style(text::secondary));
        for info in list {
            let may = enabled && admin.can_grant(admin.mine, info.flag());
            perms = perms.push(
                checkbox(form.perms.contains(info.flag()))
                    .label(info.name)
                    .size(14)
                    .text_size(12)
                    .on_toggle_maybe(may.then_some(move |_| Event::Bit(info.bit))),
            );
        }
    }
    panel = panel
        .push(small(if admin.grant_all {
            "Permissions"
        } else {
            "Permissions (you can only change ones you have yourself)"
        }))
        .push(scrollable(perms.padding([0, 8])).height(Length::Fixed(200.0)))
        .push(button(text("Save role").size(13)).on_press_maybe(enabled.then_some(Event::Save)));
    if let Some(reason) = role.move_lock {
        panel = panel.push(small(reason));
    } else if form.confirm_delete {
        panel = panel.push(
            row![
                text(format!("Delete the role {}? Members lose it.", role.name)).size(12),
                button(text("Delete").size(12))
                    .on_press_maybe(idle.then_some(Event::Confirm))
                    .style(button::danger),
                button(text("Cancel").size(12)).on_press_maybe(idle.then_some(Event::Cancel)),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );
    } else {
        panel = panel.push(
            button(text("Delete role").size(12))
                .on_press_maybe(idle.then_some(Event::Ask(Ask::DeleteRole)))
                .style(button::danger),
        );
    }
    panel.into()
}

pub(super) fn member_roles_view<'a>(
    guild: Snowflake,
    user: Snowflake,
    admin: Option<&'a GuildAdmin>,
    member: Option<&'a AdminMember>,
    idle: bool,
) -> Element<'a, Event> {
    let admin = admin.filter(|a| a.id == guild);
    let member = member.filter(|m| m.user == user);
    let (Some(admin), Some(member)) = (admin, member) else {
        return column![
            header("Roles".to_owned(), idle),
            small("This member, or your roles in this server, are not known yet."),
        ]
        .spacing(6)
        .into();
    };
    let mut panel = column![header(format!("Roles of {}", member.name), idle)].spacing(6);
    if !admin.can_manage_roles {
        panel = panel.push(small(
            fastcord_discord::state::navigation::NEEDS_MANAGE_ROLES,
        ));
    }
    panel = panel.push(lock_note(member.lock));
    let list = column(admin.roles.iter().filter(|r| !r.everyone).map(|role| {
        let held = member.roles.contains(&role.id);
        let may =
            idle && admin.can_manage_roles && member.lock.is_none() && role.assign_lock.is_none();
        let id = role.id;
        row![
            swatch(role.color),
            checkbox(held)
                .label(role.name.clone())
                .size(14)
                .text_size(13)
                .on_toggle_maybe(may.then_some(move |on| Event::ToggleMemberRole(id, on))),
            lock_note(role.assign_lock),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into()
    }))
    .spacing(3)
    .padding([0, 8]);
    panel
        .push(scrollable(list).height(Length::Fixed(240.0)))
        .push(small(
            "Each tick sends one change. It shows here when Discord confirms it.",
        ))
        .into()
}

fn tri_row<'a>(
    info: PermissionInfo,
    allow: bool,
    deny: bool,
    can_allow: bool,
    enabled: bool,
) -> Element<'a, Event> {
    let pick = |label: &'static str, on: bool, event: Option<Event>| {
        button(text(label).size(11))
            .on_press_maybe(event.filter(|_| enabled))
            .style(if on {
                button::primary
            } else {
                button::secondary
            })
    };
    row![
        text(info.name).size(12).width(Length::Fill),
        pick(
            "Allow",
            allow,
            can_allow.then_some(Event::Tri(info.bit, Tri::Allow))
        ),
        pick(
            "Neutral",
            !allow && !deny,
            Some(Event::Tri(info.bit, Tri::Neutral))
        ),
        pick("Deny", deny, Some(Event::Tri(info.bit, Tri::Deny))),
    ]
    .spacing(3)
    .align_y(iced::Alignment::Center)
    .into()
}

/// The "Permissions" section of the channel edit form.
pub(super) fn access_view<'a>(
    form: &'a ChannelForm,
    current: &'a AdminChannel,
    admin: &'a GuildAdmin,
    member: Option<&'a AdminMember>,
    idle: bool,
) -> Element<'a, Event> {
    let access = &form.access;
    let enabled = idle && current.can_manage_permissions;
    let mut panel = column![text("Permissions").size(13)].spacing(6);
    if !current.can_manage_permissions {
        panel = panel.push(small(
            "You need Manage Roles in this channel to change these.",
        ));
    }
    let parent = current
        .parent_id
        .and_then(|p| admin.channel(p))
        .filter(|_| current.synced.is_some());
    panel = panel.push(match (current.synced, parent) {
        (Some(synced), Some(parent)) => row![
            small(&if synced {
                format!("Synced with the category {}.", parent.name)
            } else {
                format!("Not synced with the category {}.", parent.name)
            }),
            button(text("Sync with category").size(12))
                .on_press_maybe((enabled && !synced).then_some(Event::Sync)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
        _ => row![small(if current.kind == AdminKind::Category {
            "A category has no parent to sync with."
        } else {
            "Not in a category."
        })],
    });
    // The overwrites that exist.
    let existing = current.overwrites.iter().map(|o| {
        let label = format!(
            "{} {}",
            if o.kind == OverwriteKind::Role {
                "Role"
            } else {
                "Member"
            },
            overwrite_name(admin, o.id, o.kind)
        );
        button(text(label).size(11))
            .on_press(Event::PickTarget(Some((o.id, o.kind))))
            .style(if access.target == Some((o.id, o.kind)) {
                button::primary
            } else {
                button::secondary
            })
            .into()
    });
    panel = panel.push(row(existing).spacing(4).wrap());
    // Add one.
    let free_roles: Vec<Item<Option<Snowflake>>> =
        std::iter::once(Item(None, "Add a role".to_owned()))
            .chain(
                admin
                    .roles
                    .iter()
                    .filter(|r| !current.overwrites.iter().any(|o| o.id == r.id))
                    .map(|r| Item(Some(r.id), r.name.clone())),
            )
            .collect();
    panel = panel.push(select(free_roles, &None, enabled, |role| {
        Event::PickTarget(role.map(|id| (id, OverwriteKind::Role)))
    }));
    let needle = access.filter.trim().to_lowercase();
    let people = admin
        .people
        .iter()
        .filter(|(id, name)| {
            !needle.is_empty()
                && name.to_lowercase().contains(&needle)
                && !current.overwrites.iter().any(|o| o.id == *id)
        })
        .take(5)
        .map(|(id, name)| {
            button(text(name.clone()).size(11))
                .on_press_maybe(
                    enabled.then_some(Event::PickTarget(Some((*id, OverwriteKind::Member)))),
                )
                .into()
        });
    panel = panel
        .push(
            text_input("Add a member by name", &access.filter)
                .on_input_maybe(enabled.then_some(|v| Event::Input(Field::TargetFilter, v)))
                .size(13),
        )
        .push(row(people).spacing(4));
    // The tri-state editor.
    if let Some((target, kind)) = access.target {
        panel = panel.push(
            text(format!(
                "{} {}",
                if kind == OverwriteKind::Role {
                    "Role"
                } else {
                    "Member"
                },
                overwrite_name(admin, target, kind)
            ))
            .size(12),
        );
        let mut rows = column![].spacing(2);
        let mut last = "";
        for (group, info) in relevant(current.kind) {
            if group != last {
                rows = rows.push(text(group).size(12).style(text::secondary));
                last = group;
            }
            rows = rows.push(tri_row(
                info,
                access.allow.contains(info.flag()),
                access.deny.contains(info.flag()),
                admin.can_grant(current.mine, info.flag()),
                enabled,
            ));
        }
        panel = panel.push(scrollable(rows.padding([0, 8])).height(Length::Fixed(220.0)));
        let exists = current.overwrites.iter().any(|o| o.id == target);
        panel = panel.push(
            button(text("Save overwrite").size(13))
                .on_press_maybe(enabled.then_some(Event::SaveOverwrite)),
        );
        if exists {
            panel = panel.push(if access.confirm_remove {
                row![
                    text("Remove this overwrite?").size(12),
                    button(text("Remove").size(12))
                        .on_press_maybe(enabled.then_some(Event::Confirm))
                        .style(button::danger),
                    button(text("Cancel").size(12)).on_press_maybe(idle.then_some(Event::Cancel)),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center)
            } else {
                row![
                    button(text("Remove overwrite").size(12))
                        .on_press_maybe(enabled.then_some(Event::Ask(Ask::RemoveOverwrite)))
                        .style(button::danger)
                ]
            });
        }
    }
    panel
        .push(inspect_view(form, current, admin, member))
        .into()
}

/// The read-only breakdown of what one member can do in this channel.
fn inspect_view<'a>(
    form: &'a ChannelForm,
    current: &'a AdminChannel,
    admin: &'a GuildAdmin,
    member: Option<&'a AdminMember>,
) -> Element<'a, Event> {
    let access = &form.access;
    let mut panel = column![text("What can a member do here?").size(13)].spacing(4);
    let Some(user) = access.inspect else {
        let needle = access.inspect_filter.trim().to_lowercase();
        let people = admin
            .people
            .iter()
            .filter(|(_, name)| !needle.is_empty() && name.to_lowercase().contains(&needle))
            .take(5)
            .map(|(id, name)| {
                button(text(name.clone()).size(11))
                    .on_press(Event::Inspect(Some(*id)))
                    .into()
            });
        return panel
            .push(
                text_input("Find a member by name", &access.inspect_filter)
                    .on_input(|v| Event::Input(Field::InspectFilter, v))
                    .size(13),
            )
            .push(row(people).spacing(4))
            .into();
    };
    let member = member.filter(|m| m.user == user);
    panel = panel.push(
        row![
            small(&member.map_or_else(|| "Loading\u{2026}".to_owned(), |m| m.name.clone())),
            button(text("Close").size(11)).on_press(Event::Inspect(None)),
        ]
        .spacing(8),
    );
    let Some(effective) = member.and_then(|m| m.effective_in(current.id)) else {
        return panel
            .push(small(
                "Their roles in this server are not known, so nothing is guessed.",
            ))
            .into();
    };
    let mut rows = column![].spacing(1);
    for (_, info) in relevant(current.kind) {
        let yes = effective.contains(info.flag());
        rows = rows.push(
            text(format!(
                "{} {}",
                if yes { "\u{2713}" } else { "\u{2717}" },
                info.name
            ))
            .size(12),
        );
    }
    panel
        .push(scrollable(rows.padding([0, 8])).height(Length::Fixed(160.0)))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(id: u64, position: i32, permissions: u64) -> AdminRole {
        AdminRole {
            id: Snowflake(id),
            name: format!("role-{id}"),
            color: 0,
            hoist: false,
            mentionable: false,
            permissions: Permissions(permissions),
            position,
            managed: false,
            everyone: false,
            edit_lock: None,
            move_lock: None,
            assign_lock: None,
        }
    }

    #[test]
    fn colours_are_hex_up_to_the_plain_rgb_range() {
        assert_eq!(parse_color(""), Some(0));
        assert_eq!(parse_color(" #3498db "), Some(0x3498DB));
        assert_eq!(parse_color("FFFFFF"), Some(0xFF_FFFF));
        assert_eq!(parse_color("1000000"), None);
        assert_eq!(parse_color("xyz"), None);
    }

    #[test]
    fn a_channel_offers_only_the_bits_that_exist_for_its_kind() {
        let names = |kind| -> Vec<&'static str> { relevant(kind).map(|(_, i)| i.name).collect() };
        let text = names(AdminKind::Text);
        assert!(text.contains(&"Send messages") && !text.contains(&"Connect"));
        assert!(!text.contains(&"Administrator") && !text.contains(&"Kick members"));
        let voice = names(AdminKind::Voice);
        assert!(voice.contains(&"Connect") && !voice.contains(&"Send messages"));
        let category = names(AdminKind::Category);
        assert!(category.contains(&"Send messages") && category.contains(&"Connect"));
        assert!(text.contains(&"View channels") && voice.contains(&"Manage roles"));
    }

    #[test]
    fn a_role_edit_sends_only_differences_and_refuses_bits_the_account_lacks() {
        let target = role(5, 1, 0b1000_0000_0000);
        let mut admin = super::super::tests::admin(false, false, false, false);
        admin.roles = vec![target.clone()];
        let mut form = RolesForm::new(admin.id);
        form.load(&target);
        assert_eq!(role_edit(&form, &target, &admin), Ok(RoleEdit::default()));
        form.name = " Renamed ".into();
        form.color = "#00ff00".into();
        form.hoist = true;
        let edit = role_edit(&form, &target, &admin).unwrap();
        assert_eq!(
            (
                edit.name.as_deref(),
                edit.color,
                edit.hoist,
                edit.permissions
            ),
            (Some("Renamed"), Some(0x00FF00), Some(true), None)
        );
        // A bit the account does not hold cannot be given.
        form.perms.0 |= 1 << 2;
        assert!(role_edit(&form, &target, &admin).is_err());
        // An administrator may.
        admin.grant_all = true;
        assert!(
            role_edit(&form, &target, &admin)
                .unwrap()
                .permissions
                .is_some()
        );
        form.color = "zz".into();
        assert!(role_edit(&form, &target, &admin).is_err());
        // A locked role reports why.
        let mut locked = target;
        locked.edit_lock = Some("locked");
        assert_eq!(role_edit(&form, &locked, &admin), Err("locked"));
    }
}
