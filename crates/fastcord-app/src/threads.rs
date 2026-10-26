//! Thread participation beside the open conversation (SPEC §16 milestone 48):
//! the archived-thread list of a channel, join/leave, and naming a thread
//! started from a message. Active threads live in the reducer's store and the
//! sidebar; opening a thread is an ordinary channel selection, so its history,
//! composer, replies and reactions are the channel ones.
//!
//! Archived threads are read one page at a time from Discord when the panel
//! opens or "Load more" is pressed and are kept only while the panel is open:
//! at most [`MAX_ARCHIVED`] of them and [`MAX_ARCHIVED_BYTES`] of names.
//! Join, leave and create send one request each; the Gateway's THREAD_* events
//! (not these answers) bring the sidebar to its new state. Archiving and
//! locking a thread (issue #95) is one explicit click and one request for a
//! thread manager: it is checked again against the current read model by the
//! caller, at most one is in flight, it is never retried, and an unconfirmed
//! answer says the change may have been applied.

use fastcord_discord::{ArchivedThreads, RestClient, RestError, ThreadError, thread_name};
use fastcord_model::{Channel, Snowflake};

use crate::i18n::{tr, trf};
use crate::ui::{button, primary_button};
use iced::widget::{column, container, row, scrollable, text, text_input};
use iced::{Color, Element, Length, Task};

pub const MAX_ARCHIVED: usize = 200;
pub const MAX_ARCHIVED_BYTES: usize = 64 * 1024;
const NAME_CHARS: usize = 100;

const PANEL: Color = crate::style::SIDEBAR;
const ROW: Color = crate::style::CHAT;
const MUTED: Color = crate::style::SUBTLE;
const WARN: Color = crate::style::RED;

#[derive(Clone, Debug)]
pub enum Event {
    /// Open or close the archived-thread list of a text channel.
    ToggleArchived(Snowflake, Snowflake),
    /// The same for a forum, listed inside the forum pane (`forum_archive`)
    /// instead of this panel.
    ToggleForumArchived(Snowflake, Snowflake),
    MoreArchived,
    /// Open the archived thread at this index of the list.
    OpenArchived(usize),
    Join(Snowflake),
    Leave(Snowflake),
    /// Archive or lock a thread (a `None` field is left alone).
    SetState {
        thread: Snowflake,
        archived: Option<bool>,
        locked: Option<bool>,
    },
    StateDone(Result<(), RestError>),
    /// Start naming a thread for a message (guild, channel, message, first text).
    Start(Snowflake, Snowflake, Snowflake, String),
    Name(String),
    Create,
    CancelCreate,
    ArchivedDone(u64, Result<ArchivedThreads, RestError>),
    Membership(Result<(), RestError>),
    Created(Result<Box<Channel>, ThreadError>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Loading,
    Failed(&'static str),
    Done,
}

struct Archived {
    guild: Snowflake,
    channel: Snowflake,
    items: Vec<Channel>,
    bytes: usize,
    cursor: Option<String>,
    has_more: bool,
    status: Status,
    /// Listed by the forum pane, not by [`view`].
    inline: bool,
}

/// The archived posts of the forum whose archive is open.
pub struct ForumArchive<'a> {
    pub posts: &'a [Channel],
    pub loading: bool,
    pub failure: Option<&'static str>,
    pub has_more: bool,
}

struct Draft {
    guild: Snowflake,
    channel: Snowflake,
    message: Snowflake,
    name: String,
    busy: bool,
}

#[derive(Default)]
pub struct State {
    archived: Option<Archived>,
    draft: Option<Draft>,
    notice: Option<&'static str>,
    /// An archive or lock request is waiting for Discord.
    state_busy: bool,
    /// Identifies the current archive request; older answers find another.
    serial: u64,
    /// Dropping this aborts the archive request in flight.
    task: Option<iced::task::Handle>,
}

fn describe(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied => tr("You are not allowed to do that with this thread."),
        RestError::ResourceGone => tr("That thread or message is gone."),
        RestError::AuthenticationRequired => tr("Discord no longer accepts this login."),
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            tr("Discord is rate limiting this. Try again shortly.")
        }
        RestError::Retryable(_) | RestError::Http(_) => tr("Discord failed. Try again."),
        RestError::WriteNotVerified(_) => tr(fastcord_discord::WRITE_NOT_VERIFIED),
        _ => tr("Discord did not accept that."),
    }
}

/// What a failed archive or lock request tells the user.
fn describe_write(error: RestError) -> &'static str {
    match error {
        RestError::Retryable(_) => tr(
            "Discord did not confirm. The change may have been applied; check the thread before trying again.",
        ),
        RestError::PermissionDenied => tr("Discord refused: you need Manage Threads here."),
        RestError::Refused(refusal) if refusal.status.as_u16() == 403 => {
            tr("Discord refused: you need Manage Threads here.")
        }
        RestError::Refused(refusal) if refusal.status.as_u16() == 404 => tr("That thread is gone."),
        other => describe(other),
    }
}

fn item_bytes(thread: &Channel) -> usize {
    64 + thread.name.as_deref().map_or(0, str::len)
}

/// The first line of a message, cut to a usable thread name.
pub fn suggest_name(content: &str) -> String {
    content
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(NAME_CHARS)
        .collect()
}

impl State {
    pub fn drop_all(&mut self) {
        *self = Self::default();
    }

    /// The channel whose archive is listed, if the panel is open.
    pub fn archive_of(&self) -> Option<Snowflake> {
        self.archived.as_ref().map(|a| a.channel)
    }

    /// The archive of `forum`, when its inline list is open.
    pub fn forum_archive(&self, forum: Snowflake) -> Option<ForumArchive<'_>> {
        let archived = self
            .archived
            .as_ref()
            .filter(|a| a.inline && a.channel == forum)?;
        Some(ForumArchive {
            posts: &archived.items,
            loading: archived.status == Status::Loading,
            failure: match archived.status {
                Status::Failed(reason) => Some(reason),
                _ => None,
            },
            has_more: archived.status == Status::Done && archived.has_more,
        })
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.archived.is_none() && self.draft.is_none() && self.notice.is_none()
    }

    /// The archived thread at `index` and its guild, for the caller to open.
    pub fn archived_thread(&self, index: usize) -> Option<(Snowflake, Channel)> {
        let archived = self.archived.as_ref()?;
        Some((archived.guild, archived.items.get(index)?.clone()))
    }

    /// Closes the archive and naming panels (a thread was opened).
    pub fn close(&mut self) {
        self.serial = self.serial.wrapping_add(1);
        self.task = None;
        self.archived = None;
        self.draft = None;
    }

    fn load(&mut self, client: &RestClient) -> Task<Event> {
        let Some(archived) = &mut self.archived else {
            return Task::none();
        };
        archived.status = Status::Loading;
        let (client, before) = (client.clone(), archived.cursor.clone());
        let (guild, channel) = (archived.guild, archived.channel);
        let serial = self.serial;
        let (task, handle) = Task::perform(
            async move {
                client
                    .archived_threads(guild, channel, before.as_deref())
                    .await
            },
            move |result| Event::ArchivedDone(serial, result),
        )
        .abortable();
        self.task = Some(handle.abort_on_drop());
        task
    }

    /// Sends the archive or lock change once. `allowed` is the caller's check
    /// of the thread, its state and Manage Threads against the current read
    /// model; without it, or while another change is out, nothing is sent.
    pub fn set_state(
        &mut self,
        change: (Snowflake, Option<bool>, Option<bool>),
        allowed: bool,
        client: &RestClient,
    ) -> Task<Event> {
        self.notice = None;
        if self.state_busy {
            return Task::none();
        }
        if !allowed {
            self.notice = Some(tr("This thread cannot be changed that way any more."));
            return Task::none();
        }
        self.state_busy = true;
        let (thread, archived, locked) = change;
        let client = client.clone();
        Task::perform(
            async move { client.set_thread_state(thread, archived, locked).await },
            Event::StateDone,
        )
    }

    pub fn update(&mut self, event: Event, client: &RestClient) -> Task<Event> {
        match event {
            Event::ToggleArchived(guild, channel) | Event::ToggleForumArchived(guild, channel) => {
                let inline = matches!(event, Event::ToggleForumArchived(..));
                if self.archive_of() == Some(channel) {
                    self.close();
                } else {
                    self.close();
                    self.notice = None;
                    self.archived = Some(Archived {
                        guild,
                        channel,
                        items: Vec::new(),
                        bytes: 0,
                        cursor: None,
                        has_more: false,
                        status: Status::Loading,
                        inline,
                    });
                    return self.load(client);
                }
            }
            Event::MoreArchived => {
                if self
                    .archived
                    .as_ref()
                    .is_some_and(|a| a.status == Status::Done && a.has_more && a.cursor.is_some())
                {
                    self.serial = self.serial.wrapping_add(1);
                    return self.load(client);
                }
            }
            Event::ArchivedDone(serial, result) if serial == self.serial => {
                self.task = None;
                let Some(archived) = &mut self.archived else {
                    return Task::none();
                };
                match result {
                    Ok(page) => {
                        let mut full = false;
                        for thread in &page.threads {
                            if archived.items.iter().any(|held| held.id == thread.id) {
                                continue;
                            }
                            if archived.items.len() >= MAX_ARCHIVED
                                || archived.bytes + item_bytes(thread) > MAX_ARCHIVED_BYTES
                            {
                                full = true;
                                break;
                            }
                            archived.bytes += item_bytes(thread);
                            archived.items.push(thread.clone());
                        }
                        archived.cursor = page.cursor().map(str::to_owned);
                        archived.has_more = page.has_more && !full && archived.cursor.is_some();
                        archived.status = Status::Done;
                    }
                    Err(error) => archived.status = Status::Failed(describe(error)),
                }
            }
            Event::Join(thread) => {
                self.notice = None;
                let client = client.clone();
                return Task::perform(async move { client.join_thread(thread).await }, |r| {
                    Event::Membership(r)
                });
            }
            Event::Leave(thread) => {
                self.notice = None;
                let client = client.clone();
                return Task::perform(async move { client.leave_thread(thread).await }, |r| {
                    Event::Membership(r)
                });
            }
            Event::Membership(result) => {
                self.notice = result.err().map(describe);
            }
            Event::StateDone(result) => {
                self.state_busy = false;
                self.notice = result.err().map(describe_write);
            }
            // Sent by the caller through `set_state`, which checks it first.
            Event::SetState { .. } => {}
            Event::Start(guild, channel, message, first_line) => {
                self.close();
                self.notice = None;
                self.draft = Some(Draft {
                    guild,
                    channel,
                    message,
                    name: suggest_name(&first_line),
                    busy: false,
                });
            }
            Event::Name(name) => {
                if let Some(draft) = self.draft.as_mut().filter(|d| !d.busy) {
                    draft.name = name.chars().take(NAME_CHARS).collect();
                }
            }
            Event::Create => {
                let Some(draft) = self.draft.as_mut().filter(|d| !d.busy) else {
                    return Task::none();
                };
                if thread_name(&draft.name).is_err() {
                    self.notice = Some(tr("Give the thread a name."));
                    return Task::none();
                }
                self.notice = None;
                draft.busy = true;
                let (client, name) = (client.clone(), draft.name.clone());
                let (guild, channel, message) = (draft.guild, draft.channel, draft.message);
                return Task::perform(
                    async move {
                        client
                            .create_thread(guild, channel, message, &name)
                            .await
                            .map(Box::new)
                    },
                    Event::Created,
                );
            }
            Event::CancelCreate => self.draft = None,
            Event::Created(Err(error)) => {
                if let Some(draft) = &mut self.draft {
                    draft.busy = false;
                }
                self.notice = Some(match error {
                    ThreadError::InvalidName => tr("Give the thread a name."),
                    ThreadError::InvalidResponse => {
                        tr("Discord answered with something unexpected.")
                    }
                    ThreadError::Rest(error) => describe(error),
                });
            }
            // Opening is the caller's: it needs the gateway. Stale answers and
            // events for panels since closed change nothing.
            Event::OpenArchived(_) | Event::Created(Ok(_)) | Event::ArchivedDone(..) => {}
        }
        Task::none()
    }
}

/// The archive and naming panels, when there is anything to show.
pub fn view(state: &State) -> Option<Element<'_, Event>> {
    let listed = state.archived.as_ref().is_some_and(|a| !a.inline);
    if !listed && state.draft.is_none() && state.notice.is_none() {
        return None;
    }
    let mut body = column![].spacing(6);
    if let Some(draft) = &state.draft {
        let mut input = text_input(tr("Thread name"), &draft.name)
            .size(14)
            .padding(6);
        if !draft.busy {
            input = input.on_input(Event::Name).on_submit(Event::Create);
        }
        body = body
            .push(text(tr("Start a thread from this message")).size(14))
            .push(
                row![
                    input,
                    primary_button(text(tr("Create")).size(13))
                        .on_press_maybe((!draft.busy).then_some(Event::Create)),
                    button(text(tr("Cancel")).size(13))
                        .on_press_maybe((!draft.busy).then_some(Event::CancelCreate)),
                ]
                .spacing(8),
            );
    }
    if let Some(archived) = state.archived.as_ref().filter(|a| !a.inline) {
        let status: Element<'_, Event> = match &archived.status {
            Status::Loading => text(tr("Loading archived threads…"))
                .size(12)
                .color(MUTED)
                .into(),
            Status::Failed(reason) => text(tr(reason)).size(12).color(WARN).into(),
            Status::Done if archived.items.is_empty() => text(tr("No archived threads."))
                .size(12)
                .color(MUTED)
                .into(),
            Status::Done => text(trf("{} archived", &[&archived.items.len()]))
                .size(12)
                .color(MUTED)
                .into(),
        };
        let mut list = column![].spacing(4);
        for (index, thread) in archived.items.iter().enumerate() {
            let locked = thread.thread_metadata.as_ref().is_some_and(|m| m.locked);
            list = list.push(
                button(
                    text(format!(
                        "{}{}",
                        thread.name.as_deref().unwrap_or(tr("Unnamed thread")),
                        if locked { tr(" (locked)") } else { "" }
                    ))
                    .size(14),
                )
                .width(Length::Fill)
                .style(|_, _| button::Style {
                    background: Some(ROW.into()),
                    text_color: Color::WHITE,
                    ..Default::default()
                })
                .on_press(Event::OpenArchived(index)),
            );
        }
        body = body.push(crate::icons::panel_title(
            tr("Archived threads"),
            Vec::new(),
            Event::ToggleArchived(archived.guild, archived.channel),
        ));
        body = body.push(status);
        if !archived.items.is_empty() {
            body = body.push(scrollable(list).height(Length::Fixed(180.0)));
        }
        if archived.status == Status::Done && archived.has_more {
            body = body.push(button(text(tr("Load more")).size(13)).on_press(Event::MoreArchived));
        }
    }
    if let Some(notice) = state.notice {
        body = body.push(text(tr(notice)).size(12).color(WARN));
    }
    Some(
        container(body)
            .padding(10)
            .width(Length::Fill)
            .style(|_| container::Style {
                background: Some(PANEL.into()),
                border: iced::Border {
                    radius: 6.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::UserToken;
    use fastcord_discord::gateway::{ClientProperties, HostOs};

    const GUILD: Snowflake = Snowflake(1);
    const PARENT: Snowflake = Snowflake(10);

    fn client() -> RestClient {
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-threads-credential".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn thread(id: u64, name: &str, archived_at: &str) -> Channel {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "type": 11, "guild_id": "1", "parent_id": "10", "name": name,
            "thread_metadata": {"archived": true, "archive_timestamp": archived_at},
        }))
        .unwrap()
    }

    fn page(threads: Vec<Channel>, has_more: bool) -> ArchivedThreads {
        ArchivedThreads { threads, has_more }
    }

    fn opened() -> State {
        let mut state = State::default();
        let _ = state.update(Event::ToggleArchived(GUILD, PARENT), &client());
        state
    }

    #[test]
    fn names_come_from_the_first_line_and_are_cut() {
        assert_eq!(
            suggest_name("  plans for friday\nsecond line"),
            "plans for friday"
        );
        assert_eq!(suggest_name(""), "");
        assert_eq!(suggest_name(&"é".repeat(300)).chars().count(), NAME_CHARS);
    }

    #[test]
    fn archive_pages_accumulate_dedupe_and_follow_the_cursor() {
        let mut state = opened();
        assert_eq!(state.archive_of(), Some(PARENT));
        let serial = state.serial;
        let first = page(
            vec![
                thread(5, "a", "2026-10-06T10:00:00+00:00"),
                thread(4, "b", "2026-10-05T10:00:00+00:00"),
            ],
            true,
        );
        let _ = state.update(Event::ArchivedDone(serial, Ok(first)), &client());
        let archived = state.archived.as_ref().unwrap();
        assert_eq!(archived.status, Status::Done);
        assert!(archived.has_more);
        assert_eq!(
            archived.cursor.as_deref(),
            Some("2026-10-05T10:00:00+00:00")
        );

        let _ = state.update(Event::MoreArchived, &client());
        let serial = state.serial;
        let second = page(
            vec![
                thread(4, "b", "2026-10-05T10:00:00+00:00"),
                thread(3, "c", "2026-10-04T10:00:00+00:00"),
            ],
            false,
        );
        let _ = state.update(Event::ArchivedDone(serial, Ok(second)), &client());
        let archived = state.archived.as_ref().unwrap();
        assert_eq!(
            archived.items.iter().map(|t| t.id.0).collect::<Vec<_>>(),
            [5, 4, 3]
        );
        assert!(!archived.has_more);
        assert_eq!(state.archived_thread(2).unwrap().0, GUILD);
        assert!(state.archived_thread(3).is_none());
    }

    #[test]
    fn stale_answers_closed_panels_and_errors_are_handled() {
        let mut state = opened();
        let stale = state.serial.wrapping_sub(1);
        let _ = state.update(
            Event::ArchivedDone(stale, Ok(page(vec![thread(5, "a", "t")], false))),
            &client(),
        );
        assert!(state.archived.as_ref().unwrap().items.is_empty());
        let serial = state.serial;
        let _ = state.update(
            Event::ArchivedDone(serial, Err(RestError::PermissionDenied)),
            &client(),
        );
        assert!(matches!(
            state.archived.as_ref().unwrap().status,
            Status::Failed(_)
        ));
        // Toggling the same channel closes; an answer after that is ignored.
        let _ = state.update(Event::ToggleArchived(GUILD, PARENT), &client());
        assert!(state.is_empty());
        let _ = state.update(
            Event::ArchivedDone(state.serial, Ok(page(vec![], false))),
            &client(),
        );
        assert!(state.is_empty());
    }

    #[test]
    fn the_archive_is_bounded_in_count_and_bytes() {
        let mut state = opened();
        let serial = state.serial;
        let many = (0..MAX_ARCHIVED as u64 + 20)
            .map(|n| thread(n + 1, "x", "t"))
            .collect();
        let _ = state.update(Event::ArchivedDone(serial, Ok(page(many, true))), &client());
        let archived = state.archived.as_ref().unwrap();
        assert!(archived.items.len() <= MAX_ARCHIVED && archived.bytes <= MAX_ARCHIVED_BYTES);
        assert!(!archived.has_more, "a full list offers no further page");
    }

    #[test]
    fn naming_a_thread_needs_a_name_and_a_failed_create_keeps_the_draft() {
        let mut state = State::default();
        let _ = state.update(
            Event::Start(GUILD, PARENT, Snowflake(77), "   ".into()),
            &client(),
        );
        let _ = state.update(Event::Create, &client());
        assert!(!state.draft.as_ref().unwrap().busy);
        assert_eq!(state.notice, Some("Give the thread a name."));
        let _ = state.update(Event::Name("plans".into()), &client());
        let _ = state.update(Event::Create, &client());
        assert!(state.draft.as_ref().unwrap().busy);
        // Typing is ignored while the request is out.
        let _ = state.update(Event::Name("other".into()), &client());
        assert_eq!(state.draft.as_ref().unwrap().name, "plans");
        let _ = state.update(
            Event::Created(Err(ThreadError::Rest(RestError::PermissionDenied))),
            &client(),
        );
        assert!(!state.draft.as_ref().unwrap().busy);
        assert!(state.notice.is_some());
        let _ = state.update(Event::CancelCreate, &client());
        assert!(state.draft.is_none());
    }

    #[tokio::test]
    async fn an_archive_or_lock_change_is_checked_sent_once_and_never_repeated() {
        let mut state = State::default();
        let thread = Snowflake(9);
        // Not allowed by the read model: nothing is sent.
        let _ = state.set_state((thread, Some(true), None), false, &client());
        assert!(!state.state_busy && state.notice.is_some());
        let _ = state.set_state((thread, Some(true), None), true, &client());
        assert!(state.state_busy && state.notice.is_none());
        // A second click while one is out sends nothing and changes nothing.
        let _ = state.set_state((thread, None, Some(true)), true, &client());
        assert!(state.state_busy && state.notice.is_none());
        let unconfirmed = RestError::Retryable(fastcord_discord::RetryableFailure::Network(
            fastcord_discord::NetworkFailure::Timeout,
        ));
        let _ = state.update(Event::StateDone(Err(unconfirmed)), &client());
        assert!(!state.state_busy);
        assert!(state.notice.unwrap().contains("may have been applied"));
        let _ = state.update(
            Event::StateDone(Err(RestError::PermissionDenied)),
            &client(),
        );
        assert!(state.notice.unwrap().contains("Manage Threads"));
        let _ = state.update(Event::StateDone(Ok(())), &client());
        assert!(state.notice.is_none());
    }

    #[test]
    fn dropping_everything_forgets_the_lists() {
        let mut state = opened();
        state.drop_all();
        assert!(state.is_empty() && state.archive_of().is_none());
    }
}
