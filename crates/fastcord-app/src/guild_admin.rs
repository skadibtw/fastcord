//! Server and channel management (SPEC §16, issue #93).
//!
//! One panel, opened from the "+" of the server rail (create a server), the
//! right-click menu of a server ("Server settings", "Create channel", "Leave
//! server") or the "Edit channel" tool of a channel header. It edits what the
//! reducer's `GuildAdmin` read model shows for the server the panel names:
//! identity and conversation settings, the channel list with Move up/down,
//! channel create/edit/delete, ownership transfer and server deletion.
//!
//! Every write is one explicit click and one request: it is checked again
//! against the current read model (permission, owner, Community, typed name)
//! immediately before it is sent, at most one is in flight, and none is ever
//! retried. The lists and settings shown change only when Discord's
//! `GUILD_UPDATE` / `CHANNEL_*` / `GUILD_*` events arrive; the form keeps what
//! was typed after any failure. Deleting a server or channel and transferring
//! ownership need an explicit Confirm (deleting a server also the typed name).
//! A CAPTCHA or verification answer pauses every write and a two-factor
//! challenge is shown as such; neither is ever answered here. Billing, boosts,
//! vanity URLs, monetization and paid media channels are not part of this
//! panel. Roles, member roles, channel permission overwrites and the read-only
//! permission inspection (#94) live in the `access` submodule.

mod access;

use access::{Access, RolesForm, Tri};
use fastcord_discord::state::navigation::{
    AdminChannel, AdminKind, GuildAdmin, NavigationSnapshot,
};
use fastcord_discord::{
    AFK_TIMEOUTS, AdminError, AdminRequestError, BASE_BITRATE, CONTENT_FILTERS, ChannelEdit,
    GuildEdit, GuildIcon, MAX_CHANNEL_TOPIC_CHARS, MAX_GUILD_DESCRIPTION_CHARS,
    MAX_GUILD_NAME_CHARS, MAX_VOICE_USER_LIMIT, MIN_BITRATE, NOTIFICATION_LEVELS, NewChannel,
    RestClient, RestError, SLOWMODE, VERIFICATION_CHOICES, VoiceRegion, bitrate_ceiling,
    needs_community,
};
use fastcord_model::{OverwriteKind, Permissions, Snowflake};
use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, space, text, text_input,
};
use iced::{Element, Length, Task};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    CreateServer,
    SaveServer,
    Transfer,
    DeleteServer,
    Mfa,
    CreateChannel,
    SaveChannel,
    DeleteChannel,
    Move,
    SaveRole,
    CreateRole,
    DeleteRole,
    MoveRole,
    MemberRole,
    PutOverwrite,
    DeleteOverwrite,
    SyncOverwrites,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    RoleName,
    RoleColor,
    TargetFilter,
    InspectFilter,
    NewServerName,
    ServerName,
    Description,
    DeleteTyped,
    TransferFilter,
    NewName,
    NewTopic,
    Name,
    Topic,
    UserLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    System(Option<Snowflake>),
    Afk(Option<Snowflake>),
    AfkTimeout(u32),
    Notifications(u8),
    Filter(u8),
    Verification(u8),
    Alerts(Option<Snowflake>),
    Kind(AdminKind),
    NewParent(Option<Snowflake>),
    Parent(Option<Snowflake>),
    Slowmode(u32),
    Bitrate(u32),
    Quality(u8),
    Region(Option<String>),
    Transfer(Snowflake, String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    DeleteServer,
    /// Requiring (or no longer requiring) two-factor authentication of moderators.
    Mfa,
    Transfer,
    DeleteChannel,
    DeleteRole,
    RemoveOverwrite,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// "Roles" of a server's settings.
    Roles(Snowflake),
    /// "Roles\u{2026}" of a member: the server and the member.
    MemberRoles(Snowflake, Snowflake),
    PickRole(Snowflake),
    NewRole,
    /// Flips one permission bit of the role being edited.
    Bit(u8),
    Hoist(bool),
    Mentionable(bool),
    Color(u32),
    MoveRole(Snowflake, bool),
    ToggleMemberRole(Snowflake, bool),
    PickTarget(Option<(Snowflake, OverwriteKind)>),
    Tri(u8, Tri),
    SaveOverwrite,
    Sync,
    Inspect(Option<Snowflake>),
    /// "+" in the server rail.
    NewServer,
    /// Right click on a server in the rail.
    Menu(Snowflake),
    Settings(Snowflake),
    /// A new channel in a server, optionally in a category.
    CreateChannel(Snowflake, Option<Snowflake>),
    /// Edit a channel; the flag says it is a voice or Stage channel.
    EditChannel(Snowflake, Snowflake, bool),
    /// The menu's "Leave server": the invite module asks its own confirmation.
    Leave(Snowflake),
    /// The menu's "Emoji and sounds": the `assets` panel takes over.
    Assets(Snowflake),
    /// "Followed announcements" of a text channel: server and channel. The
    /// caller opens the list (`announce`); the panel stays as it is.
    Followed(Snowflake, Snowflake),
    /// The menu's "Bans" and "Audit log": the moderation module shows them.
    Bans(Snowflake),
    Audit(Snowflake),
    /// "Apps and integrations" of a server's settings; the caller opens it.
    Apps(Snowflake),
    /// "Webhooks" of a text channel: server and channel; the caller opens it.
    Webhooks(Snowflake, Snowflake),
    /// "Community setup" of a server's settings: the caller opens the
    /// `community` panel and this one closes.
    Community(Snowflake),
    /// The menu's "AutoMod": the `automod` module shows it.
    AutoMod(Snowflake),
    Close,
    Input(Field, String),
    Choose(Choice),
    Nsfw(bool),
    PickIcon,
    /// `Ok(None)`: the dialog was cancelled.
    IconPicked(u64, Result<Option<GuildIcon>, &'static str>),
    RemoveIcon,
    Create,
    Save,
    Move(Snowflake, bool),
    Ask(Ask),
    Cancel,
    Confirm,
    Regions(u64, Snowflake, Result<Vec<VoiceRegion>, RestError>),
    Done(u64, Action, Result<(), AdminRequestError>),
}

#[derive(Debug, Default)]
struct NewServer {
    name: String,
    icon: Option<GuildIcon>,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum IconChange {
    #[default]
    Keep,
    Remove,
    Set(GuildIcon),
}

#[derive(Debug, Default)]
enum Danger {
    #[default]
    None,
    Delete(String),
    Transfer {
        filter: String,
        to: Option<(Snowflake, String)>,
    },
    /// Waiting for the Confirm of the two-factor requirement: the new state.
    Mfa(bool),
}

#[derive(Debug)]
struct ServerForm {
    guild: Snowflake,
    loaded: bool,
    name: String,
    description: String,
    icon: IconChange,
    system: Option<Snowflake>,
    afk: Option<Snowflake>,
    /// `None` while Discord has not said and the user has not chosen.
    afk_timeout: Option<u32>,
    notifications: u8,
    filter: u8,
    verification: u8,
    alerts: Option<Snowflake>,
    danger: Danger,
}

impl ServerForm {
    fn new(guild: Snowflake) -> Self {
        Self {
            guild,
            loaded: false,
            name: String::new(),
            description: String::new(),
            icon: IconChange::Keep,
            system: None,
            afk: None,
            afk_timeout: None,
            notifications: 0,
            filter: 0,
            verification: 0,
            alerts: None,
            danger: Danger::None,
        }
    }

    fn load(&mut self, admin: &GuildAdmin) {
        self.loaded = true;
        self.name = admin.name.clone();
        self.description = admin.description.clone();
        self.system = admin.system_channel;
        self.afk = admin.afk_channel;
        self.afk_timeout = (admin.afk_timeout != 0).then_some(admin.afk_timeout);
        self.notifications = admin.notifications;
        self.filter = admin.content_filter;
        self.verification = admin.verification_level;
        self.alerts = admin.safety_alerts_channel;
    }

    /// What differs from the server as the reducer holds it now.
    fn edit(&self, admin: &GuildAdmin) -> GuildEdit {
        let name = self.name.trim();
        let description = self.description.trim();
        GuildEdit {
            name: (name != admin.name).then(|| name.to_owned()),
            icon: match &self.icon {
                IconChange::Keep => None,
                IconChange::Remove => admin.has_icon.then_some(None),
                IconChange::Set(icon) => Some(Some(icon.clone())),
            },
            description: (description != admin.description.trim()).then(|| description.to_owned()),
            system_channel: (self.system != admin.system_channel).then_some(self.system),
            afk_channel: (self.afk != admin.afk_channel).then_some(self.afk),
            afk_timeout: self.afk_timeout.filter(|t| *t != admin.afk_timeout),
            notifications: (self.notifications != admin.notifications)
                .then_some(self.notifications),
            content_filter: (self.filter != admin.content_filter).then_some(self.filter),
            verification_level: (self.verification != admin.verification_level)
                .then_some(self.verification),
            safety_alerts_channel: (self.alerts != admin.safety_alerts_channel)
                .then_some(self.alerts),
        }
    }
}

#[derive(Debug)]
struct NewChannelForm {
    guild: Snowflake,
    kind: AdminKind,
    name: String,
    topic: String,
    parent: Option<Snowflake>,
}

#[derive(Debug)]
struct ChannelForm {
    guild: Snowflake,
    channel: Snowflake,
    loaded: bool,
    name: String,
    topic: String,
    nsfw: bool,
    slowmode: u32,
    limit: String,
    /// `None` while the server has not reported one and the user has not chosen.
    bitrate: Option<u32>,
    quality: u8,
    region: Option<String>,
    parent: Option<Snowflake>,
    confirm_delete: bool,
    access: Access,
}

const fn has_topic(kind: AdminKind) -> bool {
    matches!(
        kind,
        AdminKind::Text | AdminKind::Announcement | AdminKind::Forum
    )
}

impl ChannelForm {
    fn new(guild: Snowflake, channel: Snowflake) -> Self {
        Self {
            guild,
            channel,
            loaded: false,
            name: String::new(),
            topic: String::new(),
            nsfw: false,
            slowmode: 0,
            limit: String::new(),
            bitrate: None,
            quality: 1,
            region: None,
            parent: None,
            confirm_delete: false,
            access: Access::default(),
        }
    }

    fn load(&mut self, channel: &AdminChannel) {
        self.loaded = true;
        self.name = channel.name.clone();
        self.topic = channel.topic.clone();
        self.nsfw = channel.nsfw;
        self.slowmode = channel.slowmode;
        self.limit = if channel.user_limit == 0 {
            String::new()
        } else {
            channel.user_limit.to_string()
        };
        self.bitrate = (channel.bitrate != 0).then_some(channel.bitrate);
        self.quality = channel.video_quality;
        self.region = channel.rtc_region.clone();
        self.parent = channel.parent_id;
    }

    /// What differs from the channel as the reducer holds it now, for the
    /// settings its kind has.
    fn edit(&self, current: &AdminChannel) -> Result<ChannelEdit, &'static str> {
        let kind = current.kind;
        let mut edit = ChannelEdit::default();
        let name = self.name.trim();
        if name != current.name {
            edit.name = Some(name.to_owned());
        }
        if has_topic(kind) && self.topic.trim() != current.topic.trim() {
            edit.topic = Some(self.topic.clone());
        }
        if has_topic(kind) && self.nsfw != current.nsfw {
            edit.nsfw = Some(self.nsfw);
        }
        if matches!(kind, AdminKind::Text | AdminKind::Voice) && self.slowmode != current.slowmode {
            edit.slowmode = Some(self.slowmode);
        }
        if kind == AdminKind::Voice {
            let limit = match self.limit.trim() {
                "" => 0,
                digits => digits
                    .parse::<u32>()
                    .ok()
                    .filter(|n| *n <= MAX_VOICE_USER_LIMIT)
                    .ok_or("The user limit is a number from 1 to 99, or empty for no limit.")?,
            };
            if limit != current.user_limit {
                edit.user_limit = Some(limit);
            }
            if self.quality != current.video_quality {
                edit.video_quality = Some(self.quality);
            }
        }
        if kind.is_voice() {
            edit.bitrate = self.bitrate.filter(|b| *b != current.bitrate);
            if self.region != current.rtc_region {
                edit.rtc_region = Some(self.region.clone());
            }
        }
        if kind != AdminKind::Category && self.parent != current.parent_id {
            edit.parent = Some(self.parent);
        }
        Ok(edit)
    }
}

#[derive(Debug)]
enum Panel {
    Menu(Snowflake),
    NewServer(NewServer),
    Server(Box<ServerForm>),
    NewChannel(NewChannelForm),
    Channel(Box<ChannelForm>),
    Roles(Box<RolesForm>),
    /// The roles of one member: the server and the member.
    MemberRoles(Snowflake, Snowflake),
}

#[derive(Default)]
pub struct State {
    panel: Option<Panel>,
    busy: Option<Action>,
    notice: Option<String>,
    /// Voice regions of one server, asked for when a voice channel is edited.
    regions: Option<(Snowflake, Vec<VoiceRegion>)>,
    serial: u64,
}

/// What a failed management request tells the user.
fn describe(error: AdminRequestError) -> String {
    let error = match error {
        AdminRequestError::Invalid(AdminError::BadName) => {
            return "That name has the wrong length.".to_owned();
        }
        AdminRequestError::Invalid(AdminError::TopicTooLong) => {
            return format!("A topic has at most {MAX_CHANNEL_TOPIC_CHARS} characters.");
        }
        AdminRequestError::Invalid(AdminError::DescriptionTooLong) => {
            return format!("A description has at most {MAX_GUILD_DESCRIPTION_CHARS} characters.");
        }
        AdminRequestError::Invalid(AdminError::UnsupportedImage) => {
            return "Choose a PNG or JPEG image (an animated icon needs a boost).".to_owned();
        }
        AdminRequestError::Invalid(AdminError::ImageTooLarge) => {
            return "The image is larger than 512 KiB.".to_owned();
        }
        AdminRequestError::Invalid(AdminError::NeedsCommunity) => return community_note(),
        AdminRequestError::Invalid(AdminError::NothingToChange) => {
            return "Nothing to change.".to_owned();
        }
        AdminRequestError::Invalid(_) => return "That value is not allowed.".to_owned(),
        AdminRequestError::Rest(error) => error,
    };
    describe_rest(error)
}

fn community_note() -> String {
    "Announcement, Stage and forum channels need the Community feature, which this server does not have. Turn it on in the official Discord client (it needs rules and updates channels).".to_owned()
}

pub(crate) fn describe_rest(error: RestError) -> String {
    match error {
        RestError::PermissionDenied => "Discord refused: you lack the permission for this (Manage Server, Manage Channels, Manage Roles, a role above the one involved, or being the owner).".to_owned(),
        RestError::ResourceGone => "That server or channel no longer exists.".to_owned(),
        RestError::Retryable(_) => "Discord did not confirm. The change may have been applied; check the list before trying again.".to_owned(),
        RestError::Refused(refusal) if refusal.code == Some(60003) => "Discord asks for two-factor verification for this change. fastcord does not answer that challenge: complete it in the official Discord client. Nothing was changed here.".to_owned(),
        RestError::Refused(refusal) if refusal.status.as_u16() == 403 => describe_rest(RestError::PermissionDenied),
        RestError::Refused(refusal) if refusal.status.as_u16() == 404 => describe_rest(RestError::ResourceGone),
        other => crate::invite::describe(other),
    }
}

fn read_icon(path: &std::path::Path) -> Result<GuildIcon, &'static str> {
    let bytes = crate::group_dm::read_image_bytes(path)?;
    GuildIcon::new(&bytes).map_err(|error| match error {
        AdminError::ImageTooLarge => "The image is larger than 512 KiB.",
        _ => "Choose a PNG or JPEG image (an animated icon needs a boost).",
    })
}

/// Bitrates a voice channel may be set to: the presets up to what the server
/// allows (see [`bitrate_ceiling`]) and the channel's own current value.
fn bitrate_options(current: Option<u32>) -> Vec<u32> {
    let ceiling = bitrate_ceiling(current.unwrap_or(0));
    let mut options: Vec<u32> = [MIN_BITRATE, 16_000, 32_000, 48_000, BASE_BITRATE]
        .into_iter()
        .filter(|b| *b <= ceiling)
        .collect();
    if let Some(current) = current
        && !options.contains(&current)
    {
        options.push(current);
    }
    options
}

fn kbps(bitrate: u32) -> String {
    format!("{} kbps", bitrate / 1000)
}

impl State {
    pub fn clear(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            ..Self::default()
        };
    }

    /// The server the panel needs the read model of.
    pub fn wanted(&self) -> Option<Snowflake> {
        match self.panel.as_ref()? {
            Panel::Menu(guild) => Some(*guild),
            Panel::NewServer(_) => None,
            Panel::Server(form) => Some(form.guild),
            Panel::NewChannel(form) => Some(form.guild),
            Panel::Channel(form) => Some(form.guild),
            Panel::Roles(form) => Some(form.guild),
            Panel::MemberRoles(guild, _) => Some(*guild),
        }
    }

    /// Fills a form the first time the server's read model arrives.
    pub fn sync(&mut self, navigation: &NavigationSnapshot) {
        let admin = navigation.admin.as_ref();
        match &mut self.panel {
            Some(Panel::Server(form)) if !form.loaded => {
                if let Some(admin) = admin.filter(|a| a.id == form.guild) {
                    form.load(admin);
                }
            }
            Some(Panel::Channel(form)) if !form.loaded => {
                if let Some(channel) = admin
                    .filter(|a| a.id == form.guild)
                    .and_then(|a| a.channel(form.channel))
                {
                    form.load(channel);
                }
            }
            _ => {}
        }
    }

    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Done(
                _,
                _,
                Err(AdminRequestError::Rest(RestError::AuthenticationRequired))
            ) | Event::Regions(_, _, Err(RestError::AuthenticationRequired))
        )
    }

    fn send<F, Fut>(&mut self, action: Action, client: &RestClient, run: F) -> Task<Event>
    where
        F: FnOnce(RestClient) -> Fut,
        Fut: Future<Output = Result<(), AdminRequestError>> + Send + 'static,
    {
        self.busy = Some(action);
        self.notice = None;
        let serial = self.serial;
        Task::perform(run(client.clone()), move |result| {
            Event::Done(serial, action, result)
        })
    }

    fn open(&mut self, panel: Panel) {
        if self.busy.is_none() {
            self.panel = Some(panel);
            self.notice = None;
        }
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        client: &RestClient,
    ) -> Task<Event> {
        let admin = navigation.admin.as_ref();
        match event {
            Event::NewServer => self.open(Panel::NewServer(NewServer::default())),
            Event::Menu(guild) => self.open(Panel::Menu(guild)),
            Event::Settings(guild) => self.open(Panel::Server(Box::new(ServerForm::new(guild)))),
            Event::Roles(guild) => self.open(Panel::Roles(Box::new(RolesForm::new(guild)))),
            Event::MemberRoles(guild, user) => self.open(Panel::MemberRoles(guild, user)),
            Event::CreateChannel(guild, parent) => self.open(Panel::NewChannel(NewChannelForm {
                guild,
                kind: AdminKind::Text,
                name: String::new(),
                topic: String::new(),
                parent,
            })),
            Event::EditChannel(guild, channel, voice) => {
                self.open(Panel::Channel(Box::new(ChannelForm::new(guild, channel))));
                let known = self.regions.as_ref().is_some_and(|(g, _)| *g == guild);
                if voice && !known && self.busy.is_none() {
                    let serial = self.serial;
                    let client = client.clone();
                    return Task::perform(
                        async move { client.voice_regions(guild).await },
                        move |result| Event::Regions(serial, guild, result),
                    );
                }
            }
            Event::Followed(..) | Event::Apps(_) | Event::Webhooks(..) | Event::Community(_) => {}
            // Forwarded by the caller; the menu closes with it.
            Event::Leave(_)
            | Event::Bans(_)
            | Event::Audit(_)
            | Event::Assets(_)
            | Event::AutoMod(_) => {
                if self.busy.is_none() {
                    self.panel = None;
                }
            }
            Event::Close => {
                if self.busy.is_none() {
                    self.panel = None;
                    self.notice = None;
                }
            }
            Event::Regions(serial, guild, result) if serial == self.serial => {
                if let Ok(regions) = result {
                    self.regions = Some((guild, regions));
                }
            }
            Event::Done(serial, action, result) if serial == self.serial => {
                self.busy = None;
                match result {
                    Ok(()) => self.finished(action),
                    Err(error) => self.notice = Some(describe(error)),
                }
            }
            Event::IconPicked(serial, result) if serial == self.serial => {
                self.busy = None;
                match result {
                    Ok(Some(icon)) => match &mut self.panel {
                        Some(Panel::NewServer(form)) => form.icon = Some(icon),
                        Some(Panel::Server(form)) => form.icon = IconChange::Set(icon),
                        _ => {}
                    },
                    Ok(None) => {}
                    Err(reason) => self.notice = Some(reason.to_owned()),
                }
            }
            Event::Regions(..) | Event::Done(..) | Event::IconPicked(..) => {}
            _ if self.panel.is_none() => {}
            // Typing and choosing change only the form; every request waits
            // for the previous answer.
            Event::Input(field, value) => self.input(field, value),
            Event::Choose(choice) => self.choose(choice),
            Event::Nsfw(on) => {
                if let Some(Panel::Channel(form)) = &mut self.panel {
                    form.nsfw = on;
                }
            }
            Event::Cancel => {
                match &mut self.panel {
                    Some(Panel::Server(form)) => form.danger = Danger::None,
                    Some(Panel::Channel(form)) => form.confirm_delete = false,
                    _ => {}
                }
                self.access_cancel();
            }
            Event::Ask(ask) => self.ask(ask, admin),
            Event::PickRole(_)
            | Event::Bit(_)
            | Event::Hoist(_)
            | Event::Mentionable(_)
            | Event::Color(_)
            | Event::PickTarget(_)
            | Event::Tri(..)
            | Event::Inspect(_) => self.access_local(event, admin),
            _ if self.busy.is_some() => {
                self.notice = Some("Wait for the previous change to finish.".to_owned());
            }
            Event::PickIcon => {
                self.busy = Some(Action::SaveServer);
                self.notice = None;
                let serial = self.serial;
                return Task::perform(
                    async {
                        let Some(file) = rfd::AsyncFileDialog::new()
                            .set_title("Server icon")
                            .add_filter("Image", &["png", "jpg", "jpeg"])
                            .pick_file()
                            .await
                        else {
                            return Ok(None);
                        };
                        let path = file.path().to_owned();
                        tokio::task::spawn_blocking(move || read_icon(&path).map(Some))
                            .await
                            .unwrap_or(Err("The image could not be read."))
                    },
                    move |result| Event::IconPicked(serial, result),
                );
            }
            Event::RemoveIcon => match &mut self.panel {
                Some(Panel::NewServer(form)) => form.icon = None,
                Some(Panel::Server(form)) => form.icon = IconChange::Remove,
                _ => {}
            },
            Event::NewRole
            | Event::MoveRole(..)
            | Event::ToggleMemberRole(..)
            | Event::SaveOverwrite
            | Event::Sync => return self.access_write(event, navigation, client),
            Event::Create => return self.create(admin, client),
            Event::Save => return self.save(admin, client),
            Event::Confirm => return self.confirm(admin, client),
            Event::Move(id, down) => {
                let Some(admin) = admin.filter(|a| a.can_manage_channels) else {
                    self.notice = Some("You need Manage Channels to reorder channels.".to_owned());
                    return Task::none();
                };
                let Some(plan) = admin.plan_move(id, down) else {
                    return Task::none();
                };
                let guild = admin.id;
                return self.send(Action::Move, client, move |client| async move {
                    client.reorder_channels(guild, &plan).await
                });
            }
        }
        Task::none()
    }

    fn finished(&mut self, action: Action) {
        const SENT: &str = "Sent. It shows here when Discord confirms it.";
        match action {
            Action::CreateServer | Action::DeleteServer | Action::DeleteChannel => {
                self.panel = None;
                self.notice = Some(SENT.to_owned());
            }
            Action::CreateChannel => {
                self.panel = None;
                self.notice = Some(SENT.to_owned());
            }
            Action::Transfer => {
                if let Some(Panel::Server(form)) = &mut self.panel {
                    form.danger = Danger::None;
                }
                self.notice = Some(SENT.to_owned());
            }
            Action::SaveServer => {
                if let Some(Panel::Server(form)) = &mut self.panel {
                    form.icon = IconChange::Keep;
                }
                self.notice = Some(SENT.to_owned());
            }
            Action::SaveChannel | Action::Move => self.notice = Some(SENT.to_owned()),
            Action::Mfa => {
                if let Some(Panel::Server(form)) = &mut self.panel {
                    form.danger = Danger::None;
                }
                self.notice = Some(SENT.to_owned());
            }
            Action::SaveRole
            | Action::CreateRole
            | Action::DeleteRole
            | Action::MoveRole
            | Action::MemberRole
            | Action::PutOverwrite
            | Action::DeleteOverwrite
            | Action::SyncOverwrites => {
                self.access_finished(action);
                self.notice = Some(SENT.to_owned());
            }
        }
    }

    fn input(&mut self, field: Field, value: String) {
        if matches!(
            field,
            Field::RoleName | Field::RoleColor | Field::TargetFilter | Field::InspectFilter
        ) {
            return self.access_input(field, value);
        }
        match (&mut self.panel, field) {
            (Some(Panel::NewServer(form)), Field::NewServerName) => form.name = value,
            (Some(Panel::Server(form)), Field::ServerName) => form.name = value,
            (Some(Panel::Server(form)), Field::Description) => form.description = value,
            (Some(Panel::Server(form)), Field::DeleteTyped) => {
                if let Danger::Delete(typed) = &mut form.danger {
                    *typed = value;
                }
            }
            (Some(Panel::Server(form)), Field::TransferFilter) => {
                if let Danger::Transfer { filter, to } = &mut form.danger {
                    *filter = value;
                    *to = None;
                }
            }
            (Some(Panel::NewChannel(form)), Field::NewName) => form.name = value,
            (Some(Panel::NewChannel(form)), Field::NewTopic) => form.topic = value,
            (Some(Panel::Channel(form)), Field::Name) => form.name = value,
            (Some(Panel::Channel(form)), Field::Topic) => form.topic = value,
            (Some(Panel::Channel(form)), Field::UserLimit) => form.limit = value,
            _ => {}
        }
    }

    fn choose(&mut self, choice: Choice) {
        match (&mut self.panel, choice) {
            (Some(Panel::Server(form)), Choice::System(c)) => form.system = c,
            (Some(Panel::Server(form)), Choice::Afk(c)) => form.afk = c,
            (Some(Panel::Server(form)), Choice::AfkTimeout(t)) => form.afk_timeout = Some(t),
            (Some(Panel::Server(form)), Choice::Notifications(n)) => form.notifications = n,
            (Some(Panel::Server(form)), Choice::Filter(f)) => form.filter = f,
            (Some(Panel::Server(form)), Choice::Verification(v)) => form.verification = v,
            (Some(Panel::Server(form)), Choice::Alerts(c)) => form.alerts = c,
            (Some(Panel::Server(form)), Choice::Transfer(id, name)) => {
                if let Danger::Transfer { to, .. } = &mut form.danger {
                    *to = Some((id, name));
                }
            }
            (Some(Panel::NewChannel(form)), Choice::Kind(kind)) => form.kind = kind,
            (Some(Panel::NewChannel(form)), Choice::NewParent(p)) => form.parent = p,
            (Some(Panel::Channel(form)), Choice::Parent(p)) => form.parent = p,
            (Some(Panel::Channel(form)), Choice::Slowmode(s)) => form.slowmode = s,
            (Some(Panel::Channel(form)), Choice::Bitrate(b)) => form.bitrate = Some(b),
            (Some(Panel::Channel(form)), Choice::Quality(q)) => form.quality = q,
            (Some(Panel::Channel(form)), Choice::Region(r)) => form.region = r,
            _ => {}
        }
    }

    /// Opens a confirmation; nothing is sent.
    fn ask(&mut self, ask: Ask, admin: Option<&GuildAdmin>) {
        if self.access_ask(ask) {
            return;
        }
        match (&mut self.panel, ask) {
            (Some(Panel::Server(form)), Ask::DeleteServer) if admin.is_some_and(|a| a.owner) => {
                form.danger = Danger::Delete(String::new());
            }
            (Some(Panel::Server(form)), Ask::Mfa) if admin.is_some_and(|a| a.owner) => {
                form.danger = Danger::Mfa(admin.is_some_and(|a| !a.mfa_required));
            }
            (Some(Panel::Server(form)), Ask::Transfer) if admin.is_some_and(|a| a.owner) => {
                form.danger = Danger::Transfer {
                    filter: String::new(),
                    to: None,
                };
            }
            (Some(Panel::Channel(form)), Ask::DeleteChannel) => form.confirm_delete = true,
            _ => {}
        }
    }

    fn create(&mut self, admin: Option<&GuildAdmin>, client: &RestClient) -> Task<Event> {
        match &self.panel {
            Some(Panel::NewServer(form)) => {
                let (name, icon) = (form.name.clone(), form.icon.clone());
                self.send(Action::CreateServer, client, move |client| async move {
                    client.create_guild(&name, icon.as_ref()).await
                })
            }
            Some(Panel::NewChannel(form)) => {
                let Some(admin) = admin.filter(|a| a.id == form.guild && a.can_manage_channels)
                else {
                    self.notice = Some(
                        "You need Manage Channels in this server to create channels.".to_owned(),
                    );
                    return Task::none();
                };
                if needs_community(form.kind.model()) && !admin.community {
                    self.notice = Some(community_note());
                    return Task::none();
                }
                let guild = form.guild;
                let new = NewChannel {
                    name: form.name.clone(),
                    kind: form.kind.model(),
                    parent: form.parent.filter(|p| {
                        admin
                            .channel(*p)
                            .is_some_and(|c| c.kind == AdminKind::Category)
                    }),
                    topic: if has_topic(form.kind) {
                        form.topic.clone()
                    } else {
                        String::new()
                    },
                    community: admin.community,
                };
                self.send(Action::CreateChannel, client, move |client| async move {
                    client.create_channel(guild, &new).await
                })
            }
            _ => Task::none(),
        }
    }

    fn save(&mut self, admin: Option<&GuildAdmin>, client: &RestClient) -> Task<Event> {
        if matches!(self.panel, Some(Panel::Roles(_))) {
            return self.save_role(admin, client);
        }
        match &self.panel {
            Some(Panel::Server(form)) if form.loaded => {
                let Some(admin) = admin.filter(|a| a.id == form.guild && a.can_manage_guild) else {
                    self.notice = Some("You need Manage Server to change these.".to_owned());
                    return Task::none();
                };
                let edit = form.edit(admin);
                if edit == GuildEdit::default() {
                    self.notice = Some("Nothing to change.".to_owned());
                    return Task::none();
                }
                let guild = form.guild;
                self.send(Action::SaveServer, client, move |client| async move {
                    client.edit_guild(guild, &edit).await
                })
            }
            Some(Panel::Channel(form)) if form.loaded => {
                let Some(current) = admin
                    .filter(|a| a.id == form.guild)
                    .and_then(|a| a.channel(form.channel))
                else {
                    self.notice = Some("That channel no longer exists.".to_owned());
                    return Task::none();
                };
                if !current.can_manage {
                    self.notice = Some("You need Manage Channels for this channel.".to_owned());
                    return Task::none();
                }
                let edit = match form.edit(current) {
                    Ok(edit) if edit == ChannelEdit::default() => {
                        self.notice = Some("Nothing to change.".to_owned());
                        return Task::none();
                    }
                    Ok(edit) => edit,
                    Err(reason) => {
                        self.notice = Some(reason.to_owned());
                        return Task::none();
                    }
                };
                let (channel, ceiling) = (form.channel, bitrate_ceiling(current.bitrate));
                self.send(Action::SaveChannel, client, move |client| async move {
                    client.edit_channel(channel, &edit, ceiling).await
                })
            }
            _ => Task::none(),
        }
    }

    /// The Confirm of a pending delete or transfer, checked once more.
    fn confirm(&mut self, admin: Option<&GuildAdmin>, client: &RestClient) -> Task<Event> {
        if let Some(task) = self.confirm_access(admin, client) {
            return task;
        }
        match &self.panel {
            Some(Panel::Server(form)) => match &form.danger {
                Danger::Mfa(required) => {
                    let Some(admin) = admin.filter(|a| a.id == form.guild && a.owner) else {
                        self.notice = Some("Only the owner can change this.".to_owned());
                        return Task::none();
                    };
                    let (guild, required) = (admin.id, *required);
                    self.send(Action::Mfa, client, move |client| async move {
                        client
                            .set_mfa_level(guild, required)
                            .await
                            .map_err(Into::into)
                    })
                }
                Danger::Delete(typed) => {
                    let Some(admin) = admin.filter(|a| a.id == form.guild && a.owner) else {
                        self.notice = Some("Only the owner can delete a server.".to_owned());
                        return Task::none();
                    };
                    if typed.trim() != admin.name {
                        self.notice = Some("Type the server's exact name to confirm.".to_owned());
                        return Task::none();
                    }
                    let guild = form.guild;
                    self.send(Action::DeleteServer, client, move |client| async move {
                        client.delete_guild(guild).await.map_err(Into::into)
                    })
                }
                Danger::Transfer {
                    to: Some((to, _)), ..
                } => {
                    let Some(admin) = admin
                        .filter(|a| a.id == form.guild && a.owner)
                        .filter(|a| a.people.iter().any(|(id, _)| id == to))
                    else {
                        self.notice = Some(
                            "Only the owner can transfer, to a member of the server.".to_owned(),
                        );
                        return Task::none();
                    };
                    let (guild, to) = (admin.id, *to);
                    self.send(Action::Transfer, client, move |client| async move {
                        client
                            .transfer_ownership(guild, to)
                            .await
                            .map_err(Into::into)
                    })
                }
                _ => Task::none(),
            },
            Some(Panel::Channel(form)) if form.confirm_delete => {
                let allowed = admin
                    .filter(|a| a.id == form.guild)
                    .and_then(|a| a.channel(form.channel))
                    .is_some_and(|c| c.can_manage);
                if !allowed {
                    self.notice = Some("You need Manage Channels for this channel.".to_owned());
                    return Task::none();
                }
                let channel = form.channel;
                self.send(Action::DeleteChannel, client, move |client| async move {
                    client.delete_channel(channel).await.map_err(Into::into)
                })
            }
            _ => Task::none(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Item<T>(T, String);

impl<T> std::fmt::Display for Item<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

fn select<'a, T: Clone + PartialEq + 'a>(
    options: Vec<Item<T>>,
    current: &T,
    enabled: bool,
    on: impl Fn(T) -> Event + 'a,
) -> Element<'a, Event> {
    let selected = options.iter().find(|item| &item.0 == current).cloned();
    if !enabled {
        return text(selected.map_or_else(|| "-".to_owned(), |item| item.1))
            .size(13)
            .into();
    }
    pick_list(options, selected, move |item: Item<T>| on(item.0))
        .text_size(13)
        .placeholder("Choose")
        .into()
}

fn presets<T: Copy>(list: &[(T, &str)]) -> Vec<Item<T>> {
    list.iter()
        .map(|(v, n)| Item(*v, (*n).to_owned()))
        .collect()
}

fn channel_choices(
    admin: &GuildAdmin,
    wanted: impl Fn(AdminKind) -> bool,
) -> Vec<Item<Option<Snowflake>>> {
    std::iter::once(Item(None, "None".to_owned()))
        .chain(
            admin
                .channels
                .iter()
                .filter(|c| wanted(c.kind))
                .map(|c| Item(Some(c.id), c.name.clone())),
        )
        .collect()
}

fn categories(admin: &GuildAdmin) -> Vec<Item<Option<Snowflake>>> {
    std::iter::once(Item(None, "No category".to_owned()))
        .chain(
            admin
                .channels
                .iter()
                .filter(|c| c.kind == AdminKind::Category)
                .map(|c| Item(Some(c.id), c.name.clone())),
        )
        .collect()
}

fn small<'a>(label: &str) -> iced::widget::Text<'a> {
    text(label.to_owned()).size(12).style(text::secondary)
}

fn labelled<'a>(label: &str, control: Element<'a, Event>) -> Element<'a, Event> {
    row![text(label.to_owned()).size(12).width(150), control]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()
}

fn header<'a>(title: String, idle: bool) -> Element<'a, Event> {
    row![
        text(title).size(14),
        space::horizontal(),
        button(text("Close").size(13)).on_press_maybe(idle.then_some(Event::Close)),
    ]
    .align_y(iced::Alignment::Center)
    .into()
}

fn server_view<'a>(
    form: &'a ServerForm,
    admin: Option<&'a GuildAdmin>,
    idle: bool,
) -> Element<'a, Event> {
    let Some(admin) = admin.filter(|a| a.id == form.guild) else {
        return column![
            header("Server settings".to_owned(), idle),
            small("This server is not available, or your roles in it are not known yet."),
        ]
        .spacing(6)
        .into();
    };
    let edit = idle && admin.can_manage_guild && form.loaded;
    let mut panel = column![header(format!("Server settings: {}", admin.name), idle)].spacing(6);
    if !admin.can_manage_guild {
        panel = panel.push(small("You need Manage Server to change these settings."));
    }
    panel = panel.push(labelled(
        "Name",
        text_input("Server name", &form.name)
            .on_input_maybe(edit.then_some(|v| Event::Input(Field::ServerName, v)))
            .size(13)
            .into(),
    ));
    let icon_state = match &form.icon {
        IconChange::Set(_) => "new image chosen",
        IconChange::Remove => "will be removed",
        IconChange::Keep if admin.has_icon => "set",
        IconChange::Keep => "none",
    };
    panel = panel.push(labelled(
        "Icon",
        row![
            small(icon_state),
            button(text("Choose image").size(12)).on_press_maybe(edit.then_some(Event::PickIcon)),
            button(text("Remove").size(12)).on_press_maybe(
                (edit && (admin.has_icon || form.icon != IconChange::Keep))
                    .then_some(Event::RemoveIcon)
            ),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into(),
    ));
    panel = panel.push(labelled(
        "Description",
        text_input("Shown to people finding the server", &form.description)
            .on_input_maybe(edit.then_some(|v| Event::Input(Field::Description, v)))
            .size(13)
            .into(),
    ));
    panel = panel.push(labelled(
        "System messages",
        select(
            channel_choices(admin, |k| {
                matches!(k, AdminKind::Text | AdminKind::Announcement)
            }),
            &form.system,
            edit,
            |c| Event::Choose(Choice::System(c)),
        ),
    ));
    panel = panel.push(labelled(
        "Inactive channel",
        select(
            channel_choices(admin, |k| k == AdminKind::Voice),
            &form.afk,
            edit,
            |c| Event::Choose(Choice::Afk(c)),
        ),
    ));
    panel = panel.push(labelled(
        "Inactive timeout",
        select(
            presets(&AFK_TIMEOUTS),
            &form.afk_timeout.unwrap_or(0),
            edit,
            |t| Event::Choose(Choice::AfkTimeout(t)),
        ),
    ));
    panel = panel.push(labelled(
        "Default notifications",
        select(
            presets(&NOTIFICATION_LEVELS),
            &form.notifications,
            edit,
            |n| Event::Choose(Choice::Notifications(n)),
        ),
    ));
    panel = panel.push(labelled(
        "Explicit media filter",
        select(presets(&CONTENT_FILTERS), &form.filter, edit, |f| {
            Event::Choose(Choice::Filter(f))
        }),
    ));
    panel = panel.push(labelled(
        "Verification level",
        select(
            presets(&VERIFICATION_CHOICES),
            &form.verification,
            edit,
            |v| Event::Choose(Choice::Verification(v)),
        ),
    ));
    panel = panel.push(labelled(
        "Safety alerts channel",
        select(
            channel_choices(admin, |k| {
                matches!(k, AdminKind::Text | AdminKind::Announcement)
            }),
            &form.alerts,
            edit,
            |c| Event::Choose(Choice::Alerts(c)),
        ),
    ));
    panel = panel.push(
        row![
            small(if admin.raid_alerts_disabled {
                "Join raid alerts are off (change in the official client)."
            } else {
                "Join raid alerts are on (change in the official client)."
            }),
            space::horizontal(),
            small(if admin.mfa_required {
                "Moderators need two-factor authentication (Confirm to change)."
            } else {
                "Moderators do not need two-factor authentication."
            }),
            button(
                text(if admin.mfa_required {
                    "Stop requiring"
                } else {
                    "Require 2FA"
                })
                .size(12)
            )
            .on_press_maybe((idle && admin.owner).then_some(Event::Ask(Ask::Mfa))),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    );
    panel = panel.push(
        row![
            button(text("Save settings").size(13)).on_press_maybe(edit.then_some(Event::Save)),
            button(text("Roles").size(13)).on_press_maybe(idle.then_some(Event::Roles(admin.id))),
            button(text("Apps and integrations").size(13))
                .on_press_maybe((idle && admin.can_manage_guild).then_some(Event::Apps(admin.id))),
            button(text("Community setup").size(13))
                .on_press_maybe(idle.then_some(Event::Community(admin.id))),
            small(if admin.community {
                "Community is on."
            } else {
                "Community is off."
            }),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center),
    );
    panel = panel.push(
        row![
            text("Channels").size(13),
            space::horizontal(),
            button(text("Create channel").size(12)).on_press_maybe(
                (idle && admin.can_manage_channels).then_some(Event::CreateChannel(admin.id, None))
            ),
        ]
        .align_y(iced::Alignment::Center),
    );
    let list = column(admin.channels.iter().map(|c| channel_row(admin, c, idle)))
        .spacing(2)
        .padding([0, 8]);
    panel = panel.push(scrollable(list).height(Length::Fixed(200.0)));
    if admin.owner {
        panel = panel.push(danger_view(form, admin, idle));
    }
    panel.into()
}

fn channel_row<'a>(admin: &'a GuildAdmin, c: &'a AdminChannel, idle: bool) -> Element<'a, Event> {
    let move_up = idle && admin.can_manage_channels && admin.plan_move(c.id, false).is_some();
    let move_down = idle && admin.can_manage_channels && admin.plan_move(c.id, true).is_some();
    let indent = if c.parent_id.is_some() { 18.0 } else { 0.0 };
    row![
        space().width(indent),
        small(c.kind.label()).width(80),
        text(c.name.clone())
            .size(13)
            .width(Length::Fill)
            .wrapping(text::Wrapping::None),
        button(text("Up").size(11)).on_press_maybe(move_up.then_some(Event::Move(c.id, false))),
        button(text("Down").size(11)).on_press_maybe(move_down.then_some(Event::Move(c.id, true))),
        button(text("Edit").size(11)).on_press_maybe(
            (idle && c.can_manage).then_some(Event::EditChannel(admin.id, c.id, c.kind.is_voice()))
        ),
    ]
    .spacing(4)
    .align_y(iced::Alignment::Center)
    .into()
}

fn danger_view<'a>(form: &'a ServerForm, admin: &'a GuildAdmin, idle: bool) -> Element<'a, Event> {
    let mut panel = column![text("Owner").size(13)].spacing(6);
    match &form.danger {
        Danger::None => {
            panel = panel.push(
                row![
                    button(text("Transfer ownership").size(12))
                        .on_press_maybe(idle.then_some(Event::Ask(Ask::Transfer))),
                    button(text("Delete server").size(12))
                        .on_press_maybe(idle.then_some(Event::Ask(Ask::DeleteServer)))
                        .style(button::danger),
                ]
                .spacing(6),
            );
        }
        Danger::Delete(typed) => {
            let matches = typed.trim() == admin.name;
            panel = panel
                .push(
                    text(format!(
                        "Delete {} for everyone? This cannot be undone. Type its name to confirm.",
                        admin.name
                    ))
                    .size(12),
                )
                .push(
                    row![
                        text_input(&admin.name, typed)
                            .on_input(|v| Event::Input(Field::DeleteTyped, v))
                            .size(13),
                        button(text("Delete server").size(12))
                            .on_press_maybe((idle && matches).then_some(Event::Confirm))
                            .style(button::danger),
                        button(text("Cancel").size(12))
                            .on_press_maybe(idle.then_some(Event::Cancel)),
                    ]
                    .spacing(6),
                );
        }
        Danger::Mfa(required) => {
            panel = panel.push(
                row![
                    text(if *required {
                        "Require two-factor authentication for moderators of this server?"
                    } else {
                        "Stop requiring two-factor authentication for moderators?"
                    })
                    .size(12),
                    button(text("Confirm").size(12))
                        .on_press_maybe(idle.then_some(Event::Confirm))
                        .style(button::danger),
                    button(text("Cancel").size(12)).on_press_maybe(idle.then_some(Event::Cancel)),
                ]
                .spacing(6),
            );
        }
        Danger::Transfer { filter, to } => {
            let needle = filter.trim().to_lowercase();
            let matches = admin
                .people
                .iter()
                .filter(|(_, name)| !needle.is_empty() && name.to_lowercase().contains(&needle))
                .take(5)
                .map(|(id, name)| {
                    button(text(name.clone()).size(12))
                        .on_press_maybe(
                            idle.then_some(Event::Choose(Choice::Transfer(*id, name.clone()))),
                        )
                        .into()
                });
            panel = panel
                .push(
                    text_input("Find a member by name", filter)
                        .on_input(|v| Event::Input(Field::TransferFilter, v))
                        .size(13),
                )
                .push(row(matches).spacing(4));
            match to {
                Some((_, name)) => {
                    panel = panel.push(
                        row![
                            text(format!(
                                "Make {name} the owner of {}? You will lose owner rights.",
                                admin.name
                            ))
                            .size(12),
                            button(text("Transfer").size(12))
                                .on_press_maybe(idle.then_some(Event::Confirm))
                                .style(button::danger),
                            button(text("Cancel").size(12))
                                .on_press_maybe(idle.then_some(Event::Cancel)),
                        ]
                        .spacing(6)
                        .align_y(iced::Alignment::Center),
                    );
                }
                None => {
                    panel = panel.push(
                        row![
                            small("Only members this client already holds can be found."),
                            button(text("Cancel").size(12))
                                .on_press_maybe(idle.then_some(Event::Cancel)),
                        ]
                        .spacing(6),
                    );
                }
            }
        }
    }
    panel.into()
}

fn new_channel_view<'a>(
    form: &'a NewChannelForm,
    admin: Option<&'a GuildAdmin>,
    idle: bool,
) -> Element<'a, Event> {
    let Some(admin) = admin.filter(|a| a.id == form.guild) else {
        return column![
            header("Create channel".to_owned(), idle),
            small("This server is not available, or your roles in it are not known yet."),
        ]
        .spacing(6)
        .into();
    };
    let kinds = [
        AdminKind::Text,
        AdminKind::Voice,
        AdminKind::Category,
        AdminKind::Announcement,
        AdminKind::Stage,
        AdminKind::Forum,
    ];
    let blocked = needs_community(form.kind.model()) && !admin.community;
    let enabled = idle && admin.can_manage_channels;
    let mut panel = column![
        header(format!("Create channel in {}", admin.name), idle),
        labelled(
            "Kind",
            select(
                kinds
                    .iter()
                    .map(|k| Item(*k, k.label().to_owned()))
                    .collect(),
                &form.kind,
                enabled,
                |k| Event::Choose(Choice::Kind(k)),
            ),
        ),
        labelled(
            "Name",
            text_input("Channel name", &form.name)
                .on_input_maybe(enabled.then_some(|v| Event::Input(Field::NewName, v)))
                .on_submit_maybe((enabled && !blocked).then_some(Event::Create))
                .size(13)
                .into(),
        ),
    ]
    .spacing(6);
    if has_topic(form.kind) {
        panel = panel.push(labelled(
            "Topic",
            text_input("Optional", &form.topic)
                .on_input_maybe(enabled.then_some(|v| Event::Input(Field::NewTopic, v)))
                .size(13)
                .into(),
        ));
    }
    if form.kind != AdminKind::Category {
        panel = panel.push(labelled(
            "Category",
            select(categories(admin), &form.parent, enabled, |p| {
                Event::Choose(Choice::NewParent(p))
            }),
        ));
    }
    if blocked {
        panel = panel.push(text(community_note()).size(12).style(text::danger));
    }
    if !admin.can_manage_channels {
        panel = panel.push(small("You need Manage Channels to create channels."));
    }
    panel
        .push(button(text("Create").size(13)).on_press_maybe(
            (enabled && !blocked && !form.name.trim().is_empty()).then_some(Event::Create),
        ))
        .into()
}

fn channel_view<'a>(
    form: &'a ChannelForm,
    admin: Option<&'a GuildAdmin>,
    regions: Option<&'a [VoiceRegion]>,
    member: Option<&'a fastcord_discord::state::navigation::AdminMember>,
    idle: bool,
) -> Element<'a, Event> {
    let Some(admin) = admin.filter(|a| a.id == form.guild) else {
        return column![
            header("Edit channel".to_owned(), idle),
            small("This server is not available, or your roles in it are not known yet."),
        ]
        .spacing(6)
        .into();
    };
    let Some(current) = admin.channel(form.channel) else {
        return column![
            header("Edit channel".to_owned(), idle),
            small("That channel no longer exists."),
        ]
        .spacing(6)
        .into();
    };
    let kind = current.kind;
    let enabled = idle && current.can_manage && form.loaded;
    let mut panel = column![header(
        format!(
            "Edit {} channel: {}",
            kind.label().to_lowercase(),
            current.name
        ),
        idle
    ),]
    .spacing(6);
    if !current.can_manage {
        panel = panel.push(small("You need Manage Channels for this channel."));
    }
    panel = panel.push(labelled(
        "Name",
        text_input("Channel name", &form.name)
            .on_input_maybe(enabled.then_some(|v| Event::Input(Field::Name, v)))
            .size(13)
            .into(),
    ));
    if has_topic(kind) {
        panel = panel.push(labelled(
            "Topic",
            text_input("Topic", &form.topic)
                .on_input_maybe(enabled.then_some(|v| Event::Input(Field::Topic, v)))
                .size(13)
                .into(),
        ));
        panel = panel.push(
            checkbox(form.nsfw)
                .label("Age-restricted")
                .on_toggle_maybe(enabled.then_some(Event::Nsfw)),
        );
    }
    if matches!(kind, AdminKind::Text | AdminKind::Voice) {
        panel = panel.push(labelled(
            "Slowmode",
            select(presets(&SLOWMODE), &form.slowmode, enabled, |s| {
                Event::Choose(Choice::Slowmode(s))
            }),
        ));
    }
    if kind == AdminKind::Voice {
        panel = panel.push(labelled(
            "User limit",
            text_input("No limit", &form.limit)
                .on_input_maybe(enabled.then_some(|v| Event::Input(Field::UserLimit, v)))
                .size(13)
                .into(),
        ));
        panel = panel.push(labelled(
            "Video quality",
            select(
                vec![
                    Item(1u8, "Automatic".to_owned()),
                    Item(2, "Full (720p)".to_owned()),
                ],
                &form.quality,
                enabled,
                |q| Event::Choose(Choice::Quality(q)),
            ),
        ));
    }
    if kind.is_voice() {
        let options: Vec<Item<u32>> =
            bitrate_options(form.bitrate.or(Some(current.bitrate).filter(|b| *b != 0)))
                .into_iter()
                .map(|b| Item(b, kbps(b)))
                .collect();
        panel = panel.push(labelled(
            "Bitrate",
            select(options, &form.bitrate.unwrap_or(0), enabled, |b| {
                Event::Choose(Choice::Bitrate(b))
            }),
        ));
        let mut regions: Vec<Item<Option<String>>> =
            std::iter::once(Item(None, "Automatic".to_owned()))
                .chain(
                    regions
                        .unwrap_or_default()
                        .iter()
                        .map(|r| Item(Some(r.id.clone()), r.name.clone())),
                )
                .collect();
        if let Some(current) = &form.region
            && !regions.iter().any(|r| r.0.as_ref() == Some(current))
        {
            regions.push(Item(Some(current.clone()), current.clone()));
        }
        panel = panel.push(labelled(
            "Region",
            select(regions, &form.region, enabled, |r| {
                Event::Choose(Choice::Region(r))
            }),
        ));
    }
    if kind != AdminKind::Category {
        panel = panel.push(labelled(
            "Category",
            select(categories(admin), &form.parent, enabled, |p| {
                Event::Choose(Choice::Parent(p))
            }),
        ));
    }
    panel =
        panel.push(button(text("Save").size(13)).on_press_maybe(enabled.then_some(Event::Save)));
    if kind == AdminKind::Text && current.mine.contains(Permissions::MANAGE_WEBHOOKS) {
        panel = panel.push(
            button(text("Followed announcements").size(12))
                .on_press_maybe(idle.then_some(Event::Followed(admin.id, current.id))),
        );
        panel = panel.push(
            button(text("Webhooks").size(12))
                .on_press_maybe(idle.then_some(Event::Webhooks(admin.id, current.id))),
        );
    }
    panel = panel.push(access::access_view(form, current, admin, member, idle));
    panel = if form.confirm_delete {
        panel.push(
            row![
                text(if kind == AdminKind::Category {
                    format!("Delete the category {}? Its channels stay and become uncategorized.", current.name)
                } else {
                    format!("Delete #{}? Its messages are removed for everyone and this cannot be undone.", current.name)
                })
                .size(12),
                button(text("Delete").size(12))
                    .on_press_maybe(enabled.then_some(Event::Confirm))
                    .style(button::danger),
                button(text("Cancel").size(12)).on_press_maybe(idle.then_some(Event::Cancel)),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        )
    } else {
        panel.push(
            button(text("Delete channel").size(12))
                .on_press_maybe(enabled.then_some(Event::Ask(Ask::DeleteChannel)))
                .style(button::danger),
        )
    };
    panel.into()
}

fn new_server_view(form: &NewServer, idle: bool) -> Element<'_, Event> {
    column![
        header("Create a server".to_owned(), idle),
        text_input("Server name", &form.name)
            .on_input_maybe(idle.then_some(|v| Event::Input(Field::NewServerName, v)))
            .on_submit_maybe((idle && form.name.trim().chars().count() >= 2).then_some(Event::Create))
            .size(13),
        row![
            small(if form.icon.is_some() { "Icon: chosen" } else { "Icon: none" }),
            button(text("Choose image").size(12)).on_press_maybe(idle.then_some(Event::PickIcon)),
            button(text("Remove").size(12))
                .on_press_maybe((idle && form.icon.is_some()).then_some(Event::RemoveIcon)),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
        button(text("Create server").size(13))
            .on_press_maybe((idle && form.name.trim().chars().count() >= 2).then_some(Event::Create)),
        small(&format!(
            "A name has 2 to {MAX_GUILD_NAME_CHARS} characters. The server uses Discord's default setup."
        )),
    ]
    .spacing(6)
    .into()
}

fn menu_view<'a>(
    guild: Snowflake,
    admin: Option<&'a GuildAdmin>,
    idle: bool,
) -> Element<'a, Event> {
    let admin = admin.filter(|a| a.id == guild);
    let mut buttons = row![].spacing(6);
    if let Some(admin) = admin {
        buttons =
            buttons
                .push(button(text("Server settings").size(13)).on_press_maybe(
                    (idle && admin.can_manage_guild).then_some(Event::Settings(guild)),
                ))
                .push(
                    button(text("Create channel").size(13)).on_press_maybe(
                        (idle && admin.can_manage_channels)
                            .then_some(Event::CreateChannel(guild, None)),
                    ),
                )
                .push(button(text("Roles").size(13)).on_press_maybe(
                    (idle && admin.can_manage_roles).then_some(Event::Roles(guild)),
                ))
                .push(button(text("AutoMod").size(13)).on_press_maybe(
                    (idle && admin.can_manage_guild).then_some(Event::AutoMod(guild)),
                ))
                .push(
                    button(text("Bans").size(13)).on_press_maybe(
                        (idle && admin.mine.contains(Permissions::BAN_MEMBERS))
                            .then_some(Event::Bans(guild)),
                    ),
                )
                .push(
                    button(text("Audit log").size(13)).on_press_maybe(
                        (idle && admin.mine.contains(Permissions::VIEW_AUDIT_LOG))
                            .then_some(Event::Audit(guild)),
                    ),
                )
                .push(
                    button(text("Emoji and sounds").size(13)).on_press_maybe(
                        (idle
                            && (admin.mine.contains(Permissions::CREATE_GUILD_EXPRESSIONS)
                                || admin.mine.contains(Permissions::MANAGE_GUILD_EXPRESSIONS)))
                        .then_some(Event::Assets(guild)),
                    ),
                );
    }
    buttons = buttons.push(
        button(text("Leave server").size(13))
            .on_press_maybe(idle.then_some(Event::Leave(guild)))
            .style(button::danger),
    );
    let mut panel = column![
        header(
            admin.map_or_else(|| "Server".to_owned(), |a| a.name.clone()),
            idle
        ),
        buttons,
    ]
    .spacing(6);
    match admin {
        None => panel = panel.push(small("Your roles in this server are not known yet.")),
        Some(a) if !a.can_manage_guild && !a.can_manage_channels => {
            panel = panel.push(small("You can manage neither the server nor its channels."));
        }
        _ => {}
    }
    panel.into()
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
) -> Option<Element<'a, Event>> {
    let idle = state.busy.is_none();
    let admin = navigation.admin.as_ref();
    let mut panel = column![].spacing(6);
    match &state.panel {
        Some(Panel::Menu(guild)) => panel = panel.push(menu_view(*guild, admin, idle)),
        Some(Panel::NewServer(form)) => panel = panel.push(new_server_view(form, idle)),
        Some(Panel::Server(form)) => panel = panel.push(server_view(form, admin, idle)),
        Some(Panel::NewChannel(form)) => panel = panel.push(new_channel_view(form, admin, idle)),
        Some(Panel::Channel(form)) => {
            let regions = state
                .regions
                .as_ref()
                .filter(|(g, _)| *g == form.guild)
                .map(|(_, r)| r.as_slice());
            panel = panel.push(channel_view(
                form,
                admin,
                regions,
                navigation.admin_member.as_ref(),
                idle,
            ));
        }
        Some(Panel::Roles(form)) => panel = panel.push(access::roles_view(form, admin, idle)),
        Some(Panel::MemberRoles(guild, user)) => {
            panel = panel.push(access::member_roles_view(
                *guild,
                *user,
                admin,
                navigation.admin_member.as_ref(),
                idle,
            ));
        }
        None => {}
    }
    if state.busy.is_some() {
        panel = panel.push(small("Waiting for Discord\u{2026}"));
    }
    if let Some(notice) = &state.notice {
        panel = panel.push(
            row![
                text(notice.clone()).size(12).width(Length::Fill),
                button(text("Dismiss").size(12))
                    .on_press_maybe(state.panel.is_none().then_some(Event::Close)),
            ]
            .spacing(6),
        );
    }
    (state.panel.is_some() || state.notice.is_some())
        .then(|| container(panel).padding([6, 0]).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::Refusal;
    use fastcord_discord::StatusCode;
    use fastcord_discord::state::navigation::{
        ABOVE_YOUR_ROLE, AdminMember, AdminRole, EVERYONE_ROLE,
    };
    use fastcord_model::{PermissionOverwrite, Permissions};

    fn client() -> RestClient {
        RestClient::new(
            fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
            fastcord_discord::gateway::ClientProperties::web(
                fastcord_discord::gateway::HostOs::Linux,
                "en-US",
                1,
            ),
        )
        .unwrap()
    }

    fn channel(id: u64, kind: AdminKind, name: &str, parent: Option<u64>) -> AdminChannel {
        AdminChannel {
            id: Snowflake(id),
            name: name.into(),
            kind,
            parent_id: parent.map(Snowflake),
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
            can_manage_permissions: true,
            mine: Permissions(VIEW | SEND | MANAGE_ROLES),
            synced: None,
            everyone_view: true,
        }
    }

    const VIEW: u64 = 1 << 10;
    const SEND: u64 = 1 << 11;
    const MANAGE_ROLES: u64 = 1 << 28;

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
            everyone: id == 1,
            edit_lock: None,
            move_lock: (id == 1).then_some(EVERYONE_ROLE),
            assign_lock: (id == 1).then_some(EVERYONE_ROLE),
        }
    }

    fn overwrite(id: u64, kind: OverwriteKind, allow: u64, deny: u64) -> PermissionOverwrite {
        PermissionOverwrite {
            id: Snowflake(id),
            kind,
            allow: Permissions(allow),
            deny: Permissions(deny),
        }
    }

    pub(super) fn admin(
        owner: bool,
        manage_guild: bool,
        manage_channels: bool,
        community: bool,
    ) -> GuildAdmin {
        GuildAdmin {
            id: Snowflake(1),
            name: "Home".into(),
            has_icon: false,
            owner,
            community,
            description: "About".into(),
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
            can_manage_guild: manage_guild,
            can_manage_channels: manage_channels,
            channels: vec![
                channel(10, AdminKind::Category, "Talk", None),
                channel(11, AdminKind::Text, "general", Some(10)),
                channel(12, AdminKind::Voice, "Lounge", Some(10)),
            ],
            people: vec![(Snowflake(7), "Nell".into())],
            can_manage_roles: true,
            // Highest first, as the reducer lists them: 23 is above the
            // account's top role (5) and locked, 20 and 24 are below it.
            roles: vec![
                AdminRole {
                    edit_lock: Some(ABOVE_YOUR_ROLE),
                    move_lock: Some(ABOVE_YOUR_ROLE),
                    assign_lock: Some(ABOVE_YOUR_ROLE),
                    ..role(23, 8, 0)
                },
                role(20, 2, SEND),
                role(24, 1, 0),
                role(1, 0, VIEW | SEND),
            ],
            mine: Permissions(VIEW | SEND | MANAGE_ROLES),
            grant_all: owner,
            top_position: 5,
        }
    }

    fn nav(admin: GuildAdmin) -> NavigationSnapshot {
        NavigationSnapshot {
            admin: Some(admin),
            ..NavigationSnapshot::default()
        }
    }

    fn server(nav: &NavigationSnapshot) -> State {
        let mut state = State::default();
        let _ = state.update(Event::Settings(Snowflake(1)), nav, &client());
        state.sync(nav);
        state
    }

    fn type_into(state: &mut State, field: Field, value: &str, nav: &NavigationSnapshot) {
        let _ = state.update(Event::Input(field, value.to_owned()), nav, &client());
    }

    #[test]
    fn a_server_form_fills_from_the_read_model_and_sends_only_differences() {
        let nav = nav(admin(false, true, false, false));
        let mut state = server(&nav);
        let Some(Panel::Server(form)) = &state.panel else {
            panic!("no panel")
        };
        assert!(form.loaded);
        assert_eq!(form.edit(nav.admin.as_ref().unwrap()), GuildEdit::default());
        // Nothing changed: a save sends nothing.
        let task = state.update(Event::Save, &nav, &client());
        assert_eq!(task.units(), 0);
        assert_eq!(state.notice.as_deref(), Some("Nothing to change."));
        assert!(state.busy.is_none());
        type_into(&mut state, Field::ServerName, "  Renamed ", &nav);
        let _ = state.update(Event::Choose(Choice::Notifications(1)), &nav, &client());
        let Some(Panel::Server(form)) = &state.panel else {
            panic!()
        };
        let edit = form.edit(nav.admin.as_ref().unwrap());
        assert_eq!(edit.name.as_deref(), Some("Renamed"));
        assert_eq!(edit.notifications, Some(1));
        assert!(edit.description.is_none() && edit.afk_timeout.is_none());
    }

    #[test]
    fn safety_settings_send_only_what_changed_and_mfa_is_for_the_owner() {
        let owner = nav(admin(true, true, true, false));
        let nav = nav(admin(false, true, false, false));
        let mut state = server(&nav);
        let _ = state.update(Event::Choose(Choice::Verification(4)), &nav, &client());
        let _ = state.update(
            Event::Choose(Choice::Alerts(Some(Snowflake(11)))),
            &nav,
            &client(),
        );
        let Some(Panel::Server(form)) = &state.panel else {
            panic!("no panel")
        };
        let edit = form.edit(nav.admin.as_ref().unwrap());
        assert_eq!(edit.verification_level, Some(4));
        assert_eq!(edit.safety_alerts_channel, Some(Some(Snowflake(11))));
        assert!(edit.name.is_none() && edit.content_filter.is_none());
        assert_eq!(state.update(Event::Save, &nav, &client()).units(), 1);
        // A manager who is not the owner cannot change the two-factor rule.
        let mut state = server(&nav);
        let _ = state.update(Event::Ask(Ask::Mfa), &nav, &client());
        assert!(matches!(
            state.panel,
            Some(Panel::Server(ref f)) if matches!(f.danger, Danger::None)
        ));
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 0);
        // The owner must Confirm: asking sends nothing, Cancel drops it.
        let mut state = server(&owner);
        let _ = state.update(Event::Ask(Ask::Mfa), &owner, &client());
        let _ = state.update(Event::Cancel, &owner, &client());
        assert_eq!(state.update(Event::Confirm, &owner, &client()).units(), 0);
        assert!(state.busy.is_none());
        let _ = state.update(Event::Ask(Ask::Mfa), &owner, &client());
        // Ownership is checked again at the Confirm.
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 0);
        assert!(state.notice.as_deref().unwrap().contains("owner"));
        let _ = state.update(Event::Ask(Ask::Mfa), &owner, &client());
        assert_eq!(state.update(Event::Confirm, &owner, &client()).units(), 1);
        assert_eq!(state.busy, Some(Action::Mfa));
        let _ = state.update(Event::Ask(Ask::Mfa), &owner, &client());
        assert_eq!(state.update(Event::Confirm, &owner, &client()).units(), 0);
        let serial = state.serial;
        let demand = AdminRequestError::Rest(RestError::Refused(Refusal::new(
            StatusCode::FORBIDDEN,
            Some(60003),
            "x",
        )));
        let _ = state.update(
            Event::Done(serial, Action::Mfa, Err(demand)),
            &owner,
            &client(),
        );
        assert!(state.busy.is_none());
        assert!(state.notice.as_deref().unwrap().contains("two-factor"));
    }

    #[test]
    fn a_write_is_sent_once_and_the_next_click_waits() {
        let nav = nav(admin(false, true, false, false));
        let mut state = server(&nav);
        type_into(&mut state, Field::ServerName, "Renamed", &nav);
        let first = state.update(Event::Save, &nav, &client());
        assert_eq!(first.units(), 1);
        assert_eq!(state.busy, Some(Action::SaveServer));
        let second = state.update(Event::Save, &nav, &client());
        assert_eq!(second.units(), 0, "no duplicate in flight");
        assert!(state.notice.as_deref().unwrap().contains("Wait"));
        // Typing is still allowed, and the form survives a failure.
        type_into(&mut state, Field::Description, "New text", &nav);
        let serial = state.serial;
        let _ = state.update(
            Event::Done(
                serial,
                Action::SaveServer,
                Err(AdminRequestError::Rest(RestError::PermissionDenied)),
            ),
            &nav,
            &client(),
        );
        assert!(state.busy.is_none());
        assert!(state.notice.as_deref().unwrap().contains("refused"));
        let Some(Panel::Server(form)) = &state.panel else {
            panic!()
        };
        assert_eq!(
            (form.name.as_str(), form.description.as_str()),
            ("Renamed", "New text")
        );
    }

    #[test]
    fn permissions_are_checked_again_at_the_click() {
        // Without Manage Server nothing is sent, even with the form loaded.
        let denied = nav(admin(false, false, false, false));
        let mut state = server(&denied);
        type_into(&mut state, Field::ServerName, "Renamed", &denied);
        assert_eq!(state.update(Event::Save, &denied, &client()).units(), 0);
        assert!(state.notice.as_deref().unwrap().contains("Manage Server"));
        // Permission lost between typing and the click: refused locally.
        let allowed = nav(admin(false, true, false, false));
        let mut state = server(&allowed);
        type_into(&mut state, Field::ServerName, "Renamed", &allowed);
        assert_eq!(state.update(Event::Save, &denied, &client()).units(), 0);
        assert!(state.busy.is_none());
        // The channel list can be reordered only with Manage Channels.
        assert_eq!(
            state
                .update(Event::Move(Snowflake(11), true), &denied, &client())
                .units(),
            0
        );
    }

    #[test]
    fn deleting_a_server_needs_the_owner_and_the_typed_name() {
        let nav = nav(admin(true, true, true, false));
        let mut state = server(&nav);
        let _ = state.update(Event::Ask(Ask::DeleteServer), &nav, &client());
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 0);
        type_into(&mut state, Field::DeleteTyped, "home", &nav);
        assert_eq!(
            state.update(Event::Confirm, &nav, &client()).units(),
            0,
            "exact name only"
        );
        assert!(state.notice.as_deref().unwrap().contains("exact name"));
        type_into(&mut state, Field::DeleteTyped, "Home", &nav);
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 1);
        assert_eq!(state.busy, Some(Action::DeleteServer));
        // A member who is not the owner cannot even open the confirmation.
        let member = self::nav(admin(false, true, true, false));
        let mut state = server(&member);
        let _ = state.update(Event::Ask(Ask::DeleteServer), &member, &client());
        let Some(Panel::Server(form)) = &state.panel else {
            panic!()
        };
        assert!(matches!(form.danger, Danger::None));
        assert_eq!(state.update(Event::Confirm, &member, &client()).units(), 0);
    }

    #[test]
    fn transferring_names_a_member_and_asks_to_confirm() {
        let nav = nav(admin(true, true, true, false));
        let mut state = server(&nav);
        let _ = state.update(Event::Ask(Ask::Transfer), &nav, &client());
        assert_eq!(
            state.update(Event::Confirm, &nav, &client()).units(),
            0,
            "no target yet"
        );
        let _ = state.update(
            Event::Choose(Choice::Transfer(Snowflake(7), "Nell".into())),
            &nav,
            &client(),
        );
        // Someone who is not a held member of the server is refused.
        let _ = state.update(
            Event::Choose(Choice::Transfer(Snowflake(8), "Ghost".into())),
            &nav,
            &client(),
        );
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 0);
        let _ = state.update(
            Event::Choose(Choice::Transfer(Snowflake(7), "Nell".into())),
            &nav,
            &client(),
        );
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 1);
        assert_eq!(state.busy, Some(Action::Transfer));
    }

    #[test]
    fn channels_of_community_kinds_wait_for_the_feature() {
        let plain = nav(admin(false, false, true, false));
        let mut state = State::default();
        let _ = state.update(
            Event::CreateChannel(Snowflake(1), Some(Snowflake(10))),
            &plain,
            &client(),
        );
        let _ = state.update(
            Event::Choose(Choice::Kind(AdminKind::Stage)),
            &plain,
            &client(),
        );
        type_into(&mut state, Field::NewName, "town hall", &plain);
        assert_eq!(state.update(Event::Create, &plain, &client()).units(), 0);
        assert!(state.notice.as_deref().unwrap().contains("Community"));
        let community = nav(admin(false, false, true, true));
        assert_eq!(
            state.update(Event::Create, &community, &client()).units(),
            1
        );
        assert_eq!(state.busy, Some(Action::CreateChannel));
        // Without Manage Channels nothing is sent at all.
        let mut state = State::default();
        let _ = state.update(Event::CreateChannel(Snowflake(1), None), &plain, &client());
        type_into(&mut state, Field::NewName, "chat", &plain);
        let none = nav(admin(false, true, false, true));
        assert_eq!(state.update(Event::Create, &none, &client()).units(), 0);
    }

    #[test]
    fn a_channel_edit_covers_only_the_settings_of_its_kind() {
        let nav = nav(admin(false, false, true, false));
        let mut state = State::default();
        let _ = state.update(
            Event::EditChannel(Snowflake(1), Snowflake(12), true),
            &nav,
            &client(),
        );
        state.sync(&nav);
        type_into(&mut state, Field::UserLimit, "100", &nav);
        assert_eq!(state.update(Event::Save, &nav, &client()).units(), 0);
        assert!(state.notice.as_deref().unwrap().contains("1 to 99"));
        type_into(&mut state, Field::UserLimit, "5", &nav);
        let _ = state.update(Event::Choose(Choice::Slowmode(21_600)), &nav, &client());
        let Some(Panel::Channel(form)) = &state.panel else {
            panic!()
        };
        let current = nav.admin.as_ref().unwrap().channel(Snowflake(12)).unwrap();
        let edit = form.edit(current).unwrap();
        assert_eq!((edit.user_limit, edit.slowmode), (Some(5), Some(21_600)));
        assert!(
            edit.topic.is_none() && edit.nsfw.is_none(),
            "voice has no topic"
        );
        assert_eq!(state.update(Event::Save, &nav, &client()).units(), 1);
        // Deleting asks first, names the target and sends once.
        let mut state = State::default();
        let _ = state.update(
            Event::EditChannel(Snowflake(1), Snowflake(11), false),
            &nav,
            &client(),
        );
        state.sync(&nav);
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 0);
        let _ = state.update(Event::Ask(Ask::DeleteChannel), &nav, &client());
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 1);
        assert_eq!(
            state.update(Event::Confirm, &nav, &client()).units(),
            0,
            "one request in flight"
        );
    }

    #[test]
    fn the_bitrate_choices_stop_at_what_the_server_allows() {
        assert_eq!(
            bitrate_options(None),
            [8_000, 16_000, 32_000, 48_000, 64_000]
        );
        assert_eq!(
            bitrate_options(Some(24_000)),
            [8_000, 16_000, 32_000, 48_000, 64_000, 24_000]
        );
        assert_eq!(
            bitrate_options(Some(96_000)),
            [8_000, 16_000, 32_000, 48_000, 64_000, 96_000]
        );
    }

    #[test]
    fn a_stale_answer_from_an_earlier_session_changes_nothing() {
        let nav = nav(admin(false, true, true, false));
        let mut state = server(&nav);
        let old = state.serial;
        state.clear();
        let _ = state.update(Event::Settings(Snowflake(1)), &nav, &client());
        let _ = state.update(
            Event::Done(
                old,
                Action::SaveServer,
                Err(AdminRequestError::Rest(RestError::ResourceGone)),
            ),
            &nav,
            &client(),
        );
        assert!(state.notice.is_none());
    }

    #[test]
    fn failures_say_what_happened_including_challenges_and_limits() {
        let text = |e| describe(AdminRequestError::Rest(e));
        assert!(
            text(RestError::AuthenticationRequired).contains("Log in")
                || text(RestError::AuthenticationRequired).contains("expired")
        );
        assert!(text(RestError::PermissionDenied).contains("permission"));
        assert!(text(RestError::ResourceGone).contains("no longer exists"));
        assert!(text(RestError::Http(StatusCode::TOO_MANY_REQUESTS)).contains("rate limiting"));
        assert!(text(RestError::AccountVerificationRequired).contains("paused"));
        assert!(
            text(RestError::Retryable(
                fastcord_discord::RetryableFailure::Network(
                    fastcord_discord::NetworkFailure::Connection
                )
            ))
            .contains("may have been applied")
        );
        let mfa = RestError::Refused(Refusal::new(
            StatusCode::BAD_REQUEST,
            Some(60003),
            "Two factor is required for this operation",
        ));
        let message = text(mfa);
        assert!(message.contains("two-factor") && message.contains("official Discord client"));
        let missing = RestError::Refused(Refusal::new(
            StatusCode::NOT_FOUND,
            Some(10003),
            "Unknown Channel",
        ));
        assert!(text(missing).contains("no longer exists"));
        assert!(State::is_rejected(&Event::Done(
            0,
            Action::Move,
            Err(AdminRequestError::Rest(RestError::AuthenticationRequired))
        )));
    }

    #[test]
    fn every_panel_builds() {
        let nav = nav(admin(true, true, true, true));
        let mut state = State::default();
        for event in [
            Event::NewServer,
            Event::Menu(Snowflake(1)),
            Event::Settings(Snowflake(1)),
            Event::CreateChannel(Snowflake(1), None),
            Event::EditChannel(Snowflake(1), Snowflake(11), false),
            Event::EditChannel(Snowflake(1), Snowflake(12), true),
            Event::EditChannel(Snowflake(1), Snowflake(10), false),
        ] {
            let _ = state.update(event, &nav, &client());
            state.sync(&nav);
            assert!(view(&state, &nav).is_some());
        }
        let _ = state.update(Event::Settings(Snowflake(1)), &nav, &client());
        state.sync(&nav);
        let _ = state.update(Event::Ask(Ask::Transfer), &nav, &client());
        assert!(view(&state, &nav).is_some());
        let _ = state.update(Event::Ask(Ask::DeleteServer), &nav, &client());
        assert!(view(&state, &nav).is_some());
        assert_eq!(state.wanted(), Some(Snowflake(1)));
        state.clear();
        assert!(view(&state, &nav).is_none() && state.wanted().is_none());
    }

    // ----- roles and channel access (issue #94) -----

    fn roles(nav: &NavigationSnapshot) -> State {
        let mut state = State::default();
        let _ = state.update(Event::Roles(Snowflake(1)), nav, &client());
        state
    }

    fn done(state: &mut State, action: Action, result: Result<(), AdminRequestError>) {
        let nav = NavigationSnapshot::default();
        let serial = state.serial;
        let _ = state.update(Event::Done(serial, action, result), &nav, &client());
    }

    #[test]
    fn a_role_edit_is_sent_once_with_only_what_changed() {
        let nav = nav(admin(false, false, false, false));
        let mut state = roles(&nav);
        let _ = state.update(Event::PickRole(Snowflake(20)), &nav, &client());
        // Nothing changed: nothing is sent.
        assert_eq!(state.update(Event::Save, &nav, &client()).units(), 0);
        assert_eq!(state.notice.as_deref(), Some("Nothing to change."));
        type_into(&mut state, Field::RoleName, " Helpers ", &nav);
        let _ = state.update(Event::Hoist(true), &nav, &client());
        let _ = state.update(Event::Bit(1), &nav, &client());
        assert_eq!(state.update(Event::Save, &nav, &client()).units(), 1);
        assert_eq!(state.busy, Some(Action::SaveRole));
        assert_eq!(state.update(Event::Save, &nav, &client()).units(), 0);
        assert!(state.notice.as_deref().unwrap().contains("Wait"));
        // A rejection keeps the form, says why, and leaves it free again.
        done(
            &mut state,
            Action::SaveRole,
            Err(AdminRequestError::Rest(RestError::PermissionDenied)),
        );
        assert!(state.busy.is_none());
        assert!(state.notice.as_deref().unwrap().contains("refused"));
        let Some(Panel::Roles(form)) = &state.panel else {
            panic!()
        };
        assert_eq!(form.name, " Helpers ");
    }

    #[test]
    fn the_role_form_cannot_grant_what_the_account_lacks_or_touch_locked_roles() {
        let nav = nav(admin(false, false, false, false));
        let mut state = roles(&nav);
        let _ = state.update(Event::PickRole(Snowflake(20)), &nav, &client());
        // Ban Members (bit 2) is not held: the toggle does nothing.
        let _ = state.update(Event::Bit(2), &nav, &client());
        let Some(Panel::Roles(form)) = &state.panel else {
            panic!()
        };
        assert_eq!(form.perms, Permissions(SEND));
        // A role above the account is loaded but cannot be saved.
        let _ = state.update(Event::PickRole(Snowflake(23)), &nav, &client());
        type_into(&mut state, Field::RoleName, "Mine now", &nav);
        assert_eq!(state.update(Event::Save, &nav, &client()).units(), 0);
        assert_eq!(state.notice.as_deref(), Some(ABOVE_YOUR_ROLE));
        // Permission lost between loading and the click: refused again.
        let mut weaker = admin(false, false, false, false);
        weaker.can_manage_roles = false;
        for role in &mut weaker.roles {
            role.edit_lock = Some("You need Manage Roles.");
        }
        let weaker = self::nav(weaker);
        let _ = state.update(Event::PickRole(Snowflake(20)), &nav, &client());
        type_into(&mut state, Field::RoleName, "Other", &nav);
        assert_eq!(state.update(Event::Save, &weaker, &client()).units(), 0);
        assert!(state.busy.is_none());
    }

    #[test]
    fn deleting_a_role_needs_a_confirm_and_never_applies_to_everyone() {
        let nav = nav(admin(false, false, false, false));
        let mut state = roles(&nav);
        let _ = state.update(Event::PickRole(Snowflake(20)), &nav, &client());
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 0);
        let _ = state.update(Event::Ask(Ask::DeleteRole), &nav, &client());
        let _ = state.update(Event::Cancel, &nav, &client());
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 0);
        let _ = state.update(Event::Ask(Ask::DeleteRole), &nav, &client());
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 1);
        done(&mut state, Action::DeleteRole, Ok(()));
        let Some(Panel::Roles(form)) = &state.panel else {
            panic!()
        };
        assert!(form.selected.is_none());
        // @everyone is refused locally even if a confirm were forced.
        let _ = state.update(Event::PickRole(Snowflake(1)), &nav, &client());
        let _ = state.update(Event::Ask(Ask::DeleteRole), &nav, &client());
        assert_eq!(state.update(Event::Confirm, &nav, &client()).units(), 0);
        assert_eq!(state.notice.as_deref(), Some(EVERYONE_ROLE));
    }

    #[test]
    fn roles_move_only_between_unlocked_neighbours_and_new_roles_need_manage_roles() {
        let nav = nav(admin(false, false, false, false));
        let mut state = roles(&nav);
        // 20 and 24 swap; 23 (above the account) and @everyone cannot move.
        assert_eq!(
            state
                .update(Event::MoveRole(Snowflake(20), true), &nav, &client())
                .units(),
            1
        );
        done(&mut state, Action::MoveRole, Ok(()));
        for locked in [23, 1] {
            assert_eq!(
                state
                    .update(Event::MoveRole(Snowflake(locked), true), &nav, &client())
                    .units(),
                0
            );
        }
        assert_eq!(state.update(Event::NewRole, &nav, &client()).units(), 1);
        done(&mut state, Action::CreateRole, Ok(()));
        let mut weaker = admin(false, false, false, false);
        weaker.can_manage_roles = false;
        assert_eq!(
            state
                .update(Event::NewRole, &self::nav(weaker), &client())
                .units(),
            0
        );
    }

    fn member_nav(roles: Vec<u64>, lock: Option<&'static str>) -> NavigationSnapshot {
        let mut nav = nav(admin(false, false, false, false));
        nav.admin_member = Some(AdminMember {
            user: Snowflake(7),
            name: "Nell".into(),
            me: false,
            roles: roles.into_iter().map(Snowflake).collect(),
            lock,
            effective: vec![(Snowflake(11), Permissions(VIEW))],
        });
        nav
    }

    #[test]
    fn one_click_gives_or_takes_one_member_role_and_the_hierarchy_applies() {
        let nav = member_nav(vec![20], None);
        let mut state = State::default();
        let _ = state.update(
            Event::MemberRoles(Snowflake(1), Snowflake(7)),
            &nav,
            &client(),
        );
        assert_eq!(state.wanted_member(), Some(Snowflake(7)));
        // Taking a role the member has, and giving one they lack: one request each.
        assert_eq!(
            state
                .update(
                    Event::ToggleMemberRole(Snowflake(20), false),
                    &nav,
                    &client()
                )
                .units(),
            1
        );
        assert_eq!(state.busy, Some(Action::MemberRole));
        assert_eq!(
            state
                .update(
                    Event::ToggleMemberRole(Snowflake(24), true),
                    &nav,
                    &client()
                )
                .units(),
            0,
            "the next click waits for the answer"
        );
        done(
            &mut state,
            Action::MemberRole,
            Err(AdminRequestError::Rest(RestError::Retryable(
                fastcord_discord::RetryableFailure::Network(
                    fastcord_discord::NetworkFailure::Connection,
                ),
            ))),
        );
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("may have been applied")
        );
        assert_eq!(
            state
                .update(
                    Event::ToggleMemberRole(Snowflake(24), true),
                    &nav,
                    &client()
                )
                .units(),
            1
        );
        done(&mut state, Action::MemberRole, Ok(()));
        // Already in the asked state, above the account, or a locked member: nothing.
        for (role, give) in [(20, true), (23, true), (1, true)] {
            assert_eq!(
                state
                    .update(
                        Event::ToggleMemberRole(Snowflake(role), give),
                        &nav,
                        &client()
                    )
                    .units(),
                0
            );
        }
        let locked = member_nav(
            vec![],
            Some("This member's highest role is not below yours."),
        );
        assert_eq!(
            state
                .update(
                    Event::ToggleMemberRole(Snowflake(24), true),
                    &locked,
                    &client()
                )
                .units(),
            0
        );
    }

    fn channel_nav(channel_overwrites: Vec<PermissionOverwrite>) -> NavigationSnapshot {
        let mut admin = admin(false, false, true, false);
        admin.channels[0].overwrites = vec![
            overwrite(20, OverwriteKind::Role, 0, SEND),
            overwrite(24, OverwriteKind::Role, 0, VIEW),
        ];
        admin.channels[1].overwrites = channel_overwrites;
        admin.channels[1].synced = Some(false);
        nav(admin)
    }

    fn editing(nav: &NavigationSnapshot) -> State {
        let mut state = State::default();
        let _ = state.update(
            Event::EditChannel(Snowflake(1), Snowflake(11), false),
            nav,
            &client(),
        );
        state.sync(nav);
        state
    }

    #[test]
    fn an_overwrite_is_edited_per_bit_and_written_with_one_request() {
        let nav = channel_nav(vec![overwrite(20, OverwriteKind::Role, 0, SEND)]);
        let mut state = editing(&nav);
        let target = Some((Snowflake(20), OverwriteKind::Role));
        let _ = state.update(Event::PickTarget(target), &nav, &client());
        // Nothing changed yet.
        assert_eq!(
            state.update(Event::SaveOverwrite, &nav, &client()).units(),
            0
        );
        assert_eq!(state.notice.as_deref(), Some("Nothing to change."));
        // Deny becomes Allow for a bit the account holds, and Allow of one it lacks stays neutral.
        let _ = state.update(Event::Tri(11, Tri::Allow), &nav, &client());
        let _ = state.update(Event::Tri(2, Tri::Allow), &nav, &client());
        let Some(Panel::Channel(form)) = &state.panel else {
            panic!()
        };
        assert_eq!(
            (form.access.allow, form.access.deny),
            (Permissions(SEND), Permissions::NONE)
        );
        assert_eq!(
            state.update(Event::SaveOverwrite, &nav, &client()).units(),
            1
        );
        assert_eq!(state.busy, Some(Action::PutOverwrite));
        assert_eq!(
            state.update(Event::SaveOverwrite, &nav, &client()).units(),
            0
        );
        done(&mut state, Action::PutOverwrite, Ok(()));
        assert!(state.notice.as_deref().unwrap().starts_with("Sent"));
    }

    #[test]
    fn adding_removing_and_syncing_overwrites_are_gated_and_confirmed() {
        let nav = channel_nav(Vec::new());
        let mut state = editing(&nav);
        // A new target starts empty; saving it empty sends nothing.
        let target = Some((Snowflake(24), OverwriteKind::Role));
        let _ = state.update(Event::PickTarget(target), &nav, &client());
        assert_eq!(
            state.update(Event::SaveOverwrite, &nav, &client()).units(),
            0
        );
        let _ = state.update(Event::Tri(10, Tri::Deny), &nav, &client());
        assert_eq!(
            state.update(Event::SaveOverwrite, &nav, &client()).units(),
            1
        );
        done(&mut state, Action::PutOverwrite, Ok(()));
        // Removing needs an existing overwrite and a Confirm.
        let existing = channel_nav(vec![overwrite(24, OverwriteKind::Role, 0, VIEW)]);
        let _ = state.update(Event::Confirm, &existing, &client());
        assert!(state.busy.is_none());
        let _ = state.update(Event::Ask(Ask::RemoveOverwrite), &existing, &client());
        let _ = state.update(Event::Cancel, &existing, &client());
        assert_eq!(
            state.update(Event::Confirm, &existing, &client()).units(),
            0
        );
        let _ = state.update(Event::Ask(Ask::RemoveOverwrite), &existing, &client());
        assert_eq!(
            state.update(Event::Confirm, &existing, &client()).units(),
            1
        );
        done(&mut state, Action::DeleteOverwrite, Ok(()));
        // Sync sends the category's overwrites once, only when they differ.
        assert_eq!(state.update(Event::Sync, &nav, &client()).units(), 1);
        done(&mut state, Action::SyncOverwrites, Ok(()));
        let mut synced = admin(false, false, true, false);
        synced.channels[1].synced = Some(true);
        assert_eq!(
            state
                .update(Event::Sync, &self::nav(synced), &client())
                .units(),
            0
        );
        assert!(state.notice.as_deref().unwrap().contains("Already synced"));
    }

    #[test]
    fn overwrites_need_manage_roles_in_the_channel_at_the_click() {
        let nav = channel_nav(vec![overwrite(20, OverwriteKind::Role, 0, SEND)]);
        let mut state = editing(&nav);
        let _ = state.update(
            Event::PickTarget(Some((Snowflake(20), OverwriteKind::Role))),
            &nav,
            &client(),
        );
        let _ = state.update(Event::Tri(10, Tri::Deny), &nav, &client());
        let mut denied = admin(false, false, true, false);
        denied.channels[1].can_manage_permissions = false;
        denied.channels[1].synced = Some(false);
        let denied = self::nav(denied);
        for event in [Event::SaveOverwrite, Event::Sync] {
            assert_eq!(state.update(event, &denied, &client()).units(), 0);
        }
        assert!(state.busy.is_none());
    }

    #[test]
    fn inspecting_a_member_asks_the_reducer_and_sends_nothing() {
        let nav = member_nav(vec![20], None);
        let mut state = editing(&nav);
        assert_eq!(state.wanted_member(), None);
        let task = state.update(Event::Inspect(Some(Snowflake(7))), &nav, &client());
        assert_eq!(task.units(), 0);
        assert_eq!(state.wanted_member(), Some(Snowflake(7)));
        let _ = state.update(Event::Inspect(None), &nav, &client());
        assert_eq!(state.wanted_member(), None);
        assert!(state.busy.is_none());
    }

    #[test]
    fn a_stale_answer_after_clear_changes_nothing() {
        let nav = nav(admin(false, false, false, false));
        let mut state = roles(&nav);
        let _ = state.update(Event::NewRole, &nav, &client());
        let stale = state.serial;
        state.clear();
        let _ = state.update(
            Event::Done(
                stale,
                Action::CreateRole,
                Err(AdminRequestError::Rest(RestError::PermissionDenied)),
            ),
            &nav,
            &client(),
        );
        assert!(state.notice.is_none() && state.panel.is_none());
    }
}
