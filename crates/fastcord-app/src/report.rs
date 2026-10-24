//! Reporting a message, user or server to Discord (SPEC §16, issue #81).
//!
//! A report is a submission to Discord, so it is only ever an explicit act:
//! the person opens it from a message's More row, a profile card or the server
//! header; the categories are Discord's own menu, rendered from data
//! (`ReportMenu`, nothing hard-coded); a review step then shows what will be
//! sent; only its Submit button sends, once. Nothing is sent by opening,
//! choosing, going back or closing, and a submission is never retried: a
//! failure that Discord did not definitely refuse is reported as "may have been
//! submitted" and the person decides. Where a category asks for details this
//! client cannot collect, or Discord asks for a verification only its own
//! client can do, the panel says so and offers Discord's web form.
//!
//! Debug output of the events never contains what is reported.

use std::fmt;

use fastcord_discord::state::navigation::NavigationSnapshot;
use fastcord_discord::{
    NetworkFailure, NodeKind, RelationshipAction, ReportMenu, ReportReceipt, ReportRequestError,
    ReportTarget, RestClient, RestError, RetryableFailure,
};
use fastcord_model::Snowflake;
use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Element, Length, Task};

use crate::style;

/// Discord's own report form, the honest fallback (unverified link).
const WEB_FORM: &str = "https://dis.gd/report";
const MAX_TITLE_CHARS: usize = 64;

/// What is being reported, as shown in the review. Built from what the
/// timeline, the profile card or the server header already holds.
#[derive(Clone)]
pub struct Subject {
    pub target: ReportTarget,
    /// "message", "voice message", "user" or "server".
    pub label: &'static str,
    /// The author, user or server name.
    pub title: String,
    /// A clipped excerpt of a message; empty for the others.
    pub excerpt: String,
    /// The message link, offered for the web form.
    pub link: Option<String>,
    /// The person Ignore or Block would apply to afterwards.
    pub person: Option<(Snowflake, String)>,
}

impl Subject {
    pub fn new(target: ReportTarget, label: &'static str, title: &str) -> Self {
        Self {
            target,
            label,
            title: title.chars().take(MAX_TITLE_CHARS).collect(),
            excerpt: String::new(),
            link: None,
            person: None,
        }
    }
}

#[derive(Clone)]
pub enum Event {
    Close,
    /// Choose a category (a node of the menu) at the current step.
    Choose(u32),
    Back,
    /// Send the report. Only the review step offers it.
    Submit,
    /// Open Discord's web form in the browser.
    OpenWeb,
    CopyLink,
    /// Start reporting the open server (handled by the caller, which names it).
    OpenServer,
    /// Ignore or block the reported person, as its own separate request
    /// (handled by the caller).
    Follow(RelationshipAction, Snowflake),
    Menu(u64, Result<ReportMenu, ReportRequestError>),
    Done(u64, Result<ReportReceipt, ReportRequestError>),
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Close => "Close",
            Self::Choose(_) => "Choose",
            Self::Back => "Back",
            Self::Submit => "Submit",
            Self::OpenWeb => "OpenWeb",
            Self::CopyLink => "CopyLink",
            Self::OpenServer => "OpenServer",
            Self::Follow(..) => "Follow",
            Self::Menu(..) => "Menu",
            Self::Done(..) => "Done",
        })
    }
}

/// Why a step failed and what the person may do next.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Failure {
    text: String,
    /// The report may have reached Discord although no receipt came.
    ambiguous: bool,
    /// The web form is the way forward.
    web: bool,
}

enum Stage {
    Loading,
    Choosing {
        menu: ReportMenu,
        /// The categories chosen so far (nodes after the root).
        path: Vec<u32>,
    },
    /// The review: what will be sent, awaiting an explicit Submit.
    Confirm {
        menu: ReportMenu,
        path: Vec<u32>,
    },
    Sending,
    Done(ReportReceipt),
    Failed(Failure),
    /// The message is gone from the timeline; nothing was sent.
    Gone,
}

struct Session {
    subject: Subject,
    stage: Stage,
    notice: Option<String>,
    /// Ignore or block was already requested from this panel.
    followed: bool,
}

#[derive(Default)]
pub struct State {
    session: Option<Session>,
    serial: u64,
    /// Reports sent from the current session (never more than one).
    submissions: u32,
    /// Dropped (aborting the request) on close, logout or account change.
    task: Option<iced::task::Handle>,
}

fn describe(error: ReportRequestError, submitting: bool) -> Failure {
    let rest = match error {
        ReportRequestError::Invalid(_) => {
            return Failure {
                text: "That report could not be built from Discord's menu. Nothing was sent."
                    .to_owned(),
                ambiguous: false,
                web: true,
            };
        }
        ReportRequestError::Rest(rest) => rest,
    };
    let refused = |text: &str, web: bool| Failure {
        text: format!("{text} Nothing was submitted."),
        ambiguous: false,
        web,
    };
    match rest {
        RestError::AuthenticationRequired => refused("Your session has expired. Log in again.", false),
        RestError::WriteNotVerified(_) => refused(fastcord_discord::WRITE_NOT_VERIFIED, false),
        RestError::PermissionDenied => refused(
            "Discord refused this report for this account; it may need a verification or age check only the official client can complete.",
            true,
        ),
        RestError::ResourceGone => refused(
            "Discord can no longer find it; it was deleted or you can no longer see it.",
            false,
        ),
        RestError::InvalidRateLimit | RestError::RateLimited | RestError::SchedulerCapacityExceeded => {
            refused("Discord is rate limiting requests. Try again later.", false)
        }
        RestError::Http(status) if status.as_u16() == 429 => {
            refused("Discord is rate limiting requests. Try again later.", false)
        }
        RestError::Http(status) if status.is_client_error() => refused(
            "Discord rejected the report, or asks for a challenge or verification only the official client can complete.",
            true,
        ),
        RestError::InvalidJson if !submitting => Failure {
            text: "Discord's report menu has a shape this client does not understand."
                .to_owned(),
            ambiguous: false,
            web: true,
        },
        RestError::RequestBodyTooLarge | RestError::InvalidRoute | RestError::InvalidToken => {
            refused("The report could not be built.", true)
        }
        RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection))
            if !submitting =>
        {
            Failure {
                text: "Could not reach Discord to load the report menu. Nothing was submitted."
                    .to_owned(),
                ambiguous: false,
                web: false,
            }
        }
        _ if !submitting => Failure {
            text: "The report menu could not be loaded. Nothing was submitted.".to_owned(),
            ambiguous: false,
            web: true,
        },
        _ => Failure {
            text: "Discord did not confirm. The report may have been submitted. It is not sent again automatically; check with Discord before reporting again."
                .to_owned(),
            ambiguous: true,
            web: false,
        },
    }
}

impl State {
    #[cfg(test)]
    pub fn is_open(&self) -> bool {
        self.session.is_some()
    }

    /// Logout or account change: drops the panel and any request in flight.
    pub fn clear(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            ..Self::default()
        };
    }

    fn sending(&self) -> bool {
        matches!(
            self.session,
            Some(Session {
                stage: Stage::Sending,
                ..
            })
        )
    }

    /// The message being reported, so the caller can say whether it is still
    /// held.
    pub fn watched(&self) -> Option<(Snowflake, Snowflake)> {
        match self.session.as_ref()?.subject.target {
            ReportTarget::Message { channel, message } => Some((channel, message)),
            _ => None,
        }
    }

    /// Opens the panel on a subject and reads Discord's menu (a read; nothing
    /// is reported). A report being sent is never replaced.
    pub fn begin(&mut self, subject: Subject, client: &RestClient) -> Task<Event> {
        if self.sending() {
            return Task::none();
        }
        self.serial = self.serial.wrapping_add(1);
        self.submissions = 0;
        let (serial, target) = (self.serial, subject.target);
        self.session = Some(Session {
            subject,
            stage: Stage::Loading,
            notice: None,
            followed: false,
        });
        let client = client.clone();
        let (task, handle) = Task::perform(
            async move { client.report_menu(target).await },
            move |result| Event::Menu(serial, result),
        )
        .abortable();
        self.task = Some(handle.abort_on_drop());
        task
    }

    /// Follows the timeline: a message deleted (or no longer held) before the
    /// report was sent ends the report, with nothing submitted.
    pub fn sync(&mut self, retained: Option<bool>) {
        if retained != Some(false) {
            return;
        }
        if let Some(session) = &mut self.session
            && matches!(
                session.stage,
                Stage::Loading | Stage::Choosing { .. } | Stage::Confirm { .. }
            )
        {
            session.stage = Stage::Gone;
            self.task = None;
        }
    }

    pub fn update(&mut self, event: Event, client: &RestClient) -> Task<Event> {
        match event {
            Event::Close => {
                // A report in flight is awaited, never abandoned.
                if !self.sending() {
                    self.session = None;
                    self.task = None;
                }
            }
            Event::Menu(serial, result) if serial == self.serial => {
                self.task = None;
                if let Some(session) = self
                    .session
                    .as_mut()
                    .filter(|s| matches!(s.stage, Stage::Loading))
                {
                    session.stage = match result {
                        Ok(menu) => Stage::Choosing {
                            menu,
                            path: Vec::new(),
                        },
                        Err(error) => Stage::Failed(describe(error, false)),
                    };
                }
            }
            Event::Done(serial, result) if serial == self.serial => {
                self.task = None;
                let sending = self.sending();
                if let Some(session) = self.session.as_mut().filter(|_| sending) {
                    session.stage = match result {
                        Ok(receipt) => Stage::Done(receipt),
                        Err(error) => Stage::Failed(describe(error, true)),
                    };
                }
            }
            Event::Choose(id) => {
                if let Some(Session {
                    stage: Stage::Choosing { menu, path },
                    notice,
                    ..
                }) = &mut self.session
                {
                    let at = path.last().copied().unwrap_or(menu.root());
                    let known = menu
                        .node(at)
                        .is_some_and(|node| node.choices.iter().any(|(_, child)| *child == id));
                    let kind = menu.node(id).map(|node| node.kind.clone());
                    match (known, kind) {
                        (true, Some(NodeKind::Choice)) => {
                            path.push(id);
                            *notice = None;
                        }
                        (true, Some(NodeKind::Submit)) => {
                            path.push(id);
                            *notice = None;
                            let (menu, path) = (menu.clone(), path.clone());
                            if let Some(session) = &mut self.session {
                                session.stage = Stage::Confirm { menu, path };
                            }
                        }
                        (true, Some(NodeKind::Unsupported)) => {
                            *notice = Some(
                                "This category asks for details this client cannot collect. Use Discord's web form."
                                    .to_owned(),
                            );
                        }
                        _ => {}
                    }
                }
            }
            Event::Back => {
                if let Some(session) = &mut self.session {
                    session.notice = None;
                    session.stage = match std::mem::replace(&mut session.stage, Stage::Loading) {
                        Stage::Confirm { menu, mut path } | Stage::Choosing { menu, mut path } => {
                            path.pop();
                            Stage::Choosing { menu, path }
                        }
                        other => other,
                    };
                }
            }
            Event::Submit => {
                let Some(session) = &mut self.session else {
                    return Task::none();
                };
                let Stage::Confirm { menu, path } = &session.stage else {
                    return Task::none();
                };
                let (menu, path, target) = (menu.clone(), path.clone(), session.subject.target);
                // The menu is walked again: only a real category is sent.
                if menu.walk(&path).is_err() {
                    session.stage = Stage::Failed(describe(
                        ReportRequestError::Invalid(fastcord_discord::ReportError::InvalidPath),
                        true,
                    ));
                    return Task::none();
                }
                session.stage = Stage::Sending;
                self.submissions += 1;
                let (serial, client) = (self.serial, client.clone());
                let (task, handle) = Task::perform(
                    async move { client.submit_report(&menu, target, &path).await },
                    move |result| Event::Done(serial, result),
                )
                .abortable();
                self.task = Some(handle.abort_on_drop());
                return task;
            }
            Event::OpenWeb => {
                let _ = fastcord_platform::open_url(WEB_FORM);
            }
            Event::CopyLink => {
                if let Some(link) = self
                    .session
                    .as_ref()
                    .and_then(|session| session.subject.link.clone())
                {
                    return iced::clipboard::write(link);
                }
            }
            Event::Follow(..) => {
                if let Some(session) = &mut self.session {
                    session.followed = true;
                    session.notice = Some(
                        "Requested. The result appears under Safety (Ignored) or Friends (Blocked)."
                            .to_owned(),
                    );
                }
            }
            // A stale answer of an earlier session.
            Event::Menu(..) | Event::Done(..) | Event::OpenServer => {}
        }
        Task::none()
    }

    /// A 401 from this panel's request: the caller stops the account.
    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Menu(
                _,
                Err(ReportRequestError::Rest(RestError::AuthenticationRequired))
            ) | Event::Done(
                _,
                Err(ReportRequestError::Rest(RestError::AuthenticationRequired))
            )
        )
    }

    /// Who Ignore or Block may be offered for after a report, once.
    fn follow_offer<'a>(&'a self, navigation: &NavigationSnapshot) -> Option<(Snowflake, &'a str)> {
        let session = self.session.as_ref()?;
        let (user, name) = session.subject.person.as_ref()?;
        (!session.followed && !navigation.suppressed.contains_key(user))
            .then_some((*user, name.as_str()))
    }
}

fn small<'a>(label: &'static str, event: Option<Event>) -> Element<'a, Event> {
    button(text(label).size(12))
        .on_press_maybe(event)
        .style(style::tool_button(true, false))
        .into()
}

fn category_path(menu: &ReportMenu, path: &[u32]) -> String {
    let mut at = menu.root();
    let mut names = Vec::new();
    for id in path {
        if let Some(node) = menu.node(at)
            && let Some((label, _)) = node.choices.iter().find(|(_, child)| child == id)
        {
            names.push(label.as_str());
        }
        at = *id;
    }
    names.join(" > ")
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
) -> Option<Element<'a, Event>> {
    let session = state.session.as_ref()?;
    let subject = &session.subject;
    let idle = !state.sending();
    let mut panel = column![
        row![
            text(format!("Report {}", subject.label)).size(13),
            space::horizontal(),
            crate::icons::close(idle.then_some(Event::Close)),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
        text(
            "Reports go to Discord's Trust & Safety. Nothing is sent until you press Submit on the review step."
        )
        .size(11)
        .style(text::secondary),
    ]
    .spacing(6);
    let mut footer = row![].spacing(6);
    match &session.stage {
        Stage::Loading => {
            panel = panel.push(text("Loading Discord's report menu…").size(12));
        }
        Stage::Choosing { menu, path } => {
            let at = path.last().copied().unwrap_or(menu.root());
            if let Some(node) = menu.node(at) {
                panel = panel.push(text(node.header.as_str()).size(13));
                if !node.subheader.is_empty() {
                    panel = panel.push(
                        text(node.subheader.as_str())
                            .size(11)
                            .style(text::secondary),
                    );
                }
                let mut list = column![].spacing(4);
                for (label, id) in &node.choices {
                    let web_only = menu
                        .node(*id)
                        .is_some_and(|child| child.kind == NodeKind::Unsupported);
                    list = list.push(
                        button(
                            text(if web_only {
                                format!("{label} (web form)")
                            } else {
                                label.clone()
                            })
                            .size(12),
                        )
                        .on_press(Event::Choose(*id))
                        .width(Length::Fill)
                        .style(style::tool_button(!web_only, false)),
                    );
                }
                panel = panel.push(scrollable(list).height(Length::Shrink));
            }
            if !path.is_empty() {
                footer = footer.push(small("Back", Some(Event::Back)));
            }
        }
        Stage::Confirm { menu, path } => {
            panel = panel.push(text("Review before sending").size(13)).push(
                text(format!(
                    "Reporting: {} by/of {}",
                    subject.label, subject.title
                ))
                .size(12),
            );
            if !subject.excerpt.is_empty() {
                panel = panel.push(text(format!("\"{}\"", subject.excerpt)).size(12));
            }
            panel = panel
                .push(text(format!("Reason: {}", category_path(menu, path))).size(12))
                .push(
                    text(format!(
                        "Discord receives the {}'s identity, your chosen reason and your account. Discord attaches the {} itself; it is not copied from this window. The report is sent once and never retried automatically.",
                        subject.label, subject.label
                    ))
                    .size(11)
                    .style(text::secondary),
                );
            footer = footer
                .push(
                    button(text("Submit report").size(12))
                        .on_press(Event::Submit)
                        .style(button::danger),
                )
                .push(small("Back", Some(Event::Back)));
        }
        Stage::Sending => {
            panel = panel.push(text("Sending… waiting for Discord.").size(12));
        }
        Stage::Done(receipt) => {
            panel = panel.push(
                text(match &receipt.id {
                    Some(id) => format!("Discord received the report (reference {id})."),
                    None => "Discord received the report.".to_owned(),
                })
                .size(12),
            );
            if let Some((user, name)) = state.follow_offer(navigation) {
                panel = panel.push(
                    text(format!(
                        "You can also stop seeing {name}. These are separate requests."
                    ))
                    .size(11)
                    .style(text::secondary),
                );
                footer = footer
                    .push(small(
                        "Ignore",
                        Some(Event::Follow(RelationshipAction::Ignore, user)),
                    ))
                    .push(small(
                        "Block",
                        Some(Event::Follow(RelationshipAction::Block, user)),
                    ));
            }
        }
        Stage::Failed(failure) => {
            panel = panel.push(text(failure.text.as_str()).size(12).style(text::danger));
            if failure.web {
                footer = footer.push(small("Open Discord's web form", Some(Event::OpenWeb)));
                if subject.link.is_some() {
                    footer = footer.push(small("Copy message link", Some(Event::CopyLink)));
                }
            }
        }
        Stage::Gone => {
            panel = panel.push(
                text("That message is no longer there (deleted, or no longer loaded). Nothing was submitted. Open the report again from the message if it is still visible.")
                    .size(12)
                    .style(text::danger),
            );
        }
    }
    if matches!(
        session.stage,
        Stage::Choosing { .. } | Stage::Confirm { .. }
    ) {
        footer = footer.push(small("Open Discord's web form", Some(Event::OpenWeb)));
        if subject.link.is_some() {
            footer = footer.push(small("Copy message link", Some(Event::CopyLink)));
        }
    }
    panel = panel.push(footer);
    if let Some(notice) = &session.notice {
        panel = panel.push(text(notice.as_str()).size(12).style(text::secondary));
    }
    Some(
        container(panel)
            .padding(8)
            .width(Length::Fill)
            .style(style::card)
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::gateway::{ClientProperties, HostOs};
    use fastcord_discord::state::Suppression;
    use fastcord_discord::{ReportError, StatusCode};

    fn rest() -> RestClient {
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            fastcord_discord::UserToken::new("dummy".into()),
            ClientProperties::web(HostOs::Linux, "en-US", 1),
        )
        .unwrap()
    }

    const TARGET: ReportTarget = ReportTarget::Message {
        channel: Snowflake(11),
        message: Snowflake(22),
    };

    fn subject() -> Subject {
        let mut subject = Subject::new(TARGET, "message", "Alice");
        subject.excerpt = "hello".into();
        subject.person = Some((Snowflake(7), "Alice".into()));
        subject
    }

    fn menu() -> ReportMenu {
        let all: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/rest/report-menu.json")).unwrap();
        ReportMenu::parse(&serde_json::to_vec(&all["message"]).unwrap(), TARGET).unwrap()
    }

    /// Opened and with the menu loaded.
    fn choosing() -> (State, RestClient) {
        let client = rest();
        let mut state = State::default();
        let _ = state.begin(subject(), &client);
        let serial = state.serial;
        let _ = state.update(Event::Menu(serial, Ok(menu())), &client);
        (state, client)
    }

    fn confirming() -> (State, RestClient) {
        let (mut state, client) = choosing();
        let _ = state.update(Event::Choose(1), &client);
        (state, client)
    }

    fn stage(state: &State) -> &Stage {
        &state.session.as_ref().unwrap().stage
    }

    fn failure(state: &State) -> Failure {
        match stage(state) {
            Stage::Failed(failure) => failure.clone(),
            _ => panic!("not failed"),
        }
    }

    fn http(status: u16) -> ReportRequestError {
        ReportRequestError::Rest(RestError::Http(StatusCode::from_u16(status).unwrap()))
    }

    #[test]
    fn nothing_is_submitted_until_the_review_step_is_confirmed() {
        let client = rest();
        let mut state = State::default();
        let _ = state.begin(subject(), &client);
        assert!(matches!(stage(&state), Stage::Loading));
        // No amount of choosing, going back or submitting early sends one.
        let _ = state.update(Event::Submit, &client);
        let serial = state.serial;
        let _ = state.update(Event::Menu(serial, Ok(menu())), &client);
        let _ = state.update(Event::Submit, &client);
        let _ = state.update(Event::Choose(3), &client);
        let _ = state.update(Event::Submit, &client);
        let _ = state.update(Event::Back, &client);
        assert!(matches!(stage(&state), Stage::Choosing { path, .. } if path.is_empty()));
        assert_eq!(state.submissions, 0);
        // A leaf category leads to the review, which still has not sent.
        let _ = state.update(Event::Choose(1), &client);
        assert!(matches!(stage(&state), Stage::Confirm { .. }));
        assert_eq!(state.submissions, 0);
    }

    #[test]
    fn dismissal_at_any_step_leaves_nothing_submitted() {
        for steps in [0, 1, 2] {
            let (mut state, client) = choosing();
            if steps >= 1 {
                let _ = state.update(Event::Choose(1), &client);
            }
            if steps >= 2 {
                let _ = state.update(Event::Back, &client);
            }
            let _ = state.update(Event::Close, &client);
            assert!(!state.is_open() && state.task.is_none());
            // Nothing left to confirm: a late Submit does nothing.
            let _ = state.update(Event::Submit, &client);
            assert_eq!(state.submissions, 0);
        }
    }

    #[test]
    fn submit_sends_once_and_a_receipt_is_distinct_from_a_failure() {
        let (mut state, client) = confirming();
        let _ = state.update(Event::Submit, &client);
        assert_eq!(state.submissions, 1);
        assert!(state.sending());
        // Repeats, closing and a new report while in flight are refused.
        let _ = state.update(Event::Submit, &client);
        let _ = state.update(Event::Close, &client);
        let _ = state.begin(subject(), &client);
        assert_eq!(state.submissions, 1);
        assert!(state.sending());
        let serial = state.serial;
        let _ = state.update(
            Event::Done(
                serial,
                Ok(ReportReceipt {
                    id: Some("9".into()),
                }),
            ),
            &client,
        );
        assert!(matches!(stage(&state), Stage::Done(r) if r.id.as_deref() == Some("9")));
        // Once finished it cannot be sent again from the same review.
        let _ = state.update(Event::Submit, &client);
        assert_eq!(state.submissions, 1);
    }

    #[test]
    fn a_category_needing_input_is_not_chosen_and_points_to_the_web_form() {
        let (mut state, client) = choosing();
        let _ = state.update(Event::Choose(2), &client);
        assert!(matches!(stage(&state), Stage::Choosing { path, .. } if path.is_empty()));
        assert!(
            state
                .session
                .as_ref()
                .unwrap()
                .notice
                .as_deref()
                .is_some_and(|n| n.contains("web form"))
        );
        // A node that is not a choice of this step cannot be chosen either.
        let _ = state.update(Event::Choose(4), &client);
        assert!(matches!(stage(&state), Stage::Choosing { path, .. } if path.is_empty()));
    }

    #[test]
    fn a_message_deleted_before_submitting_ends_the_report() {
        let (mut state, client) = confirming();
        state.sync(None);
        state.sync(Some(true));
        assert!(matches!(stage(&state), Stage::Confirm { .. }));
        state.sync(Some(false));
        assert!(matches!(stage(&state), Stage::Gone));
        let _ = state.update(Event::Submit, &client);
        assert_eq!(state.submissions, 0);
        // A report already sent is not rewritten by a later deletion.
        let (mut state, client) = confirming();
        let _ = state.update(Event::Submit, &client);
        state.sync(Some(false));
        assert!(state.sending());
        assert_eq!(state.watched(), Some((Snowflake(11), Snowflake(22))));
    }

    #[test]
    fn refusals_say_nothing_was_submitted_and_challenges_point_to_the_web() {
        for (error, web) in [
            (http(400), true),
            (ReportRequestError::Rest(RestError::PermissionDenied), true),
            (ReportRequestError::Rest(RestError::ResourceGone), false),
            (http(429), false),
            (ReportRequestError::Rest(RestError::InvalidRateLimit), false),
        ] {
            let failure = describe(error, true);
            assert!(!failure.ambiguous, "{error:?}");
            assert_eq!(failure.web, web, "{error:?}");
            assert!(failure.text.contains("Nothing was submitted"), "{error:?}");
        }
    }

    #[test]
    fn an_ambiguous_failure_may_have_been_submitted_and_is_never_retried() {
        for error in [
            ReportRequestError::Rest(RestError::Retryable(RetryableFailure::Network(
                NetworkFailure::Timeout,
            ))),
            ReportRequestError::Rest(RestError::Retryable(RetryableFailure::Server(
                StatusCode::BAD_GATEWAY,
            ))),
            ReportRequestError::Rest(RestError::ResponseBodyTooLarge),
        ] {
            let (mut state, client) = confirming();
            let _ = state.update(Event::Submit, &client);
            let serial = state.serial;
            let _ = state.update(Event::Done(serial, Err(error)), &client);
            let failure = failure(&state);
            assert!(failure.ambiguous && failure.text.contains("may have been submitted"));
            assert!(failure.text.contains("not sent again automatically"));
            // The panel offers no way to send again from here.
            let _ = state.update(Event::Submit, &client);
            assert_eq!(state.submissions, 1);
            assert!(!state.sending());
        }
    }

    #[test]
    fn menu_failures_are_reads_and_never_ambiguous() {
        let (mut state, client) = (State::default(), rest());
        let _ = state.begin(subject(), &client);
        let serial = state.serial;
        let _ = state.update(
            Event::Menu(
                serial,
                Err(ReportRequestError::Rest(RestError::InvalidJson)),
            ),
            &client,
        );
        let failure = failure(&state);
        assert!(!failure.ambiguous && failure.web);
        let wrong = describe(ReportRequestError::Invalid(ReportError::WrongMenu), false);
        assert!(!wrong.ambiguous);
    }

    #[test]
    fn answers_of_an_earlier_session_are_dropped() {
        let (mut state, client) = confirming();
        let stale = state.serial;
        let _ = state.update(Event::Submit, &client);
        state.clear();
        assert!(!state.is_open());
        let _ = state.update(Event::Done(stale, Ok(ReportReceipt { id: None })), &client);
        let _ = state.update(Event::Menu(stale, Ok(menu())), &client);
        assert!(!state.is_open());
        // A newer report ignores the older request's answer too.
        let (mut state, client) = (State::default(), rest());
        let _ = state.begin(subject(), &client);
        let first = state.serial;
        let _ = state.begin(subject(), &client);
        let _ = state.update(Event::Menu(first, Ok(menu())), &client);
        assert!(matches!(stage(&state), Stage::Loading));
    }

    #[test]
    fn a_401_stops_the_account() {
        let unauthorized = ReportRequestError::Rest(RestError::AuthenticationRequired);
        assert!(State::is_rejected(&Event::Menu(1, Err(unauthorized))));
        assert!(State::is_rejected(&Event::Done(1, Err(unauthorized))));
        assert!(!State::is_rejected(&Event::Done(1, Err(http(403)))));
    }

    #[test]
    fn ignore_and_block_are_offered_once_and_only_for_a_visible_person() {
        let (mut state, client) = confirming();
        let mut navigation = NavigationSnapshot::default();
        assert_eq!(
            state.follow_offer(&navigation),
            Some((Snowflake(7), "Alice"))
        );
        navigation.suppressed =
            std::sync::Arc::new([(Snowflake(7), Suppression::Ignored)].into_iter().collect());
        assert_eq!(state.follow_offer(&navigation), None);
        navigation.suppressed = Default::default();
        let _ = state.update(
            Event::Follow(RelationshipAction::Block, Snowflake(7)),
            &client,
        );
        assert_eq!(state.follow_offer(&navigation), None);
    }

    #[test]
    fn debug_output_names_nothing_reported() {
        let text = format!(
            "{:?} {:?} {:?}",
            Event::Choose(31337),
            Event::Menu(1, Ok(menu())),
            Event::Follow(RelationshipAction::Block, Snowflake(424242)),
        );
        assert!(
            !text.contains("31337") && !text.contains("424242"),
            "{text}"
        );
    }
}
