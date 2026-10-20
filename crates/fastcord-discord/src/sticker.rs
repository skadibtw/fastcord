//! Stickers the account may send in a server (`GET /guilds/{id}/stickers`).
//!
//! Only the open server's own stickers are offered: using another server's or a
//! standard pack's stickers is a Nitro privilege in the official client and the
//! server stays authoritative about what is allowed. The picker never sends
//! stickers through this module; it only lists them. Sending reuses
//! [`OutgoingMessage::with_sticker`](crate::OutgoingMessage::with_sticker).

use fastcord_model::{MAX_STICKER_NAME_CHARS, Snowflake};
use serde::Deserialize;

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route};

/// Most stickers kept from one answer (Discord's largest server allows 60 to 250).
pub const GUILD_STICKERS: usize = 250;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuildSticker {
    pub id: Snowflake,
    pub name: String,
    /// Discord's `format_type`: 1 PNG, 2 APNG, 3 Lottie, 4 GIF.
    pub format_type: u8,
    /// `false` when the server lost the boost level that kept it usable.
    pub available: bool,
}

#[derive(Deserialize)]
struct Wire {
    id: Snowflake,
    #[serde(default)]
    name: String,
    #[serde(default)]
    format_type: u8,
    #[serde(default = "yes")]
    available: bool,
}

fn yes() -> bool {
    true
}

impl<C: Clock> RestClient<C> {
    /// The stickers of `guild`, sorted by name. Sent once; a refusal means the
    /// account may not list them.
    pub async fn guild_stickers(&self, guild: Snowflake) -> Result<Vec<GuildSticker>, RestError> {
        let route = Route::new(Method::GET, &format!("/guilds/{guild}/stickers"))?;
        let response = self
            .execute(RestRequest::new(route, Priority::UserRead))
            .await?;
        parse_stickers(response.body())
    }
}

pub(crate) fn parse_stickers(body: &[u8]) -> Result<Vec<GuildSticker>, RestError> {
    let mut stickers: Vec<GuildSticker> = serde_json::from_slice::<Vec<Wire>>(body)
        .map_err(|_| RestError::InvalidJson)?
        .into_iter()
        .take(GUILD_STICKERS)
        .map(|wire| GuildSticker {
            id: wire.id,
            name: wire.name.chars().take(MAX_STICKER_NAME_CHARS).collect(),
            format_type: wire.format_type,
            available: wire.available,
        })
        .collect();
    stickers.sort_by_cached_key(|sticker| (sticker.name.to_lowercase(), sticker.id));
    Ok(stickers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stickers_parse_sorted_with_availability_and_garbage_is_refused() {
        let body = br#"[{"id":"2","name":"Zed","format_type":1,"available":false,"tags":"x"},
            {"id":"1","name":"alpha","format_type":3}]"#;
        let stickers = parse_stickers(body).unwrap();
        assert_eq!(stickers[0].name, "alpha");
        assert!(stickers[0].available, "available defaults to true");
        assert_eq!(stickers[0].format_type, 3);
        assert!(!stickers[1].available);
        assert!(parse_stickers(b"{}").is_err());
        assert_eq!(parse_stickers(b"[]").unwrap(), vec![]);
    }
}
