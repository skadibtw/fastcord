//! Presentation of parsed message Markdown, mentions and embeds.
//!
//! The source text is never rewritten: every span borrows from it (copying and
//! editing use the original), and mentions, spoilers and links are only shown
//! differently. Nothing here is fetched: emoji images keep using the bounded
//! emoji views and the timeline's `Occurrence` visibility reports, and embed
//! pictures come from the bounded avatar store (`embed_keys` says which).

use std::collections::{HashMap, HashSet};

use crate::ui::button;
use fastcord_model::markup::{self, Block, BlockKind, Kind, Span as Part, Style};
use fastcord_model::{Embed, EmbedMedia, Message, SnapshotMessage, Snowflake, User};
use iced::font::{self, Font, Weight};
use iced::widget::{column, container, image, rich_text, row, space, span, text};
use iced::{Background, Color, Element, Length};

use crate::avatars::AvatarKey;
use crate::emoji::{self, Asset, MAX_INLINE_ASSETS, Origin, View};
use crate::style::{MENTION, MENTION_BG};
use crate::timeline::Event;

/// Most role and channel names one snapshot carries.
pub const MAX_NAMES: usize = 256;
/// Spoilers a viewer has uncovered, per account session (oldest dropped).
pub const MAX_REVEALED: usize = 256;

const LINK: Color = Color::from_rgb(0.18, 0.5, 0.88);
const SPOILER: Color = Color::from_rgb(0.5, 0.5, 0.5);
const FAINT: Color = Color::from_rgba(0.5, 0.5, 0.5, 0.25);

/// Role and channel names the open channel's rows mention.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    pub roles: Vec<(Snowflake, String)>,
    pub channels: Vec<(Snowflake, String)>,
}

impl Names {
    fn role(&self, id: Snowflake) -> Option<&str> {
        find(&self.roles, id)
    }

    fn channel(&self, id: Snowflake) -> Option<&str> {
        find(&self.channels, id)
    }
}

fn find(list: &[(Snowflake, String)], id: Snowflake) -> Option<&str> {
    list.iter()
        .find(|(known, _)| *known == id)
        .map(|(_, name)| name.as_str())
}

/// Which role and channel mentions the given contents reference.
pub fn wanted<'a>(contents: impl IntoIterator<Item = &'a str>) -> (Vec<Snowflake>, Vec<Snowflake>) {
    let (mut roles, mut channels) = (Vec::new(), Vec::new());
    for content in contents {
        if !content.contains("<@&") && !content.contains("<#") {
            continue;
        }
        for block in markup::parse(content).blocks {
            for part in block.spans {
                match part.kind {
                    Kind::Role(id) if !roles.contains(&id) && roles.len() < MAX_NAMES => {
                        roles.push(id);
                    }
                    Kind::Channel(id) if !channels.contains(&id) && channels.len() < MAX_NAMES => {
                        channels.push(id);
                    }
                    _ => {}
                }
            }
        }
    }
    (roles, channels)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Url(String),
    Spoiler(Snowflake, u16),
}

fn to_event(link: Link) -> Event {
    match link {
        Link::Url(url) => Event::OpenLink(url),
        Link::Spoiler(message_id, spoiler) => Event::ToggleSpoiler {
            message_id,
            spoiler,
        },
    }
}

/// Everything rendering one message body needs besides the message.
#[derive(Clone, Copy)]
pub struct Cx<'a> {
    pub names: &'a Names,
    pub revealed: &'a HashSet<(Snowflake, u16)>,
    pub emojis: &'a HashMap<Asset, View>,
    /// Loaded embed pictures, keyed by [`embed_keys`].
    pub pictures: &'a HashMap<AvatarKey, image::Handle>,
    /// Unix seconds, for `<t:…:R>`; read when the row is built, never ticking.
    pub now: i64,
}

/// What rendering reads of a message: the ordinary one or a forward's snapshot
/// (which borrows its parent's ID, so spoiler state keys stay per row).
#[derive(Clone, Copy)]
pub struct Parts<'a> {
    id: Snowflake,
    mentions: &'a [User],
    everyone: bool,
    embeds: &'a [Embed],
}

impl<'a> From<&'a Message> for Parts<'a> {
    fn from(message: &'a Message) -> Self {
        Self {
            id: message.id,
            mentions: &message.mentions,
            everyone: message.mention_everyone,
            embeds: &message.embeds,
        }
    }
}

impl<'a> Parts<'a> {
    /// Text that belongs to no message (the server guide): no mentions or embeds.
    pub fn none() -> Self {
        Self {
            id: Snowflake(0),
            mentions: &[],
            everyone: false,
            embeds: &[],
        }
    }

    pub fn snapshot(parent: &Message, snapshot: &'a SnapshotMessage) -> Self {
        Self {
            id: parent.id,
            mentions: &snapshot.mentions,
            everyone: false,
            embeds: &snapshot.embeds,
        }
    }
}

struct Renderer<'a> {
    cx: Cx<'a>,
    message: Parts<'a>,
    channel: Snowflake,
    revision: u64,
    /// Emoji images are only shown (and reported visible) in the message
    /// body; embeds show an emoji's source text instead.
    images: bool,
    /// Emoji run ordinal, matching [`emoji_assets`].
    ordinal: usize,
}

/// The emoji of `content` that are shown as images, in display order. This is
/// the single definition both the view and the visibility check use.
pub fn emoji_assets(content: &str) -> Vec<Asset> {
    let parsed = markup::parse(content);
    let mut found = Vec::new();
    let mut take = |text: &str| {
        for run in emoji::runs(text) {
            if let emoji::Run::Emoji { asset, .. } = run
                && found.len() < MAX_INLINE_ASSETS
            {
                found.push(asset);
            }
        }
    };
    if !parsed.formatted {
        take(content);
        return found;
    }
    for block in &parsed.blocks {
        if matches!(block.kind, BlockKind::Code(_)) {
            continue;
        }
        for part in &block.spans {
            if part.kind == Kind::Text && !part.style.has(Style::CODE) {
                take(part.text);
            }
        }
    }
    found
}

/// The message body when it needs more than plain text, otherwise `None` (the
/// caller shows it with the plain, jumbo-aware emoji path).
pub fn body<'a>(
    content: &'a str,
    message: impl Into<Parts<'a>>,
    channel: Snowflake,
    revision: u64,
    cx: Cx<'a>,
) -> Option<Element<'a, Event>> {
    let parsed = markup::parse(content);
    if !parsed.formatted {
        return None;
    }
    let mut renderer = Renderer {
        cx,
        message: message.into(),
        channel,
        revision,
        images: true,
        ordinal: 0,
    };
    Some(renderer.blocks(&parsed.blocks))
}

/// Largest box of an embed thumbnail and of an embed image, in pixels.
const THUMBNAIL_BOX: (u16, u16) = (80, 80);
const IMAGE_BOX: (u16, u16) = (400, 300);

/// The size a picture of the dimensions Discord reported is shown at: inside
/// `bx`, same aspect, never enlarged. Unreported dimensions use the box.
fn shown(media: &EmbedMedia, bx: (u16, u16)) -> (u16, u16) {
    let (Some(w), Some(h)) = (
        media.width.filter(|&w| w > 0),
        media.height.filter(|&h| h > 0),
    ) else {
        return bx;
    };
    let scale = (f64::from(bx.0) / f64::from(w))
        .min(f64::from(bx.1) / f64::from(h))
        .min(1.0);
    (
        ((f64::from(w) * scale).round() as u16).max(1),
        ((f64::from(h) * scale).round() as u16).max(1),
    )
}

/// The picture to fetch for one embed medium. Only Discord's media proxy copy
/// qualifies, never the third-party original, so nothing reaches another host.
fn picture_key(media: &EmbedMedia, thumbnail: bool) -> Option<AvatarKey> {
    let bx = if thumbnail { THUMBNAIL_BOX } else { IMAGE_BOX };
    AvatarKey::embed(media.proxy_url.as_deref()?, shown(media, bx))
}

/// The pictures these embeds ask for: thumbnail first, then image.
pub fn embed_keys(embeds: &[Embed]) -> impl Iterator<Item = AvatarKey> {
    embeds.iter().flat_map(|embed| {
        [
            embed.thumbnail.as_ref().and_then(|m| picture_key(m, true)),
            embed.image.as_ref().and_then(|m| picture_key(m, false)),
        ]
        .into_iter()
        .flatten()
    })
}

/// Embeds below a message. They take their text from the supplied metadata
/// only; remote images are not fetched.
pub fn embeds<'a>(
    message: impl Into<Parts<'a>>,
    channel: Snowflake,
    revision: u64,
    cx: Cx<'a>,
) -> Vec<Element<'a, Event>> {
    let mut renderer = Renderer {
        cx,
        message: message.into(),
        channel,
        revision,
        images: false,
        ordinal: 0,
    };
    renderer
        .message
        .embeds
        .iter()
        .map(|embed| renderer.embed(embed))
        .collect()
}

fn bar(rgb: Option<u32>) -> Color {
    rgb.map_or(Color::from_rgb(0.45, 0.45, 0.5), |c| {
        Color::from_rgb8((c >> 16) as u8, (c >> 8) as u8, c as u8)
    })
}

/// Whether `parts[n]` is the last part of its link: a masked label can be
/// split into several formatted parts that share one target.
fn ends_link(parts: &[Part<'_>], n: usize) -> bool {
    !matches!((parts[n].kind, parts.get(n + 1)), (Kind::Link(a), Some(next)) if next.kind == Kind::Link(a))
}

/// The host (and port) a link opens, from the same parse that opens it, so a
/// masked label's hint cannot name another host (#161).
fn host(url: &str) -> String {
    link_target(url).map_or_else(String::new, |url| {
        let host = url.host_str().unwrap_or_default();
        match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        }
    })
}

impl<'a> Renderer<'a> {
    fn blocks(&mut self, blocks: &[Block<'a>]) -> Element<'a, Event> {
        let mut out = column![].spacing(2).width(Length::Fill);
        for block in blocks {
            out = out.push(self.block(block));
        }
        out.into()
    }

    fn block(&mut self, block: &Block<'a>) -> Element<'a, Event> {
        match block.kind {
            BlockKind::Code(_) => {
                let code = block.spans.first().map_or("", |part| part.text);
                container(
                    text(code)
                        .font(Font::MONOSPACE)
                        .size(13)
                        .width(Length::Fill)
                        .wrapping(text::Wrapping::WordOrGlyph),
                )
                .padding(6)
                .width(Length::Fill)
                .style(container::rounded_box)
                .into()
            }
            _ if block.spans.is_empty() => space().height(8).into(),
            BlockKind::Paragraph => self.line(&block.spans, 14.0, None, false),
            BlockKind::Quote => self.line(&block.spans, 14.0, Some("\u{2502} "), false),
            BlockKind::Heading(level) => {
                let size = match level {
                    1 => 22.0,
                    2 => 19.0,
                    _ => 16.0,
                };
                self.line(&block.spans, size, None, true)
            }
            BlockKind::Subtext => self.line(&block.spans, 11.0, None, false),
        }
    }

    /// One source line. Text without emoji images stays one wrapping text
    /// widget; otherwise words and emoji flow in a wrapping row.
    fn line(
        &mut self,
        parts: &[Part<'a>],
        size: f32,
        prefix: Option<&'static str>,
        heading: bool,
    ) -> Element<'a, Event> {
        let has_emoji = self.images
            && parts.iter().any(|part| {
                part.kind == Kind::Text
                    && !part.style.has(Style::CODE)
                    && emoji::runs(part.text).any(|run| matches!(run, emoji::Run::Emoji { .. }))
            });
        if !has_emoji {
            let mut spans = Vec::new();
            if let Some(prefix) = prefix {
                spans.push(span(prefix).color(SPOILER));
            }
            for (n, part) in parts.iter().enumerate() {
                let last = ends_link(parts, n);
                self.push_spans(&mut spans, part, part.text, size, heading, last);
            }
            return rich_text(spans)
                .on_link_click(to_event)
                .size(size)
                .width(Length::Fill)
                .wrapping(text::Wrapping::WordOrGlyph)
                .into();
        }
        let new_line = || {
            row![]
                .spacing(0)
                .align_y(iced::alignment::Vertical::Center)
                .width(Length::Fill)
        };
        let mut line = new_line();
        if let Some(prefix) = prefix {
            line = line.push(text(prefix).size(size).color(SPOILER));
        }
        let emoji_size = 22.0;
        for (n, part) in parts.iter().enumerate() {
            if part.kind != Kind::Text || part.style.has(Style::CODE) {
                let mut spans = Vec::new();
                let last = ends_link(parts, n);
                self.push_spans(&mut spans, part, part.text, size, heading, last);
                line = line.push(rich_text(spans).on_link_click(to_event).size(size));
                continue;
            }
            for run in emoji::runs(part.text) {
                match run {
                    emoji::Run::Text(chunk) => {
                        for word in chunk.split_inclusive(char::is_whitespace) {
                            let mut spans = Vec::new();
                            self.push_spans(&mut spans, part, word, size, heading, true);
                            line = line.push(rich_text(spans).on_link_click(to_event).size(size));
                        }
                    }
                    emoji::Run::Emoji {
                        source: token,
                        asset,
                    } => {
                        if self.ordinal >= MAX_INLINE_ASSETS {
                            line = line.push(text(token).size(size));
                            continue;
                        }
                        let occurrence = emoji::Occurrence {
                            origin: Origin::Timeline {
                                channel: self.channel,
                                message: self.message.id,
                                revision: self.revision,
                                run: self.ordinal as u16,
                            },
                            asset,
                        };
                        // Ordinals count every emoji, hidden or not, so they
                        // keep matching `emoji_assets`.
                        self.ordinal += 1;
                        if self.spoiler_hidden(part) {
                            // A hidden spoiler hides its emoji too: a grey,
                            // clickable box in place of the image (#158).
                            let mut spans = Vec::new();
                            self.push_spans(&mut spans, part, token, size, heading, true);
                            line = line.push(rich_text(spans).on_link_click(to_event).size(size));
                            continue;
                        }
                        line = line.push(emoji::sensed(
                            emoji::asset_view(asset, token, self.cx.emojis, emoji_size),
                            occurrence,
                            Event::EmojiVisibility,
                        ));
                    }
                }
            }
        }
        line.width(Length::Fill).wrap().into()
    }

    /// The part is inside a spoiler the viewer has not uncovered.
    fn spoiler_hidden(&self, part: &Part<'_>) -> bool {
        let prefs = crate::chat_prefs::current();
        part.spoiler.is_some_and(|id| {
            prefs.spoiler_hidden(self.cx.revealed.contains(&(self.message.id, id)))
        })
    }

    /// The styled iced spans for `shown` text of one parsed part.
    fn push_spans(
        &self,
        out: &mut Vec<iced::widget::text::Span<'a, Link, Font>>,
        part: &Part<'a>,
        shown: &'a str,
        size: f32,
        heading: bool,
        last_of_link: bool,
    ) {
        let message = self.message;
        let label: std::borrow::Cow<'a, str> = match part.kind {
            Kind::User(id) => message
                .mentions
                .iter()
                .find(|user| user.id == id)
                .map_or(crate::i18n::tr("@unknown-user").into(), |user| {
                    format!("@{}", user.display_name()).into()
                }),
            Kind::Role(id) => self
                .cx
                .names
                .role(id)
                .map_or(crate::i18n::tr("@unknown-role").into(), |name| {
                    format!("@{name}").into()
                }),
            Kind::Channel(id) => self
                .cx
                .names
                .channel(id)
                .map_or(crate::i18n::tr("#unknown-channel").into(), |name| {
                    format!("#{name}").into()
                }),
            Kind::Timestamp { unix, style } => {
                crate::localtime::markup(unix, style, crate::localtime::offset(), self.cx.now)
                    .map_or(shown.into(), Into::into)
            }
            _ => shown.into(),
        };
        let mut font = Font::DEFAULT;
        if part.style.has(Style::BOLD) || heading {
            font.weight = Weight::Bold;
        }
        if part.style.has(Style::ITALIC) {
            font.style = font::Style::Italic;
        }
        if part.style.has(Style::CODE) {
            font = Font::MONOSPACE;
        }
        let mut item = span(label).font(font).size(size);
        if part.style.has(Style::UNDERLINE) {
            item = item.underline(true);
        }
        if part.style.has(Style::STRIKE) {
            item = item.strikethrough(true);
        }
        if part.style.has(Style::CODE) {
            item = item.background(FAINT);
        }
        let mut target = None;
        match part.kind {
            Kind::Link(url) => {
                item = item.color(LINK).underline(true);
                target = Some(url);
            }
            Kind::User(_) | Kind::Role(_) | Kind::Channel(_) | Kind::Timestamp { .. } => {
                item = item.color(MENTION).background(MENTION_BG);
            }
            Kind::Everyone | Kind::Here if message.everyone => {
                item = item.color(MENTION).background(MENTION_BG);
            }
            _ => {}
        }
        let hidden = self.spoiler_hidden(part);
        if let Some(id) = part.spoiler {
            if hidden {
                item = item
                    .color(SPOILER)
                    .background(SPOILER)
                    .link(Link::Spoiler(message.id, id));
            } else {
                item = item.background(FAINT);
            }
        }
        if !hidden && let Some(url) = target {
            item = item.link(Link::Url(url.to_owned()));
        }
        out.push(item);
        // A masked link also names where it goes, so the label cannot hide it.
        if !hidden
            && last_of_link
            && let Some(url) = target
            && part.text != url
        {
            out.push(
                span(format!(" ({})", host(url)))
                    .size(size - 2.0)
                    .color(SPOILER),
            );
        }
    }

    fn markup(&mut self, source: &'a str) -> Element<'a, Event> {
        let parsed = markup::parse(source);
        self.blocks(&parsed.blocks)
    }

    fn embed(&mut self, embed: &'a Embed) -> Element<'a, Event> {
        let accent = bar(embed.color);
        let mut body = column![].spacing(3);
        let mut head = Vec::new();
        if let Some(provider) = &embed.provider {
            head.push(provider.name.as_str());
        }
        if let Some(author) = &embed.author {
            head.push(author.name.as_str());
        }
        if !head.is_empty() {
            body = body.push(
                text(head.join(" \u{b7} "))
                    .size(12)
                    .color(crate::style::muted()),
            );
        }
        if let Some(title) = &embed.title {
            let mut item = span(title.as_str()).size(15).font(Font {
                weight: Weight::Bold,
                ..Font::DEFAULT
            });
            item = item.color(accent);
            let mut spans = Vec::new();
            if let Some(url) = embed.url.as_deref().filter(|u| safe_url(u)) {
                item = item.underline(true).link(Link::Url(url.to_owned()));
            }
            spans.push(item);
            body = body.push(
                rich_text(spans)
                    .on_link_click(to_event)
                    .width(Length::Fill)
                    .wrapping(text::Wrapping::WordOrGlyph),
            );
        }
        if let Some(description) = &embed.description {
            body = body.push(self.markup(description));
        }
        for field in &embed.fields {
            body = body
                .push(
                    text(field.name.as_str())
                        .size(13)
                        .font(Font {
                            weight: Weight::Bold,
                            ..Font::DEFAULT
                        })
                        .wrapping(text::Wrapping::WordOrGlyph),
                )
                .push(self.markup(&field.value));
        }
        let mut thumbnail = None;
        for (label, media) in [("Image", &embed.image), ("Thumbnail", &embed.thumbnail)] {
            let is_thumbnail = label == "Thumbnail";
            let loaded = media.as_ref().and_then(|m| {
                let key = picture_key(m, is_thumbnail)?;
                let handle = self.cx.pictures.get(&key)?.clone();
                let AvatarKey::Embed { fit, .. } = key else {
                    return None;
                };
                let view = image(handle)
                    .width(f32::from(fit.0))
                    .height(f32::from(fit.1))
                    .content_fit(iced::ContentFit::Contain);
                Some(match safe_url(&m.url) {
                    true => Element::from(
                        button(view)
                            .padding(0)
                            .style(button::text)
                            .on_press(to_event(Link::Url(m.url.clone()))),
                    ),
                    false => view.into(),
                })
            });
            if let Some(view) = loaded {
                if is_thumbnail {
                    thumbnail = Some(view);
                } else {
                    body = body.push(view);
                }
                continue;
            }
            if let Some(media) = media.as_ref().filter(|m| safe_url(&m.url)) {
                body = body.push(
                    rich_text([span(crate::i18n::trf(
                        "{}: {}",
                        &[&crate::i18n::tr(label), &host(&media.url)],
                    ))
                    .size(12)
                    .color(LINK)
                    .underline(true)
                    .link(Link::Url(media.url.clone()))])
                    .on_link_click(to_event),
                );
            }
        }
        if let Some(footer) = &embed.footer {
            body = body.push(
                text(footer.text.as_str())
                    .size(11)
                    .color(crate::style::muted())
                    .wrapping(text::Wrapping::WordOrGlyph),
            );
        }
        let content: Element<'a, Event> = match thumbnail {
            Some(thumbnail) => row![body.width(Length::Fill), thumbnail].spacing(8).into(),
            None => body.into(),
        };
        container(content)
            .padding(8)
            .max_width(520)
            .style(move |theme: &iced::Theme| {
                let mut style = container::bordered_box(theme);
                style.border.color = accent;
                style.background = Some(Background::Color(FAINT));
                style
            })
            .into()
    }
}

/// Whether a link target may be handed to the operating system.
pub fn safe_url(url: &str) -> bool {
    link_target(url).is_some()
}

/// A link that may go to the OS handler: one http(s) token with a host and no
/// credentials, parsed as a browser would (a backslash is a path separator). Open
/// passes this normalized form, never the raw text.
pub fn link_target(raw: &str) -> Option<reqwest::Url> {
    if raw.len() > 2048
        || !(raw.starts_with("https://") || raw.starts_with("http://"))
        || raw.contains(|c: char| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let url = reqwest::Url::parse(raw).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_for_the_os_are_http_only_and_single_token() {
        assert!(safe_url("https://example.com/a?b=1#c"));
        assert!(safe_url("http://example.com"));
        for bad in [
            "javascript:alert(1)",
            "file:///C:/Windows/System32/calc.exe",
            "ftp://example.com",
            "https://",
            "https://exa mple.com",
            "https://example.com/\u{7}",
            "",
        ] {
            assert!(!safe_url(bad), "{bad:?}");
        }
        // Credentials are refused: they are how a label hides the real host.
        assert!(!safe_url("https://user:pw@example.com:8080/x"));
        assert_eq!(host("https://example.com:8080/x"), "example.com:8080");
        // A browser reads a backslash as "/": the host is the first name, not
        // the one after "@", and that is what is shown and opened.
        let spoof = "https://evil.test\\@discord.com/gift";
        assert_eq!(host(spoof), "evil.test");
        assert_eq!(
            link_target(spoof).unwrap().as_str(),
            "https://evil.test/@discord.com/gift"
        );
    }

    #[test]
    fn the_host_suffix_follows_only_the_last_part_of_a_formatted_label() {
        let parsed = markup::parse("[a **b** c](<https://x.test/p>) [d](https://y.test)");
        let parts = &parsed.blocks[0].spans;
        let ends: Vec<_> = (0..parts.len())
            .filter(|&n| matches!(parts[n].kind, Kind::Link(_)) && ends_link(parts, n))
            .map(|n| parts[n].text)
            .collect();
        assert_eq!(ends, [" c", "d"]);
    }

    #[test]
    fn emoji_order_skips_code_and_matches_the_plain_path() {
        let plain = "a 🦊 b 🐶";
        assert_eq!(emoji_assets(plain).len(), 2);
        let formatted = "**🦊** `🐶` 🐱\n```\n🐭\n```";
        assert_eq!(emoji_assets(formatted).len(), 2);
    }

    fn sample() -> Message {
        serde_json::from_value(serde_json::json!({
            "id": "10", "channel_id": "20",
            "author": {"id": "1", "username": "a"},
            "content": "**hi** <@2> <@&5> <#7> ||secret|| [x](https://example.com) 🦊 `c`
> q
```rs
let a;
```",
            "timestamp": "2026-10-16T00:00:00.000000+00:00",
            "mentions": [{"id": "2", "username": "b", "global_name": "Bee"}],
            "embeds": [{"title": "T", "url": "https://example.com", "description": "**d**",
                        "color": 16711680, "fields": [{"name": "n", "value": "v"}],
                        "footer": {"text": "f"}, "image": {"url": "https://example.com/i.png"}}]
        }))
        .unwrap()
    }

    #[test]
    fn a_formatted_message_and_its_embed_build_without_a_renderer() {
        let message = sample();
        let names = Names {
            roles: vec![(Snowflake(5), "mods".to_owned())],
            channels: vec![(Snowflake(7), "general".to_owned())],
        };
        let revealed = HashSet::new();
        let emojis = HashMap::new();
        let cx = Cx {
            names: &names,
            revealed: &revealed,
            emojis: &emojis,
            pictures: &HashMap::new(),
            now: 0,
        };
        assert!(body(&message.content, &message, Snowflake(20), 1, cx).is_some());
        assert!(body("plain text", &message, Snowflake(20), 1, cx).is_none());
        assert_eq!(embeds(&message, Snowflake(20), 1, cx).len(), 1);
        assert_eq!(emoji_assets(&message.content).len(), 1);
    }

    #[test]
    fn wanted_collects_unique_role_and_channel_ids_only() {
        let (roles, channels) = wanted(["<@&5> <@&5> <#7> <@9>", "plain", "`<#8>`"]);
        assert_eq!(roles, [Snowflake(5)]);
        assert_eq!(channels, [Snowflake(7)]);
    }

    fn media(json: serde_json::Value) -> EmbedMedia {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn embed_pictures_prefer_the_proxy_and_are_capped() {
        let proxy = "https://images-ext-1.discordapp.net/external/k/https/shop.test/p.jpg";
        let thumb = media(serde_json::json!({"url": "https://shop.test/p.jpg",
            "proxy_url": proxy, "width": 1000, "height": 500}));
        assert_eq!(shown(&thumb, THUMBNAIL_BOX), (80, 40));
        assert_eq!(shown(&thumb, IMAGE_BOX), (400, 200));
        let small = media(serde_json::json!({"url": "u", "width": 20, "height": 10}));
        assert_eq!(shown(&small, IMAGE_BOX), (20, 10), "never enlarged");
        assert_eq!(
            shown(&media(serde_json::json!({"url": "u"})), IMAGE_BOX),
            IMAGE_BOX
        );
        let embeds: Vec<Embed> = vec![
            serde_json::from_value(serde_json::json!({
                "title": "t",
                "thumbnail": {"url": "https://shop.test/p.jpg", "proxy_url": proxy,
                              "width": 1000, "height": 500},
                "image": {"url": "https://shop.test/q.jpg"},
            }))
            .unwrap(),
        ];
        let keys: Vec<_> = embed_keys(&embeds).collect();
        assert_eq!(keys.len(), 1, "no proxy copy, no fetch of the original");
        assert_eq!(keys[0].url(), proxy);
        assert!(matches!(keys[0], AvatarKey::Embed { fit: (80, 40), .. }));
    }

    fn nodes(tree: &iced_renderer::core::widget::Tree) -> usize {
        1 + tree.children.iter().map(nodes).sum::<usize>()
    }

    #[test]
    fn a_hidden_spoiler_hides_its_emoji_until_uncovered() {
        let message = sample();
        let names = Names::default();
        let emojis = HashMap::new();
        let pictures = HashMap::new();
        let count = |revealed: &HashSet<(Snowflake, u16)>| {
            let cx = Cx {
                names: &names,
                revealed,
                emojis: &emojis,
                pictures: &pictures,
                now: 0,
            };
            let element = body("a ||\u{1F98A}|| b", &message, Snowflake(20), 1, cx).unwrap();
            nodes(&iced_renderer::core::widget::Tree::new(&element))
        };
        let hidden = count(&HashSet::new());
        let shown = count(&HashSet::from([(message.id, 0)]));
        // Uncovered, the emoji is a sensed view (sensor + view); hidden, it is
        // one spoiler text in its place.
        assert!(hidden < shown, "hidden {hidden}, shown {shown}");
    }

    #[test]
    fn a_loaded_picture_replaces_the_text_line_and_a_missing_one_keeps_it() {
        let proxy = "https://media.discordapp.net/external/k/p.jpg";
        let message: Message = serde_json::from_value(serde_json::json!({
            "id": "10", "channel_id": "20", "author": {"id": "1", "username": "a"},
            "content": "", "timestamp": "2026-10-16T00:00:00.000000+00:00",
            "embeds": [{"title": "t", "thumbnail": {"url": "https://shop.test/p.jpg",
                        "proxy_url": proxy, "width": 80, "height": 80}}],
        }))
        .unwrap();
        let key = embed_keys(&message.embeds).next().unwrap();
        let names = Names::default();
        let revealed = HashSet::new();
        let emojis = HashMap::new();
        let count = |pictures: &HashMap<AvatarKey, image::Handle>| {
            let cx = Cx {
                names: &names,
                revealed: &revealed,
                emojis: &emojis,
                pictures,
                now: 0,
            };
            embeds(&message, Snowflake(20), 1, cx)
                .iter()
                .map(|element| nodes(&iced_renderer::core::widget::Tree::new(element)))
                .sum::<usize>()
        };
        let fallback = count(&HashMap::new());
        let handle = image::Handle::from_rgba(1, 1, vec![0u8, 0, 0, 255]);
        let loaded = count(&HashMap::from([(key, handle)]));
        assert_ne!(loaded, fallback, "the picture replaces the text line");
    }
}
