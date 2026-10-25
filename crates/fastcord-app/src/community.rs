//! Configuring how a server welcomes members (SPEC §16, issue #97): the rules
//! screening, the onboarding questions with default channels and role
//! assignments, the server guide, and turning Community on or off. This is the
//! administrator side of `onboarding` (#91).
//!
//! Each editor loads what Discord holds when the panel opens, keeps a baseline
//! next to the draft (so "Unsaved changes" and Discard are exact), and sends
//! the whole object in one request on one click. At most one write is in
//! flight, none is retried, and the form keeps what was typed after any
//! failure. What the account may do comes from the reducer's `GuildAdmin`
//! (Manage Server; roles only from the assignable ones) and is checked again at
//! the click. After a save the editor shows Discord's answer (or loads it
//! again when the answer cannot be read); Community shows what `GUILD_UPDATE`
//! brings. Discord's validation messages are shown as it wrote them.

use fastcord_discord::state::navigation::{AdminKind, GuildAdmin, NavigationSnapshot};
use fastcord_discord::{
    CommunityError, CommunityRequestError, GuideDoc, GuideEntry, Known,
    MAX_ENTRY_DESCRIPTION_CHARS, MAX_ENTRY_TITLE_CHARS, MAX_NEW_MEMBER_ACTIONS,
    MAX_OPTION_DESCRIPTION_CHARS, MAX_OPTION_TITLE_CHARS, MAX_OPTIONS_EDITED,
    MAX_PROMPT_TITLE_CHARS, MAX_PROMPTS_EDITED, MAX_RESOURCE_CHANNELS, MAX_RULE_CHARS,
    MAX_RULES_DESCRIPTION_CHARS, MAX_RULES_EDITED, MAX_WELCOME_CHARS, ONBOARDING_MODES,
    OnboardingDoc, OptionDoc, PromptDoc, RestClient, RestError, RulesEdit, Screening,
    VERIFICATION_LEVELS, disable_community_body, enable_community_body, fresh_id,
};
use fastcord_model::Snowflake;
use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, space, text, text_input,
};
use iced::{Element, Length, Task};

use crate::i18n::{tr, trf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Rules,
    Onboarding,
    Guide,
    Community,
}

impl Tab {
    const ALL: [Self; 4] = [Self::Rules, Self::Onboarding, Self::Guide, Self::Community];

    fn label(self) -> &'static str {
        match self {
            Self::Rules => "Rules",
            Self::Onboarding => "Onboarding",
            Self::Guide => "Server guide",
            Self::Community => "Community",
        }
    }
}

#[derive(Clone, Debug)]
pub enum RulesMsg {
    Enabled(bool),
    Description(String),
    Rule(usize, String),
    Add,
    Remove(usize),
}

#[derive(Clone, Debug)]
pub enum OnbMsg {
    Enabled(bool),
    Mode(u8),
    ToggleDefault(Snowflake),
    AddPrompt,
    RemovePrompt(usize),
    MovePrompt(usize, bool),
    SelectPrompt(usize),
    PromptTitle(String),
    Single(bool),
    Required(bool),
    InOnboarding(bool),
    AddOption,
    RemoveOption(usize),
    SelectOption(usize),
    OptionTitle(String),
    OptionDescription(String),
    OptionEmoji(String),
    ToggleRole(Snowflake),
    ToggleChannel(Snowflake),
}

/// The two entry lists of the guide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum List {
    Actions,
    Resources,
}

#[derive(Clone, Debug)]
pub enum GuideMsg {
    Welcome(String),
    Add(List, Snowflake),
    Remove(List, usize),
    Title(List, usize, String),
    Description(List, usize, String),
    Emoji(usize, String),
    Channel(List, usize, Snowflake),
}

#[derive(Clone, Debug)]
pub enum CommMsg {
    Rules(Snowflake),
    Updates(Snowflake),
    Level(u8),
}

/// What a save answered.
#[derive(Clone, Debug)]
pub enum Saved {
    Rules(Option<Screening>),
    Onboarding(Option<OnboardingDoc>),
    Guide(Option<GuideDoc>),
    Community,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// "Community setup" of a server's settings.
    Open(Snowflake),
    Close,
    Tab(Tab),
    RulesLoaded(u64, Result<Option<Screening>, RestError>),
    OnboardingLoaded(u64, Result<OnboardingDoc, RestError>),
    GuideLoaded(u64, Result<GuideDoc, RestError>),
    Rules(RulesMsg),
    Onboarding(OnbMsg),
    Guide(GuideMsg),
    Community(CommMsg),
    Discard,
    Save,
    AskDisable,
    ConfirmDisable,
    Cancel,
    Saved(u64, Tab, Result<Saved, CommunityRequestError>),
}

#[derive(Debug)]
enum Slot<T> {
    Loading,
    Ready(T),
    Failed(String),
}

#[derive(Debug)]
struct RulesEd {
    baseline: Option<Screening>,
    saved: RulesEdit,
    edit: RulesEdit,
}

impl RulesEd {
    fn new(baseline: Option<Screening>) -> Self {
        let saved = RulesEdit::from_screening(baseline.as_ref());
        Self {
            edit: saved.clone(),
            saved,
            baseline,
        }
    }
}

#[derive(Debug)]
struct OnbEd {
    baseline: OnboardingDoc,
    doc: OnboardingDoc,
    prompt: Option<usize>,
    option: Option<usize>,
}

impl OnbEd {
    fn new(doc: OnboardingDoc) -> Self {
        Self {
            baseline: doc.clone(),
            doc,
            prompt: None,
            option: None,
        }
    }

    fn prompt_mut(&mut self) -> Option<&mut PromptDoc> {
        self.doc.prompts.get_mut(self.prompt?)
    }

    fn option_mut(&mut self) -> Option<&mut OptionDoc> {
        let at = self.option?;
        self.prompt_mut()?.options.get_mut(at)
    }
}

#[derive(Debug)]
struct GuideEd {
    baseline: GuideDoc,
    doc: GuideDoc,
}

/// The Community form. It follows the server as the Gateway reports it: when
/// the server's values change the form starts over from them.
#[derive(Debug)]
struct CommEd {
    seen: (bool, Option<Snowflake>, Option<Snowflake>, u8),
    rules: Option<Snowflake>,
    updates: Option<Snowflake>,
    level: u8,
    touched: bool,
    confirm_disable: bool,
}

impl CommEd {
    fn of(admin: &GuildAdmin) -> Self {
        Self {
            seen: (
                admin.community,
                admin.rules_channel,
                admin.public_updates_channel,
                admin.verification_level,
            ),
            rules: admin.rules_channel,
            updates: admin.public_updates_channel,
            level: admin.verification_level.clamp(1, 4),
            touched: false,
            confirm_disable: false,
        }
    }
}

#[derive(Debug)]
struct Panel {
    guild: Snowflake,
    tab: Tab,
    rules: Slot<RulesEd>,
    onboarding: Slot<OnbEd>,
    guide: Slot<GuideEd>,
    community: Option<CommEd>,
}

#[derive(Default)]
pub struct State {
    panel: Option<Panel>,
    busy: Option<Tab>,
    notice: Option<String>,
    serial: u64,
    /// Numbers the ids generated for new prompts, answers and entries.
    sequence: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// At most `max` characters of typed text.
fn cut(value: String, max: usize) -> String {
    value.chars().take(max).collect()
}

fn describe(error: CommunityRequestError) -> String {
    match error {
        CommunityRequestError::Invalid(error) => error.to_string(),
        CommunityRequestError::Rest(error) => crate::guild_admin::describe_rest(error),
    }
}

const NEEDS_MANAGE_GUILD: &str = "You need Manage Server to change this.";

/// The ids the account may reference, from the read model.
struct Refs {
    channels: Vec<Snowflake>,
    roles: Vec<Snowflake>,
    assignable: Vec<Snowflake>,
    default_ok: Vec<Snowflake>,
}

impl Refs {
    fn of(admin: &GuildAdmin) -> Self {
        let text_like = |kind| matches!(kind, AdminKind::Text | AdminKind::Announcement);
        Self {
            channels: admin
                .channels
                .iter()
                .filter(|c| c.kind != AdminKind::Category)
                .map(|c| c.id)
                .collect(),
            roles: admin.roles.iter().map(|r| r.id).collect(),
            assignable: admin
                .roles
                .iter()
                .filter(|r| r.assign_lock.is_none())
                .map(|r| r.id)
                .collect(),
            default_ok: admin
                .channels
                .iter()
                .filter(|c| text_like(c.kind) && c.everyone_view)
                .map(|c| c.id)
                .collect(),
        }
    }

    fn known(&self) -> Known<'_> {
        Known {
            channels: &self.channels,
            roles: &self.roles,
            assignable: &self.assignable,
            default_ok: &self.default_ok,
        }
    }
}

impl State {
    /// Logout or account change.
    pub fn clear(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            ..Self::default()
        };
    }

    /// The server the panel needs the read model of.
    pub fn wanted(&self) -> Option<Snowflake> {
        self.panel.as_ref().map(|p| p.guild)
    }

    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::RulesLoaded(_, Err(RestError::AuthenticationRequired))
                | Event::OnboardingLoaded(_, Err(RestError::AuthenticationRequired))
                | Event::GuideLoaded(_, Err(RestError::AuthenticationRequired))
                | Event::Saved(
                    _,
                    _,
                    Err(CommunityRequestError::Rest(
                        RestError::AuthenticationRequired
                    ))
                )
        )
    }

    /// Starts (or restarts, after a change of the server) the Community form
    /// from the read model.
    pub fn sync(&mut self, navigation: &NavigationSnapshot) {
        let Some(panel) = &mut self.panel else {
            return;
        };
        let Some(admin) = navigation.admin.as_ref().filter(|a| a.id == panel.guild) else {
            return;
        };
        let fresh = CommEd::of(admin);
        match &mut panel.community {
            Some(form) if form.seen == fresh.seen => {}
            slot => *slot = Some(fresh),
        }
    }

    fn load_rules(&self, guild: Snowflake, client: &RestClient) -> Task<Event> {
        let (serial, client) = (self.serial, client.clone());
        Task::perform(
            async move { client.screening_for_edit(guild).await },
            move |r| Event::RulesLoaded(serial, r),
        )
    }

    fn load_onboarding(&self, guild: Snowflake, client: &RestClient) -> Task<Event> {
        let (serial, client) = (self.serial, client.clone());
        Task::perform(
            async move { client.onboarding_for_edit(guild).await },
            move |r| Event::OnboardingLoaded(serial, r),
        )
    }

    fn load_guide(&self, guild: Snowflake, client: &RestClient) -> Task<Event> {
        let (serial, client) = (self.serial, client.clone());
        Task::perform(
            async move { client.guide_for_edit(guild).await },
            move |r| Event::GuideLoaded(serial, r),
        )
    }

    /// Whether the editor of `tab` differs from what Discord holds.
    fn dirty(&self, tab: Tab, admin: Option<&GuildAdmin>) -> bool {
        let Some(panel) = &self.panel else {
            return false;
        };
        match tab {
            Tab::Rules => matches!(&panel.rules, Slot::Ready(e) if e.edit != e.saved),
            Tab::Onboarding => matches!(&panel.onboarding, Slot::Ready(e) if e.doc != e.baseline),
            Tab::Guide => matches!(&panel.guide, Slot::Ready(e) if e.doc != e.baseline),
            Tab::Community => {
                admin.is_some_and(|a| !a.community)
                    && panel.community.as_ref().is_some_and(|c| c.touched)
            }
        }
    }

    /// The admin read model, only when the account may change these settings.
    fn allowed<'a>(&self, navigation: &'a NavigationSnapshot) -> Result<&'a GuildAdmin, String> {
        let guild = self.wanted();
        navigation
            .admin
            .as_ref()
            .filter(|a| Some(a.id) == guild && a.can_manage_guild)
            .ok_or_else(|| NEEDS_MANAGE_GUILD.to_owned())
    }

    fn next_id(&mut self) -> Snowflake {
        self.sequence += 1;
        fresh_id(now_ms(), self.sequence)
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        client: &RestClient,
        me: Snowflake,
    ) -> Task<Event> {
        match event {
            Event::Open(guild) => {
                if self.busy.is_some() {
                    return Task::none();
                }
                self.serial = self.serial.wrapping_add(1);
                self.notice = None;
                self.panel = Some(Panel {
                    guild,
                    tab: Tab::Rules,
                    rules: Slot::Loading,
                    onboarding: Slot::Loading,
                    guide: Slot::Loading,
                    community: None,
                });
                self.sync(navigation);
                return Task::batch([
                    self.load_rules(guild, client),
                    self.load_onboarding(guild, client),
                    self.load_guide(guild, client),
                ]);
            }
            Event::Close => {
                if self.busy.is_none() {
                    self.panel = None;
                    self.notice = None;
                }
            }
            Event::Tab(tab) => {
                if let Some(panel) = &mut self.panel {
                    panel.tab = tab;
                }
            }
            Event::RulesLoaded(serial, result) if serial == self.serial => {
                if let Some(panel) = &mut self.panel {
                    panel.rules = match result {
                        Ok(screening) => Slot::Ready(RulesEd::new(screening)),
                        Err(error) => Slot::Failed(crate::guild_admin::describe_rest(error)),
                    };
                }
            }
            Event::OnboardingLoaded(serial, result) if serial == self.serial => {
                if let Some(panel) = &mut self.panel {
                    panel.onboarding = match result {
                        Ok(doc) => Slot::Ready(OnbEd::new(doc)),
                        Err(error) => Slot::Failed(crate::guild_admin::describe_rest(error)),
                    };
                }
            }
            Event::GuideLoaded(serial, result) if serial == self.serial => {
                if let Some(panel) = &mut self.panel {
                    panel.guide = match result {
                        Ok(doc) => Slot::Ready(GuideEd {
                            baseline: doc.clone(),
                            doc,
                        }),
                        Err(error) => Slot::Failed(crate::guild_admin::describe_rest(error)),
                    };
                }
            }
            Event::Saved(serial, tab, result)
                if serial == self.serial && self.busy == Some(tab) =>
            {
                self.busy = None;
                match result {
                    Ok(saved) => return self.finished(saved, client),
                    Err(error) => self.notice = Some(describe(error)),
                }
            }
            Event::RulesLoaded(..)
            | Event::OnboardingLoaded(..)
            | Event::GuideLoaded(..)
            | Event::Saved(..) => {}
            _ if self.panel.is_none() => {}
            // Editing waits for the answer of a save in flight.
            Event::Rules(_)
            | Event::Onboarding(_)
            | Event::Guide(_)
            | Event::Community(_)
            | Event::Discard
            | Event::AskDisable
                if self.busy.is_some() =>
            {
                self.notice = Some("Wait for the previous change to finish.".to_owned());
            }
            Event::Rules(msg) => self.rules_msg(msg),
            Event::Onboarding(msg) => self.onboarding_msg(msg),
            Event::Guide(msg) => self.guide_msg(msg),
            Event::Community(msg) => {
                if let Some(form) = self.panel.as_mut().and_then(|p| p.community.as_mut()) {
                    match msg {
                        CommMsg::Rules(c) => form.rules = Some(c),
                        CommMsg::Updates(c) => form.updates = Some(c),
                        CommMsg::Level(l) => form.level = l,
                    }
                    form.touched = true;
                }
            }
            Event::Discard => self.discard(navigation),
            Event::AskDisable => {
                if let Some(form) = self.panel.as_mut().and_then(|p| p.community.as_mut()) {
                    form.confirm_disable = true;
                }
            }
            Event::Cancel => {
                if let Some(form) = self.panel.as_mut().and_then(|p| p.community.as_mut()) {
                    form.confirm_disable = false;
                }
            }
            _ if self.busy.is_some() => {
                self.notice = Some("Wait for the previous change to finish.".to_owned());
            }
            Event::Save => return self.save(navigation, client, me),
            Event::ConfirmDisable => return self.disable(navigation, client),
        }
        Task::none()
    }

    fn rules_msg(&mut self, msg: RulesMsg) {
        let Some(Slot::Ready(ed)) = self.panel.as_mut().map(|p| &mut p.rules) else {
            return;
        };
        match msg {
            RulesMsg::Enabled(on) => ed.edit.enabled = on,
            RulesMsg::Description(d) => {
                ed.edit.description = cut(d, MAX_RULES_DESCRIPTION_CHARS + 20);
            }
            RulesMsg::Rule(at, rule) => {
                if let Some(slot) = ed.edit.rules.get_mut(at) {
                    *slot = cut(rule, MAX_RULE_CHARS + 20);
                }
            }
            RulesMsg::Add => {
                if ed.edit.rules.len() < MAX_RULES_EDITED {
                    ed.edit.rules.push(String::new());
                }
            }
            RulesMsg::Remove(at) => {
                if at < ed.edit.rules.len() {
                    ed.edit.rules.remove(at);
                }
            }
        }
    }

    fn onboarding_msg(&mut self, msg: OnbMsg) {
        let (id_a, id_b) = (self.next_id(), self.next_id());
        let Some(Slot::Ready(ed)) = self.panel.as_mut().map(|p| &mut p.onboarding) else {
            return;
        };
        match msg {
            OnbMsg::Enabled(on) => ed.doc.enabled = on,
            OnbMsg::Mode(mode) => ed.doc.mode = mode,
            OnbMsg::ToggleDefault(id) => toggle(&mut ed.doc.default_channels, id),
            OnbMsg::AddPrompt => {
                if ed.doc.prompts.len() < MAX_PROMPTS_EDITED {
                    let mut prompt = PromptDoc::new(id_a);
                    prompt.title = "New question".to_owned();
                    let mut option = OptionDoc::new(id_b);
                    option.title = "New answer".to_owned();
                    prompt.options.push(option);
                    ed.doc.prompts.push(prompt);
                    ed.prompt = Some(ed.doc.prompts.len() - 1);
                    ed.option = Some(0);
                }
            }
            OnbMsg::RemovePrompt(at) => {
                if at < ed.doc.prompts.len() {
                    ed.doc.prompts.remove(at);
                    ed.prompt = None;
                    ed.option = None;
                }
            }
            OnbMsg::MovePrompt(at, down) => {
                let to = if down { at + 1 } else { at.wrapping_sub(1) };
                if at < ed.doc.prompts.len() && to < ed.doc.prompts.len() {
                    ed.doc.prompts.swap(at, to);
                    if ed.prompt == Some(at) {
                        ed.prompt = Some(to);
                    } else if ed.prompt == Some(to) {
                        ed.prompt = Some(at);
                    }
                }
            }
            OnbMsg::SelectPrompt(at) => {
                if at < ed.doc.prompts.len() {
                    ed.prompt = Some(at);
                    ed.option = None;
                }
            }
            OnbMsg::PromptTitle(t) => {
                if let Some(p) = ed.prompt_mut() {
                    p.title = cut(t, MAX_PROMPT_TITLE_CHARS + 20);
                }
            }
            OnbMsg::Single(on) => {
                if let Some(p) = ed.prompt_mut() {
                    p.single_select = on;
                }
            }
            OnbMsg::Required(on) => {
                if let Some(p) = ed.prompt_mut() {
                    p.required = on;
                }
            }
            OnbMsg::InOnboarding(on) => {
                if let Some(p) = ed.prompt_mut() {
                    p.in_onboarding = on;
                }
            }
            OnbMsg::AddOption => {
                if let Some(p) = ed.prompt_mut()
                    && p.options.len() < MAX_OPTIONS_EDITED
                {
                    let mut option = OptionDoc::new(id_a);
                    option.title = "New answer".to_owned();
                    p.options.push(option);
                    let last = p.options.len() - 1;
                    ed.option = Some(last);
                }
            }
            OnbMsg::RemoveOption(at) => {
                if let Some(p) = ed.prompt_mut()
                    && at < p.options.len()
                {
                    p.options.remove(at);
                    ed.option = None;
                }
            }
            OnbMsg::SelectOption(at) => {
                if ed.prompt_mut().is_some_and(|p| at < p.options.len()) {
                    ed.option = Some(at);
                }
            }
            OnbMsg::OptionTitle(t) => {
                if let Some(o) = ed.option_mut() {
                    o.title = cut(t, MAX_OPTION_TITLE_CHARS + 20);
                }
            }
            OnbMsg::OptionDescription(t) => {
                if let Some(o) = ed.option_mut() {
                    o.description = cut(t, MAX_OPTION_DESCRIPTION_CHARS + 20);
                }
            }
            OnbMsg::OptionEmoji(t) => {
                if let Some(o) = ed.option_mut() {
                    o.emoji = o.emoji.typed(&t);
                }
            }
            OnbMsg::ToggleRole(id) => {
                if let Some(o) = ed.option_mut() {
                    toggle(&mut o.role_ids, id);
                }
            }
            OnbMsg::ToggleChannel(id) => {
                if let Some(o) = ed.option_mut() {
                    toggle(&mut o.channel_ids, id);
                }
            }
        }
    }

    fn guide_msg(&mut self, msg: GuideMsg) {
        let Some(Slot::Ready(ed)) = self.panel.as_mut().map(|p| &mut p.guide) else {
            return;
        };
        fn list(ed: &mut GuideEd, list: List) -> &mut Vec<GuideEntry> {
            match list {
                List::Actions => &mut ed.doc.actions,
                List::Resources => &mut ed.doc.resources,
            }
        }
        match msg {
            GuideMsg::Welcome(w) => ed.doc.welcome = cut(w, MAX_WELCOME_CHARS + 20),
            GuideMsg::Add(which, channel) => {
                let max = match which {
                    List::Actions => MAX_NEW_MEMBER_ACTIONS,
                    List::Resources => MAX_RESOURCE_CHANNELS,
                };
                let entries = list(ed, which);
                if entries.len() < max {
                    entries.push(GuideEntry::new(channel));
                }
            }
            GuideMsg::Remove(which, at) => {
                let entries = list(ed, which);
                if at < entries.len() {
                    entries.remove(at);
                }
            }
            GuideMsg::Title(which, at, t) => {
                if let Some(e) = list(ed, which).get_mut(at) {
                    e.title = cut(t, MAX_ENTRY_TITLE_CHARS + 20);
                }
            }
            GuideMsg::Description(which, at, t) => {
                if let Some(e) = list(ed, which).get_mut(at) {
                    e.description = cut(t, MAX_ENTRY_DESCRIPTION_CHARS + 20);
                }
            }
            GuideMsg::Emoji(at, t) => {
                if let Some(e) = ed.doc.actions.get_mut(at) {
                    e.emoji = e.emoji.typed(&t);
                }
            }
            GuideMsg::Channel(which, at, channel) => {
                if let Some(e) = list(ed, which).get_mut(at) {
                    e.channel = channel;
                }
            }
        }
    }

    fn discard(&mut self, navigation: &NavigationSnapshot) {
        let Some(panel) = &mut self.panel else {
            return;
        };
        match panel.tab {
            Tab::Rules => {
                if let Slot::Ready(ed) = &mut panel.rules {
                    ed.edit = ed.saved.clone();
                }
            }
            Tab::Onboarding => {
                if let Slot::Ready(ed) = &mut panel.onboarding {
                    *ed = OnbEd::new(ed.baseline.clone());
                }
            }
            Tab::Guide => {
                if let Slot::Ready(ed) = &mut panel.guide {
                    ed.doc = ed.baseline.clone();
                }
            }
            Tab::Community => {
                panel.community = navigation
                    .admin
                    .as_ref()
                    .filter(|a| a.id == panel.guild)
                    .map(CommEd::of);
            }
        }
        self.notice = None;
    }

    fn send<F, Fut>(&mut self, tab: Tab, client: &RestClient, run: F) -> Task<Event>
    where
        F: FnOnce(RestClient) -> Fut,
        Fut: Future<Output = Result<Saved, CommunityRequestError>> + Send + 'static,
    {
        self.busy = Some(tab);
        self.notice = None;
        let serial = self.serial;
        Task::perform(run(client.clone()), move |r| Event::Saved(serial, tab, r))
    }

    fn save(
        &mut self,
        navigation: &NavigationSnapshot,
        client: &RestClient,
        me: Snowflake,
    ) -> Task<Event> {
        let admin = match self.allowed(navigation) {
            Ok(admin) => admin,
            Err(reason) => {
                self.notice = Some(reason);
                return Task::none();
            }
        };
        let Some(panel) = &self.panel else {
            return Task::none();
        };
        let (guild, tab) = (panel.guild, panel.tab);
        if !self.dirty(tab, Some(admin)) {
            self.notice = Some("Nothing to change.".to_owned());
            return Task::none();
        }
        match tab {
            Tab::Rules => {
                let Some(Slot::Ready(ed)) = self.panel.as_ref().map(|p| &p.rules) else {
                    return Task::none();
                };
                let (baseline, edit) = (ed.baseline.clone(), ed.edit.clone());
                self.send(tab, client, move |client| async move {
                    client
                        .save_screening(guild, baseline.as_ref(), &edit)
                        .await
                        .map(Saved::Rules)
                })
            }
            Tab::Onboarding => {
                let Some(Slot::Ready(ed)) = self.panel.as_ref().map(|p| &p.onboarding) else {
                    return Task::none();
                };
                let (baseline, doc) = (ed.baseline.clone(), ed.doc.clone());
                let refs = Refs::of(admin);
                self.send(tab, client, move |client| async move {
                    client
                        .save_onboarding(guild, &doc, &baseline, &refs.known())
                        .await
                        .map(Saved::Onboarding)
                })
            }
            Tab::Guide => {
                let Some(Slot::Ready(ed)) = self.panel.as_ref().map(|p| &p.guide) else {
                    return Task::none();
                };
                let (baseline, doc) = (ed.baseline.clone(), ed.doc.clone());
                let channels = Refs::of(admin).channels;
                self.send(tab, client, move |client| async move {
                    client
                        .save_guide(guild, &doc, &baseline, &channels, me)
                        .await
                        .map(Saved::Guide)
                })
            }
            Tab::Community => {
                if admin.community {
                    self.notice = Some("Community is already on.".to_owned());
                    return Task::none();
                }
                let Some(form) = self.panel.as_ref().and_then(|p| p.community.as_ref()) else {
                    return Task::none();
                };
                let text_channel = |id: Option<Snowflake>| {
                    id.filter(|id| {
                        admin
                            .channels
                            .iter()
                            .any(|c| c.id == *id && c.kind == AdminKind::Text)
                    })
                };
                let level = form.level.max(admin.verification_level);
                let body = enable_community_body(
                    &admin.features,
                    text_channel(form.rules),
                    text_channel(form.updates),
                    level,
                );
                match body {
                    Ok(body) => self.send(tab, client, move |client| async move {
                        client
                            .set_community(guild, &body)
                            .await
                            .map(|()| Saved::Community)
                    }),
                    Err(error) => {
                        self.notice = Some(error.to_string());
                        Task::none()
                    }
                }
            }
        }
    }

    fn disable(&mut self, navigation: &NavigationSnapshot, client: &RestClient) -> Task<Event> {
        let admin = match self.allowed(navigation) {
            Ok(admin) => admin,
            Err(reason) => {
                self.notice = Some(reason);
                return Task::none();
            }
        };
        let confirmed = self
            .panel
            .as_ref()
            .and_then(|p| p.community.as_ref())
            .is_some_and(|c| c.confirm_disable);
        if !confirmed || !admin.community {
            return Task::none();
        }
        let (guild, body) = (admin.id, disable_community_body(&admin.features));
        self.send(Tab::Community, client, move |client| async move {
            client
                .set_community(guild, &body)
                .await
                .map(|()| Saved::Community)
        })
    }

    fn finished(&mut self, saved: Saved, client: &RestClient) -> Task<Event> {
        let Some(panel) = &mut self.panel else {
            return Task::none();
        };
        let guild = panel.guild;
        match saved {
            Saved::Rules(answer) => {
                let sent = match &panel.rules {
                    Slot::Ready(ed) => ed.edit.enabled,
                    _ => true,
                };
                match answer {
                    Some(screening) => {
                        let mut ed = RulesEd::new(Some(screening));
                        if ed.baseline.as_ref().is_some_and(|s| s.enabled.is_none()) {
                            ed.saved.enabled = sent;
                            ed.edit.enabled = sent;
                        }
                        panel.rules = Slot::Ready(ed);
                        self.notice = Some("Saved.".to_owned());
                    }
                    None => {
                        panel.rules = Slot::Loading;
                        self.notice = Some("Saved. Loading the rules again.".to_owned());
                        return self.load_rules(guild, client);
                    }
                }
            }
            Saved::Onboarding(answer) => match answer {
                Some(doc) => {
                    panel.onboarding = Slot::Ready(OnbEd::new(doc));
                    self.notice = Some("Saved.".to_owned());
                }
                None => {
                    panel.onboarding = Slot::Loading;
                    self.notice = Some("Saved. Loading the onboarding again.".to_owned());
                    return self.load_onboarding(guild, client);
                }
            },
            Saved::Guide(answer) => match answer {
                Some(doc) => {
                    panel.guide = Slot::Ready(GuideEd {
                        baseline: doc.clone(),
                        doc,
                    });
                    self.notice = Some("Saved.".to_owned());
                }
                None => {
                    panel.guide = Slot::Loading;
                    self.notice = Some("Saved. Loading the guide again.".to_owned());
                    return self.load_guide(guild, client);
                }
            },
            Saved::Community => {
                if let Some(form) = &mut panel.community {
                    form.touched = false;
                    form.confirm_disable = false;
                }
                self.notice = Some("Sent. It shows here when Discord confirms it.".to_owned());
            }
        }
        Task::none()
    }
}

fn toggle(list: &mut Vec<Snowflake>, id: Snowflake) {
    if let Some(at) = list.iter().position(|x| *x == id) {
        list.remove(at);
    } else {
        list.push(id);
    }
}

// ----------------------------------------------------------------- view

#[derive(Clone, PartialEq, Eq)]
struct Item<T>(T, String);

impl<T> std::fmt::Display for Item<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

fn small<'a>(label: impl Into<String>) -> iced::widget::Text<'a> {
    let label = label.into();
    text(tr(&label).to_owned()).size(12).style(text::secondary)
}

fn labelled<'a>(label: &str, control: Element<'a, Event>) -> Element<'a, Event> {
    row![text(tr(label).to_owned()).size(12).width(130), control]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()
}

fn input<'a>(
    placeholder: &str,
    value: &str,
    enabled: bool,
    on: impl Fn(String) -> Event + 'a,
) -> Element<'a, Event> {
    text_input(tr(placeholder), value)
        .size(13)
        .on_input_maybe(enabled.then_some(on))
        .into()
}

fn choose<'a, T: Clone + PartialEq + 'a>(
    options: Vec<Item<T>>,
    current: Option<&T>,
    enabled: bool,
    placeholder: &str,
    on: impl Fn(T) -> Event + 'a,
) -> Element<'a, Event> {
    let selected = current.and_then(|c| options.iter().find(|i| &i.0 == c).cloned());
    if !enabled {
        return text(selected.map_or_else(|| "-".to_owned(), |i| i.1))
            .size(13)
            .into();
    }
    pick_list(options, selected, move |item: Item<T>| on(item.0))
        .text_size(13)
        .placeholder(tr(placeholder).to_owned())
        .into()
}

fn channel_items(admin: &GuildAdmin, wanted: impl Fn(AdminKind) -> bool) -> Vec<Item<Snowflake>> {
    admin
        .channels
        .iter()
        .filter(|c| wanted(c.kind))
        .map(|c| Item(c.id, format!("#{}", c.name)))
        .collect()
}

fn channel_name(admin: &GuildAdmin, id: Snowflake) -> String {
    admin
        .channel(id)
        .map_or_else(|| trf("channel {}", &[&id]), |c| format!("#{}", c.name))
}

fn text_like(kind: AdminKind) -> bool {
    matches!(kind, AdminKind::Text | AdminKind::Announcement)
}

fn counter(len: usize, max: usize) -> String {
    format!("{len}/{max}")
}

fn rules_view<'a>(ed: &'a RulesEd, edit: bool) -> Element<'a, Event> {
    let rules = &ed.edit;
    let mut panel = column![
        checkbox(rules.enabled)
            .label(tr("Ask new members to accept the rules"))
            .on_toggle_maybe(edit.then_some(|on| Event::Rules(RulesMsg::Enabled(on)))),
        labelled(
            "Description",
            input("Shown above the rules", &rules.description, edit, |d| {
                Event::Rules(RulesMsg::Description(d))
            }),
        ),
        small(counter(
            rules.description.chars().count(),
            MAX_RULES_DESCRIPTION_CHARS
        )),
    ]
    .spacing(6);
    for (at, rule) in rules.rules.iter().enumerate() {
        panel = panel.push(
            row![
                small(format!("{}.", at + 1)).width(24),
                input("Rule", rule, edit, move |r| Event::Rules(RulesMsg::Rule(
                    at, r
                ))),
                small(counter(rule.chars().count(), MAX_RULE_CHARS)).width(60),
                button(text(tr("Remove")).size(11))
                    .on_press_maybe(edit.then_some(Event::Rules(RulesMsg::Remove(at)))),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );
    }
    panel = panel.push(button(text(tr("Add rule")).size(12)).on_press_maybe(
        (edit && rules.rules.len() < MAX_RULES_EDITED).then_some(Event::Rules(RulesMsg::Add)),
    ));
    if let Some(screening) = &ed.baseline {
        for (kind, label) in RulesEdit::other_fields(screening) {
            panel = panel.push(small(trf(
                "{}: {} (edit in the official client)",
                &[&tr(&kind), &tr(&label)],
            )));
        }
        if !screening.complete() {
            panel = panel.push(small(CommunityError::NotEditable.to_string()));
        }
    }
    panel.into()
}

fn onboarding_view<'a>(ed: &'a OnbEd, admin: &'a GuildAdmin, edit: bool) -> Element<'a, Event> {
    let doc = &ed.doc;
    let msg = |m: OnbMsg| Event::Onboarding(m);
    let mut panel = column![
        checkbox(doc.enabled)
            .label(tr("Onboarding is on"))
            .on_toggle_maybe(edit.then_some(move |on| msg(OnbMsg::Enabled(on)))),
        labelled(
            "Mode",
            choose(
                ONBOARDING_MODES
                    .iter()
                    .map(|(m, n)| Item(*m, tr(n).to_owned()))
                    .collect(),
                Some(&doc.mode),
                edit,
                "Mode",
                move |m| msg(OnbMsg::Mode(m)),
            ),
        ),
        text(tr("Default channels")).size(13),
        small("Text channels @everyone can view. Discord decides how many it needs."),
    ]
    .spacing(6);
    let defaults = column(
        admin
            .channels
            .iter()
            .filter(|c| {
                text_like(c.kind) && (c.everyone_view || doc.default_channels.contains(&c.id))
            })
            .map(|c| {
                let id = c.id;
                let reachable = c.everyone_view || ed.baseline.default_channels.contains(&id);
                checkbox(doc.default_channels.contains(&id))
                    .label(format!("#{}", c.name))
                    .on_toggle_maybe(
                        (edit && reachable).then_some(move |_| msg(OnbMsg::ToggleDefault(id))),
                    )
                    .into()
            }),
    )
    .spacing(2)
    .padding([0, 8]);
    panel = panel.push(scrollable(defaults).height(Length::Fixed(110.0)));
    panel = panel.push(row![
        text(tr("Questions")).size(13),
        space::horizontal(),
        button(text(tr("Add question")).size(12)).on_press_maybe(
            (edit && doc.prompts.len() < MAX_PROMPTS_EDITED).then_some(msg(OnbMsg::AddPrompt))
        ),
    ]);
    for (at, prompt) in doc.prompts.iter().enumerate() {
        let selected = ed.prompt == Some(at);
        panel = panel.push(
            row![
                button(
                    text(format!(
                        "{}{}",
                        if selected { "> " } else { "" },
                        if prompt.title.is_empty() {
                            tr("(untitled)")
                        } else {
                            &prompt.title
                        }
                    ))
                    .size(13)
                )
                .on_press(msg(OnbMsg::SelectPrompt(at)))
                .width(Length::Fill),
                button(text(tr("Up")).size(11))
                    .on_press_maybe((edit && at > 0).then_some(msg(OnbMsg::MovePrompt(at, false)))),
                button(text(tr("Down")).size(11)).on_press_maybe(
                    (edit && at + 1 < doc.prompts.len())
                        .then_some(msg(OnbMsg::MovePrompt(at, true)))
                ),
                button(text(tr("Delete")).size(11))
                    .on_press_maybe(edit.then_some(msg(OnbMsg::RemovePrompt(at)))),
            ]
            .spacing(4),
        );
    }
    if let Some(prompt) = ed.prompt.and_then(|at| doc.prompts.get(at)) {
        panel = panel.push(prompt_view(ed, prompt, admin, edit));
    }
    panel.into()
}

fn prompt_view<'a>(
    ed: &'a OnbEd,
    prompt: &'a PromptDoc,
    admin: &'a GuildAdmin,
    edit: bool,
) -> Element<'a, Event> {
    let msg = |m: OnbMsg| Event::Onboarding(m);
    let mut panel = column![
        labelled(
            "Question",
            input("Question", &prompt.title, edit, move |t| msg(
                OnbMsg::PromptTitle(t)
            )),
        ),
        row![
            checkbox(prompt.single_select)
                .label(tr("Single choice"))
                .on_toggle_maybe(edit.then_some(move |on| msg(OnbMsg::Single(on)))),
            checkbox(prompt.required)
                .label(tr("Required"))
                .on_toggle_maybe(edit.then_some(move |on| msg(OnbMsg::Required(on)))),
            checkbox(prompt.in_onboarding)
                .label(tr("Ask when joining"))
                .on_toggle_maybe(edit.then_some(move |on| msg(OnbMsg::InOnboarding(on)))),
        ]
        .spacing(12),
        row![
            text(tr("Answers")).size(13),
            space::horizontal(),
            button(text(tr("Add answer")).size(12)).on_press_maybe(
                (edit && prompt.options.len() < MAX_OPTIONS_EDITED)
                    .then_some(msg(OnbMsg::AddOption))
            ),
        ],
    ]
    .spacing(6)
    .padding([0, 10]);
    for (at, option) in prompt.options.iter().enumerate() {
        let selected = ed.option == Some(at);
        panel = panel.push(
            row![
                button(
                    text(format!(
                        "{}{}{}",
                        if selected { "> " } else { "" },
                        option.emoji.text(),
                        if option.title.is_empty() {
                            tr("(untitled)")
                        } else {
                            &option.title
                        }
                    ))
                    .size(13)
                )
                .on_press(msg(OnbMsg::SelectOption(at)))
                .width(Length::Fill),
                button(text(tr("Delete")).size(11))
                    .on_press_maybe(edit.then_some(msg(OnbMsg::RemoveOption(at)))),
            ]
            .spacing(4),
        );
    }
    if let Some(option) = ed.option.and_then(|at| prompt.options.get(at)) {
        panel = panel.push(option_view(ed, option, admin, edit));
    }
    panel.into()
}

fn option_view<'a>(
    ed: &'a OnbEd,
    option: &'a OptionDoc,
    admin: &'a GuildAdmin,
    edit: bool,
) -> Element<'a, Event> {
    let msg = |m: OnbMsg| Event::Onboarding(m);
    let old_roles: Vec<Snowflake> = ed
        .baseline
        .prompts
        .iter()
        .flat_map(|p| &p.options)
        .flat_map(|o| o.role_ids.iter().copied())
        .collect();
    let emoji_note = if option.emoji.is_custom() {
        "Custom emoji kept; type a Unicode emoji to replace it."
    } else {
        "Unicode emoji only."
    };
    let mut panel = column![
        labelled(
            "Answer",
            input("Answer", &option.title, edit, move |t| msg(
                OnbMsg::OptionTitle(t)
            )),
        ),
        labelled(
            "Description",
            input("Optional", &option.description, edit, move |t| {
                msg(OnbMsg::OptionDescription(t))
            }),
        ),
        labelled(
            "Emoji",
            input("Optional", option.emoji.text(), edit, move |t| msg(
                OnbMsg::OptionEmoji(t)
            )),
        ),
        small(emoji_note),
        text(tr("Roles given")).size(13),
    ]
    .spacing(4)
    .padding([0, 10]);
    if !admin.can_manage_roles {
        panel = panel.push(small(
            "You need Manage Roles to change the roles of an answer.",
        ));
    }
    let roles = column(admin.roles.iter().filter(|r| !r.everyone).map(|r| {
        let id = r.id;
        let on = option.role_ids.contains(&id);
        let reachable = r.assign_lock.is_none() || old_roles.contains(&id);
        let label = match r.assign_lock {
            Some(reason) if !on => trf("{} ({})", &[&r.name, &tr(reason)]),
            _ => r.name.clone(),
        };
        checkbox(on)
            .label(label)
            .on_toggle_maybe((edit && reachable).then_some(move |_| msg(OnbMsg::ToggleRole(id))))
            .into()
    }))
    .spacing(2);
    panel = panel.push(scrollable(roles).height(Length::Fixed(90.0)));
    panel = panel.push(text(tr("Channels shown")).size(13));
    let channels = column(
        admin
            .channels
            .iter()
            .filter(|c| c.kind != AdminKind::Category)
            .map(|c| {
                let id = c.id;
                checkbox(option.channel_ids.contains(&id))
                    .label(format!("#{}", c.name))
                    .on_toggle_maybe(edit.then_some(move |_| msg(OnbMsg::ToggleChannel(id))))
                    .into()
            }),
    )
    .spacing(2);
    panel
        .push(scrollable(channels).height(Length::Fixed(90.0)))
        .into()
}

fn entries_view<'a>(
    title: &'a str,
    which: List,
    entries: &'a [GuideEntry],
    max: usize,
    admin: &'a GuildAdmin,
    edit: bool,
) -> Element<'a, Event> {
    let msg = |m: GuideMsg| Event::Guide(m);
    let mut panel = column![
        row![
            text(tr(title)).size(13),
            small(counter(entries.len(), max)),
            space::horizontal(),
            choose(
                channel_items(admin, |k| k != AdminKind::Category),
                None,
                edit && entries.len() < max,
                "Add a channel",
                move |c| msg(GuideMsg::Add(which, c)),
            ),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
    ]
    .spacing(4);
    for (at, entry) in entries.iter().enumerate() {
        let mut line = row![
            container(choose(
                channel_items(admin, |k| k != AdminKind::Category),
                Some(&entry.channel),
                edit,
                "Channel",
                move |c| msg(GuideMsg::Channel(which, at, c)),
            ))
            .width(130),
            input("Title", &entry.title, edit, move |t| msg(GuideMsg::Title(
                which, at, t
            ))),
            input("Description", &entry.description, edit, move |t| {
                msg(GuideMsg::Description(which, at, t))
            }),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        if which == List::Actions {
            line = line.push(
                text_input(tr("Emoji"), entry.emoji.text())
                    .size(13)
                    .width(60)
                    .on_input_maybe(edit.then_some(move |t| msg(GuideMsg::Emoji(at, t)))),
            );
        }
        line = line.push(
            button(text(tr("Remove")).size(11))
                .on_press_maybe(edit.then_some(msg(GuideMsg::Remove(which, at)))),
        );
        panel = panel.push(line);
    }
    panel.into()
}

fn guide_view<'a>(ed: &'a GuideEd, admin: &'a GuildAdmin, edit: bool) -> Element<'a, Event> {
    let doc = &ed.doc;
    let author = if doc.authors.is_empty() {
        tr("You").to_owned()
    } else {
        doc.authors
            .iter()
            .map(|id| {
                admin
                    .people
                    .iter()
                    .find(|(p, _)| p == id)
                    .map_or_else(|| trf("user {}", &[&id]), |(_, name)| name.clone())
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    column![
        labelled(
            "Welcome message",
            input("Welcome", &doc.welcome, edit, |w| Event::Guide(
                GuideMsg::Welcome(w)
            )),
        ),
        small(trf(
            "{}  Author shown: {}",
            &[
                &counter(doc.welcome.chars().count(), MAX_WELCOME_CHARS),
                &author
            ]
        )),
        entries_view(
            "New-member actions",
            List::Actions,
            &doc.actions,
            MAX_NEW_MEMBER_ACTIONS,
            admin,
            edit
        ),
        entries_view(
            "Resource channels",
            List::Resources,
            &doc.resources,
            MAX_RESOURCE_CHANNELS,
            admin,
            edit
        ),
    ]
    .spacing(8)
    .into()
}

fn community_view<'a>(form: &'a CommEd, admin: &'a GuildAdmin, edit: bool) -> Element<'a, Event> {
    let msg = |m: CommMsg| Event::Community(m);
    let texts = |kind| kind == AdminKind::Text;
    if admin.community {
        let name = |id: Option<Snowflake>| {
            id.map_or_else(|| tr("none").to_owned(), |c| channel_name(admin, c))
        };
        let mut panel = column![
            text(tr("Community is on.")).size(13),
            small(trf("Rules channel: {}", &[&name(admin.rules_channel)])),
            small(trf(
                "Public updates channel: {}",
                &[&name(admin.public_updates_channel)]
            )),
        ]
        .spacing(4);
        panel = if form.confirm_disable {
            panel
                .push(small(
                    "Turning Community off removes its channels and features from the server (announcement, Stage and forum channels, discovery).",
                ))
                .push(
                    row![
                        button(text(tr("Turn off Community")).size(13).style(text::danger))
                            .on_press_maybe(edit.then_some(Event::ConfirmDisable)),
                        button(text(tr("Cancel")).size(13)).on_press(Event::Cancel),
                    ]
                    .spacing(8),
                )
        } else {
            panel.push(
                button(text(tr("Turn off Community\u{2026}")).size(13))
                    .on_press_maybe(edit.then_some(Event::AskDisable)),
            )
        };
        return panel.into();
    }
    column![
        text(tr("Community is off.")).size(13),
        small("Turning it on needs a rules channel, a public updates channel, a verification level of at least Low and the explicit media filter set to all members."),
        labelled(
            "Rules channel",
            choose(channel_items(admin, texts), form.rules.as_ref(), edit, "Choose", move |c| {
                msg(CommMsg::Rules(c))
            }),
        ),
        labelled(
            "Updates channel",
            choose(channel_items(admin, texts), form.updates.as_ref(), edit, "Choose", move |c| {
                msg(CommMsg::Updates(c))
            }),
        ),
        labelled(
            "Verification",
            choose(
                VERIFICATION_LEVELS
                    .iter()
                    .map(|(l, n)| Item(*l, tr(n).to_owned()))
                    .collect(),
                Some(&form.level),
                edit,
                "Level",
                move |l| msg(CommMsg::Level(l)),
            ),
        ),
    ]
    .spacing(6)
    .into()
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
) -> Option<Element<'a, Event>> {
    let panel = state.panel.as_ref()?;
    let admin = navigation.admin.as_ref().filter(|a| a.id == panel.guild);
    let idle = state.busy.is_none();
    let edit = idle && admin.is_some_and(|a| a.can_manage_guild);
    let dirty = state.dirty(panel.tab, admin);
    let tabs = row(Tab::ALL.iter().map(|tab| {
        let label = if state.dirty(*tab, admin) {
            format!("{} *", tr(tab.label()))
        } else {
            tr(tab.label()).to_owned()
        };
        button(text(label).size(12))
            .on_press_maybe((panel.tab != *tab).then_some(Event::Tab(*tab)))
            .into()
    }))
    .spacing(6);
    let mut body = column![
        row![
            text(trf(
                "Community setup: {}",
                &[&admin.map_or(tr("server"), |a| a.name.as_str())]
            ))
            .size(14),
            space::horizontal(),
            crate::icons::close(idle.then_some(Event::Close)),
        ]
        .align_y(iced::Alignment::Center),
        tabs,
    ]
    .spacing(6);
    let Some(admin) = admin else {
        body = body.push(small("This server is not available."));
        return Some(container(body).padding([6, 0]).into());
    };
    if !admin.can_manage_guild {
        body = body.push(small(NEEDS_MANAGE_GUILD));
    }
    fn slot<'a, T>(
        slot: &'a Slot<T>,
        show: impl FnOnce(&'a T) -> Element<'a, Event>,
    ) -> Element<'a, Event> {
        match slot {
            Slot::Loading => small("Loading\u{2026}").into(),
            Slot::Failed(reason) => text(tr(reason).to_owned())
                .size(12)
                .style(text::danger)
                .into(),
            Slot::Ready(value) => show(value),
        }
    }
    let content = match panel.tab {
        Tab::Rules => slot(&panel.rules, |ed| rules_view(ed, edit)),
        Tab::Onboarding => slot(&panel.onboarding, |ed| onboarding_view(ed, admin, edit)),
        Tab::Guide => slot(&panel.guide, |ed| guide_view(ed, admin, edit)),
        Tab::Community => match &panel.community {
            Some(form) => community_view(form, admin, edit),
            None => small("Loading\u{2026}").into(),
        },
    };
    body = body.push(scrollable(container(content).padding([0, 8])).height(Length::Fixed(320.0)));
    let save_label = if panel.tab == Tab::Community {
        "Turn on Community"
    } else {
        "Save"
    };
    body = body.push(
        row![
            small(if dirty {
                "Unsaved changes"
            } else {
                "No changes"
            }),
            space::horizontal(),
            button(text(tr("Discard")).size(13))
                .on_press_maybe((edit && dirty).then_some(Event::Discard)),
            button(text(tr(save_label)).size(13))
                .on_press_maybe((edit && dirty).then_some(Event::Save)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    );
    if state.busy.is_some() {
        body = body.push(small("Waiting for Discord\u{2026}"));
    }
    if let Some(notice) = &state.notice {
        body = body.push(text(tr(notice).to_owned()).size(12));
    }
    Some(container(body).padding([6, 0]).into())
}

#[cfg(test)]
mod tests;
