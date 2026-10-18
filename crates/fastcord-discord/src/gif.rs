//! GIF search as user clients read it (`GET /gifs/search`, `GET /gifs/trending`).
//!
//! Discord proxies its GIF provider, so only Discord ever sees the token; the
//! provider is contacted only for credential-free preview downloads by the
//! caller. Both routes answer a bare array or `{"gifs":[...]}`; every
//! result must carry an https `url` (what is posted) and an https preview.

use reqwest::Url;
use serde::Deserialize;

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route};

/// Most results kept from one answer.
pub const GIF_RESULTS: usize = 24;
/// Longest search text sent.
pub const MAX_GIF_QUERY_CHARS: usize = 100;
const TITLE_CHARS: usize = 100;
const MAX_URL_BYTES: usize = 2048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GifResult {
    pub title: String,
    /// The page URL a message carries; Discord embeds it as a GIF.
    pub url: String,
    /// A small image to preview the result with.
    pub preview: String,
}

fn gif_route(query: Option<&str>) -> Result<Route, RestError> {
    let mut url = Url::parse("https://discord.invalid/").map_err(|_| RestError::InvalidRoute)?;
    {
        let mut pairs = url.query_pairs_mut();
        if let Some(query) = query {
            pairs.append_pair("q", query);
        }
        pairs
            .append_pair("provider", "tenor")
            .append_pair("locale", "en-US")
            .append_pair("media_format", "gif");
    }
    let q = url.query().unwrap_or_default();
    let path = if query.is_some() {
        "search"
    } else {
        "trending"
    };
    Route::new(Method::GET, &format!("/gifs/{path}?{q}"))
}

impl<C: Clock> RestClient<C> {
    /// Trending GIFs, or those matching `query`. Each call is sent once.
    pub async fn gifs(&self, query: Option<&str>) -> Result<Vec<GifResult>, RestError> {
        let query = query.map(|q| q.chars().take(MAX_GIF_QUERY_CHARS).collect::<String>());
        let request = RestRequest::new(gif_route(query.as_deref())?, Priority::UserRead);
        parse_gifs(self.execute(request).await?.body())
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Wire {
    Wrapped { gifs: Vec<Item> },
    Bare(Vec<Item>),
}

#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    preview: Option<String>,
    #[serde(default)]
    gif_src: Option<String>,
    #[serde(default)]
    src: Option<String>,
}

fn https(url: &str) -> bool {
    url.len() <= MAX_URL_BYTES && Url::parse(url).is_ok_and(|u| u.scheme() == "https")
}

pub(crate) fn parse_gifs(body: &[u8]) -> Result<Vec<GifResult>, RestError> {
    let items = match serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)? {
        Wire::Wrapped { gifs } => gifs,
        Wire::Bare(items) => items,
    };
    Ok(items
        .into_iter()
        .filter_map(|item| {
            let preview = [item.preview, item.gif_src, item.src]
                .into_iter()
                .flatten()
                .find(|u| https(u))?;
            https(&item.url).then(|| GifResult {
                title: item.title.chars().take(TITLE_CHARS).collect(),
                url: item.url,
                preview,
            })
        })
        .take(GIF_RESULTS)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_shapes_parse_and_unusable_items_are_dropped() {
        let bare = br#"[{"title":"a","url":"https://tenor.com/view/a-1","preview":"https://media.tenor.com/a.gif"},
            {"title":"b","url":"http://tenor.com/b","preview":"https://x/b.gif"},
            {"title":"c","url":"https://tenor.com/c"}]"#;
        let gifs = parse_gifs(bare).unwrap();
        assert_eq!(gifs.len(), 1);
        assert_eq!(parse_gifs(br#"{"gifs":[]}"#).unwrap(), vec![]);
        assert!(parse_gifs(b"{}").is_err());
        assert_eq!(
            gif_route(Some("a b")).unwrap().key().normalized(),
            "/gifs/search"
        );
    }
}
