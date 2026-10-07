//! Typed channel-history reads through the account's existing REST scheduler.
//!
//! One request returns one page of at most [`MESSAGE_PAGE_LIMIT`] messages,
//! newest first, exactly as Discord sends it. Merging pages into bounded
//! memory is the [`message_store`](crate::message_store)'s job; this module
//! only builds the route, decodes the page with a hard record limit, and checks
//! that the response belongs to the requested channel and cursor.

use std::fmt;

use fastcord_model::{Message, Snowflake};
use serde::de::{Error as _, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route};

/// Discord's default page size and the only one this client requests.
pub const MESSAGE_PAGE_LIMIT: usize = 50;

/// Which page of a channel's history to read. The three cursors are mutually
/// exclusive, as in the API. Reply jumps (`around`) are a separate feature.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistoryCursor {
    /// The newest messages.
    #[default]
    Latest,
    /// The messages immediately older than this one.
    Before(Snowflake),
    /// The messages immediately newer than this one.
    After(Snowflake),
}

fn history_route(channel: Snowflake, cursor: HistoryCursor) -> Result<Route, RestError> {
    let path = match cursor {
        HistoryCursor::Latest => {
            format!("/channels/{channel}/messages?limit={MESSAGE_PAGE_LIMIT}")
        }
        HistoryCursor::Before(id) => {
            format!("/channels/{channel}/messages?limit={MESSAGE_PAGE_LIMIT}&before={id}")
        }
        HistoryCursor::After(id) => {
            format!("/channels/{channel}/messages?limit={MESSAGE_PAGE_LIMIT}&after={id}")
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
                HistoryCursor::Latest => true,
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
    fn history_routes_use_one_fixed_limit_and_exclusive_cursors() {
        let latest = history_route(id(500), HistoryCursor::Latest).unwrap();
        let before = history_route(id(500), HistoryCursor::Before(id(12))).unwrap();
        let after = history_route(id(500), HistoryCursor::After(id(12))).unwrap();
        let expected = [
            "/channels/500/messages?limit=50",
            "/channels/500/messages?limit=50&before=12",
            "/channels/500/messages?limit=50&after=12",
        ];
        for (route, path) in [&latest, &before, &after].into_iter().zip(expected) {
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
        assert_eq!(latest.key().major().channel_id(), Some(id(500)));
        assert_ne!(
            latest.key(),
            history_route(id(501), HistoryCursor::Latest).unwrap().key()
        );
        assert_eq!(HistoryCursor::default(), HistoryCursor::Latest);
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
        ] {
            assert!(matches!(
                client.channel_messages(id(500), cursor).await,
                Err(RestError::AuthenticationRequired)
            ));
        }
    }
}
