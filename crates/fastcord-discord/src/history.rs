//! Typed channel-history reads through the account's existing REST scheduler.
//!
//! One request returns one page of at most [`MESSAGE_PAGE_LIMIT`] messages,
//! newest first, exactly as Discord sends it. Merging pages into bounded
//! memory is the [`message_store`](crate::message_store)'s job; this module
//! only builds the route, decodes the page with a hard record limit, and checks
//! that the response belongs to the requested channel and cursor.
//!
//! A single message (a reply's target) is read the way user clients read one:
//! the one-message page around its ID ([`RestClient::channel_message`]).
//! Discord's `GET /channels/{channel}/messages/{message}` is not usable by user
//! accounts.

use std::fmt;

use fastcord_model::{Message, Snowflake};
use serde::de::{Error as _, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route};

/// Discord's around pagination may return one extra item for an even limit.
/// Keep the requested count odd so the strict 50-record decoder remains safe.
const AROUND_PAGE_LIMIT: usize = 49;

/// Discord's ordinary page size and the hard upper bound for decoded records.
pub const MESSAGE_PAGE_LIMIT: usize = 50;

/// Which page of a channel's history to read. The cursors are mutually
/// exclusive, as in the API.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistoryCursor {
    /// The newest messages.
    #[default]
    Latest,
    /// The messages immediately older than this one.
    Before(Snowflake),
    /// The messages immediately newer than this one.
    After(Snowflake),
    /// The messages surrounding this one, itself included if it exists: a jump
    /// to a message that is not retained (a reply's target).
    Around(Snowflake),
}

fn history_route(channel: Snowflake, cursor: HistoryCursor) -> Result<Route, RestError> {
    let limit = if matches!(cursor, HistoryCursor::Around(_)) {
        AROUND_PAGE_LIMIT
    } else {
        MESSAGE_PAGE_LIMIT
    };
    page_route(channel, cursor, limit)
}

fn page_route(channel: Snowflake, cursor: HistoryCursor, limit: usize) -> Result<Route, RestError> {
    let path = match cursor {
        HistoryCursor::Latest => format!("/channels/{channel}/messages?limit={limit}"),
        HistoryCursor::Before(id) => {
            format!("/channels/{channel}/messages?limit={limit}&before={id}")
        }
        HistoryCursor::After(id) => {
            format!("/channels/{channel}/messages?limit={limit}&after={id}")
        }
        HistoryCursor::Around(id) => {
            format!("/channels/{channel}/messages?limit={limit}&around={id}")
        }
    };
    Route::new(Method::GET, &path)
}

impl<C: Clock> RestClient<C> {
    /// Fetches one history page. Messages stay in wire order (newest first).
    ///
    /// The request goes through the account scheduler like every other route:
    /// rate limits, the response byte budget, cancellation on authentication
    /// failure, and categorical errors (`PermissionDenied`, `ResourceGone`, ...)
    /// are the transport's. Network and server errors are not retried here.
    /// A page that holds more than [`MESSAGE_PAGE_LIMIT`] records, a message of
    /// another channel, or a message on the wrong side of the cursor is
    /// `InvalidJson`: the response is not applied rather than partly trusted.
    pub async fn channel_messages(
        &self,
        channel: Snowflake,
        cursor: HistoryCursor,
    ) -> Result<Vec<Message>, RestError> {
        let request = RestRequest::new(history_route(channel, cursor)?, Priority::UserRead);
        let response = self.execute(request).await?;
        parse_page(response.body(), channel, cursor)
    }

    /// Reads one message of `channel` for a reply preview: the one-message page
    /// around it, as user clients do. `Ok(None)` means Discord has no message
    /// with that ID there (it was deleted); a neighbouring message the page
    /// may hold instead is never taken for it. This is a speculative read: it
    /// yields to anything the user asked for.
    pub async fn channel_message(
        &self,
        channel: Snowflake,
        message: Snowflake,
    ) -> Result<Option<Message>, RestError> {
        let route = page_route(channel, HistoryCursor::Around(message), 1)?;
        let response = self
            .execute(RestRequest::new(route, Priority::SpeculativeRead))
            .await?;
        parse_single(response.body(), channel, message)
    }
}

pub(crate) fn parse_single(
    body: &[u8],
    channel: Snowflake,
    message: Snowflake,
) -> Result<Option<Message>, RestError> {
    let page = parse_page(body, channel, HistoryCursor::Around(message))?;
    if page.len() > 1 {
        return Err(RestError::InvalidJson);
    }
    Ok(page.into_iter().find(|found| found.id == message))
}

pub(crate) fn parse_page(
    body: &[u8],
    channel: Snowflake,
    cursor: HistoryCursor,
) -> Result<Vec<Message>, RestError> {
    let page: HistoryPage = serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    let consistent = page.0.iter().all(|message| {
        message.channel_id == channel
            && match cursor {
                HistoryCursor::Latest | HistoryCursor::Around(_) => true,
                HistoryCursor::Before(id) => message.id < id,
                HistoryCursor::After(id) => message.id > id,
            }
    });
    if consistent {
        Ok(page.0)
    } else {
        Err(RestError::InvalidJson)
    }
}

/// Limits decoded records as well as transport bytes: it never first collects
/// an arbitrarily long array and only then notices the page is too large.
struct HistoryPage(Vec<Message>);

impl<'de> Deserialize<'de> for HistoryPage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PageVisitor;

        impl<'de> Visitor<'de> for PageVisitor {
            type Value = HistoryPage;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "at most {MESSAGE_PAGE_LIMIT} channel messages")
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut messages = Vec::new();
                while messages.len() < MESSAGE_PAGE_LIMIT {
                    match sequence.next_element::<Message>()? {
                        Some(message) => messages.push(message),
                        None => return Ok(HistoryPage(messages)),
                    }
                }
                if sequence.next_element::<IgnoredAny>()?.is_some() {
                    return Err(A::Error::custom("message page exceeds its fixed limit"));
                }
                Ok(HistoryPage(messages))
            }
        }

        deserializer.deserialize_seq(PageVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserToken;

    const PAGE: &str = include_str!("../../../fixtures/rest/channel-messages.json");

    fn id(n: u64) -> Snowflake {
        Snowflake(n)
    }

    #[test]
    fn history_routes_use_bounded_limits_and_exclusive_cursors() {
        let latest = history_route(id(500), HistoryCursor::Latest).unwrap();
        let before = history_route(id(500), HistoryCursor::Before(id(12))).unwrap();
        let after = history_route(id(500), HistoryCursor::After(id(12))).unwrap();
        let around = history_route(id(500), HistoryCursor::Around(id(12))).unwrap();
        let single = page_route(id(500), HistoryCursor::Around(id(12)), 1).unwrap();
        let expected = [
            "/channels/500/messages?limit=50",
            "/channels/500/messages?limit=50&before=12",
            "/channels/500/messages?limit=50&after=12",
            "/channels/500/messages?limit=49&around=12",
            "/channels/500/messages?limit=1&around=12",
        ];
        for (route, path) in [&latest, &before, &after, &around, &single]
            .into_iter()
            .zip(expected)
        {
            assert_eq!(
                route.url.as_str(),
                format!("https://discord.com/api/v10{path}")
            );
            assert_eq!(route.key().method(), &Method::GET);
            // The request's Debug output names the normalized route, never the
            // concrete URL or cursor values.
            let shown = format!("{:?}", RestRequest::new(route.clone(), Priority::UserRead));
            assert!(shown.contains("/channels/{channel_id}/messages"));
            assert!(!shown.contains(path));
        }
        // Cursors are request parameters, not rate-limit identity.
        assert_eq!(latest.key(), before.key());
        assert_eq!(latest.key(), after.key());
        assert_eq!(latest.key(), around.key());
        assert_eq!(latest.key(), single.key());
        assert_eq!(latest.key().major().channel_id(), Some(id(500)));
        assert_ne!(
            latest.key(),
            history_route(id(501), HistoryCursor::Latest).unwrap().key()
        );
        assert_eq!(HistoryCursor::default(), HistoryCursor::Latest);
    }

    #[test]
    fn a_single_message_is_only_ever_the_exact_id_asked_for() {
        let page: Vec<Message> = serde_json::from_str(PAGE).unwrap();
        let body = |messages: &[Message]| serde_json::to_vec(messages).unwrap();
        let reply = &page[2];
        // The one-message page around the ID holds it.
        let found = parse_single(&body(std::slice::from_ref(reply)), id(500), reply.id).unwrap();
        assert_eq!(found.as_ref().map(|message| message.id), Some(reply.id));
        assert_eq!(found.unwrap().referenced_message, reply.referenced_message);
        // A deleted message: Discord answers with nothing, or with a neighbour,
        // which is never taken for it.
        assert_eq!(parse_single(b"[]", id(500), reply.id), Ok(None));
        assert_eq!(
            parse_single(&body(&page[..1]), id(500), reply.id),
            Ok(None),
            "a neighbour is not the message"
        );
        // Anything but one message of this channel is not trusted at all.
        assert_eq!(
            parse_single(&body(&page[1..]), id(500), reply.id),
            Err(RestError::InvalidJson),
            "more than the one record asked for"
        );
        assert_eq!(
            parse_single(&body(std::slice::from_ref(reply)), id(501), reply.id),
            Err(RestError::InvalidJson),
            "another channel"
        );
        assert_eq!(
            parse_single(b"{}", id(500), reply.id),
            Err(RestError::InvalidJson)
        );
    }

    #[test]
    fn sanitized_rest_history_page_decodes_real_shapes_and_ignores_unknown_fields() {
        let messages = parse_page(PAGE.as_bytes(), id(500), HistoryCursor::Latest).unwrap();
        let ids: Vec<u64> = messages.iter().map(|message| message.id.0).collect();
        assert_eq!(
            ids,
            [
                1_290_000_000_000_000_012,
                1_290_000_000_000_000_011,
                1_290_000_000_000_000_010
            ],
            "wire order (newest first) is preserved"
        );
        assert_eq!(messages[0].author.display_name(), "Fixture Author");
        assert_eq!(
            messages[0].content,
            "latest message with unicode \u{e9} and <:wave:777>"
        );
        assert_eq!(messages[0].edited_timestamp, None);
        assert_eq!(messages[0].guild_id, None);
        assert_eq!(messages[1].author.display_name(), "other_fixture");
        assert!(messages[1].author.bot && messages[1].pinned);
        assert_eq!(
            messages[1].edited_timestamp.as_deref(),
            Some("2026-10-07T12:05:00.000000+00:00")
        );
        assert_eq!(messages[1].reactions.len(), 2);
        assert!(messages[1].reactions[0].me && !messages[1].reactions[1].me);
        assert_eq!(messages[1].reactions[1].emoji.id, Some(id(777)));
        assert_eq!(messages[2].kind, 19);
        assert_eq!(messages[2].attachments[0].width, Some(640));
        assert_eq!(
            messages[2]
                .message_reference
                .as_ref()
                .and_then(|r| r.message_id),
            Some(id(1_289_999_999_999_999_999))
        );
        // A reply carries what its target looked like; plain messages do not.
        assert_eq!(
            messages[2].replied_to(),
            Some(id(1_289_999_999_999_999_999))
        );
        match &messages[2].referenced_message {
            fastcord_model::Referenced::Message(preview) => {
                assert_eq!(preview.author.display_name(), "other_fixture");
                assert_eq!(preview.content, "the message being replied to");
            }
            other => panic!("expected a preview, got {other:?}"),
        }
        assert!(messages[0].referenced_message.is_unknown());
        assert_eq!(messages[0].replied_to(), None);
    }

    #[test]
    fn pages_that_do_not_belong_to_the_request_are_rejected_not_partly_trusted() {
        let page: Vec<Message> = serde_json::from_str(PAGE).unwrap();
        let body = |messages: &[Message]| serde_json::to_vec(messages).unwrap();

        // Another channel.
        assert_eq!(
            parse_page(&body(&page), id(501), HistoryCursor::Latest),
            Err(RestError::InvalidJson)
        );
        // Messages must be strictly older than `before` / newer than `after`.
        let newest = page[0].id;
        let oldest = page[2].id;
        assert_eq!(
            parse_page(&body(&page), id(500), HistoryCursor::Before(newest)),
            Err(RestError::InvalidJson)
        );
        assert_eq!(
            parse_page(&body(&page), id(500), HistoryCursor::After(oldest)),
            Err(RestError::InvalidJson)
        );
        assert_eq!(
            parse_page(
                &body(&page),
                id(500),
                HistoryCursor::Before(Snowflake(newest.0 + 1))
            )
            .map(|messages| messages.len()),
            Ok(3)
        );
        assert_eq!(
            parse_page(
                &body(&page),
                id(500),
                HistoryCursor::After(Snowflake(oldest.0 - 1))
            )
            .map(|messages| messages.len()),
            Ok(3)
        );
    }

    #[test]
    fn page_length_and_json_shape_are_enforced_while_decoding() {
        let page: Vec<Message> = serde_json::from_str(PAGE).unwrap();
        let repeated = |count: usize| {
            let mut messages = Vec::with_capacity(count);
            for n in 0..count {
                let mut message = page[0].clone();
                message.id = Snowflake(n as u64 + 1);
                messages.push(message);
            }
            serde_json::to_vec(&messages).unwrap()
        };
        let parsed = |body: &[u8]| parse_page(body, id(500), HistoryCursor::Latest);
        assert_eq!(parsed(&repeated(0)).map(|messages| messages.len()), Ok(0));
        assert_eq!(
            parsed(&repeated(MESSAGE_PAGE_LIMIT)).map(|messages| messages.len()),
            Ok(MESSAGE_PAGE_LIMIT)
        );
        let around = |body: &[u8]| parse_page(body, id(500), HistoryCursor::Around(id(20)));
        assert_eq!(
            around(&repeated(MESSAGE_PAGE_LIMIT)).map(|messages| messages.len()),
            Ok(MESSAGE_PAGE_LIMIT),
            "the 50-record decoder accepts a possible limit+1 response to limit=49"
        );
        assert_eq!(
            parsed(&repeated(MESSAGE_PAGE_LIMIT + 1)),
            Err(RestError::InvalidJson)
        );
        assert_eq!(
            parsed(&repeated(MESSAGE_PAGE_LIMIT * 4)),
            Err(RestError::InvalidJson)
        );
        // Not an array, a record without required fields, and truncated JSON.
        let malformed: [&[u8]; 5] = [
            br#"{"message":"You are being rate limited."}"#,
            br#"[{"id":"1"}]"#,
            br"[null]",
            b"[{",
            b"",
        ];
        for bad in malformed {
            assert_eq!(parsed(bad), Err(RestError::InvalidJson), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn stopped_history_client_uses_the_account_stop_without_touching_the_network() {
        let client =
            RestClient::new(UserToken::new("offline-history-credential".to_owned())).unwrap();
        client.stop_authenticated_work();
        for cursor in [
            HistoryCursor::Latest,
            HistoryCursor::Before(id(9)),
            HistoryCursor::After(id(9)),
            HistoryCursor::Around(id(9)),
        ] {
            assert!(matches!(
                client.channel_messages(id(500), cursor).await,
                Err(RestError::AuthenticationRequired)
            ));
        }
        assert_eq!(
            client.channel_message(id(500), id(9)).await,
            Err(RestError::AuthenticationRequired)
        );
    }
}
