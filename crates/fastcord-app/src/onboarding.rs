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

use std::collections::{HashMap, HashSet};

use fastcord_discord::state::navigation::NavigationSnapshot;
use fastcord_discord::{
    ApplicationStatus, Guide, Onboarding, OnboardingPrompt, RestClient, RestError, Screening,
    onboarding_due,
};
use fastcord_model::Snowflake;
use iced::widget::{button, column, container, image, row, scrollable, text};
use iced::{Element, Length, Task};

use crate::avatars::AvatarKey;
use crate::emoji::{Asset, View};
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
    Submit,
    Submitted(u64, Snowflake, Result<(), RestError>),
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
}

/// Nothing to draw for the welcome text's emoji and pictures.
#[derive(Default)]
struct Plain {
    emojis: HashMap<Asset, View>,
    pictures: HashMap<AvatarKey, image::Handle>,
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
    generation: u64,
}

/// What a failed request tells the user. `what` names the change a write
/// may have made.
pub(crate) fn failure(error: RestError, what: &str) -> String {
    match error {
        RestError::Retryable(_) => format!(
            "Discord did not confirm. {what} may have been applied; it was not retried. Check in the official client before trying again."
        ),
        RestError::ResourceGone => "Discord could not find that for this server.".to_owned(),
        RestError::PermissionDenied => {
            "Discord refused (403): you may not do this in this server.".to_owned()
        }
        RestError::Refused(refusal) if refusal.status.as_u16() == 404 => {
            "Discord could not find that for this server.".to_owned()
        }
        RestError::Refused(refusal) if refusal.status.as_u16() == 403 => {
            format!("Discord refused (403): {}", refusal.message())
        }
        other => crate::invite::describe(other),
    }
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
    /// Logout or account change.
    pub fn clear(&mut self) {
        *self = Self {
            generation: self.generation.wrapping_add(1),
            ..Self::default()
        };
    }

    /// A 401 stops the account like every other request.
    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Rules(_, _, Err(RestError::AuthenticationRequired))
                | Event::Accepted(_, _, Err(RestError::AuthenticationRequired))
                | Event::Questions(_, _, Err(RestError::AuthenticationRequired))
                | Event::Submitted(_, _, Err(RestError::AuthenticationRequired))
                | Event::GuideLoaded(_, _, Err(RestError::AuthenticationRequired))
        )
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

    pub fn update(
        &mut self,
        event: Event,
        navigation: &NavigationSnapshot,
        client: &RestClient,
    ) -> Task<Event> {
        let generation = self.generation;
        match event {
            Event::OpenRules(guild) => {
                if matches!(&self.panel, Some(Panel::Rules(r)) if r.guild == guild && !matches!(r.load, Load::Failed(_)))
                {
                    return Task::none();
                }
                self.panel = Some(Panel::Rules(Rules {
                    guild,
                    load: Load::Loading,
                    error: None,
                }));
                let client = client.clone();
                Task::perform(
                    async move { client.member_verification(guild).await },
                    move |result| Event::Rules(generation, guild, result),
                )
            }
            Event::Rules(answered, guild, result) => {
                if answered == generation
                    && let Some(Panel::Rules(rules)) = &mut self.panel
                    && rules.guild == guild
                    && matches!(rules.load, Load::Loading)
                {
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
                self.panel = Some(Panel::Questions(Questions {
                    guild,
                    revisit,
                    load: Load::Loading,
                    chosen: Vec::new(),
                    error: None,
                    saved: false,
                }));
                let client = client.clone();
                Task::perform(
                    async move { client.onboarding(guild).await },
                    move |result| Event::Questions(generation, guild, result),
                )
            }
            Event::Questions(answered, guild, result) => {
                let held: Vec<Snowflake> = navigation
                    .current_user
                    .as_ref()
                    .and_then(|me| navigation.members.get(&me.id))
                    .map(|member| member.roles.clone())
                    .unwrap_or_default();
                if answered == generation
                    && let Some(Panel::Questions(q)) = &mut self.panel
                    && q.guild == guild
                    && matches!(q.load, Load::Loading)
                {
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
                Task::perform(
                    async move { client.new_member_welcome(guild).await },
                    move |result| Event::GuideLoaded(generation, guild, result),
                )
            }
            Event::GuideLoaded(answered, guild, result) => {
                if answered == generation
                    && let Some(Panel::Guide(view)) = &mut self.panel
                    && view.guild == guild
                    && matches!(view.load, Load::Loading)
                {
                    view.load = match result {
                        Ok(guide) => Load::Ready(guide),
                        Err(error) if is_absent(&error) => {
                            Load::Failed("This server has no guide.".to_owned())
                        }
                        Err(error) => Load::Failed(failure(error, "Nothing")),
                    };
                }
                Task::none()
            }
            // Handled by the application (selection, spoilers and links).
            Event::GoChannel(..) | Event::Rich(_) => Task::none(),
            Event::Close => {
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
    let mut line = row![text(message).size(13).width(Length::Fill)]
        .spacing(8)
        .align_y(iced::Alignment::Center);
    if let Some((label, event)) = action {
        line = line.push(button(text(label).size(13)).on_press(event));
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
            text(label).size(13).width(Length::Fill),
            button(text("Close").size(13)).on_press_maybe((!busy).then_some(Event::Close)),
        ]
    };
    let body: Element<'a, Event> = match panel {
        Panel::Rules(rules) => column![
            title("Server rules", state.accepting.is_some()),
            rules_body(state, rules),
        ]
        .spacing(6)
        .into(),
        Panel::Questions(q) => column![
            title("Channels & Roles", state.submitting),
            questions_body(state, q),
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
        Load::Loading => Some(text("Loading\u{2026}").size(12).into()),
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
                text(format!(
                    "{}: answer this in the official client.",
                    if field.label.is_empty() {
                        "A question"
                    } else {
                        &field.label
                    }
                ))
                .size(12)
                .style(text::secondary),
            );
        }
    }
    let mut footer = column![].spacing(6);
    if let Some(error) = &rules.error {
        footer = footer.push(text(error.as_str()).size(12).style(text::danger));
    }
    footer = footer.push(match state.outcome(rules.guild) {
        Some(Outcome::Waiting) => {
            text("Waiting for the server's moderators.").size(12).into()
        }
        Some(Outcome::Approved | Outcome::Sent) => text(
            "Sent. Writing opens when Discord updates your membership.",
        )
        .size(12)
        .into(),
        Some(Outcome::Rejected) => text("The moderators declined your application.")
            .size(12)
            .style(text::danger)
            .into(),
        None if screening.can_accept() => {
            let busy = state.accepting.is_some();
            Element::from(
                button(
                    text(if busy { "Sending\u{2026}" } else { "I have read and agree" }).size(13),
                )
                .on_press_maybe((!busy).then_some(Event::AcceptRules)),
            )
        }
        None => text("This server asks for more than this client can answer. Finish joining in the official client.")
            .size(12)
            .style(text::secondary)
            .into(),
    });
    column![scrollable(list).height(Length::Fixed(260.0)), footer]
        .spacing(6)
        .into()
}

fn questions_body<'a>(state: &'a State, q: &'a Questions) -> Element<'a, Event> {
    if let Some(shown) = loading(&q.load) {
        return shown;
    }
    let Load::Ready(onboarding) = &q.load else {
        return column![].into();
    };
    if onboarding.prompts.is_empty() {
        return text("This server has no onboarding questions.")
            .size(12)
            .into();
    }
    let mut list = column![].spacing(8);
    for prompt in &onboarding.prompts {
        let mut block = column![
            text(format!(
                "{}{}",
                prompt.title,
                if prompt.required { " (required)" } else { "" }
            ))
            .size(13)
        ]
        .spacing(4);
        block = block.push(
            text(if prompt.single_select {
                "Choose one"
            } else {
                "Choose any"
            })
            .size(11)
            .style(text::secondary),
        );
        for option in &prompt.options {
            let picked = q.chosen.contains(&option.id);
            let mut label = format!("{} {}", if picked { "[x]" } else { "[ ]" }, option.title);
            if !option.emoji.is_empty() {
                label = format!("{label} {}", option.emoji);
            }
            let mut item = column![
                button(text(label).size(13))
                    .on_press_maybe(
                        (!state.submitting).then_some(Event::Choose(prompt.id, option.id))
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
                        .style(text::secondary),
                );
            }
            block = block.push(item);
        }
        list = list.push(block);
    }
    let missing = missing_required(&onboarding.prompts, &q.chosen);
    let mut footer = column![
        text("Roles and channels change only when Discord sends them after you save.")
            .size(11)
            .style(text::secondary)
    ]
    .spacing(4);
    if let Some(error) = &q.error {
        footer = footer.push(text(error.as_str()).size(12).style(text::danger));
    }
    if q.saved {
        footer = footer.push(text("Saved.").size(12));
    }
    if let Some(title) = missing {
        footer = footer.push(text(format!("Answer \"{title}\" to continue.")).size(12));
    }
    footer = footer.push(
        button(
            text(if state.submitting {
                "Saving\u{2026}"
            } else {
                "Save choices"
            })
            .size(13),
        )
        .on_press_maybe((!state.submitting && missing.is_none()).then_some(Event::Submit)),
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
        return text("This server has no guide.").size(12).into();
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
        list = list.push(text(heading).size(13));
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
                        .style(text::secondary),
                );
            }
            if !open {
                line = line.push(
                    text("You cannot open this channel.")
                        .size(11)
                        .style(text::secondary),
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
            fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
            fastcord_discord::gateway::ClientProperties::web(
                fastcord_discord::gateway::HostOs::Linux,
                "en-US",
                1,
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

    fn guild() -> Snowflake {
        Snowflake(100)
    }

    fn rules_open(state: &mut State) {
        let nav = NavigationSnapshot::default();
        let _ = state.update(Event::OpenRules(guild()), &nav, &client());
        let _ = state.update(
            Event::Rules(state.generation, guild(), Ok(screening())),
            &nav,
            &client(),
        );
    }

    fn questions_open(state: &mut State, revisit: bool, nav: &NavigationSnapshot) {
        let _ = state.update(Event::OpenQuestions(guild(), revisit), nav, &client());
        let _ = state.update(
            Event::Questions(state.generation, guild(), Ok(onboarding())),
            nav,
            &client(),
        );
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
        let _ = state.update(Event::Rules(0, guild(), Ok(asked)), &nav, &client());
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
                RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)),
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
        let old = state.generation;
        state.clear();
        let _ = state.update(Event::OpenRules(guild()), &nav, &client());
        let _ = state.update(Event::Rules(old, guild(), Ok(screening())), &nav, &client());
        assert!(matches!(&state.panel, Some(Panel::Rules(r)) if matches!(r.load, Load::Loading)));
        let _ = state.update(
            Event::Rules(state.generation, Snowflake(7), Ok(screening())),
            &nav,
            &client(),
        );
        assert!(matches!(&state.panel, Some(Panel::Rules(r)) if matches!(r.load, Load::Loading)));
        let _ = state.update(
            Event::Accepted(old, guild(), Ok(ApplicationStatus::Waiting)),
            &nav,
            &client(),
        );
        assert!(state.outcome(guild()).is_none());
        assert!(State::is_rejected(&Event::Rules(
            0,
            guild(),
            Err(RestError::AuthenticationRequired)
        )));
        assert!(!State::is_rejected(&Event::Rules(
            0,
            guild(),
            Err(RestError::ResourceGone)
        )));
    }

    #[test]
    fn single_choice_keeps_one_and_multiple_choice_keeps_many() {
        let mut state = State::default();
        let nav = NavigationSnapshot::default();
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
        let nav = NavigationSnapshot::default();
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
                RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)),
                "may have been applied",
            ),
        ] {
            let mut state = State::default();
            let nav = NavigationSnapshot::default();
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
            let _ = state.update(Event::GuideLoaded(0, guild(), Err(error)), &nav, &client());
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
                0,
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
