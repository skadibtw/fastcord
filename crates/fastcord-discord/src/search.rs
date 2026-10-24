//! Server-backed message search as user clients send it
//! (`GET /guilds/{id}/messages/search`, `GET /channels/{id}/messages/search`).
//!
//! One request returns one page of at most [`SEARCH_PAGE_SIZE`] hits chosen by
//! Discord; nothing here crawls history. Each hit arrives as a group of the
//! matching message plus a little context; only the message flagged `hit` is
//! kept. While Discord has not built the index yet it answers `202` with a
//! `retry_after`; that is returned to the caller ([`SearchOutcome::IndexNotReady`])
//! so it can wait instead of re-asking at once.

use std::time::Duration;

use fastcord_model::{Message, Snowflake};
use reqwest::Url;
use serde::Deserialize;

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route, StatusCode};

/// Discord's fixed search page size; a page with more groups is refused.
pub const SEARCH_PAGE_SIZE: usize = 25;
/// Discord rejects offsets beyond this.
pub const MAX_SEARCH_OFFSET: u32 = 9975;
/// Longest query text sent (Discord's own limit).
pub const MAX_SEARCH_CHARS: usize = 1024;
/// Context messages per hit are not kept, and a group this long is malformed.
const MAX_GROUP: usize = 8;
const MIN_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);

/// Where to search. `Channel` covers DMs and group DMs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchScope {
    Guild(Snowflake),
    Channel(Snowflake),
}

/// The `has` filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchHas {
    Image,
    Video,
    Sound,
    File,
    Link,
    Embed,
    Sticker,
}

impl SearchHas {
    fn wire(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Sound => "sound",
            Self::File => "file",
            Self::Link => "link",
            Self::Embed => "embed",
            Self::Sticker => "sticker",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery {
    pub scope: SearchScope,
    pub content: String,
    pub author: Option<Snowflake>,
    /// Restricts a guild search to one channel (ignored for `Channel` scope).
    pub channel: Option<Snowflake>,
    pub has: Option<SearchHas>,
    /// Only messages newer than this ID (date filters are snowflakes).
    pub after: Option<Snowflake>,
    /// Only messages older than this ID.
    pub before: Option<Snowflake>,
    pub offset: u32,
}

impl SearchQuery {
    pub fn new(scope: SearchScope, content: impl Into<String>) -> Self {
        Self {
            scope,
            content: content.into(),
            author: None,
            channel: None,
            has: None,
            after: None,
            before: None,
            offset: 0,
        }
    }

    /// Whether the query names something to look for and is within range.
    pub fn is_sendable(&self) -> bool {
        self.route().is_ok()
    }

    pub(crate) fn route(&self) -> Result<Route, RestError> {
        let content: String = self.content.trim().chars().take(MAX_SEARCH_CHARS).collect();
        if self.offset > MAX_SEARCH_OFFSET
            || (content.is_empty() && self.author.is_none() && self.has.is_none())
        {
            return Err(RestError::InvalidRoute);
        }
        let mut url =
            Url::parse("https://discord.invalid/").map_err(|_| RestError::InvalidRoute)?;
        {
            let mut pairs = url.query_pairs_mut();
            if !content.is_empty() {
                pairs.append_pair("content", &content);
            }
            if let Some(author) = self.author {
                pairs.append_pair("author_id", &author.to_string());
            }
            if let (SearchScope::Guild(_), Some(channel)) = (self.scope, self.channel) {
                pairs.append_pair("channel_id", &channel.to_string());
            }
            if let Some(has) = self.has {
                pairs.append_pair("has", has.wire());
            }
            if let Some(id) = self.after {
                pairs.append_pair("min_id", &id.to_string());
            }
            if let Some(id) = self.before {
                pairs.append_pair("max_id", &id.to_string());
            }
            if self.offset > 0 {
                pairs.append_pair("offset", &self.offset.to_string());
            }
        }
        let resource = match self.scope {
            SearchScope::Guild(id) => format!("/guilds/{id}/messages/search"),
            SearchScope::Channel(id) => format!("/channels/{id}/messages/search"),
        };
        let query = url.query().unwrap_or_default();
        Route::new(Method::GET, &format!("{resource}?{query}"))
    }
}

/// One page of hits, in Discord's order (newest first by default).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchPage {
    /// Discord's count of all matches, not of this page.
    pub total: u32,
    pub hits: Vec<Message>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchOutcome {
    Page(SearchPage),
    /// `202`: the index is being built. Ask again after the delay (1-30 s).
    IndexNotReady {
        retry_after: Duration,
    },
}

impl<C: Clock> RestClient<C> {
    /// Runs one search request through the account scheduler (rate limits,
    /// byte budget, categorical errors). Never retries by itself.
    pub async fn search_messages(&self, query: &SearchQuery) -> Result<SearchOutcome, RestError> {
        let request = RestRequest::new(query.route()?, Priority::UserRead);
        let response = self.execute(request).await?;
        parse_outcome(response.status(), response.body(), query)
    }
}

#[derive(Deserialize)]
struct Wire {
    #[serde(default)]
    total_results: u64,
    #[serde(default)]
    messages: Vec<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct NotReady {
    #[serde(default)]
    retry_after: Option<f64>,
}

pub(crate) fn parse_outcome(
    status: StatusCode,
    body: &[u8],
    query: &SearchQuery,
) -> Result<SearchOutcome, RestError> {
    if status == StatusCode::ACCEPTED {
        // A missing or absurd delay still waits: never an immediate re-ask.
        let seconds = serde_json::from_slice::<NotReady>(body)
            .ok()
            .and_then(|wire| wire.retry_after)
            .filter(|seconds| seconds.is_finite())
            .unwrap_or(0.0);
        let retry_after = Duration::from_secs_f64(seconds.max(0.0).min(MAX_RETRY.as_secs_f64()))
            .clamp(MIN_RETRY, MAX_RETRY);
        return Ok(SearchOutcome::IndexNotReady { retry_after });
    }
    let wire: Wire = serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    if wire.messages.len() > SEARCH_PAGE_SIZE {
        return Err(RestError::InvalidJson);
    }
    let mut hits = Vec::with_capacity(wire.messages.len());
    for group in wire.messages {
        if group.len() > MAX_GROUP {
            return Err(RestError::InvalidJson);
        }
        let Some(hit) = group
            .into_iter()
            .find(|value| value.get("hit").and_then(|hit| hit.as_bool()) == Some(true))
        else {
            continue;
        };
        let message: Message = serde_json::from_value(hit).map_err(|_| RestError::InvalidJson)?;
        let belongs = match query.scope {
            SearchScope::Channel(channel) => message.channel_id == channel,
            SearchScope::Guild(_) => query.channel.is_none_or(|c| message.channel_id == c),
        };
        if !belongs {
            return Err(RestError::InvalidJson);
        }
        hits.push(message);
    }
    Ok(SearchOutcome::Page(SearchPage {
        total: u32::try_from(wire.total_results).unwrap_or(u32::MAX),
        hits,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserToken;
    use crate::gateway::{ClientProperties, HostOs};

    const PAGE: &str = include_str!("../../../fixtures/rest/message-search.json");
    const NOT_READY: &str = include_str!("../../../fixtures/rest/message-search-not-ready.json");

    fn id(n: u64) -> Snowflake {
        Snowflake(n)
    }

    fn guild_query() -> SearchQuery {
        SearchQuery::new(SearchScope::Guild(id(7)), "needle")
    }

    fn path(query: &SearchQuery) -> String {
        let route = query.route().unwrap();
        let url = route.url.as_str().to_owned();
        url.strip_prefix("https://discord.com/api/v10")
            .unwrap()
            .to_owned()
    }

    #[test]
    fn routes_carry_encoded_filters_and_pagination() {
        assert_eq!(
            path(&guild_query()),
            "/guilds/7/messages/search?content=needle"
        );
        let mut full = SearchQuery::new(SearchScope::Guild(id(7)), " a b&c#d ");
        full.author = Some(id(2));
        full.channel = Some(id(500));
        full.has = Some(SearchHas::Image);
        full.after = Some(id(10));
        full.before = Some(id(20));
        full.offset = 25;
        assert_eq!(
            path(&full),
            "/guilds/7/messages/search?content=a+b%26c%23d&author_id=2&channel_id=500&has=image&min_id=10&max_id=20&offset=25"
        );
        let mut dm = SearchQuery::new(SearchScope::Channel(id(9)), "x");
        dm.channel = Some(id(1));
        assert_eq!(path(&dm), "/channels/9/messages/search?content=x");
    }

    #[test]
    fn empty_or_out_of_range_queries_are_never_sent() {
        let blank = SearchQuery::new(SearchScope::Guild(id(7)), "   ");
        assert!(blank.route().is_err());
        let mut far = guild_query();
        far.offset = MAX_SEARCH_OFFSET + 1;
        assert!(far.route().is_err());
        let mut by_author = blank;
        by_author.author = Some(id(2));
        assert!(by_author.route().is_ok());
    }

    #[test]
    fn a_page_keeps_only_the_hit_of_each_group() {
        let outcome = parse_outcome(StatusCode::OK, PAGE.as_bytes(), &guild_query()).unwrap();
        let SearchOutcome::Page(page) = outcome else {
            panic!("expected a page");
        };
        assert_eq!(page.total, 27);
        let ids: Vec<u64> = page.hits.iter().map(|m| m.id.0).collect();
        assert_eq!(ids, [1290000000000000011, 1290000000000000005]);
        assert_eq!(page.hits[0].attachments.len(), 1);
    }

    #[test]
    fn a_hit_outside_the_requested_channel_is_refused() {
        let mut query = guild_query();
        query.channel = Some(id(500));
        assert_eq!(
            parse_outcome(StatusCode::OK, PAGE.as_bytes(), &query),
            Err(RestError::InvalidJson)
        );
        let dm = SearchQuery::new(SearchScope::Channel(id(500)), "needle");
        assert_eq!(
            parse_outcome(StatusCode::OK, PAGE.as_bytes(), &dm),
            Err(RestError::InvalidJson)
        );
    }

    #[test]
    fn not_ready_waits_at_least_a_second_and_at_most_thirty() {
        let wait = |body: &[u8]| match parse_outcome(StatusCode::ACCEPTED, body, &guild_query())
            .unwrap()
        {
            SearchOutcome::IndexNotReady { retry_after } => retry_after,
            SearchOutcome::Page(_) => panic!("202 is not a page"),
        };
        assert_eq!(wait(NOT_READY.as_bytes()), Duration::from_secs(2));
        assert_eq!(wait(br#"{"retry_after":0}"#), MIN_RETRY);
        assert_eq!(wait(br#"{"retry_after":99999}"#), MAX_RETRY);
        assert_eq!(wait(b"not json"), MIN_RETRY);
        assert_eq!(wait(br#"{"retry_after":-4}"#), MIN_RETRY);
    }

    #[test]
    fn malformed_and_oversized_pages_are_rejected() {
        let many = format!(
            r#"{{"total_results":1,"messages":[{}]}}"#,
            vec!["[]"; 26].join(",")
        );
        let long_group = format!(
            r#"{{"total_results":1,"messages":[[{}]]}}"#,
            ["{}"; 9].join(",")
        );
        for bad in [
            many.as_bytes(),
            long_group.as_bytes(),
            br#"{"messages":[[{"hit":true,"id":"1"}]]}"#,
            b"\"x\"",
            b"",
        ] {
            assert_eq!(
                parse_outcome(StatusCode::OK, bad, &guild_query()),
                Err(RestError::InvalidJson),
                "{bad:?}"
            );
        }
        let none = parse_outcome(
            StatusCode::OK,
            br#"{"total_results":0,"messages":[]}"#,
            &guild_query(),
        );
        assert_eq!(
            none,
            Ok(SearchOutcome::Page(SearchPage {
                total: 0,
                hits: Vec::new()
            }))
        );
    }

    #[tokio::test]
    async fn stopped_client_uses_the_account_stop_without_touching_the_network() {
        let properties = ClientProperties::web(HostOs::Linux, "en-US", 1);
        let client = RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-search-credential".to_owned()),
            properties,
        )
        .unwrap();
        client.stop_authenticated_work();
        assert_eq!(
            client.search_messages(&guild_query()).await,
            Err(RestError::AuthenticationRequired)
        );
    }
}
