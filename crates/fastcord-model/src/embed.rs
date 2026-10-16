//! Embeds Discord attaches to a message (link unfurls, bot and webhook cards).

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// Most embeds one message keeps; Discord allows 10.
pub const MAX_EMBEDS: usize = 10;
/// Most fields one embed keeps; Discord allows 25.
pub const MAX_EMBED_FIELDS: usize = 25;

/// An image or thumbnail. The URL may be signed: never log it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedMedia {
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub inline: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedText {
    pub name: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedFooter {
    pub text: String,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Embed {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// 0xRRGGBB.
    #[serde(default)]
    pub color: Option<u32>,
    #[serde(default)]
    pub author: Option<EmbedText>,
    #[serde(default)]
    pub provider: Option<EmbedText>,
    #[serde(default)]
    pub footer: Option<EmbedFooter>,
    #[serde(default)]
    pub image: Option<EmbedMedia>,
    #[serde(default)]
    pub thumbnail: Option<EmbedMedia>,
    #[serde(default, deserialize_with = "bounded_fields")]
    pub fields: Vec<EmbedField>,
}

impl Embed {
    /// Whether anything of this embed can be shown.
    pub fn is_visible(&self) -> bool {
        self.title.is_some()
            || self.description.is_some()
            || self.author.is_some()
            || self.footer.is_some()
            || self.image.is_some()
            || self.thumbnail.is_some()
            || !self.fields.is_empty()
    }

    /// Releases spare capacity so [`heap_bytes`](Self::heap_bytes) is exact.
    pub fn compact(&mut self) {
        let shrink = |v: &mut Option<String>| {
            if let Some(v) = v {
                v.shrink_to_fit();
            }
        };
        shrink(&mut self.title);
        shrink(&mut self.description);
        shrink(&mut self.url);
        if let Some(a) = &mut self.author {
            a.name.shrink_to_fit();
        }
        if let Some(a) = &mut self.provider {
            a.name.shrink_to_fit();
        }
        if let Some(f) = &mut self.footer {
            f.text.shrink_to_fit();
        }
        for media in [&mut self.image, &mut self.thumbnail].into_iter().flatten() {
            media.url.shrink_to_fit();
        }
        self.fields.shrink_to_fit();
        for field in &mut self.fields {
            field.name.shrink_to_fit();
            field.value.shrink_to_fit();
        }
    }

    /// Heap bytes held, for the message store's byte budget.
    pub fn heap_bytes(&self) -> usize {
        let text = |value: &Option<String>| value.as_ref().map_or(0, String::capacity);
        let media = |value: &Option<EmbedMedia>| value.as_ref().map_or(0, |m| m.url.capacity());
        text(&self.title)
            + text(&self.description)
            + text(&self.url)
            + self.author.as_ref().map_or(0, |a| a.name.capacity())
            + self.provider.as_ref().map_or(0, |a| a.name.capacity())
            + self.footer.as_ref().map_or(0, |f| f.text.capacity())
            + media(&self.image)
            + media(&self.thumbnail)
            + self.fields.capacity() * size_of::<EmbedField>()
            + self
                .fields
                .iter()
                .map(|f| f.name.capacity() + f.value.capacity())
                .sum::<usize>()
    }
}

impl fmt::Debug for Embed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Embed")
            .field("fields", &self.fields.len())
            .finish_non_exhaustive()
    }
}

fn bounded_fields<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<EmbedField>, D::Error> {
    let mut fields = Vec::<EmbedField>::deserialize(d)?;
    fields.truncate(MAX_EMBED_FIELDS);
    Ok(fields)
}

/// Deserializes a message's `embeds`, keeping the first [`MAX_EMBEDS`] that
/// show something. A malformed list is "no embeds", never a failed message.
pub(crate) fn lenient_embeds<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Embed>, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    let serde_json::Value::Array(items) = raw else {
        return Ok(Vec::new());
    };
    Ok(items
        .into_iter()
        .filter_map(|item| serde_json::from_value::<Embed>(item).ok())
        .filter(Embed::is_visible)
        .take(MAX_EMBEDS)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_are_bounded_and_malformed_ones_dropped() {
        #[derive(Deserialize)]
        struct M {
            #[serde(default, deserialize_with = "lenient_embeds")]
            embeds: Vec<Embed>,
        }
        let fields: Vec<_> = (0..40)
            .map(|n| serde_json::json!({"name": n.to_string(), "value": "v"}))
            .collect();
        let mut items = vec![
            serde_json::json!({"title": "t", "color": 255, "fields": fields}),
            serde_json::json!({"type": "rich"}),
            serde_json::json!({"title": 7}),
        ];
        items.extend((0..20).map(|_| serde_json::json!({"description": "d"})));
        let m: M = serde_json::from_value(serde_json::json!({ "embeds": items })).unwrap();
        assert_eq!(m.embeds.len(), MAX_EMBEDS);
        assert_eq!(m.embeds[0].fields.len(), MAX_EMBED_FIELDS);
        assert_eq!(m.embeds[0].color, Some(255));
        let m: M = serde_json::from_value(serde_json::json!({ "embeds": "x" })).unwrap();
        assert!(m.embeds.is_empty());
    }
}
