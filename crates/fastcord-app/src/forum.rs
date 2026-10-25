//! Forum and media channels (SPEC §16 milestone 49): the post list of the open
//! forum, its tag filter and sort, the archived posts, and the new-post form.
//!
//! A post is a thread of the forum, so opening one is an ordinary channel
//! selection and its replies use the normal history and composer. The active
//! posts come from the reducer's snapshot (`ForumView`, at most 200); archived
//! posts are paged in from Discord by [`crate::threads`] only when asked for and
//! are shown here. Filter and sort are local to the forum and not persisted.
//!
//! The new-post form is a draft kept per forum (at most [`MAX_DRAFTS`]) for as
//! long as the session lives: a validation error, a permission error, a missing
//! tag or a failed upload leaves every field as typed. The post is requested
//! once. When the answer is lost the request may have created a post, so the
//! draft is shown as unconfirmed and posting again needs an explicit click,
//! unless the forum's list already holds a new post of the account with the
//! same title, which is then offered instead.

use std::path::PathBuf;
use std::sync::Arc;

use fastcord_discord::state::navigation::{ForumView, PostRow};
use fastcord_discord::{
    ForumPost, MAX_UPLOAD_FILES, PostError, PostInvalid, PostRules, RestClient, RestError,
    SendError, UploadError, UploadFile, UploadProgress, Uploader,
};
use fastcord_model::markup::TimeStyle;
use fastcord_model::{Channel, MAX_APPLIED_TAGS, Snowflake};
use iced::widget::{
    button, column, container, row, scrollable, space, text, text_editor, text_input,
};
use iced::{Color, Element, Length, Task};

use crate::composer::Act;
use crate::i18n::{tr, trf, trn};
use crate::threads::ForumArchive;
use crate::{Message, localtime, style, threads};

const BOLD: iced::Font = iced::Font {
    weight: iced::font::Weight::Bold,
    ..iced::Font::DEFAULT
};

/// Unsent drafts kept at once; the oldest idle one makes room.
pub const MAX_DRAFTS: usize = 8;
const TITLE_CHARS: usize = 100;

const MUTED: Color = crate::style::SUBTLE;
const WARN: Color = crate::style::RED;

/// Why a post did not reach Discord or was not confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Upload(UploadError),
    Post(PostError),
}

#[derive(Clone, Debug)]
pub enum Event {
    /// Newest activity first (`false`) or newest post first (`true`).
    Sort(bool),
    Filter(Option<Snowflake>),
    /// Open (or resume) the new-post form of the open forum.
    New,
    Title(String),
    Edit(Act),
    Tag(Snowflake),
    Attach,
    Picked(Vec<PathBuf>),
    Unattach(usize),
    Submit,
    /// The answer to the post with this serial in this forum.
    Posted(Snowflake, u64, Box<Result<Channel, Failure>>),
    /// Hide the form, keeping the draft.
    Close,
    Discard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Editing,
    Posting,
    /// The request may have created a post; only the user may repeat it.
    Unconfirmed,
}

struct Draft {
    forum: Snowflake,
    open: bool,
    title: String,
    body: text_editor::Content,
    tags: Vec<Snowflake>,
    files: Vec<UploadFile>,
    stage: Stage,
    notice: Option<String>,
    /// The forum's posts when the request left, to recognise a new one.
    known: Vec<Snowflake>,
    progress: Arc<UploadProgress>,
    serial: u64,
}

impl Draft {
    fn new(forum: Snowflake) -> Self {
        Self {
            forum,
            open: true,
            title: String::new(),
            body: text_editor::Content::new(),
            tags: Vec::new(),
            files: Vec::new(),
            stage: Stage::Editing,
            notice: None,
            known: Vec::new(),
            progress: Arc::default(),
            serial: 0,
        }
    }

    fn idle(&self) -> bool {
        self.stage == Stage::Editing
    }
}

#[derive(Default)]
pub struct State {
    /// The forum the filter and sort below belong to.
    scope: Option<Snowflake>,
    filter: Option<Snowflake>,
    creation: Option<bool>,
    drafts: Vec<Draft>,
    serial: u64,
}

fn describe(error: PostError) -> String {
    let cause = match error {
        PostError::Invalid(invalid) => return tr(invalid.reason()).to_owned(),
        PostError::Send(send) => send.cause(),
    };
    let ambiguous = matches!(error, PostError::Send(SendError::Ambiguous(_)));
    if ambiguous {
        return tr(
            "Discord did not confirm the post. It may exist: check the list before posting again.",
        )
        .to_owned();
    }
    match cause {
        RestError::PermissionDenied => {
            tr("You cannot post in this forum: a permission is missing.").to_owned()
        }
        RestError::ResourceGone => tr("This forum no longer exists.").to_owned(),
        RestError::AuthenticationRequired => tr("Discord no longer accepts this login.").to_owned(),
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            tr("Discord is rate limiting this. Try again shortly.").to_owned()
        }
        RestError::RequestBodyTooLarge => tr("The post is too large.").to_owned(),
        RestError::Refused(refusal) => match refusal.code {
            Some(50001 | 50013) => {
                tr("You cannot post in this forum: a permission is missing.").to_owned()
            }
            Some(50035) => {
                tr("Discord rejected the title, the tags or the message. Check them and try again.")
                    .to_owned()
            }
            Some(code) => trf(
                "Discord refused the post: {} ({})",
                &[&refusal.message(), &code],
            ),
            None => trf(
                "Discord refused the post ({}): {}",
                &[&refusal.status.as_u16(), &refusal.message()],
            ),
        },
        RestError::WriteNotVerified(_) => tr(fastcord_discord::WRITE_NOT_VERIFIED).to_owned(),
        _ => tr("Discord did not accept the post. Try again.").to_owned(),
    }
}

fn failure_text(failure: Failure) -> (String, bool) {
    match failure {
        Failure::Upload(error) => (tr(error.reason()).to_owned(), false),
        Failure::Post(error) => (
            describe(error),
            matches!(error, PostError::Send(SendError::Ambiguous(_))),
        ),
    }
}

impl State {
    pub fn drop_all(&mut self) {
        *self = Self::default();
    }

    fn draft(&mut self, forum: Snowflake) -> Option<&mut Draft> {
        self.drafts.iter_mut().find(|d| d.forum == forum)
    }

    /// Filter and sort belong to one forum: another forum starts clean.
    fn enter(&mut self, forum: Snowflake) {
        if self.scope != Some(forum) {
            self.scope = Some(forum);
            self.filter = None;
            self.creation = None;
        }
    }

    /// The draft of `forum` has a post that arrived with the account's name and
    /// the draft's title, after the request left.
    fn arrived(&self, view: &ForumView, me: Snowflake) -> Option<Snowflake> {
        let draft = self.drafts.iter().find(|d| d.forum == view.channel)?;
        if draft.stage != Stage::Unconfirmed {
            return None;
        }
        view.posts
            .iter()
            .find(|post| {
                post.owner == Some(me)
                    && post.title == draft.title.trim()
                    && !draft.known.contains(&post.id)
            })
            .map(|post| post.id)
    }

    /// Applies one event to the open forum `view`. Returns the task and, once
    /// Discord answered a post, the new thread for the caller to open.
    pub fn update(
        &mut self,
        event: Event,
        client: &RestClient,
        view: Option<&ForumView>,
    ) -> (Task<Event>, Option<Channel>) {
        if let Some(view) = view {
            self.enter(view.channel);
        }
        match event {
            Event::Sort(creation) => self.creation = Some(creation),
            Event::Filter(tag) => self.filter = tag,
            Event::New => {
                if let Some(view) = view.filter(|v| v.can_post) {
                    if self.draft(view.channel).is_none() {
                        if self.drafts.len() >= MAX_DRAFTS
                            && let Some(at) = self.drafts.iter().position(Draft::idle)
                        {
                            self.drafts.remove(at);
                        }
                        if self.drafts.len() < MAX_DRAFTS {
                            self.drafts.push(Draft::new(view.channel));
                        }
                    }
                    if let Some(draft) = self.draft(view.channel) {
                        draft.open = true;
                    }
                }
            }
            Event::Title(title) => {
                if let Some(draft) = view
                    .and_then(|v| self.draft(v.channel))
                    .filter(|d| d.idle())
                {
                    draft.title = title.chars().take(TITLE_CHARS).collect();
                    draft.notice = None;
                }
            }
            Event::Edit(Act(action)) => {
                if let Some(draft) = view
                    .and_then(|v| self.draft(v.channel))
                    .filter(|d| d.idle())
                {
                    draft.body.perform(action);
                }
            }
            Event::Tag(tag) => {
                if let Some(draft) = view
                    .and_then(|v| self.draft(v.channel))
                    .filter(|d| d.idle())
                    && let Some(view) = view
                    && view
                        .tags
                        .iter()
                        .any(|t| t.id == tag && (!t.moderated || view.can_manage))
                {
                    if let Some(at) = draft.tags.iter().position(|t| *t == tag) {
                        draft.tags.remove(at);
                        draft.notice = None;
                    } else if draft.tags.len() < MAX_APPLIED_TAGS {
                        draft.tags.push(tag);
                        draft.notice = None;
                    } else {
                        draft.notice = Some(tr(PostInvalid::TooManyTags.reason()).to_owned());
                    }
                }
            }
            Event::Attach => {
                if view.is_some_and(|v| {
                    v.can_attach && self.drafts.iter().any(|d| d.forum == v.channel)
                }) {
                    return (
                        Task::perform(
                            async {
                                rfd::AsyncFileDialog::new()
                                    .set_title(tr("Attach files to the post"))
                                    .pick_files()
                                    .await
                                    .map(|files| {
                                        files.into_iter().map(|f| f.path().to_owned()).collect()
                                    })
                                    .unwrap_or_default()
                            },
                            Event::Picked,
                        ),
                        None,
                    );
                }
            }
            Event::Picked(paths) => {
                if let Some(draft) = view
                    .and_then(|v| self.draft(v.channel))
                    .filter(|d| d.idle())
                {
                    for path in paths {
                        if draft.files.len() >= MAX_UPLOAD_FILES {
                            draft.notice = Some(tr(UploadError::TooMany.reason()).to_owned());
                            break;
                        }
                        match UploadFile::inspect(&path) {
                            Ok(file) => draft.files.push(file),
                            Err(error) => draft.notice = Some(tr(error.reason()).to_owned()),
                        }
                    }
                }
            }
            Event::Unattach(index) => {
                if let Some(draft) = view
                    .and_then(|v| self.draft(v.channel))
                    .filter(|d| d.idle())
                    && index < draft.files.len()
                {
                    draft.files.remove(index);
                }
            }
            Event::Submit => return (self.submit(client, view), None),
            Event::Posted(forum, serial, result) => {
                let Some(draft) = self
                    .draft(forum)
                    .filter(|d| d.serial == serial && d.stage == Stage::Posting)
                else {
                    return (Task::none(), None);
                };
                match *result {
                    Ok(thread) => {
                        self.drafts.retain(|d| d.forum != forum);
                        return (Task::none(), Some(thread));
                    }
                    Err(failure) => {
                        let (notice, ambiguous) = failure_text(failure);
                        draft.notice = Some(notice);
                        draft.stage = if ambiguous {
                            Stage::Unconfirmed
                        } else {
                            Stage::Editing
                        };
                    }
                }
            }
            Event::Close => {
                if let Some(draft) = view.and_then(|v| self.draft(v.channel)) {
                    draft.open = false;
                }
            }
            Event::Discard => {
                if let Some(view) = view {
                    self.drafts
                        .retain(|d| d.forum != view.channel || d.stage == Stage::Posting);
                }
            }
        }
        (Task::none(), None)
    }

    /// Checks the draft and, if it is sound, uploads its files and posts once.
    fn submit(&mut self, client: &RestClient, view: Option<&ForumView>) -> Task<Event> {
        let Some(view) = view else {
            return Task::none();
        };
        let serial = self.serial.wrapping_add(1);
        let Some(draft) = self
            .drafts
            .iter_mut()
            .find(|d| d.forum == view.channel && d.stage != Stage::Posting)
        else {
            return Task::none();
        };
        self.serial = serial;
        let rules = PostRules {
            require_tag: view.require_tag,
            require_file: view.media,
        };
        let body = draft.body.text();
        if !view.can_post {
            draft.notice =
                Some(tr("You cannot post in this forum: a permission is missing.").into());
            return Task::none();
        }
        if let Err(invalid) =
            ForumPost::check(&draft.title, &body, &draft.tags, draft.files.len(), rules)
        {
            draft.notice = Some(tr(invalid.reason()).to_owned());
            return Task::none();
        }
        draft.stage = Stage::Posting;
        draft.notice = None;
        draft.serial = serial;
        draft.known = view.posts.iter().map(|p| p.id).collect();
        draft.progress = Arc::default();
        let (client, guild, forum) = (client.clone(), view.guild, view.channel);
        let (title, tags, files) = (draft.title.clone(), draft.tags.clone(), draft.files.clone());
        let progress = Arc::clone(&draft.progress);
        Task::perform(
            async move {
                let uploaded = if files.is_empty() {
                    Vec::new()
                } else {
                    let uploader = Uploader::new().map_err(Failure::Upload)?;
                    client
                        .upload_files(&uploader, forum, &files, progress)
                        .await
                        .map_err(Failure::Upload)?
                };
                let post = ForumPost::new(&title, &body, &tags, uploaded, rules)
                    .map_err(|e| Failure::Post(PostError::Invalid(e)))?;
                client
                    .create_forum_post(guild, forum, &post)
                    .await
                    .map_err(Failure::Post)
            },
            move |result| Event::Posted(forum, serial, Box::new(result)),
        )
    }
}

fn chip<'a>(label: String, on: bool, press: Option<Message>) -> Element<'a, Message> {
    button(text(label).size(12))
        .padding([2, 8])
        .on_press_maybe(press)
        .style(move |theme, status| {
            if on {
                style::action_button(false)(theme, status)
            } else {
                style::tool_button(true, false)(theme, status)
            }
        })
        .into()
}

/// One post as a button: title, tags, and a line of numbers.
fn card<'a>(
    title: String,
    badges: &str,
    tags: Vec<String>,
    meta: String,
    unread: bool,
    open: Message,
) -> Element<'a, Message> {
    let mut head = row![].spacing(6).align_y(iced::alignment::Vertical::Center);
    if unread {
        head = head.push(text("\u{2022}").size(14));
    }
    head = head.push(text(title).size(15).font(BOLD));
    if !badges.is_empty() {
        head = head.push(text(badges.to_owned()).size(12).color(MUTED));
    }
    let mut chips = row![].spacing(4);
    for tag in tags {
        chips = chips.push(
            container(text(tag).size(11))
                .padding([1, 6])
                .style(style::badge),
        );
    }
    button(column![head, chips, text(meta).size(12).color(MUTED)].spacing(3))
        .width(Length::Fill)
        .padding([6, 10])
        .style(style::row_button(false, unread))
        .on_press(open)
        .into()
}

fn tag_names(view: &ForumView, ids: &[Snowflake]) -> Vec<String> {
    ids.iter()
        .filter_map(|id| view.tags.iter().find(|t| t.id == *id))
        .map(|t| match &t.emoji {
            Some(emoji) => format!("{emoji} {}", t.name),
            None => t.name.clone(),
        })
        .collect()
}

fn ago(id: Snowflake, now: i64) -> String {
    localtime::markup(
        (id.timestamp_ms() / 1000) as i64,
        TimeStyle::Relative,
        None,
        now,
    )
    .unwrap_or_default()
}

fn replies(count: u32) -> String {
    trn(count as u64, "{} reply", "{} replies")
}

fn post_card<'a>(view: &ForumView, post: &PostRow, now: i64) -> Element<'a, Message> {
    let mut badges = String::new();
    if post.pinned {
        badges.push_str(tr("pinned "));
    }
    if post.locked {
        badges.push_str(tr("locked "));
    }
    let by = if post.author.is_empty() {
        String::new()
    } else {
        format!(" \u{b7} {}", post.author)
    };
    card(
        post.title.clone(),
        badges.trim_end(),
        tag_names(view, &post.tags),
        trf(
            "{} \u{b7} active {}{}",
            &[&replies(post.replies), &ago(post.last_activity, now), &by],
        ),
        post.unread,
        Message::SelectChannel(view.guild, post.id),
    )
}

/// The forum pane: controls, the new-post form, active posts, archived posts.
pub fn view<'a>(
    state: &'a State,
    forum: &'a ForumView,
    archive: Option<ForumArchive<'a>>,
    me: Snowflake,
    now: i64,
) -> Element<'a, Message> {
    let creation = if state.scope == Some(forum.channel) {
        state.creation
    } else {
        None
    }
    .unwrap_or(forum.sort_creation);
    let filter = state.filter.filter(|_| state.scope == Some(forum.channel));
    let event = |e: Event| Some(Message::Forum(e));

    let mut controls = row![text(tr("Sort")).size(12).color(MUTED)]
        .spacing(6)
        .align_y(iced::alignment::Vertical::Center);
    controls = controls
        .push(chip(
            tr("Latest activity").into(),
            !creation,
            event(Event::Sort(false)),
        ))
        .push(chip(
            tr("Date posted").into(),
            creation,
            event(Event::Sort(true)),
        ));
    let mut tags: Vec<Element<'a, Message>> = Vec::new();
    if !forum.tags.is_empty() {
        tags.push(chip(
            tr("All").into(),
            filter.is_none(),
            event(Event::Filter(None)),
        ));
        for tag in &forum.tags {
            tags.push(chip(
                match &tag.emoji {
                    Some(emoji) => format!("{emoji} {}", tag.name),
                    None => tag.name.clone(),
                },
                filter == Some(tag.id),
                event(Event::Filter(Some(tag.id))),
            ));
        }
    }
    let mut body = column![controls, row(tags).spacing(6).wrap()].spacing(8);

    if let Some(draft) = state.drafts.iter().find(|d| d.forum == forum.channel)
        && draft.open
    {
        body = body.push(form(state, draft, forum, me));
    }

    let mut list = column![].spacing(4);
    let mut shown = 0;
    let mut posts: Vec<&PostRow> = forum
        .posts
        .iter()
        .filter(|p| filter.is_none_or(|tag| p.tags.contains(&tag)))
        .collect();
    if creation {
        posts.sort_by_key(|p| (std::cmp::Reverse(p.pinned), std::cmp::Reverse(p.id)));
    }
    for post in posts {
        shown += 1;
        list = list.push(post_card(forum, post, now));
    }
    if shown == 0 {
        list = list.push(
            text(if forum.posts.is_empty() {
                tr("No active posts.")
            } else {
                tr("No active post has this tag.")
            })
            .size(13)
            .color(MUTED),
        );
    }
    if forum.total > forum.posts.len() {
        list = list.push(
            text(trf(
                "Showing the {} most recent of {} active posts.",
                &[&forum.posts.len(), &forum.total],
            ))
            .size(12)
            .color(MUTED),
        );
    }

    list = list.push(
        row![
            text(tr("Archived posts")).size(13).color(MUTED),
            space::horizontal(),
            button(
                text(if archive.is_some() {
                    tr("Hide")
                } else {
                    tr("Load archived")
                })
                .size(12)
            )
            .on_press(Message::Threads(threads::Event::ToggleForumArchived(
                forum.guild,
                forum.channel
            )))
            .style(style::tool_button(true, false)),
        ]
        .padding([8, 0]),
    );
    if let Some(archive) = archive {
        for (index, post) in archive.posts.iter().enumerate() {
            if filter.is_some_and(|tag| !post.applied_tags.contains(&tag)) {
                continue;
            }
            let locked = post.thread_metadata.as_ref().is_some_and(|m| m.locked);
            list = list.push(card(
                post.name
                    .clone()
                    .unwrap_or_else(|| tr("Untitled post").into()),
                if locked {
                    tr("archived locked")
                } else {
                    tr("archived")
                },
                tag_names(forum, &post.applied_tags),
                trf(
                    "{} \u{b7} active {}",
                    &[
                        &replies(post.message_count.unwrap_or(0)),
                        &ago(post.last_message_id.unwrap_or(post.id), now),
                    ],
                ),
                false,
                Message::Threads(threads::Event::OpenArchived(index)),
            ));
        }
        if let Some(reason) = archive.failure {
            list = list.push(text(tr(reason)).size(12).color(WARN));
        } else if archive.loading {
            list = list.push(
                text(tr("Loading archived posts\u{2026}"))
                    .size(12)
                    .color(MUTED),
            );
        } else if archive.posts.is_empty() {
            list = list.push(text(tr("No archived posts.")).size(12).color(MUTED));
        }
        if archive.has_more {
            list = list.push(
                button(text(tr("Load more")).size(12))
                    .on_press(Message::Threads(threads::Event::MoreArchived))
                    .style(style::tool_button(true, false)),
            );
        }
    }
    body = body.push(scrollable(list).height(Length::Fill));
    container(body).padding([8, 16]).into()
}

fn form<'a>(
    state: &'a State,
    draft: &'a Draft,
    forum: &'a ForumView,
    me: Snowflake,
) -> Element<'a, Message> {
    let editing = draft.stage == Stage::Editing;
    let mut title = text_input(tr("Post title"), &draft.title)
        .size(14)
        .padding(6);
    if editing {
        title = title.on_input(|t| Message::Forum(Event::Title(t)));
    }
    let mut editor = text_editor(&draft.body)
        .placeholder(if forum.media {
            tr("Describe the post (a file is required)")
        } else {
            tr("First message of the post")
        })
        .size(14)
        .padding(8)
        .min_height(70)
        .max_height(180);
    if editing {
        editor = editor.on_action(|a| Message::Forum(Event::Edit(Act(a))));
    }
    let mut col = column![
        row![
            text(tr("New post")).size(14).font(BOLD),
            space::horizontal(),
            button(text(tr("Hide")).size(12))
                .on_press(Message::Forum(Event::Close))
                .style(style::tool_button(true, false)),
        ],
        title,
        editor,
    ]
    .spacing(6);

    if !forum.tags.is_empty() {
        let mut tags: Vec<Element<'a, Message>> = vec![
            text(if forum.require_tag {
                tr("Tags (required)")
            } else {
                tr("Tags")
            })
            .size(12)
            .color(MUTED)
            .into(),
            text(format!("{}/{MAX_APPLIED_TAGS}", draft.tags.len()))
                .size(12)
                .color(MUTED)
                .into(),
        ];
        for tag in forum
            .tags
            .iter()
            .filter(|t| !t.moderated || forum.can_manage)
        {
            tags.push(chip(
                tag.name.clone(),
                draft.tags.contains(&tag.id),
                editing.then_some(Message::Forum(Event::Tag(tag.id))),
            ));
        }
        col = col.push(row(tags).spacing(6).wrap());
    }

    let mut files: Vec<Element<'a, Message>> = Vec::new();
    if forum.can_attach {
        files.push(
            button(text(tr("Attach files")).size(12))
                .on_press_maybe(editing.then_some(Message::Forum(Event::Attach)))
                .style(style::tool_button(true, false))
                .into(),
        );
    }
    for (index, file) in draft.files.iter().enumerate() {
        files.push(chip(
            format!("{} \u{d7}", file.filename()),
            false,
            editing.then_some(Message::Forum(Event::Unattach(index))),
        ));
    }
    col = col.push(row(files).spacing(6).wrap());

    if draft.stage == Stage::Posting {
        let total = draft.progress.total();
        col = col.push(
            text(match (draft.progress.sent() * 100).checked_div(total) {
                Some(percent) => trf("Posting\u{2026} uploaded {}%", &[&percent]),
                None => tr("Posting\u{2026}").to_owned(),
            })
            .size(12)
            .color(MUTED),
        );
    }
    if let Some(notice) = &draft.notice {
        col = col.push(text(tr(notice.as_str())).size(12).color(WARN));
    }
    let mut actions = row![].spacing(8);
    if let Some(found) = state.arrived(forum, me) {
        col = col.push(
            text(tr("A post with this title from you is in the list."))
                .size(12)
                .color(MUTED),
        );
        actions = actions
            .push(
                button(text(tr("Open it")).size(13))
                    .on_press(Message::SelectChannel(forum.guild, found))
                    .style(style::action_button(false)),
            )
            .push(
                button(text(tr("Discard draft")).size(13))
                    .on_press(Message::Forum(Event::Discard))
                    .style(style::action_button(true)),
            );
    } else {
        actions = actions
            .push(
                button(
                    text(match draft.stage {
                        Stage::Unconfirmed => tr("Post again"),
                        _ => tr("Post"),
                    })
                    .size(13),
                )
                .on_press_maybe(
                    (draft.stage != Stage::Posting).then_some(Message::Forum(Event::Submit)),
                )
                .style(style::action_button(false)),
            )
            .push(
                button(text(tr("Discard draft")).size(13))
                    .on_press_maybe(
                        (draft.stage != Stage::Posting).then_some(Message::Forum(Event::Discard)),
                    )
                    .style(style::action_button(true)),
            );
    }
    container(col.push(actions))
        .padding(10)
        .width(Length::Fill)
        .style(style::card)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::state::navigation::TagView;
    use fastcord_discord::{UserToken, gateway::ClientProperties, gateway::HostOs};

    const GUILD: Snowflake = Snowflake(1);
    const FORUM: Snowflake = Snowflake(10);
    const ME: Snowflake = Snowflake(5);

    fn client() -> RestClient {
        RestClient::new(
            UserToken::new("offline-forum-credential".into()),
            ClientProperties::web(HostOs::Linux, "en-US", 1),
        )
        .unwrap()
    }

    fn forum(require_tag: bool) -> ForumView {
        ForumView {
            channel: FORUM,
            guild: GUILD,
            media: false,
            tags: vec![
                TagView {
                    id: Snowflake(7),
                    name: "bug".into(),
                    moderated: false,
                    emoji: None,
                },
                TagView {
                    id: Snowflake(8),
                    name: "staff".into(),
                    moderated: true,
                    emoji: None,
                },
            ],
            require_tag,
            sort_creation: false,
            can_post: true,
            can_attach: true,
            can_manage: false,
            posts: Vec::new(),
            total: 0,
        }
    }

    fn post(id: u64, title: &str, owner: u64) -> PostRow {
        PostRow {
            id: Snowflake(id),
            title: title.into(),
            tags: Vec::new(),
            replies: 0,
            owner: Some(Snowflake(owner)),
            author: String::new(),
            last_activity: Snowflake(id),
            pinned: false,
            locked: false,
            joined: false,
            unread: false,
            mentions: 0,
        }
    }

    fn run(state: &mut State, event: Event, view: &ForumView) -> Option<Channel> {
        state.update(event, &client(), Some(view)).1
    }

    fn typed(state: &mut State, view: &ForumView, title: &str, body: &str) {
        run(state, Event::New, view);
        run(state, Event::Title(title.into()), view);
        for ch in body.chars() {
            run(
                state,
                Event::Edit(Act(text_editor::Action::Edit(text_editor::Edit::Insert(
                    ch,
                )))),
                view,
            );
        }
    }

    fn draft(state: &State) -> &Draft {
        &state.drafts[0]
    }

    fn thread(id: u64) -> Channel {
        serde_json::from_value(serde_json::json!({"id": id.to_string(), "type": 11,
            "guild_id": "1", "parent_id": "10", "name": "t"}))
        .unwrap()
    }

    #[test]
    fn a_required_tag_blocks_the_post_and_keeps_every_field() {
        let view = forum(true);
        let mut state = State::default();
        typed(&mut state, &view, "Crash", "it crashes");
        run(&mut state, Event::Submit, &view);
        let d = draft(&state);
        assert_eq!(d.stage, Stage::Editing, "nothing was sent");
        assert_eq!(d.notice.as_deref(), Some(PostInvalid::TagRequired.reason()));
        assert_eq!(
            (d.title.as_str(), d.body.text().trim()),
            ("Crash", "it crashes")
        );
        // Moderated tags cannot be picked without Manage Threads; others can.
        run(&mut state, Event::Tag(Snowflake(8)), &view);
        assert!(draft(&state).tags.is_empty());
        run(&mut state, Event::Tag(Snowflake(7)), &view);
        assert_eq!(draft(&state).tags, [Snowflake(7)]);
        run(&mut state, Event::Tag(Snowflake(7)), &view);
        assert!(draft(&state).tags.is_empty(), "a second click removes it");
    }

    #[test]
    fn other_local_validation_keeps_the_draft_too() {
        let view = forum(false);
        let mut state = State::default();
        typed(&mut state, &view, "  ", "text");
        run(&mut state, Event::Submit, &view);
        assert_eq!(
            draft(&state).notice.as_deref(),
            Some(PostInvalid::Title.reason())
        );
        run(&mut state, Event::Title("t".into()), &view);
        assert!(
            draft(&state).notice.is_none(),
            "typing clears the old notice"
        );
        let media = ForumView {
            media: true,
            ..forum(false)
        };
        run(&mut state, Event::Submit, &media);
        assert_eq!(
            draft(&state).notice.as_deref(),
            Some(PostInvalid::FileRequired.reason())
        );
        let read_only = ForumView {
            can_post: false,
            ..forum(false)
        };
        run(&mut state, Event::Submit, &read_only);
        assert!(
            draft(&state)
                .notice
                .as_deref()
                .unwrap()
                .contains("permission")
        );
        assert_eq!(draft(&state).body.text().trim(), "text");
    }

    #[test]
    fn a_post_needs_permission_to_start_a_draft() {
        let mut state = State::default();
        let read_only = ForumView {
            can_post: false,
            ..forum(false)
        };
        run(&mut state, Event::New, &read_only);
        assert!(state.drafts.is_empty());
    }

    #[tokio::test]
    async fn refusals_and_lost_answers_keep_the_draft_and_only_a_lost_answer_waits() {
        let view = forum(false);
        for (cause, expect) in [
            (RestError::PermissionDenied, "permission"),
            (RestError::ResourceGone, "no longer exists"),
            (RestError::InvalidRateLimit, "rate limiting"),
            (
                RestError::Refused(fastcord_discord::Refusal::new(
                    reqwest::StatusCode::BAD_REQUEST,
                    Some(50035),
                    "Invalid Form Body",
                )),
                "tags",
            ),
        ] {
            let mut state = State::default();
            typed(&mut state, &view, "Crash", "it crashes");
            let _ = state.update(Event::Submit, &client(), Some(&view));
            assert_eq!(draft(&state).stage, Stage::Posting);
            let serial = draft(&state).serial;
            // A second click while the request is out does nothing.
            let before = state.serial;
            let _ = state.update(Event::Submit, &client(), Some(&view));
            assert_eq!(state.serial, before);
            assert_eq!(
                draft(&state).serial,
                serial,
                "the request in flight stays the one"
            );
            let failure = Failure::Post(PostError::Send(SendError::NotSent(cause)));
            run(
                &mut state,
                Event::Posted(FORUM, serial, Box::new(Err(failure))),
                &view,
            );
            let d = draft(&state);
            assert_eq!(d.stage, Stage::Editing, "{cause:?}");
            assert!(d.notice.as_deref().unwrap().contains(expect), "{cause:?}");
            assert_eq!(
                (d.title.as_str(), d.body.text().trim()),
                ("Crash", "it crashes")
            );
        }
        // An answer lost on the way: the post may exist, repeating needs a click.
        let mut state = State::default();
        typed(&mut state, &view, "Crash", "it crashes");
        let _ = state.update(Event::Submit, &client(), Some(&view));
        let serial = draft(&state).serial;
        let lost = Failure::Post(PostError::Send(SendError::Ambiguous(
            RestError::ResponseBodyTooLarge,
        )));
        run(
            &mut state,
            Event::Posted(FORUM, serial, Box::new(Err(lost))),
            &view,
        );
        assert_eq!(draft(&state).stage, Stage::Unconfirmed);
        assert!(
            draft(&state)
                .notice
                .as_deref()
                .unwrap()
                .contains("may exist")
        );
        // The post shows up in the list: it is recognised, not duplicated.
        assert_eq!(state.arrived(&view, ME), None);
        let seen = ForumView {
            posts: vec![post(100, "Crash", 5), post(101, "Crash", 6)],
            ..forum(false)
        };
        assert_eq!(state.arrived(&seen, ME), Some(Snowflake(100)));
        // Posting again is the user's explicit choice.
        let _ = state.update(Event::Submit, &client(), Some(&view));
        assert_eq!(draft(&state).stage, Stage::Posting);
    }

    #[tokio::test]
    async fn success_removes_the_draft_and_hands_back_the_thread_once() {
        let view = forum(false);
        let mut state = State::default();
        typed(&mut state, &view, "Crash", "it crashes");
        let _ = state.update(Event::Submit, &client(), Some(&view));
        let serial = draft(&state).serial;
        // A stale answer (another serial) is ignored.
        let stale = run(
            &mut state,
            Event::Posted(FORUM, serial + 1, Box::new(Ok(thread(9)))),
            &view,
        );
        assert!(stale.is_none() && state.drafts.len() == 1);
        let opened = run(
            &mut state,
            Event::Posted(FORUM, serial, Box::new(Ok(thread(9)))),
            &view,
        );
        assert_eq!(opened.map(|t| t.id), Some(Snowflake(9)));
        assert!(state.drafts.is_empty());
        assert!(
            run(
                &mut state,
                Event::Posted(FORUM, serial, Box::new(Ok(thread(9)))),
                &view
            )
            .is_none()
        );
    }

    #[test]
    fn tags_are_capped_filters_are_per_forum_and_drafts_are_bounded() {
        let mut view = forum(false);
        view.tags = (1..=8)
            .map(|n| TagView {
                id: Snowflake(n),
                name: format!("t{n}"),
                moderated: false,
                emoji: None,
            })
            .collect();
        let mut state = State::default();
        run(&mut state, Event::New, &view);
        for n in 1..=6 {
            run(&mut state, Event::Tag(Snowflake(n)), &view);
        }
        assert_eq!(draft(&state).tags.len(), MAX_APPLIED_TAGS);
        assert!(draft(&state).notice.is_some());

        run(&mut state, Event::Filter(Some(Snowflake(3))), &view);
        run(&mut state, Event::Sort(true), &view);
        assert_eq!(
            (state.filter, state.creation),
            (Some(Snowflake(3)), Some(true))
        );
        let other = ForumView {
            channel: Snowflake(11),
            ..forum(false)
        };
        run(&mut state, Event::Close, &other);
        assert_eq!(
            (state.filter, state.creation),
            (None, None),
            "another forum starts clean"
        );

        for n in 0..MAX_DRAFTS as u64 + 4 {
            let v = ForumView {
                channel: Snowflake(100 + n),
                ..forum(false)
            };
            run(&mut state, Event::New, &v);
        }
        assert!(state.drafts.len() <= MAX_DRAFTS);
        state.drop_all();
        assert!(state.drafts.is_empty());
    }

    #[test]
    fn closing_hides_the_form_but_keeps_the_draft() {
        let view = forum(false);
        let mut state = State::default();
        typed(&mut state, &view, "Crash", "x");
        run(&mut state, Event::Close, &view);
        assert!(!draft(&state).open);
        run(&mut state, Event::New, &view);
        assert!(draft(&state).open);
        assert_eq!(draft(&state).title, "Crash");
        run(&mut state, Event::Discard, &view);
        assert!(state.drafts.is_empty());
    }
}
