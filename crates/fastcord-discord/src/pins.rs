//! Pinned messages (SPEC §16 milestone 44): the paged list as user clients read
//! it (`GET /channels/{id}/messages/pins?limit=50[&before={pinned_at}]`) and
//! pinning/unpinning one message (`PUT`/`DELETE /channels/{id}/pins/{message}`).
//!
//! Discord has answered the list in two shapes: a bare array of messages (no
//! paging) and `{"items":[{"pinned_at","message"}],"has_more"}`. Both are
//! accepted; a page never holds more than [`PINS_PAGE_SIZE`] items and every
//! message must belong to the asked channel. Each request is sent once.

use fastcord_model::{Message, Snowflake};
use reqwest::Url;
use serde::Deserialize;

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route};

/// Most pins one page may hold (the `limit` asked for).
pub const PINS_PAGE_SIZE: usize = 50;

/// One pinned message with the time it was pinned (the paging cursor).
#[derive(Clone, Debug, PartialEq)]
pub struct PinnedMessage {
    pub pinned_at: Option<String>,
    pub message: Message,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PinsPage {
    pub items: Vec<PinnedMessage>,
    /// Whether another page follows `before` the last item's `pinned_at`.
    pub has_more: bool,
}

fn pins_route(channel: Snowflake, before: Option<&str>) -> Result<Route, RestError> {
    let mut url = Url::parse("https://discord.invalid/").map_err(|_| RestError::InvalidRoute)?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("limit", &PINS_PAGE_SIZE.to_string());
        if let Some(before) = before {
            pairs.append_pair("before", before);
        }
    }
    let query = url.query().unwrap_or_default();
    Route::new(
        Method::GET,
        &format!("/channels/{channel}/messages/pins?{query}"),
    )
}

fn pin_route(method: Method, channel: Snowflake, message: Snowflake) -> Result<Route, RestError> {
    Route::new(method, &format!("/channels/{channel}/pins/{message}"))
}

impl<C: Clock> RestClient<C> {
    /// Reads one page of the channel's pins, newest pin first. `before` is the
    /// `pinned_at` of the last item already held.
    pub async fn pinned_messages(
        &self,
        channel: Snowflake,
        before: Option<&str>,
    ) -> Result<PinsPage, RestError> {
        let request = RestRequest::new(pins_route(channel, before)?, Priority::UserRead);
        let response = self.execute(request).await?;
        parse_pins(response.body(), channel)
    }

    /// Pins (`pinned`) or unpins one message. Needs Manage Messages in a server.
    pub async fn set_pinned(
        &self,
        channel: Snowflake,
        message: Snowflake,
        pinned: bool,
    ) -> Result<(), RestError> {
        let method = if pinned { Method::PUT } else { Method::DELETE };
        let request = RestRequest::new(pin_route(method, channel, message)?, Priority::UserWrite);
        self.execute(request).await.map(drop)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Wire {
    Items {
        items: Vec<serde_json::Value>,
        #[serde(default)]
        has_more: bool,
    },
    Bare(Vec<serde_json::Value>),
}

pub(crate) fn parse_pins(body: &[u8], channel: Snowflake) -> Result<PinsPage, RestError> {
    let (raw, has_more) = match serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)? {
        Wire::Items { items, has_more } => (items, has_more),
        Wire::Bare(items) => (items, false),
    };
    if raw.len() > PINS_PAGE_SIZE {
        return Err(RestError::InvalidJson);
    }
    let mut items = Vec::with_capacity(raw.len());
    for mut value in raw {
        let pinned_at = value
            .get("pinned_at")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        // A wrapper holds the message under `message`; a bare one is the message.
        let message = match value.get_mut("message") {
            Some(inner) => inner.take(),
            None => value,
        };
        let message: Message =
            serde_json::from_value(message).map_err(|_| RestError::InvalidJson)?;
        if message.channel_id != channel {
            return Err(RestError::InvalidJson);
        }
        items.push(PinnedMessage { pinned_at, message });
    }
    Ok(PinsPage { items, has_more })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ITEMS: &str = include_str!("../../../fixtures/rest/pins-items.json");
    const BARE: &str = include_str!("../../../fixtures/rest/pins-array.json");
    const CHANNEL: Snowflake = Snowflake(500);

    fn path(route: &Route) -> String {
        route
            .url
            .as_str()
            .strip_prefix("https://discord.com/api/v9")
            .unwrap()
            .to_owned()
    }

    #[test]
    fn routes_page_with_the_pinned_at_cursor_and_pin_by_put_or_delete() {
        assert_eq!(
            path(&pins_route(CHANNEL, None).unwrap()),
            "/channels/500/messages/pins?limit=50"
        );
        assert_eq!(
            path(&pins_route(CHANNEL, Some("2026-10-07T12:00:00.000000+00:00")).unwrap()),
            "/channels/500/messages/pins?limit=50&before=2026-10-07T12%3A00%3A00.000000%2B00%3A00"
        );
        let put = pin_route(Method::PUT, CHANNEL, Snowflake(9)).unwrap();
        assert_eq!(put.key.method(), &Method::PUT);
        assert_eq!(path(&put), "/channels/500/pins/9");
    }

    #[test]
    fn the_wrapped_shape_keeps_cursors_and_has_more() {
        let page = parse_pins(ITEMS.as_bytes(), CHANNEL).unwrap();
        assert!(page.has_more);
        assert_eq!(page.items.len(), 2);
        assert_eq!(
            page.items[0].pinned_at.as_deref(),
            Some("2026-10-07T12:05:00.000000+00:00")
        );
        assert_eq!(page.items[0].message.content, "pinned one");
    }

    #[test]
    fn the_bare_array_shape_is_one_final_page() {
        let page = parse_pins(BARE.as_bytes(), CHANNEL).unwrap();
        assert!(!page.has_more);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].pinned_at, None);
    }

    #[test]
    fn foreign_oversized_and_malformed_pages_are_refused() {
        assert_eq!(
            parse_pins(ITEMS.as_bytes(), Snowflake(501)),
            Err(RestError::InvalidJson)
        );
        let many = format!("[{}]", vec!["{}"; PINS_PAGE_SIZE + 1].join(","));
        assert_eq!(
            parse_pins(many.as_bytes(), CHANNEL),
            Err(RestError::InvalidJson)
        );
        assert_eq!(parse_pins(b"{}", CHANNEL), Err(RestError::InvalidJson));
        assert_eq!(parse_pins(b"[]", CHANNEL).unwrap().items.len(), 0);
    }
}
