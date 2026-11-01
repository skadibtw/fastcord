//! Joining a server as a member (SPEC §16, issue #91): the rules a new member
//! accepts (membership screening), the onboarding questions that choose roles
//! and channels, and the read-only server guide. One panel is open at a time.
//!
//! Every request is one explicit click and is never retried: accepting the
//! rules (`PUT /guilds/{id}/requests/@me`) and sending the answers
//! (`PUT /guilds/{id}/onboarding-responses`) are refused a second time while
//! one is in flight, and an ambiguous failure is shown as "may have been
//! applied". Nothing here changes what the account may do: writing stays off
//! while the Gateway reports the member as pending, and roles and channels
//! change only through the roles and channels Discord then sends. Answers are
//! ignored after logout (generation counter).

mod preview;

use std::collections::{HashMap, HashSet};

use crate::ui::button;
use fastcord_discord::state::navigation::NavigationSnapshot;
use fastcord_discord::{
    ApplicationStatus, Guide, Onboarding, OnboardingPrompt, RestClient, RestError, Screening,
    onboarding_due,
};
use fastcord_model::Snowflake;
use iced::widget::{column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Task};

use crate::emoji::{Asset, View};
use crate::i18n::{tr, trf};
use crate::panel_kit::auth_rejected;
use crate::{rich, timeline};

/// Guilds whose accepted rules are remembered (until the Gateway answers).
const MAX_OUTCOMES: usize = 16;

#[derive(Clone, Debug)]
pub enum Event {
    /// Load the rules of a server.
    OpenRules(Snowflake),
    Rules(u64, Snowflake, Result<Screening, RestError>),
    AcceptRules,
    Accepted(u64, Snowflake, Result<ApplicationStatus, RestError>),
    /// Load the questions of a server; `true` is the settings entry that
    /// shows every question and preselects what the roles already say.
    OpenQuestions(Snowflake, bool),
    Questions(u64, Snowflake, Result<Onboarding, RestError>),
    Choose(Snowflake, Snowflake),
    Preview(u64, preview::Event),
    Submit,
    Submitted(u64, Snowflake, Result<(), RestError>),
    OpenBrowse(Snowflake),
    BrowseQuery(String),
    BrowsePage(usize),
    BrowseGo(Snowflake),
    OpenGuide(Snowflake),
    GuideLoaded(u64, Snowflake, Result<Guide, RestError>),
    /// A channel of the guide; selected only if the account can see it.
    GoChannel(Snowflake, Snowflake),
    /// Spoilers and links of the guide's welcome text.
    Rich(timeline::Event),
    Close,
}

#[derive(Debug)]
enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// What Discord said to an accepted application, kept until the Gateway
/// reports the member as no longer pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Waiting,
    Approved,
    Rejected,
    Sent,
}

#[derive(Debug)]
struct Rules {
    guild: Snowflake,
    load: Load<Screening>,
    error: Option<String>,
}

#[derive(Debug)]
struct Questions {
    guild: Snowflake,
    revisit: bool,
    load: Load<Onboarding>,
    chosen: Vec<Snowflake>,
    error: Option<String>,
    saved: bool,
}

#[derive(Debug)]
struct GuideView {
    guild: Snowflake,
    load: Load<Guide>,
    names: rich::Names,
}

#[derive(Debug)]
enum Panel {
    Rules(Rules),
    Questions(Questions),
    Guide(GuideView),
    Browse {
        guild: Snowflake,
        query: String,
        offset: usize,
    },
}

/// Nothing to draw for the welcome text's emoji and pictures.
#[derive(Default)]
struct Plain {
    emojis: HashMap<Asset, View>,
    pictures: crate::avatars::Views,
}

impl std::fmt::Debug for Plain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Plain")
    }
}

#[derive(Debug, Default)]
pub struct State {
    panel: Option<Panel>,
    outcomes: Vec<(Snowflake, Outcome)>,
    /// The server whose rules are being sent; blocks a second send.
    accepting: Option<Snowflake>,
    /// Answers are being sent.
    submitting: bool,
    plain: Plain,
    previews: preview::Pool,
    /// Account ownership for writes and preview events.
    generation: u64,
    /// One read request for the currently open panel, independent of writes.
    read_serial: u64,
    read_task: Option<iced::task::Handle>,
}

/// What a failed request tells the user. `what` names the change a write
/// may have made.
pub(crate) fn failure(error: RestError, what: &str) -> String {
    use crate::rest_text::{DEFAULT, Wording, describe};
    let unconfirmed = trf(
        "Discord did not confirm. {} may have been applied; it was not retried. Check in the official client before trying again.",
        &[&tr(what)],
    );
    describe(
        error,
        &Wording {
            forbidden: "Discord refused (403): you may not do this in this server.",
            gone: "Discord could not find that for this server.",
            unconfirmed: &unconfirmed,
            ..DEFAULT
        },
    )
}

/// Whether the guide is simply not there for this account.
fn is_absent(error: &RestError) -> bool {
    match error {
        RestError::ResourceGone | RestError::PermissionDenied => true,
        RestError::Refused(refusal) => matches!(refusal.status.as_u16(), 403 | 404),
        _ => false,
    }
}

/// Selects or clears one option, keeping a single-choice question to one.
fn toggle(
    prompts: &[OnboardingPrompt],
    chosen: &mut Vec<Snowflake>,
    prompt: Snowflake,
    option: Snowflake,
) {
    let Some(prompt) = prompts.iter().find(|p| p.id == prompt) else {
        return;
    };
    if !prompt.options.iter().any(|o| o.id == option) {
        return;
    }
    let was = chosen.contains(&option);
    if prompt.single_select {
        chosen.retain(|c| !prompt.options.iter().any(|o| o.id == *c));
    } else {
        chosen.retain(|c| *c != option);
    }
    if !was {
        chosen.push(option);
    }
}

/// The first required question with no answer.
fn missing_required<'a>(prompts: &'a [OnboardingPrompt], chosen: &[Snowflake]) -> Option<&'a str> {
    prompts
        .iter()
        .find(|p| p.required && !p.options.iter().any(|o| chosen.contains(&o.id)))
        .map(|p| p.title.as_str())
}

/// Options whose roles the account already holds (an option without roles
/// cannot be told from the roles).
fn preselected(prompts: &[OnboardingPrompt], held: &[Snowflake]) -> Vec<Snowflake> {
    let mut chosen = Vec::new();
    for prompt in prompts {
        for option in &prompt.options {
            if !option.role_ids.is_empty()
                && option.role_ids.iter().all(|r| held.contains(r))
                && (!prompt.single_select || !prompt.options.iter().any(|o| chosen.contains(&o.id)))
            {
                chosen.push(option.id);
            }
        }
    }
    chosen
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl State {
    /// The rules, questions or guide are shown (not just the reminder line).
    pub fn is_open(&self) -> bool {
        self.panel.is_some()
    }

    pub fn browse_request(&self) -> Option<(Snowflake, &str, usize)> {
        match &self.panel {
            Some(Panel::Browse {
                guild,
                query,
                offset,
            }) => Some((*guild, query, *offset)),
            _ => None,
        }
    }

    fn browse_page<'a>(
        &self,
        navigation: &'a NavigationSnapshot,
    ) -> Option<&'a fastcord_discord::state::navigation::BrowseChannels> {
        let (guild, query, offset) = self.browse_request()?;
        navigation
            .browse
            .as_ref()
            .filter(|page| page.guild == guild && page.query == query && page.requested == offset)
    }

    pub fn browse_target(
        &self,
        navigation: &NavigationSnapshot,
        channel: Snowflake,
    ) -> Option<Snowflake> {
        let page = self.browse_page(navigation)?;
        page.window
            .rows
            .iter()
            .any(|row| row.id == channel)
            .then_some(page.guild)
    }

    /// Logout or account change.
    pub fn clear(&mut self) {
        self.clear_read();
        *self = Self {
            generation: self.generation.wrapping_add(1),
            read_serial: self.read_serial,
            ..Self::default()
        };
    }

    fn clear_read(&mut self) {
        if let Some(task) = self.read_task.take() {
            task.abort();
        }
        self.read_serial = self.read_serial.wrapping_add(1);
    }

    fn reading(&mut self, task: Task<Event>) -> Task<Event> {
        let (task, handle) = task.abortable();
        self.read_task = Some(handle.abort_on_drop());
        task
    }

    fn owns_read(&self, event: &Event) -> bool {
        match (event, &self.panel) {
            (Event::Rules(serial, guild, _), Some(Panel::Rules(panel))) => {
                *serial == self.read_serial
                    && *guild == panel.guild
                    && matches!(panel.load, Load::Loading)
            }
            (Event::Questions(serial, guild, _), Some(Panel::Questions(panel))) => {
                *serial == self.read_serial
                    && *guild == panel.guild
                    && matches!(panel.load, Load::Loading)
            }
            (Event::GuideLoaded(serial, guild, _), Some(Panel::Guide(panel))) => {
                *serial == self.read_serial
                    && *guild == panel.guild
                    && matches!(panel.load, Load::Loading)
            }
            _ => false,
        }
    }

    /// A 401 stops the account like every other request.
    pub fn is_rejected(&self, event: &Event) -> bool {
        // A cancelled or retired request must not reject the current account.
        let current = match event {
            Event::Rules(..) | Event::Questions(..) | Event::GuideLoaded(..) => {
                self.owns_read(event)
            }
            Event::Accepted(serial, ..) | Event::Submitted(serial, ..) => {
                *serial == self.generation
            }
            _ => false,
        };
        if !current {
            return false;
        }
        match event {
            Event::Rules(_, _, r) => auth_rejected(r),
            Event::Accepted(_, _, r) => auth_rejected(r),
            Event::Questions(_, _, r) => auth_rejected(r),
            Event::Submitted(_, _, r) => auth_rejected(r),
            Event::GuideLoaded(_, _, r) => auth_rejected(r),
            _ => false,
        }
    }

    fn outcome(&self, guild: Snowflake) -> Option<Outcome> {
        self.outcomes
            .iter()
            .find(|(g, _)| *g == guild)
            .map(|(_, o)| *o)
    }

    fn remember(&mut self, guild: Snowflake, outcome: Outcome) {
        self.outcomes.retain(|(g, _)| *g != guild);
        if self.outcomes.len() >= MAX_OUTCOMES {
            self.outcomes.remove(0);
        }
        self.outcomes.push((guild, outcome));
    }

    pub(crate) fn sync_previews(
        &mut self,
        cache: Option<&crate::attachments::AttachmentCache>,
    ) -> Task<Event> {
        let generation = self.generation;
        self.previews
            .sync(cache)
            .map(move |event| Event::Preview(generation, event))
    }

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        client: &RestClient,
    ) -> Task<Event> {
        let generation = self.generation;
        if matches!(
            &event,
            Event::OpenRules(_) | Event::OpenBrowse(_) | Event::OpenGuide(_) | Event::Close
        ) || matches!(&event, Event::OpenQuestions(guild, _) if !matches!(&self.panel, Some(Panel::Questions(q)) if q.guild == *guild && !matches!(q.load, Load::Failed(_))))
        {
            self.previews.clear();
        }
        match event {
            Event::Preview(answered, event) => {
                if answered == generation && matches!(&self.panel, Some(Panel::Questions(_))) {
                    self.previews.update(event);
                }
                Task::none()
            }
            Event::OpenBrowse(guild) => {
                if self.accepting.is_none() && !self.submitting {
                    self.clear_read();
                    self.panel = Some(Panel::Browse {
                        guild,
                        query: String::new(),
                        offset: 0,
                    });
                }
                Task::none()
            }
            Event::BrowseQuery(value) => {
                if let Some(Panel::Browse { query, offset, .. }) = &mut self.panel {
                    *query = value
                        .chars()
                        .take(fastcord_discord::state::navigation::BROWSE_QUERY_CHARS)
                        .collect();
                    *offset = 0;
                }
                Task::none()
            }
            Event::BrowsePage(wanted) => {
                if let Some(page) = self.browse_page(navigation) {
                    let offset = page.window.offset;
                    let size = fastcord_discord::state::navigation::BROWSE_PAGE;
                    if ((wanted == offset.saturating_sub(size))
                        || (wanted == offset.saturating_add(size) && wanted < page.window.total))
                        && let Some(Panel::Browse { offset, .. }) = &mut self.panel
                    {
                        *offset = wanted;
                    }
                }
                Task::none()
            }
            Event::BrowseGo(_) => Task::none(),
            Event::OpenRules(guild) => {
                if matches!(&self.panel, Some(Panel::Rules(r)) if r.guild == guild && !matches!(r.load, Load::Failed(_)))
                {
                    return Task::none();
                }
                self.clear_read();
                let serial = self.read_serial;
                self.panel = Some(Panel::Rules(Rules {
                    guild,
                    load: Load::Loading,
                    error: None,
                }));
                let client = client.clone();
                self.reading(Task::perform(
                    async move { client.member_verification(guild).await },
                    move |result| Event::Rules(serial, guild, result),
                ))
            }
            Event::Rules(answered, guild, result) => {
                if answered == self.read_serial
                    && let Some(Panel::Rules(rules)) = &mut self.panel
                    && rules.guild == guild
                    && matches!(rules.load, Load::Loading)
                {
                    self.read_task = None;
                    rules.load = match result {
                        Ok(screening) => Load::Ready(screening),
                        Err(error) => Load::Failed(failure(error, "Nothing")),
                    };
                }
                Task::none()
            }
            Event::AcceptRules => {
                let Some(Panel::Rules(rules)) = &mut self.panel else {
                    return Task::none();
                };
                let Load::Ready(screening) = &rules.load else {
                    return Task::none();
                };
                if self.accepting.is_some() || !screening.can_accept() {
                    return Task::none();
                }
                let guild = rules.guild;
                rules.error = None;
                self.accepting = Some(guild);
                let (client, screening) = (client.clone(), screening.clone());
                Task::perform(
                    async move { client.accept_rules(guild, &screening).await },
                    move |result| Event::Accepted(generation, guild, result),
                )
            }
            Event::Accepted(answered, guild, result) => {
                if answered != generation {
                    return Task::none();
                }
                if self.accepting == Some(guild) {
                    self.accepting = None;
                }
                match result {
                    Ok(status) => self.remember(
                        guild,
                        match status {
                            ApplicationStatus::Approved => Outcome::Approved,
                            ApplicationStatus::Waiting => Outcome::Waiting,
                            ApplicationStatus::Rejected => Outcome::Rejected,
                            ApplicationStatus::Unknown => Outcome::Sent,
                        },
                    ),
                    Err(error) => {
                        if let Some(Panel::Rules(rules)) = &mut self.panel
                            && rules.guild == guild
                        {
                            rules.error = Some(failure(error, "Accepting the rules"));
                        }
                    }
                }
                Task::none()
            }
            Event::OpenQuestions(guild, revisit) => {
                if matches!(&self.panel, Some(Panel::Questions(q)) if q.guild == guild && !matches!(q.load, Load::Failed(_)))
                {
                    return Task::none();
                }
                self.clear_read();
                let serial = self.read_serial;
                self.panel = Some(Panel::Questions(Questions {
                    guild,
                    revisit,
                    load: Load::Loading,
                    chosen: Vec::new(),
                    error: None,
                    saved: false,
                }));
                let client = client.clone();
                self.reading(Task::perform(
                    async move { client.onboarding(guild).await },
                    move |result| Event::Questions(serial, guild, result),
                ))
            }
            Event::Questions(answered, guild, result) => {
                if answered == self.read_serial
                    && let Some(Panel::Questions(q)) = &mut self.panel
                    && q.guild == guild
                    && matches!(q.load, Load::Loading)
                {
                    self.read_task = None;
                    // Members belong only to the selected guild. Never seed
                    // this form with another server's roles after navigation.
                    if navigation.selected_guild != Some(q.guild) {
                        q.load = Load::Failed(tr("The selected server changed. Reopen Channels & Roles in the original server.").to_owned());
                        return Task::none();
                    }
                    let held: Vec<Snowflake> = navigation
                        .current_user
                        .as_ref()
                        .and_then(|me| navigation.members.get(&me.id))
                        .map(|member| member.roles.clone())
                        .unwrap_or_default();
                    q.load = match result {
                        Ok(mut onboarding) => {
                            let revisit = q.revisit;
                            onboarding.prompts.retain(|p| p.in_onboarding || revisit);
                            if revisit {
                                q.chosen = preselected(&onboarding.prompts, &held);
                            }
                            Load::Ready(onboarding)
                        }
                        Err(error) => Load::Failed(failure(error, "Nothing")),
                    };
                }
                Task::none()
            }
            Event::Choose(prompt, option) => {
                if let Some(Panel::Questions(q)) = &mut self.panel
                    && let Load::Ready(onboarding) = &q.load
                    && navigation.selected_guild == Some(q.guild)
                    && !self.submitting
                {
                    toggle(&onboarding.prompts, &mut q.chosen, prompt, option);
                    q.saved = false;
                }
                Task::none()
            }
            Event::Submit => {
                let Some(Panel::Questions(q)) = &mut self.panel else {
                    return Task::none();
                };
                let Load::Ready(onboarding) = &q.load else {
                    return Task::none();
                };
                if navigation.selected_guild != Some(q.guild) {
                    q.error =
                        Some(tr("Return to this server before saving your choices.").to_owned());
                    return Task::none();
                }
                if self.submitting || missing_required(&onboarding.prompts, &q.chosen).is_some() {
                    return Task::none();
                }
                let guild = q.guild;
                q.error = None;
                q.saved = false;
                self.submitting = true;
                let (client, prompts, chosen) =
                    (client.clone(), onboarding.prompts.clone(), q.chosen.clone());
                Task::perform(
                    async move {
                        client
                            .submit_onboarding(guild, &prompts, &chosen, now_ms())
                            .await
                    },
                    move |result| Event::Submitted(generation, guild, result),
                )
            }
            Event::Submitted(answered, guild, result) => {
                if answered != generation {
                    return Task::none();
                }
                self.submitting = false;
                if let Some(Panel::Questions(q)) = &mut self.panel
                    && q.guild == guild
                {
                    match result {
                        Ok(()) => q.saved = true,
                        Err(error) => q.error = Some(failure(error, "Your choice")),
                    }
                }
                Task::none()
            }
            Event::OpenGuide(guild) => {
                if matches!(&self.panel, Some(Panel::Guide(g)) if g.guild == guild && !matches!(g.load, Load::Failed(_)))
                {
                    return Task::none();
                }
                self.clear_read();
                let serial = self.read_serial;
                let mut names = rich::Names {
                    roles: navigation.guild_roles.clone(),
                    channels: Vec::new(),
                };
                names.channels = navigation
                    .channels
                    .rows
                    .iter()
                    .map(|row| (row.id, row.name.clone()))
                    .collect();
                self.panel = Some(Panel::Guide(GuideView {
                    guild,
                    load: Load::Loading,
                    names,
                }));
                let client = client.clone();
                self.reading(Task::perform(
                    async move { client.new_member_welcome(guild).await },
                    move |result| Event::GuideLoaded(serial, guild, result),
                ))
            }
            Event::GuideLoaded(answered, guild, result) => {
                if answered == self.read_serial
                    && let Some(Panel::Guide(view)) = &mut self.panel
                    && view.guild == guild
                    && matches!(view.load, Load::Loading)
                {
                    self.read_task = None;
                    view.load = match result {
                        Ok(guide) => Load::Ready(guide),
                        Err(error) if is_absent(&error) => {
                            Load::Failed(tr("This server has no guide.").to_owned())
                        }
                        Err(error) => Load::Failed(failure(error, "Nothing")),
                    };
                }
                Task::none()
            }
            // Handled by the application (selection, spoilers and links).
            Event::GoChannel(..) | Event::Rich(_) => Task::none(),
            Event::Close => {
                self.clear_read();
                self.panel = None;
                Task::none()
            }
        }
    }
}

/// Whether the account can open this channel: it is in the snapshot's rows
/// and enabled. A guide entry never makes a channel visible.
pub fn visible_channel(navigation: &NavigationSnapshot, channel: Snowflake) -> bool {
    navigation
        .channels
        .rows
        .iter()
        .any(|row| row.id == channel && row.enabled)
}

fn banner_line<'a>(message: &'a str, action: Option<(&'a str, Event)>) -> Element<'a, Event> {
    let mut line = row![text(tr(message)).size(13).width(Length::Fill)]
        .spacing(8)
        .align_y(iced::Alignment::Center);
    if let Some((label, event)) = action {
        line = line.push(button(text(tr(label)).size(13)).on_press(event));
    }
    container(line)
        .padding([4, 8])
        .style(container::rounded_box)
        .into()
}

/// The open panel, or the banner that offers one; `None` when neither applies.
pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
    revealed: &'a HashSet<(Snowflake, u16)>,
) -> Option<Element<'a, Event>> {
    let Some(panel) = &state.panel else {
        return banner(state, navigation);
    };
    let title = |label: &'static str, busy: bool| {
        row![
            text(tr(label)).size(13).width(Length::Fill),
            crate::icons::close((!busy).then_some(Event::Close)),
        ]
    };
    let body: Element<'a, Event> = match panel {
        Panel::Browse { query, .. } => column![
            title("Browse channels", false),
            text_input(tr("Search channels"), query).on_input(Event::BrowseQuery),
            browse_body(state, navigation),
        ]
        .spacing(6)
        .into(),
        Panel::Rules(rules) => column![
            title("Server rules", state.accepting.is_some()),
            rules_body(state, rules),
        ]
        .spacing(6)
        .into(),
        Panel::Questions(q) => column![
            title("Channels & Roles", state.submitting),
            questions_body(state, q, navigation.selected_guild == Some(q.guild)),
        ]
        .spacing(6)
        .into(),
        Panel::Guide(guide) => column![
            title("Server guide", false),
            guide_body(state, navigation, guide, revealed),
        ]
        .spacing(6)
        .into(),
    };
    Some(container(body).max_width(720).into())
}

fn browse_body<'a>(state: &State, navigation: &'a NavigationSnapshot) -> Element<'a, Event> {
    let Some(page) = state.browse_page(navigation) else {
        return text(tr("Loading…")).into();
    };
    let mut rows = column![].spacing(4);
    for channel in &page.window.rows {
        use fastcord_discord::state::navigation::ChannelKind;
        let kind = match channel.kind {
            ChannelKind::Voice if channel.stage => "Stage",
            ChannelKind::Voice => "Voice",
            ChannelKind::Forum => "Forum",
            ChannelKind::Announcement => "Announcement",
            ChannelKind::Thread => "Thread",
            _ => "Text",
        };
        rows = rows.push(
            button(row![
                text(channel.name.as_str()).width(Length::Fill),
                text(tr(kind)).size(12)
            ])
            .width(Length::Fill)
            .on_press(Event::BrowseGo(channel.id)),
        );
    }
    if page.window.rows.is_empty() {
        rows = rows.push(text(tr("No accessible channels match.")));
    }
    let size = fastcord_discord::state::navigation::BROWSE_PAGE;
    let offset = page.window.offset;
    column![
        scrollable(rows).height(Length::Fixed(320.0)),
        row![
            button(text(tr("Previous"))).on_press_maybe(
                (offset > 0).then_some(Event::BrowsePage(offset.saturating_sub(size)))
            ),
            text(trf("{} channels", &[&page.window.total])).width(Length::Fill),
            button(text(tr("Next"))).on_press_maybe(
                (offset.saturating_add(size) < page.window.total)
                    .then_some(Event::BrowsePage(offset.saturating_add(size)))
            ),
        ]
        .spacing(8),
    ]
    .spacing(8)
    .into()
}

fn banner<'a>(state: &'a State, navigation: &NavigationSnapshot) -> Option<Element<'a, Event>> {
    let guild = navigation.selected_guild?;
    if navigation.own_pending {
        return Some(match state.outcome(guild) {
            Some(Outcome::Waiting) => banner_line(
                "Your application is waiting for the server's moderators.",
                None,
            ),
            Some(Outcome::Approved | Outcome::Sent) => banner_line(
                "Rules accepted. Writing opens when Discord updates your membership.",
                None,
            ),
            Some(Outcome::Rejected) => {
                banner_line("The server's moderators declined your application.", None)
            }
            None => banner_line(
                "You have not accepted this server's rules. Writing and reacting are off until you do.",
                Some(("Review rules", Event::OpenRules(guild))),
            ),
        });
    }
    onboarding_due(navigation.own_flags, false).then(|| {
        banner_line(
            "This server has questions that choose your channels and roles.",
            Some(("Choose", Event::OpenQuestions(guild, false))),
        )
    })
}

fn loading<'a, T>(load: &'a Load<T>) -> Option<Element<'a, Event>> {
    match load {
        Load::Loading => Some(text(tr("Loading\u{2026}")).size(12).into()),
        Load::Failed(error) => Some(text(error.as_str()).size(12).style(text::danger).into()),
        Load::Ready(_) => None,
    }
}

fn rules_body<'a>(state: &'a State, rules: &'a Rules) -> Element<'a, Event> {
    if let Some(shown) = loading(&rules.load) {
        return shown;
    }
    let Load::Ready(screening) = &rules.load else {
        return column![].into();
    };
    let mut list = column![].spacing(6);
    if !screening.description.is_empty() {
        list = list.push(text(screening.description.as_str()).size(13));
    }
    for field in &screening.fields {
        if field.is_terms() {
            if !field.label.is_empty() {
                list = list.push(text(field.label.as_str()).size(13));
            }
            for (n, rule) in field.rules.iter().enumerate() {
                list = list.push(text(format!("{}. {rule}", n + 1)).size(13));
            }
        } else {
            list = list.push(
                text(trf(
                    "{}: answer this in the official client.",
                    &[&if field.label.is_empty() {
                        tr("A question")
                    } else {
                        &field.label
                    }],
                ))
                .size(12)
                .color(crate::style::muted()),
            );
        }
    }
    let mut footer = column![].spacing(6);
    if let Some(error) = &rules.error {
        footer = footer.push(text(error.as_str()).size(12).style(text::danger));
    }
    footer = footer.push(match state.outcome(rules.guild) {
        Some(Outcome::Waiting) => {
            text(tr("Waiting for the server's moderators.")).size(12).into()
        }
        Some(Outcome::Approved | Outcome::Sent) => text(tr(
            "Sent. Writing opens when Discord updates your membership.",
        ))
        .size(12)
        .into(),
        Some(Outcome::Rejected) => text(tr("The moderators declined your application."))
            .size(12)
            .style(text::danger)
            .into(),
        None if screening.can_accept() => {
            let busy = state.accepting.is_some();
            Element::from(
                button(
                    text(if busy {
                        tr("Sending\u{2026}")
                    } else {
                        tr("I have read and agree")
                    })
                    .size(13),
                )
                .on_press_maybe((!busy).then_some(Event::AcceptRules)),
            )
        }
        None => text(tr("This server asks for more than this client can answer. Finish joining in the official client."))
            .size(12)
            .color(crate::style::muted())
            .into(),
    });
    column![scrollable(list).height(Length::Fixed(260.0)), footer]
        .spacing(6)
        .into()
}

fn questions_body<'a>(state: &'a State, q: &'a Questions, in_guild: bool) -> Element<'a, Event> {
    if let Some(shown) = loading(&q.load) {
        return shown;
    }
    let Load::Ready(onboarding) = &q.load else {
        return column![].into();
    };
    if onboarding.prompts.is_empty() {
        return text(tr("This server has no onboarding questions."))
            .size(12)
            .into();
    }
    let mut list = column![].spacing(8);
    for prompt in &onboarding.prompts {
        let mut block = column![
            text(format!(
                "{}{}",
                prompt.title,
                if prompt.required {
                    tr(" (required)")
                } else {
                    ""
                }
            ))
            .size(13)
        ]
        .spacing(4);
        block = block.push(
            text(if prompt.single_select {
                tr("Choose one")
            } else {
                tr("Choose any")
            })
            .size(11)
            .color(crate::style::muted()),
        );
        for option in &prompt.options {
            let picked = q.chosen.contains(&option.id);
            let mut label = format!("{} {}", if picked { "[x]" } else { "[ ]" }, option.title);
            if option.custom_emoji.is_none() && !option.emoji.is_empty() {
                label = format!("{label} {}", option.emoji);
            }
            let mut option_label = row![text(label).size(13)].spacing(6);
            if let Some(id) = option.custom_emoji {
                let generation = state.generation;
                let fallback = if option.emoji.is_empty() {
                    ":emoji:"
                } else {
                    &option.emoji
                };
                option_label = option_label.push(
                    state
                        .previews
                        .view((prompt.id, option.id), id, fallback)
                        .map(move |event| Event::Preview(generation, event)),
                );
            }
            let mut item = column![
                button(option_label)
                    .on_press_maybe(
                        (in_guild && !state.submitting)
                            .then_some(Event::Choose(prompt.id, option.id))
                    )
                    .style(if picked {
                        button::primary
                    } else {
                        button::secondary
                    })
            ];
            if !option.description.is_empty() {
                item = item.push(
                    text(option.description.as_str())
                        .size(11)
                        .color(crate::style::muted()),
                );
            }
            block = block.push(item);
        }
        list = list.push(block);
    }
    let missing = missing_required(&onboarding.prompts, &q.chosen);
    let mut footer = column![
        text(tr(
            "Roles and channels change only when Discord sends them after you save."
        ))
        .size(11)
        .color(crate::style::muted())
    ]
    .spacing(4);
    if !in_guild {
        footer = footer.push(
            text(tr("Return to this server before saving your choices."))
                .size(12)
                .style(text::danger),
        );
    }
    if let Some(error) = &q.error {
        footer = footer.push(text(error.as_str()).size(12).style(text::danger));
    }
    if q.saved {
        footer = footer.push(text(tr("Saved.")).size(12));
    }
    if let Some(title) = missing {
        footer = footer.push(text(trf("Answer \"{}\" to continue.", &[&title])).size(12));
    }
    footer = footer.push(
        button(
            text(if state.submitting {
                tr("Saving\u{2026}")
            } else {
                tr("Save choices")
            })
            .size(13),
        )
        .on_press_maybe(
            (in_guild && !state.submitting && missing.is_none()).then_some(Event::Submit),
        ),
    );
    column![scrollable(list).height(Length::Fixed(280.0)), footer]
        .spacing(6)
        .into()
}

fn guide_body<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
    view: &'a GuideView,
    revealed: &'a HashSet<(Snowflake, u16)>,
) -> Element<'a, Event> {
    if let Some(shown) = loading(&view.load) {
        return shown;
    }
    let Load::Ready(guide) = &view.load else {
        return column![].into();
    };
    if guide.is_empty() {
        return text(tr("This server has no guide.")).size(12).into();
    }
    let mut list = column![].spacing(8);
    if !guide.welcome.is_empty() {
        let cx = rich::Cx {
            names: &view.names,
            revealed,
            emojis: &state.plain.emojis,
            pictures: &state.plain.pictures,
            now: crate::localtime::now(),
        };
        list = list.push(
            rich::body(&guide.welcome, rich::Parts::none(), Snowflake(0), 0, cx)
                .unwrap_or_else(|| text(guide.welcome.as_str()).size(13).into())
                .map(Event::Rich),
        );
    }
    for (heading, items) in [
        ("New member actions", &guide.actions),
        ("Resources", &guide.resources),
    ] {
        if items.is_empty() {
            continue;
        }
        list = list.push(text(tr(heading)).size(13));
        for item in items {
            let open = visible_channel(navigation, item.channel);
            let mut line = column![
                button(text(item.title.as_str()).size(13))
                    .on_press_maybe(open.then_some(Event::GoChannel(view.guild, item.channel)))
            ];
            if !item.description.is_empty() {
                line = line.push(
                    text(item.description.as_str())
                        .size(11)
                        .color(crate::style::muted()),
                );
            }
            if !open {
                line = line.push(
                    text(tr("You cannot open this channel."))
                        .size(11)
                        .color(crate::style::muted()),
                );
            }
            list = list.push(line);
        }
    }
    scrollable(list).height(Length::Fixed(280.0)).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::{NetworkFailure, RetryableFailure};

    fn client() -> RestClient {
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
            fastcord_discord::gateway::ClientProperties::bundled(
                fastcord_discord::gateway::HostOs::Linux,
                "en-US",
            ),
        )
        .unwrap()
    }

    fn screening() -> Screening {
        let body = include_str!("../../../fixtures/rest/member-verification-synthetic.json");
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        Screening::from_json(value["terms"].to_string().as_bytes()).unwrap()
    }

    fn onboarding() -> Onboarding {
        let body = include_str!("../../../fixtures/rest/onboarding-synthetic.json");
        Onboarding::from_json(body.as_bytes()).unwrap()
    }

    #[test]
    fn channel_browser_rejects_stale_pages_bounds_query_and_clears_on_reset() {
        use fastcord_discord::state::navigation::{BrowseChannels, BrowseRow, ChannelKind, Window};
        let mut state = State::default();
        let mut nav = NavigationSnapshot::default();
        let _ = state.update(Event::OpenBrowse(guild()), &nav, &client());
        nav.browse = Some(BrowseChannels {
            guild: guild(),
            query: String::new(),
            requested: 0,
            window: Window {
                offset: 0,
                total: 100,
                rows: vec![BrowseRow {
                    id: Snowflake(9),
                    name: "Shown".into(),
                    kind: ChannelKind::Text,
                    stage: false,
                }],
            },
        });
        assert_eq!(state.browse_target(&nav, Snowflake(9)), Some(guild()));
        let _ = state.update(Event::BrowsePage(50), &nav, &client());
        assert!(state.browse_target(&nav, Snowflake(9)).is_none());
        let _ = state.update(Event::BrowseQuery("😀".repeat(1000)), &nav, &client());
        let (_, query, offset) = state.browse_request().unwrap();
        assert_eq!(query.chars().count(), 100);
        assert_eq!(offset, 0);
        assert!(state.browse_target(&nav, Snowflake(9)).is_none());
        state.clear();
        assert!(state.browse_request().is_none());
        assert!(state.browse_target(&nav, Snowflake(9)).is_none());
    }

    fn navigation() -> NavigationSnapshot {
        NavigationSnapshot {
            selected_guild: Some(guild()),
            ..NavigationSnapshot::default()
        }
    }

    fn guild() -> Snowflake {
        Snowflake(100)
    }

    fn rules_open(state: &mut State) {
        let nav = NavigationSnapshot::default();
        let _ = state.update(Event::OpenRules(guild()), &nav, &client());
        let _ = state.update(
            Event::Rules(state.read_serial, guild(), Ok(screening())),
            &nav,
            &client(),
        );
    }

    fn questions_open(state: &mut State, revisit: bool, nav: &NavigationSnapshot) {
        let _ = state.update(Event::OpenQuestions(guild(), revisit), nav, &client());
        let _ = state.update(
            Event::Questions(state.read_serial, guild(), Ok(onboarding())),
            nav,
            &client(),
        );
    }

    fn authenticated_app() -> crate::App {
        let (mut app, _) = crate::App::boot();
        app.phase = crate::Phase::Authenticating {
            saved_account: None,
        };
        let _ = app.update(crate::Message::LoginFinished(Ok(
            crate::LoginOutcome::Authenticated(
                crate::login::fixture_session(),
                crate::Persistence::MemoryOnly,
            ),
        )));
        publish_guild(&mut app, guild(), vec![Snowflake(401)]);
        app
    }

    fn publish_guild(app: &mut crate::App, selected: Snowflake, roles: Vec<Snowflake>) {
        let gateway = app.phase.gateway().unwrap();
        let id = gateway.id;
        let me = crate::login::fixture_session().user.clone();
        let mut nav = NavigationSnapshot {
            selected_guild: Some(selected),
            current_user: Some(me.clone()),
            ..NavigationSnapshot::default()
        };
        nav.members.insert(
            me.id,
            fastcord_discord::state::navigation::MemberView {
                roles,
                ..Default::default()
            },
        );
        gateway.controls.publish(
            nav,
            crate::timeline::Snapshot::default(),
            crate::emoji_picker::Snapshot::default(),
            None,
        );
        let _ = app.handle(crate::Message::Navigation(id));
        assert_eq!(
            app.phase.gateway().unwrap().navigation.selected_guild,
            Some(selected)
        );
    }

    fn navigate_away(app: &mut crate::App, keyboard: bool) {
        if keyboard {
            use fastcord_discord::state::navigation::{Step, StepScope};
            let _ = app.handle(crate::Message::WindowShortcut(
                app.main_window,
                crate::switcher::Shortcut::Step(Step {
                    scope: StepScope::Server,
                    forward: true,
                }),
            ));
        } else {
            let _ = app.open_link(crate::desktop::DeepLink::Channel {
                guild: Some(Snowflake(200)),
                channel: Snowflake(201),
                message: None,
            });
        }
        assert!(
            app.ui.nav_pending,
            "the actual keyboard/deep-link route requested navigation"
        );
        // The reducer's next published read model contains only server B's roles.
        publish_guild(app, Snowflake(200), vec![Snowflake(402), Snowflake(403)]);
    }

    fn app_questions(app: &crate::App) -> &Questions {
        let Some(Panel::Questions(q)) = &app.ui.onboarding.panel else {
            panic!("questions remain visible")
        };
        q
    }

    #[derive(Clone, Copy, Debug)]
    enum ReadPanel {
        Rules,
        Questions,
        Guide,
    }

    impl ReadPanel {
        fn open(self) -> Event {
            match self {
                Self::Rules => Event::OpenRules(guild()),
                Self::Questions => Event::OpenQuestions(guild(), true),
                Self::Guide => Event::OpenGuide(guild()),
            }
        }

        fn answer(self, serial: u64, result: Result<&str, RestError>) -> Event {
            match self {
                Self::Rules => Event::Rules(
                    serial,
                    guild(),
                    result.map(|label| {
                        let mut form = screening();
                        form.description = label.into();
                        form
                    }),
                ),
                Self::Questions => Event::Questions(
                    serial,
                    guild(),
                    result.map(|label| {
                        let mut form = onboarding();
                        form.prompts[0].title = label.into();
                        form
                    }),
                ),
                Self::Guide => Event::GuideLoaded(
                    serial,
                    guild(),
                    result.map(|label| Guide {
                        welcome: label.into(),
                        actions: vec![],
                        resources: vec![],
                    }),
                ),
            }
        }
    }

    fn loaded_label(state: &State) -> Option<&str> {
        match &state.panel {
            Some(Panel::Rules(Rules {
                load: Load::Ready(form),
                ..
            })) => Some(&form.description),
            Some(Panel::Questions(Questions {
                load: Load::Ready(form),
                ..
            })) => Some(&form.prompts[0].title),
            Some(Panel::Guide(GuideView {
                load: Load::Ready(form),
                ..
            })) => Some(&form.welcome),
            _ => None,
        }
    }

    fn read_failed(state: &State) -> bool {
        matches!(
            &state.panel,
            Some(Panel::Rules(Rules {
                load: Load::Failed(_),
                ..
            })) | Some(Panel::Questions(Questions {
                load: Load::Failed(_),
                ..
            })) | Some(Panel::Guide(GuideView {
                load: Load::Failed(_),
                ..
            }))
        )
    }

    fn reopen(app: &mut crate::App, kind: ReadPanel, reset: bool) {
        if reset {
            let _ = app.teardown_account();
            app.phase = crate::Phase::Authenticating {
                saved_account: None,
            };
            let _ = app.update(crate::Message::LoginFinished(Ok(
                crate::LoginOutcome::Authenticated(
                    crate::login::fixture_session(),
                    crate::Persistence::MemoryOnly,
                ),
            )));
            publish_guild(app, guild(), vec![Snowflake(401)]);
        } else {
            let _ = app.handle(crate::Message::Onboarding(Event::Close));
        }
        let task = app.handle(crate::Message::Onboarding(kind.open()));
        assert_eq!(
            task.units(),
            1,
            "one new read task, deliberately not polled"
        );
    }

    #[test]
    fn read_router_rejects_retired_reads_in_both_orders_and_after_account_reset() {
        for kind in [ReadPanel::Rules, ReadPanel::Questions, ReadPanel::Guide] {
            for reset in [false, true] {
                for current_first in [false, true] {
                    for stale_auth in [false, true] {
                        let mut app = authenticated_app();
                        let _ = app.handle(crate::Message::Onboarding(kind.open()));
                        let old = app.ui.onboarding.read_serial;
                        let old_task = app.ui.onboarding.read_task.as_ref().unwrap().clone();
                        reopen(&mut app, kind, reset);
                        assert!(old_task.is_aborted(), "only the retired GET is cancelled");
                        let current = app.ui.onboarding.read_serial;
                        let current_task = app.ui.onboarding.read_task.as_ref().unwrap().clone();
                        assert_ne!(old, current);
                        if current_first {
                            let _ = app.handle(crate::Message::Onboarding(
                                kind.answer(current, Ok("current")),
                            ));
                        }
                        let stale = if stale_auth {
                            Err(RestError::AuthenticationRequired)
                        } else {
                            Ok("retired")
                        };
                        let _ = app.handle(crate::Message::Onboarding(kind.answer(old, stale)));
                        assert!(
                            app.phase.gateway().is_some(),
                            "retired read cannot reject this account"
                        );
                        if !current_first {
                            assert!(loaded_label(&app.ui.onboarding).is_none());
                            assert!(!read_failed(&app.ui.onboarding));
                            assert!(
                                !current_task.is_aborted(),
                                "retired answer cannot cancel current GET"
                            );
                            let _ = app.handle(crate::Message::Onboarding(
                                kind.answer(current, Ok("current")),
                            ));
                        }
                        assert_eq!(
                            loaded_label(&app.ui.onboarding),
                            Some("current"),
                            "{kind:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn read_router_keeps_current_errors_and_current_auth_still_rejects_login() {
        for kind in [ReadPanel::Rules, ReadPanel::Questions, ReadPanel::Guide] {
            for current_first in [false, true] {
                let mut app = authenticated_app();
                let _ = app.handle(crate::Message::Onboarding(kind.open()));
                let old = app.ui.onboarding.read_serial;
                reopen(&mut app, kind, false);
                let current = app.ui.onboarding.read_serial;
                if current_first {
                    let _ = app.handle(crate::Message::Onboarding(
                        kind.answer(current, Err(RestError::PermissionDenied)),
                    ));
                }
                let _ = app.handle(crate::Message::Onboarding(kind.answer(old, Ok("retired"))));
                if !current_first {
                    let _ = app.handle(crate::Message::Onboarding(
                        kind.answer(current, Err(RestError::PermissionDenied)),
                    ));
                }
                assert!(read_failed(&app.ui.onboarding));
                assert!(loaded_label(&app.ui.onboarding).is_none());
                // Explicit retry gets a fresh owner, whose 401 still stops the account.
                let _ = app.handle(crate::Message::Onboarding(kind.open()));
                let retry = app.ui.onboarding.read_serial;
                assert_ne!(retry, current);
                let _ = app.handle(crate::Message::Onboarding(
                    kind.answer(retry, Err(RestError::AuthenticationRequired)),
                ));
                assert!(app.phase.gateway().is_none());
            }
        }
    }

    #[test]
    fn closing_or_replacing_reads_does_not_release_or_repeat_pending_writes() {
        for kind in [ReadPanel::Rules, ReadPanel::Questions] {
            let mut app = authenticated_app();
            let _ = app.handle(crate::Message::Onboarding(kind.open()));
            let serial = app.ui.onboarding.read_serial;
            let _ = app.handle(crate::Message::Onboarding(
                kind.answer(serial, Ok("current")),
            ));
            let write = match kind {
                ReadPanel::Rules => Event::AcceptRules,
                _ => Event::Submit,
            };
            let task = app.handle(crate::Message::Onboarding(write.clone()));
            assert_eq!(task.units(), 1);
            let account = app.ui.onboarding.generation;
            reopen(&mut app, kind, false);
            let serial = app.ui.onboarding.read_serial;
            let _ = app.handle(crate::Message::Onboarding(
                kind.answer(serial, Ok("replacement")),
            ));
            assert_eq!(
                app.ui.onboarding.generation, account,
                "read lifetime cannot retire the write"
            );
            let duplicate = app.handle(crate::Message::Onboarding(write));
            assert_eq!(duplicate.units(), 0, "pending write remains single-flight");
            let done = match kind {
                ReadPanel::Rules => {
                    assert_eq!(app.ui.onboarding.accepting, Some(guild()));
                    Event::Accepted(account, guild(), Ok(ApplicationStatus::Waiting))
                }
                _ => {
                    assert!(app.ui.onboarding.submitting);
                    Event::Submitted(account, guild(), Ok(()))
                }
            };
            let _ = app.handle(crate::Message::Onboarding(done));
            assert!(app.ui.onboarding.accepting.is_none());
            assert!(!app.ui.onboarding.submitting);
            assert_eq!(loaded_label(&app.ui.onboarding), Some("replacement"));
        }
    }

    #[test]
    fn question_load_after_keyboard_or_deep_link_navigation_is_visibly_invalidated() {
        for keyboard in [true, false] {
            let mut app = authenticated_app();
            let _ = app.handle(crate::Message::Onboarding(Event::OpenQuestions(
                guild(),
                true,
            )));
            let generation = app.ui.onboarding.read_serial;
            navigate_away(&mut app, keyboard);
            let _ = app.handle(crate::Message::Onboarding(Event::Questions(
                generation,
                guild(),
                Ok(onboarding()),
            )));
            assert!(
                matches!(&app_questions(&app).load, Load::Failed(reason) if reason.contains("selected server changed"))
            );
            assert!(
                app_questions(&app).chosen.is_empty(),
                "server B roles must never seed server A answers"
            );
            let task = app.handle(crate::Message::Onboarding(Event::Submit));
            assert_eq!(task.units(), 0);
            assert!(!app.ui.onboarding.submitting);
        }
    }

    #[test]
    fn loaded_choices_survive_keyboard_and_deep_link_navigation_but_cannot_save_cross_guild() {
        for keyboard in [true, false] {
            let mut app = authenticated_app();
            let _ = app.handle(crate::Message::Onboarding(Event::OpenQuestions(
                guild(),
                true,
            )));
            let generation = app.ui.onboarding.read_serial;
            let _ = app.handle(crate::Message::Onboarding(Event::Questions(
                generation,
                guild(),
                Ok(onboarding()),
            )));
            assert_eq!(app_questions(&app).chosen, vec![Snowflake(301)]);
            navigate_away(&mut app, keyboard);
            let _ = app.handle(crate::Message::Onboarding(Event::Choose(
                Snowflake(300),
                Snowflake(302),
            )));
            let task = app.handle(crate::Message::Onboarding(Event::Submit));
            assert_eq!(
                task.units(),
                0,
                "cross-guild Submit must not schedule a write"
            );
            assert!(!app.ui.onboarding.submitting);
            assert_eq!(app_questions(&app).chosen, vec![Snowflake(301)]);
            assert!(
                app_questions(&app)
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("Return to this server")
            );
            publish_guild(&mut app, guild(), vec![Snowflake(401)]);
            let task = app.handle(crate::Message::Onboarding(Event::Submit));
            assert_eq!(
                task.units(),
                1,
                "explicit Save after returning schedules the existing write once"
            );
            assert!(app.ui.onboarding.submitting);
            assert_eq!(app_questions(&app).guild, guild());
            assert_eq!(app_questions(&app).chosen, vec![Snowflake(301)]);
            assert!(app_questions(&app).error.is_none());
        }
    }

    #[test]
    fn custom_preview_visibility_is_account_scoped_and_close_clears_it() {
        let (mut state, nav) = (State::default(), navigation());
        questions_open(&mut state, false, &nav);
        let visible =
            preview::Event::Visible(1, (Snowflake(300), Snowflake(302)), Snowflake(900), true);
        let _ = state.update(Event::Preview(0, visible.clone()), &nav, &client());
        assert_eq!(state.previews.interests(), 1);
        assert!(view(&state, &nav, &HashSet::new()).is_some());
        state.clear();
        questions_open(&mut state, false, &nav);
        let _ = state.update(Event::Preview(0, visible.clone()), &nav, &client());
        assert_eq!(
            state.previews.interests(),
            0,
            "old account visibility ignored"
        );
        let _ = state.update(
            Event::Preview(state.generation, visible.clone()),
            &nav,
            &client(),
        );
        assert_eq!(state.previews.interests(), 1);
        let _ = state.update(Event::Close, &nav, &client());
        let _ = state.update(Event::Preview(state.generation, visible), &nav, &client());
        assert_eq!(state.previews.interests(), 0);
    }

    fn chosen(state: &State) -> Vec<u64> {
        let Some(Panel::Questions(q)) = &state.panel else {
            panic!("no questions");
        };
        q.chosen.iter().map(|s| s.0).collect()
    }

    #[test]
    fn rules_are_accepted_once_and_a_second_click_while_sending_does_nothing() {
        let mut state = State::default();
        let nav = NavigationSnapshot::default();
        // Nothing to accept before the rules are loaded.
        let _ = state.update(Event::AcceptRules, &nav, &client());
        assert!(state.accepting.is_none());
        rules_open(&mut state);
        let _ = state.update(Event::AcceptRules, &nav, &client());
        assert_eq!(state.accepting, Some(guild()));
        let _ = state.update(Event::AcceptRules, &nav, &client());
        assert_eq!(state.accepting, Some(guild()), "still the one request");
        let _ = state.update(
            Event::Accepted(state.generation, guild(), Ok(ApplicationStatus::Waiting)),
            &nav,
            &client(),
        );
        assert!(state.accepting.is_none());
        assert_eq!(state.outcome(guild()), Some(Outcome::Waiting));
    }

    #[test]
    fn a_question_the_client_cannot_answer_blocks_the_accept_button() {
        let body = include_str!("../../../fixtures/rest/member-verification-synthetic.json");
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        let mut state = State::default();
        let nav = NavigationSnapshot::default();
        let _ = state.update(Event::OpenRules(guild()), &nav, &client());
        let asked = Screening::from_json(value["with_question"].to_string().as_bytes()).unwrap();
        let _ = state.update(
            Event::Rules(state.read_serial, guild(), Ok(asked)),
            &nav,
            &client(),
        );
        let _ = state.update(Event::AcceptRules, &nav, &client());
        assert!(state.accepting.is_none());
    }

    #[test]
    fn accepting_failures_are_shown_and_never_applied() {
        let nav = NavigationSnapshot::default();
        for (error, expect) in [
            (RestError::PermissionDenied, "403"),
            (RestError::ResourceGone, "could not find"),
            (
                RestError::Http(reqwest::StatusCode::TOO_MANY_REQUESTS),
                "rate limiting",
            ),
            (
                RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout)),
                "may have been applied",
            ),
        ] {
            let mut state = State::default();
            rules_open(&mut state);
            let _ = state.update(Event::AcceptRules, &nav, &client());
            let _ = state.update(
                Event::Accepted(state.generation, guild(), Err(error)),
                &nav,
                &client(),
            );
            let Some(Panel::Rules(rules)) = &state.panel else {
                panic!("rules closed");
            };
            let shown = rules.error.as_deref().unwrap();
            assert!(shown.contains(expect), "{shown}");
            assert!(state.accepting.is_none() && state.outcome(guild()).is_none());
        }
    }

    #[test]
    fn approval_and_application_statuses_are_remembered_not_applied() {
        for (status, outcome) in [
            (ApplicationStatus::Approved, Outcome::Approved),
            (ApplicationStatus::Rejected, Outcome::Rejected),
            (ApplicationStatus::Unknown, Outcome::Sent),
        ] {
            let mut state = State::default();
            let nav = NavigationSnapshot::default();
            let _ = state.update(Event::Accepted(0, guild(), Ok(status)), &nav, &client());
            assert_eq!(state.outcome(guild()), Some(outcome));
        }
        let mut state = State::default();
        for n in 0..(MAX_OUTCOMES as u64 + 5) {
            state.remember(Snowflake(n), Outcome::Sent);
        }
        assert_eq!(state.outcomes.len(), MAX_OUTCOMES);
    }

    #[test]
    fn late_answers_after_logout_or_for_another_server_are_ignored() {
        let mut state = State::default();
        let nav = NavigationSnapshot::default();
        let _ = state.update(Event::OpenRules(guild()), &nav, &client());
        let old = state.read_serial;
        let old_account = state.generation;
        state.clear();
        let _ = state.update(Event::OpenRules(guild()), &nav, &client());
        let _ = state.update(Event::Rules(old, guild(), Ok(screening())), &nav, &client());
        assert!(matches!(&state.panel, Some(Panel::Rules(r)) if matches!(r.load, Load::Loading)));
        let _ = state.update(
            Event::Rules(state.read_serial, Snowflake(7), Ok(screening())),
            &nav,
            &client(),
        );
        assert!(matches!(&state.panel, Some(Panel::Rules(r)) if matches!(r.load, Load::Loading)));
        let _ = state.update(
            Event::Accepted(old_account, guild(), Ok(ApplicationStatus::Waiting)),
            &nav,
            &client(),
        );
        assert!(state.outcome(guild()).is_none());
        assert!(state.is_rejected(&Event::Rules(
            state.read_serial,
            guild(),
            Err(RestError::AuthenticationRequired)
        )));
        assert!(!state.is_rejected(&Event::Rules(
            state.read_serial,
            guild(),
            Err(RestError::ResourceGone)
        )));
    }

    #[test]
    fn single_choice_keeps_one_and_multiple_choice_keeps_many() {
        let mut state = State::default();
        let nav = navigation();
        questions_open(&mut state, false, &nav);
        // The settings-only question (320) is not asked on joining.
        let Some(Panel::Questions(q)) = &state.panel else {
            panic!()
        };
        let Load::Ready(o) = &q.load else { panic!() };
        assert_eq!(o.prompts.len(), 2);
        for (prompt, option) in [(300, 301), (300, 302), (310, 311), (310, 312)] {
            let _ = state.update(
                Event::Choose(Snowflake(prompt), Snowflake(option)),
                &nav,
                &client(),
            );
        }
        assert_eq!(chosen(&state), vec![301, 302, 312], "one region, two games");
        let _ = state.update(
            Event::Choose(Snowflake(310), Snowflake(312)),
            &nav,
            &client(),
        );
        assert_eq!(chosen(&state), vec![301, 302], "a click clears it");
        // An option of another question or an unknown one changes nothing.
        let _ = state.update(
            Event::Choose(Snowflake(300), Snowflake(311)),
            &nav,
            &client(),
        );
        assert_eq!(chosen(&state), vec![301, 302]);
    }

    #[test]
    fn a_required_question_blocks_the_submit_and_a_second_submit_waits() {
        let mut state = State::default();
        let nav = navigation();
        questions_open(&mut state, false, &nav);
        let _ = state.update(Event::Submit, &nav, &client());
        assert!(!state.submitting, "question 300 is required");
        let _ = state.update(
            Event::Choose(Snowflake(300), Snowflake(301)),
            &nav,
            &client(),
        );
        let _ = state.update(Event::Submit, &nav, &client());
        assert!(state.submitting);
        // Choices are frozen and a second submit is not sent.
        let _ = state.update(
            Event::Choose(Snowflake(300), Snowflake(302)),
            &nav,
            &client(),
        );
        assert_eq!(chosen(&state), vec![301]);
        let _ = state.update(Event::Submit, &nav, &client());
        assert!(state.submitting);
        let _ = state.update(
            Event::Submitted(state.generation, guild(), Ok(())),
            &nav,
            &client(),
        );
        assert!(!state.submitting);
        assert!(matches!(&state.panel, Some(Panel::Questions(q)) if q.saved));
    }

    #[test]
    fn submit_failures_keep_the_choices_and_say_what_may_have_happened() {
        for (error, expect) in [
            (RestError::PermissionDenied, "403"),
            (
                RestError::Http(reqwest::StatusCode::TOO_MANY_REQUESTS),
                "rate limiting",
            ),
            (
                RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout)),
                "may have been applied",
            ),
        ] {
            let mut state = State::default();
            let nav = navigation();
            questions_open(&mut state, false, &nav);
            let _ = state.update(
                Event::Choose(Snowflake(300), Snowflake(301)),
                &nav,
                &client(),
            );
            let _ = state.update(Event::Submit, &nav, &client());
            let _ = state.update(
                Event::Submitted(state.generation, guild(), Err(error)),
                &nav,
                &client(),
            );
            let Some(Panel::Questions(q)) = &state.panel else {
                panic!()
            };
            assert!(q.error.as_deref().unwrap().contains(expect));
            assert!(!q.saved && q.chosen == vec![Snowflake(301)] && !state.submitting);
        }
    }

    #[test]
    fn revisiting_preselects_options_whose_roles_are_all_held() {
        let o = onboarding();
        let held = [
            Snowflake(401),
            Snowflake(402),
            Snowflake(404),
            Snowflake(405),
        ];
        // 302 needs 402 and 403, so it is not preselected; 321 is a settings-only question.
        assert_eq!(
            preselected(&o.prompts, &held),
            vec![Snowflake(301), Snowflake(311), Snowflake(321)]
        );
        // A single-choice question preselects at most one.
        let both = [Snowflake(404)];
        let mut single = o.prompts[1].clone();
        single.options[1].role_ids = vec![Snowflake(404)];
        assert_eq!(preselected(&[single], &both), vec![Snowflake(311)]);
    }

    #[test]
    fn a_missing_guide_is_said_plainly() {
        for error in [
            RestError::ResourceGone,
            RestError::PermissionDenied,
            RestError::Refused(fastcord_discord::Refusal::new(
                reqwest::StatusCode::NOT_FOUND,
                None,
                "",
            )),
        ] {
            let mut state = State::default();
            let nav = NavigationSnapshot::default();
            let _ = state.update(Event::OpenGuide(guild()), &nav, &client());
            let _ = state.update(
                Event::GuideLoaded(state.read_serial, guild(), Err(error)),
                &nav,
                &client(),
            );
            let Some(Panel::Guide(view)) = &state.panel else {
                panic!()
            };
            assert!(
                matches!(&view.load, Load::Failed(text) if text == "This server has no guide.")
            );
        }
        let mut state = State::default();
        let nav = NavigationSnapshot::default();
        let _ = state.update(Event::OpenGuide(guild()), &nav, &client());
        let _ = state.update(
            Event::GuideLoaded(
                state.read_serial,
                guild(),
                Err(RestError::Retryable(RetryableFailure::Network(
                    NetworkFailure::Connection,
                ))),
            ),
            &nav,
            &client(),
        );
        assert!(
            matches!(&state.panel, Some(Panel::Guide(v)) if matches!(&v.load, Load::Failed(t) if t != "This server has no guide."))
        );
    }

    #[test]
    fn guide_links_open_only_channels_the_account_can_see() {
        use fastcord_discord::state::navigation::{
            ChannelKind, ChannelRow, PermissionSummary, Window,
        };
        let row = |id: u64, enabled: bool| ChannelRow {
            id: Snowflake(id),
            name: format!("c{id}"),
            kind: ChannelKind::Text,
            parent_id: None,
            enabled,
            selected: false,
            permissions: PermissionSummary::default(),
            unread: false,
            mentions: 0,
            thread: None,
            occupant: None,
            stage: false,
            collapsed: false,
        };
        let nav = NavigationSnapshot {
            channels: Window {
                offset: 0,
                total: 2,
                rows: vec![row(201, true), row(202, false)],
            },
            ..NavigationSnapshot::default()
        };
        assert!(visible_channel(&nav, Snowflake(201)));
        assert!(!visible_channel(&nav, Snowflake(202)), "listed but closed");
        assert!(
            !visible_channel(&nav, Snowflake(999)),
            "not in the snapshot"
        );
    }

    #[test]
    fn the_banner_follows_the_gateway_not_the_answer() {
        let mut state = State::default();
        let mut nav = NavigationSnapshot {
            selected_guild: Some(guild()),
            own_pending: true,
            ..NavigationSnapshot::default()
        };
        assert!(banner(&state, &nav).is_some());
        state.remember(guild(), Outcome::Approved);
        assert!(
            banner(&state, &nav).is_some(),
            "still pending until the Gateway says so"
        );
        nav.own_pending = false;
        assert!(banner(&state, &nav).is_none());
        nav.own_flags = fastcord_discord::STARTED_ONBOARDING;
        assert!(
            banner(&state, &nav).is_some(),
            "onboarding started, not completed"
        );
        nav.own_flags |= fastcord_discord::COMPLETED_ONBOARDING;
        assert!(banner(&state, &nav).is_none());
    }
}
