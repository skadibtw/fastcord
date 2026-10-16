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
//! Active threads arrive on the Gateway (GUILD_CREATE `threads`,
//! THREAD_LIST_SYNC, THREAD_CREATE/UPDATE/DELETE); nothing here lists them. The
//! wire shapes are from community documentation and observed web-client
//! traffic and are unverified against a live account (`docs/TESTING.md`).

use fastcord_model::{Channel, Snowflake};
use reqwest::Url;
use serde::Deserialize;

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route};

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
