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
//! (not these answers) bring the sidebar to its new state.

use fastcord_discord::{ArchivedThreads, RestClient, RestError, ThreadError, thread_name};
use fastcord_model::{Channel, Snowflake};
use iced::widget::{button, column, container, row, scrollable, space, text, text_input};
use iced::{Color, Element, Length, Task};

pub const MAX_ARCHIVED: usize = 200;
pub const MAX_ARCHIVED_BYTES: usize = 64 * 1024;
const NAME_CHARS: usize = 100;

const PANEL: Color = Color::from_rgb8(0x2b, 0x2d, 0x31);
const ROW: Color = Color::from_rgb8(0x31, 0x33, 0x38);
const MUTED: Color = Color::from_rgb8(0xb5, 0xba, 0xc1);
const WARN: Color = Color::from_rgb8(0xf2, 0x3f, 0x42);

#[derive(Clone, Debug)]
pub enum Event {
    /// Open or close the archived-thread list of a text channel.
    ToggleArchived(Snowflake, Snowflake),
    MoreArchived,
    /// Open the archived thread at this index of the list.
    OpenArchived(usize),
    Join(Snowflake),
    Leave(Snowflake),
    /// Start naming a thread for a message (guild, channel, message, first text).
    Start(Snowflake, Snowflake, Snowflake, String),
    Name(String),
    Create,
    CancelCreate,
    ArchivedDone(u64, Result<ArchivedThreads, RestError>),
    Membership(Result<(), RestError>),
    Created(Result<Channel, ThreadError>),
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
    /// Identifies the current archive request; older answers find another.
    serial: u64,
    /// Dropping this aborts the archive request in flight.
    task: Option<iced::task::Handle>,
}

fn describe(error: RestError) -> &'static str {
    match error {
        RestError::PermissionDenied => "You are not allowed to do that with this thread.",
        RestError::ResourceGone => "That thread or message is gone.",
        RestError::AuthenticationRequired => "Discord no longer accepts this login.",
        RestError::InvalidRateLimit | RestError::SchedulerCapacityExceeded => {
            "Discord is rate limiting this. Try again shortly."
        }
        RestError::Retryable(_) | RestError::Http(_) => "Discord failed. Try again.",
        _ => "Discord did not accept that.",
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

    pub fn update(&mut self, event: Event, client: &RestClient) -> Task<Event> {
        match event {
            Event::ToggleArchived(guild, channel) => {
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
                    self.notice = Some("Give the thread a name.");
                    return Task::none();
                }
                self.notice = None;
                draft.busy = true;
                let (client, name) = (client.clone(), draft.name.clone());
                let (guild, channel, message) = (draft.guild, draft.channel, draft.message);
                return Task::perform(
                    async move { client.create_thread(guild, channel, message, &name).await },
                    Event::Created,
                );
            }
            Event::CancelCreate => self.draft = None,
            Event::Created(Err(error)) => {
                if let Some(draft) = &mut self.draft {
                    draft.busy = false;
                }
                self.notice = Some(match error {
                    ThreadError::InvalidName => "Give the thread a name.",
                    ThreadError::InvalidResponse => "Discord answered with something unexpected.",
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
    if state.is_empty() {
        return None;
    }
    let mut body = column![].spacing(6);
    if let Some(draft) = &state.draft {
        let mut input = text_input("Thread name", &draft.name).size(14).padding(6);
        if !draft.busy {
            input = input.on_input(Event::Name).on_submit(Event::Create);
        }
        body = body
            .push(text("Start a thread from this message").size(14))
            .push(
                row![
                    input,
                    button(text("Create").size(13))
                        .on_press_maybe((!draft.busy).then_some(Event::Create)),
                    button(text("Cancel").size(13))
                        .on_press_maybe((!draft.busy).then_some(Event::CancelCreate)),
                ]
                .spacing(8),
            );
    }
    if let Some(archived) = &state.archived {
        let status: Element<'_, Event> = match &archived.status {
            Status::Loading => text("Loading archived threads…")
                .size(12)
                .color(MUTED)
                .into(),
            Status::Failed(reason) => text(*reason).size(12).color(WARN).into(),
            Status::Done if archived.items.is_empty() => {
                text("No archived threads.").size(12).color(MUTED).into()
            }
            Status::Done => text(format!("{} archived", archived.items.len()))
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
                        thread.name.as_deref().unwrap_or("Unnamed thread"),
                        if locked { " (locked)" } else { "" }
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
        body = body.push(row![
            text("Archived threads").size(14),
            space::horizontal(),
            button(text("Close").size(13))
                .on_press(Event::ToggleArchived(archived.guild, archived.channel)),
        ]);
        body = body.push(status);
        if !archived.items.is_empty() {
            body = body.push(scrollable(list).height(Length::Fixed(180.0)));
        }
        if archived.status == Status::Done && archived.has_more {
            body = body.push(button(text("Load more").size(13)).on_press(Event::MoreArchived));
        }
    }
    if let Some(notice) = state.notice {
        body = body.push(text(notice).size(12).color(WARN));
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
            UserToken::new("offline-threads-credential".into()),
            ClientProperties::web(HostOs::Linux, "en-US", 1),
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

    #[test]
    fn dropping_everything_forgets_the_lists() {
        let mut state = opened();
        state.drop_all();
        assert!(state.is_empty() && state.archive_of().is_none());
    }
}
