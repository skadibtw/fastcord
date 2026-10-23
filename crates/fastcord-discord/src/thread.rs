//! Thread participation (SPEC §16 milestone 48). Each call is one explicit user
//! action and exactly one request, sent once:
//!
//! - join / leave: `PUT` / `DELETE /channels/{thread}/thread-members/@me`
//! - create from a message: `POST /channels/{channel}/messages/{message}/threads`
//!   with `{"name", "auto_archive_duration"}`, answering the new thread channel
//! - archived public threads, newest archived first:
//!   `GET /channels/{channel}/threads/archived/public?limit=25[&before=<archive_timestamp>]`
//!   answering `{"threads", "members", "has_more"}`
//!
//! - a forum post (milestone 49): `POST /channels/{forum}/threads` with
//!   `{"name", "applied_tags", "message": {"content", "allowed_mentions",
//!   "attachments"}}`, answering the new thread; sent once, never repeated by
//!   the transport (see [`PostError`])
//!
//! Active threads arrive on the Gateway (GUILD_CREATE `threads`,
//! THREAD_LIST_SYNC, THREAD_CREATE/UPDATE/DELETE); nothing here lists them. The
//! wire shapes are from community documentation and observed web-client
//! traffic and are unverified against a live account (`docs/TESTING.md`).

use fastcord_model::{Channel, MAX_APPLIED_TAGS, Snowflake};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::send::{MAX_CONTENT_CHARS, MentionPolicy, SendError, WireAttachment, wire_attachments};
use crate::upload::UploadedFile;
use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route, WriteGroup};

/// Archived threads per page (the `limit` asked for, and the most accepted).
pub const ARCHIVED_PAGE_SIZE: usize = 25;
/// Discord's cap on a thread name.
pub const MAX_THREAD_NAME_CHARS: usize = 100;
/// One day, the web client's default auto-archive.
const AUTO_ARCHIVE_MINUTES: u32 = 1440;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadError {
    /// Empty after trimming, or longer than [`MAX_THREAD_NAME_CHARS`].
    InvalidName,
    /// Discord answered with something that is not the asked-for thread.
    InvalidResponse,
    Rest(RestError),
}

impl From<RestError> for ThreadError {
    fn from(error: RestError) -> Self {
        Self::Rest(error)
    }
}

/// One page of archived public threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedThreads {
    /// Newest archived first, all threads of the asked channel.
    pub threads: Vec<Channel>,
    pub has_more: bool,
}

impl ArchivedThreads {
    /// The `before` cursor of the next page.
    pub fn cursor(&self) -> Option<&str> {
        self.threads
            .last()?
            .thread_metadata
            .as_ref()?
            .archive_timestamp
            .as_deref()
    }
}

/// Trims a typed thread name; 1 to 100 characters.
pub fn thread_name(input: &str) -> Result<&str, ThreadError> {
    let name = input.trim();
    if name.is_empty() || name.chars().count() > MAX_THREAD_NAME_CHARS {
        return Err(ThreadError::InvalidName);
    }
    Ok(name)
}

/// Why a forum post is not sent, decided before any request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostInvalid {
    /// The title is empty after trimming or longer than 100 characters.
    Title,
    /// No text and no file: a post needs a first message.
    Empty,
    TooLong,
    /// More than five tags.
    TooManyTags,
    /// The forum requires a tag and none is chosen.
    TagRequired,
    /// A media channel requires a file in the first message.
    FileRequired,
}

impl PostInvalid {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Title => "Give the post a title of 1 to 100 characters.",
            Self::Empty => "Write the first message of the post or attach a file.",
            Self::TooLong => "The first message is too long.",
            Self::TooManyTags => "A post carries at most 5 tags.",
            Self::TagRequired => "This forum requires at least one tag.",
            Self::FileRequired => "This channel requires a file in every post.",
        }
    }
}

/// A forum post failed. `Send(NotSent)`: Discord definitely created nothing, so
/// fixing the cause and posting again is safe. `Send(Ambiguous)`: a post may
/// exist; the caller must not repeat the request without the user's say-so
/// (the request has no nonce Discord could deduplicate on).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostError {
    Invalid(PostInvalid),
    Send(SendError),
}

/// What the forum demands of a post.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PostRules {
    pub require_tag: bool,
    pub require_file: bool,
}

/// A validated new forum post. `Debug` never prints the text.
#[derive(Clone, PartialEq, Eq)]
pub struct ForumPost {
    name: String,
    content: String,
    tags: Vec<Snowflake>,
    files: Vec<UploadedFile>,
}

impl std::fmt::Debug for ForumPost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForumPost")
            .field("tags", &self.tags.len())
            .field("files", &self.files.len())
            .finish_non_exhaustive()
    }
}

impl ForumPost {
    /// Checks everything Discord would refuse for a reason the client knows,
    /// before any file is uploaded. `files` is the number of files to attach.
    pub fn check(
        name: &str,
        content: &str,
        tags: &[Snowflake],
        files: usize,
        rules: PostRules,
    ) -> Result<(), PostInvalid> {
        thread_name(name).map_err(|_| PostInvalid::Title)?;
        let content = content.trim();
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(PostInvalid::TooLong);
        }
        if content.is_empty() && files == 0 {
            return Err(PostInvalid::Empty);
        }
        if rules.require_file && files == 0 {
            return Err(PostInvalid::FileRequired);
        }
        let mut unique: Vec<Snowflake> = Vec::new();
        for tag in tags {
            if !unique.contains(tag) {
                unique.push(*tag);
            }
        }
        if unique.len() > MAX_APPLIED_TAGS {
            return Err(PostInvalid::TooManyTags);
        }
        if rules.require_tag && unique.is_empty() {
            return Err(PostInvalid::TagRequired);
        }
        Ok(())
    }

    /// A checked post carrying the uploaded `files`. Tags are deduplicated,
    /// keeping the order chosen.
    pub fn new(
        name: &str,
        content: &str,
        tags: &[Snowflake],
        files: Vec<UploadedFile>,
        rules: PostRules,
    ) -> Result<Self, PostInvalid> {
        Self::check(name, content, tags, files.len(), rules)?;
        let mut unique: Vec<Snowflake> = Vec::new();
        for tag in tags {
            if !unique.contains(tag) {
                unique.push(*tag);
            }
        }
        Ok(Self {
            name: name.trim().to_owned(),
            content: content.trim().to_owned(),
            tags: unique,
            files,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn body(&self) -> PostBody<'_> {
        PostBody {
            name: &self.name,
            applied_tags: &self.tags,
            message: PostMessage {
                content: &self.content,
                allowed_mentions: AllowedMentions {
                    parse: MentionPolicy::default().parse(),
                },
                attachments: wire_attachments(&self.files),
            },
        }
    }
}

#[derive(Serialize)]
struct PostBody<'a> {
    name: &'a str,
    applied_tags: &'a [Snowflake],
    message: PostMessage<'a>,
}

#[derive(Serialize)]
struct PostMessage<'a> {
    content: &'a str,
    allowed_mentions: AllowedMentions,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<WireAttachment<'a>>,
}

#[derive(Serialize)]
struct AllowedMentions {
    parse: Vec<&'static str>,
}

fn post_route(forum: Snowflake) -> Result<Route, RestError> {
    Route::new(Method::POST, &format!("/channels/{forum}/threads"))
}

/// The created post must be a thread of the asked forum; anything else on a
/// success status means a post probably exists that cannot be identified.
fn parse_post(body: &[u8], forum: Snowflake) -> Result<Channel, SendError> {
    serde_json::from_slice::<Channel>(body)
        .ok()
        .filter(|post| post.kind.is_thread() && post.parent_id == Some(forum))
        .ok_or(SendError::Ambiguous(RestError::InvalidJson))
}

fn membership_route(method: Method, thread: Snowflake) -> Result<Route, RestError> {
    Route::new(method, &format!("/channels/{thread}/thread-members/@me"))
}

fn archived_route(channel: Snowflake, before: Option<&str>) -> Result<Route, RestError> {
    let mut url = Url::parse("https://discord.invalid/").map_err(|_| RestError::InvalidRoute)?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("limit", &ARCHIVED_PAGE_SIZE.to_string());
        if let Some(before) = before {
            pairs.append_pair("before", before);
        }
    }
    let query = url.query().unwrap_or_default();
    Route::new(
        Method::GET,
        &format!("/channels/{channel}/threads/archived/public?{query}"),
    )
}

#[derive(Deserialize)]
struct ArchivedWire {
    #[serde(default)]
    threads: Vec<Channel>,
    #[serde(default)]
    has_more: bool,
}

/// Lenient about members and unknown fields; strict that every thread is a
/// thread of `channel` and that the page is not larger than asked.
pub(crate) fn parse_archived(
    body: &[u8],
    guild: Snowflake,
    channel: Snowflake,
) -> Result<ArchivedThreads, RestError> {
    let mut wire: ArchivedWire =
        serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    if wire.threads.len() > ARCHIVED_PAGE_SIZE
        || wire
            .threads
            .iter()
            .any(|t| !t.kind.is_thread() || t.parent_id != Some(channel))
    {
        return Err(RestError::InvalidJson);
    }
    for thread in &mut wire.threads {
        thread.guild_id.get_or_insert(guild);
    }
    Ok(ArchivedThreads {
        threads: wire.threads,
        has_more: wire.has_more,
    })
}

impl<C: Clock> RestClient<C> {
    /// Joins a thread the account can see.
    pub async fn join_thread(&self, thread: Snowflake) -> Result<(), RestError> {
        let request = RestRequest::new(membership_route(Method::PUT, thread)?, Priority::UserWrite);
        self.execute(request).await.map(drop)
    }

    /// Leaves a thread.
    pub async fn leave_thread(&self, thread: Snowflake) -> Result<(), RestError> {
        let request = RestRequest::new(
            membership_route(Method::DELETE, thread)?,
            Priority::UserWrite,
        );
        self.execute(request).await.map(drop)
    }

    /// Archives or locks a thread (`PATCH /channels/{thread}` `{archived,
    /// locked}`); a `None` field is left alone. It needs Manage Threads and the
    /// header follows `THREAD_UPDATE`.
    pub async fn set_thread_state(
        &self,
        thread: Snowflake,
        archived: Option<bool>,
        locked: Option<bool>,
    ) -> Result<(), RestError> {
        let mut body = serde_json::Map::new();
        if let Some(archived) = archived {
            body.insert("archived".into(), archived.into());
        }
        if let Some(locked) = locked {
            body.insert("locked".into(), locked.into());
        }
        let request = RestRequest::new(
            Route::new(Method::PATCH, &format!("/channels/{thread}"))?,
            Priority::UserWrite,
        )
        .in_group(WriteGroup::Threads)
        .with_error_detail()
        .json(&body)?;
        self.execute(request).await.map(drop)
    }

    /// Starts a public thread on a message (the thread takes the message's ID).
    /// Needs Create Public Threads in the channel.
    pub async fn create_thread(
        &self,
        guild: Snowflake,
        channel: Snowflake,
        message: Snowflake,
        name: &str,
    ) -> Result<Channel, ThreadError> {
        let name = thread_name(name)?;
        let request = RestRequest::new(
            Route::new(
                Method::POST,
                &format!("/channels/{channel}/messages/{message}/threads"),
            )?,
            Priority::UserWrite,
        )
        .json(&serde_json::json!({
            "name": name,
            "auto_archive_duration": AUTO_ARCHIVE_MINUTES,
        }))?;
        let mut thread: Channel = self.execute(request).await?.json()?;
        if !thread.kind.is_thread() || thread.parent_id != Some(channel) {
            return Err(ThreadError::InvalidResponse);
        }
        thread.guild_id.get_or_insert(guild);
        Ok(thread)
    }

    /// Starts a post in a forum or media channel. Sent exactly once; see
    /// [`PostError`] for what a failure allows.
    pub async fn create_forum_post(
        &self,
        guild: Snowflake,
        forum: Snowflake,
        post: &ForumPost,
    ) -> Result<Channel, PostError> {
        let request = post_route(forum)
            .and_then(|route| {
                RestRequest::new(route, Priority::UserWrite)
                    .with_error_detail()
                    .json(&post.body())
            })
            .map_err(|e| PostError::Send(SendError::NotSent(e)))?;
        let response = self
            .execute(request)
            .await
            .map_err(|e| PostError::Send(SendError::from_transport(e)))?;
        let mut thread = parse_post(response.body(), forum).map_err(PostError::Send)?;
        thread.guild_id.get_or_insert(guild);
        Ok(thread)
    }

    /// One page of the channel's archived public threads, newest archived
    /// first. `before` is the `archive_timestamp` of the last thread held.
    pub async fn archived_threads(
        &self,
        guild: Snowflake,
        channel: Snowflake,
        before: Option<&str>,
    ) -> Result<ArchivedThreads, RestError> {
        let request = RestRequest::new(archived_route(channel, before)?, Priority::UserRead);
        let response = self.execute(request).await?;
        parse_archived(response.body(), guild, channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_model::ChannelKind;

    const ARCHIVED: &str = include_str!("../../../fixtures/rest/threads-archived-synthetic.json");
    const GUILD: Snowflake = Snowflake(1000);
    const PARENT: Snowflake = Snowflake(500);

    fn path(route: &Route) -> String {
        route
            .url
            .as_str()
            .strip_prefix("https://discord.com/api/v10")
            .unwrap()
            .to_owned()
    }

    #[test]
    fn routes_are_exactly_the_documented_ones() {
        let put = membership_route(Method::PUT, Snowflake(7)).unwrap();
        assert_eq!(put.key.method(), &Method::PUT);
        assert_eq!(path(&put), "/channels/7/thread-members/@me");
        assert_eq!(
            membership_route(Method::DELETE, Snowflake(7))
                .unwrap()
                .key
                .method(),
            &Method::DELETE
        );
        assert_eq!(
            path(&archived_route(PARENT, None).unwrap()),
            "/channels/500/threads/archived/public?limit=25"
        );
        assert_eq!(
            path(&archived_route(PARENT, Some("2026-10-07T12:00:00.000000+00:00")).unwrap()),
            "/channels/500/threads/archived/public?limit=25&before=2026-10-07T12%3A00%3A00.000000%2B00%3A00"
        );
    }

    fn rules(require_tag: bool) -> PostRules {
        PostRules {
            require_tag,
            require_file: false,
        }
    }

    #[test]
    fn post_request_has_the_documented_shape() {
        assert_eq!(path(&post_route(PARENT).unwrap()), "/channels/500/threads");
        let post = ForumPost::new(
            "  Bug: crash  ",
            " it crashes ",
            &[Snowflake(7), Snowflake(8), Snowflake(7)],
            Vec::new(),
            rules(true),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(post.body()).unwrap(),
            serde_json::json!({
                "name": "Bug: crash",
                "applied_tags": ["7", "8"],
                "message": {"content": "it crashes", "allowed_mentions": {"parse": ["users", "roles"]}},
            })
        );
        assert!(!format!("{post:?}").contains("crashes"));
    }

    #[test]
    fn post_is_validated_before_any_request() {
        let new = |name: &str, body: &str, tags: &[u64], rules| {
            let tags: Vec<_> = tags.iter().map(|t| Snowflake(*t)).collect();
            ForumPost::new(name, body, &tags, Vec::new(), rules).map(drop)
        };
        assert_eq!(new(" ", "x", &[], rules(false)), Err(PostInvalid::Title));
        assert_eq!(new("t", "  ", &[], rules(false)), Err(PostInvalid::Empty));
        assert_eq!(
            new("t", &"x".repeat(MAX_CONTENT_CHARS + 1), &[], rules(false)),
            Err(PostInvalid::TooLong)
        );
        assert_eq!(
            new("t", "x", &[], rules(true)),
            Err(PostInvalid::TagRequired)
        );
        assert_eq!(new("t", "x", &[1], rules(true)), Ok(()));
        assert_eq!(
            new("t", "x", &[1, 2, 3, 4, 5, 6], rules(false)),
            Err(PostInvalid::TooManyTags)
        );
        assert_eq!(
            new("t", "x", &[1, 1, 1, 1, 1, 1], rules(false)),
            Ok(()),
            "duplicates are one tag"
        );
        let media = PostRules {
            require_tag: false,
            require_file: true,
        };
        assert_eq!(new("t", "x", &[], media), Err(PostInvalid::FileRequired));
    }

    #[test]
    fn a_created_post_must_be_a_thread_of_the_forum() {
        let ok = br#"{"id":"9","type":11,"parent_id":"500","name":"t","applied_tags":["7"]}"#;
        assert_eq!(parse_post(ok, PARENT).unwrap().applied_tags, [Snowflake(7)]);
        for bad in [
            &br#"{"id":"9","type":11,"parent_id":"501"}"#[..],
            br#"{"id":"9","type":0,"parent_id":"500"}"#,
            b"[]",
        ] {
            assert_eq!(
                parse_post(bad, PARENT),
                Err(SendError::Ambiguous(RestError::InvalidJson))
            );
        }
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(thread_name("  plans  "), Ok("plans"));
        assert_eq!(thread_name("   "), Err(ThreadError::InvalidName));
        assert_eq!(
            thread_name(&"x".repeat(MAX_THREAD_NAME_CHARS + 1)),
            Err(ThreadError::InvalidName)
        );
        assert!(thread_name(&"é".repeat(MAX_THREAD_NAME_CHARS)).is_ok());
    }

    #[test]
    fn archived_page_keeps_state_cursor_and_learns_the_guild() {
        let page = parse_archived(ARCHIVED.as_bytes(), GUILD, PARENT).unwrap();
        assert!(page.has_more);
        assert_eq!(page.threads.len(), 2);
        assert!(page.threads.iter().all(|t| t.guild_id == Some(GUILD)));
        assert_eq!(page.threads[0].kind, ChannelKind::PublicThread);
        assert!(page.threads[1].thread_metadata.as_ref().unwrap().locked);
        assert_eq!(
            page.cursor(),
            Some("2026-10-05T09:00:00.000000+00:00"),
            "the next page starts before the oldest archived thread held"
        );
    }

    #[test]
    fn foreign_oversized_and_malformed_pages_are_refused() {
        assert_eq!(
            parse_archived(ARCHIVED.as_bytes(), GUILD, Snowflake(501)),
            Err(RestError::InvalidJson)
        );
        let text_channel = r#"{"threads":[{"id":"1","type":0,"parent_id":"500"}]}"#;
        assert_eq!(
            parse_archived(text_channel.as_bytes(), GUILD, PARENT),
            Err(RestError::InvalidJson)
        );
        let many = format!(
            r#"{{"threads":[{}]}}"#,
            vec![r#"{"id":"1","type":11,"parent_id":"500"}"#; ARCHIVED_PAGE_SIZE + 1].join(",")
        );
        assert_eq!(
            parse_archived(many.as_bytes(), GUILD, PARENT),
            Err(RestError::InvalidJson)
        );
        assert_eq!(
            parse_archived(b"7", GUILD, PARENT),
            Err(RestError::InvalidJson)
        );
        assert!(
            parse_archived(b"{}", GUILD, PARENT)
                .unwrap()
                .threads
                .is_empty()
        );
    }

    #[tokio::test]
    async fn create_refuses_a_bad_name_before_any_request() {
        use crate::UserToken;
        use crate::gateway::{ClientProperties, HostOs};
        let properties = ClientProperties::web(HostOs::Linux, "en-US", 1);
        let client = RestClient::new(UserToken::new("dummy".into()), properties).unwrap();
        assert_eq!(
            client
                .create_thread(GUILD, PARENT, Snowflake(9), "  ")
                .await,
            Err(ThreadError::InvalidName)
        );
    }
}
