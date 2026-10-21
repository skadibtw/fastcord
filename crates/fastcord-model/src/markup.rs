//! Discord's message Markdown as borrowed spans: bold, italic, underline,
//! strike-through, inline and fenced code, quotes, headings, spoilers, links
//! and mentions.
//!
//! The parser never changes or copies the source: every span borrows from it,
//! so copying or editing a message always works on the original text. It is
//! line based (an emphasis never crosses a newline; fenced code does) and
//! bounded: nesting depth, blocks per message and the length of a line that is
//! scanned for emphasis are capped, and anything beyond a cap is shown as plain
//! text rather than refused.

use crate::Snowflake;

/// Most blocks parsed from one message; the rest stays one plain paragraph.
pub const MAX_BLOCKS: usize = 200;
/// Longest line (bytes) scanned for inline formatting; longer lines are plain.
pub const MAX_INLINE_LINE: usize = 4096;
const MAX_DEPTH: u8 = 6;
/// Longest link target kept (bytes).
const MAX_URL: usize = 2048;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style(u8);

impl Style {
    pub const BOLD: Style = Style(1);
    pub const ITALIC: Style = Style(2);
    pub const UNDERLINE: Style = Style(4);
    pub const STRIKE: Style = Style(8);
    pub const CODE: Style = Style(16);
    pub const SPOILER: Style = Style(32);

    pub fn has(self, other: Style) -> bool {
        self.0 & other.0 == other.0
    }

    fn with(self, other: Style) -> Style {
        Style(self.0 | other.0)
    }

    pub fn is_plain(self) -> bool {
        self.0 == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    Text,
    /// `text` is what to show; the target is separate (masked links differ).
    Link(&'a str),
    User(Snowflake),
    Role(Snowflake),
    Channel(Snowflake),
    Everyone,
    Here,
    /// `<t:unix[:style]>`; the app formats it in local time.
    Timestamp {
        unix: i64,
        style: TimeStyle,
    },
}

/// The `<t:…:X>` display styles; no style means `ShortDateTime`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeStyle {
    ShortTime,
    LongTime,
    ShortDate,
    LongDate,
    ShortDateTime,
    LongDateTime,
    Relative,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span<'a> {
    pub text: &'a str,
    pub style: Style,
    pub kind: Kind<'a>,
    /// Which spoiler of the message this belongs to (0-based, in source order).
    pub spoiler: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind<'a> {
    Paragraph,
    Quote,
    Heading(u8),
    Subtext,
    Code(Option<&'a str>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block<'a> {
    pub kind: BlockKind<'a>,
    pub spans: Vec<Span<'a>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed<'a> {
    pub blocks: Vec<Block<'a>>,
    /// Whether anything but raw lines of text was recognized. When `false`
    /// the source can be shown exactly as written.
    pub formatted: bool,
}

struct Ctx {
    spoilers: u16,
    formatted: bool,
}

pub fn parse(source: &str) -> Parsed<'_> {
    let mut ctx = Ctx {
        spoilers: 0,
        formatted: false,
    };
    let mut blocks = Vec::new();
    let mut starts = Vec::new();
    let mut lines: Vec<&str> = Vec::new();
    let mut offset = 0;
    for line in source.split('\n') {
        starts.push(offset);
        offset += line.len() + 1;
        lines.push(line.strip_suffix('\r').unwrap_or(line));
    }
    let mut at = 0;
    let mut quote_rest = false;
    while at < lines.len() {
        if blocks.len() >= MAX_BLOCKS {
            // ponytail: cap reached, the remaining lines are one plain block.
            blocks.push(Block {
                kind: BlockKind::Paragraph,
                spans: vec![plain(&source[starts[at]..])],
            });
            break;
        }
        let line = lines[at];
        if line.starts_with("```")
            && let Some((block, next)) = fenced(source, &starts, &lines, at)
        {
            {
                ctx.formatted = true;
                blocks.push(block);
                at = next;
                continue;
            }
        }
        let (kind, body) = if quote_rest {
            (BlockKind::Quote, line)
        } else if let Some(rest) = line.strip_prefix(">>> ") {
            quote_rest = true;
            (BlockKind::Quote, rest)
        } else if let Some(rest) = line.strip_prefix("> ") {
            (BlockKind::Quote, rest)
        } else if let Some(rest) = heading(line) {
            rest
        } else if let Some(rest) = line.strip_prefix("-# ") {
            (BlockKind::Subtext, rest)
        } else {
            (BlockKind::Paragraph, line)
        };
        let mut spans = Vec::new();
        if kind != BlockKind::Paragraph {
            ctx.formatted = true;
        }
        if kind == BlockKind::Paragraph
            && let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* "))
            && !item.is_empty()
        {
            ctx.formatted = true;
            spans.push(plain("\u{2022} "));
            inline(item, Style::default(), 0, &mut spans, &mut ctx);
        } else if body.len() <= MAX_INLINE_LINE {
            inline(body, Style::default(), 0, &mut spans, &mut ctx);
        } else if !body.is_empty() {
            spans.push(plain(body));
        }
        blocks.push(Block { kind, spans });
        at += 1;
    }
    Parsed {
        blocks,
        formatted: ctx.formatted,
    }
}

fn plain(text: &str) -> Span<'_> {
    Span {
        text,
        style: Style::default(),
        kind: Kind::Text,
        spoiler: None,
    }
}

fn heading(line: &str) -> Option<(BlockKind<'_>, &str)> {
    for (level, prefix) in [(1u8, "# "), (2, "## "), (3, "### ")] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return (!rest.is_empty()).then_some((BlockKind::Heading(level), rest));
        }
    }
    None
}

/// A fenced block opened on line `at`, when a later line closes it. Returns
/// the block and the index of the line after it.
fn fenced<'a>(
    source: &'a str,
    starts: &[usize],
    lines: &[&'a str],
    at: usize,
) -> Option<(Block<'a>, usize)> {
    let open = &lines[at][3..];
    if open.contains("```") {
        return None; // opened and closed on one line: inline code
    }
    let language = open.trim();
    let language = (!language.is_empty()
        && language.len() <= 32
        && language
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'#' | b'_')))
    .then_some(language);
    let (end, closer) = lines
        .iter()
        .enumerate()
        .skip(at + 1)
        .find_map(|(n, line)| line.find("```").map(|p| (n, p)))?;
    // A language word is not code; anything else after the fence is.
    let from = if language.is_some() || open.trim().is_empty() {
        starts[at + 1]
    } else {
        starts[at] + 3
    };
    let to = starts[end] + closer;
    let code = source.get(from..to.max(from))?;
    let code = code.trim_end_matches(['\n', '\r']);
    Some((
        Block {
            kind: BlockKind::Code(language),
            spans: vec![plain(code)],
        },
        end + 1,
    ))
}

/// Parses one line (or heading/quote body) into spans.
fn inline<'a>(line: &'a str, base: Style, depth: u8, out: &mut Vec<Span<'a>>, ctx: &mut Ctx) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut start = 0;
    let push_text = |out: &mut Vec<Span<'a>>, from: usize, to: usize, ctx: &Ctx| {
        let _ = ctx;
        if from < to {
            out.push(Span {
                text: &line[from..to],
                style: base,
                kind: Kind::Text,
                spoiler: None,
            });
        }
    };
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\\' if bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation) => {
                push_text(out, start, i, ctx);
                out.push(Span {
                    text: &line[i + 1..i + 2],
                    style: base,
                    kind: Kind::Text,
                    spoiler: None,
                });
                ctx.formatted = true;
                i += 2;
                start = i;
            }
            b'`' => {
                let run = run_len(bytes, i, b'`');
                if run <= 2
                    && let Some(close) = find_run(bytes, i + run, b'`', run)
                    && close > i + run
                {
                    push_text(out, start, i, ctx);
                    let inner = &line[i + run..close];
                    let inner = if inner.len() > 2
                        && inner.starts_with(' ')
                        && inner.ends_with(' ')
                        && !inner.trim().is_empty()
                    {
                        &inner[1..inner.len() - 1]
                    } else {
                        inner
                    };
                    out.push(Span {
                        text: inner,
                        style: base.with(Style::CODE),
                        kind: Kind::Text,
                        spoiler: None,
                    });
                    ctx.formatted = true;
                    i = close + run;
                    start = i;
                } else {
                    i += run;
                }
            }
            b'<' => {
                if let Some((len, kind, text)) = angle(&line[i..]) {
                    match kind {
                        Some(kind) => {
                            push_text(out, start, i, ctx);
                            out.push(Span {
                                text,
                                style: base,
                                kind,
                                spoiler: None,
                            });
                            ctx.formatted = true;
                            i += len;
                            start = i;
                        }
                        // A custom emoji token stays inside the text run.
                        None => i += len,
                    }
                } else {
                    i += 1;
                }
            }
            b'h' | b'@' if boundary(line, i) => {
                if let Some((len, kind)) = bare(&line[i..]) {
                    push_text(out, start, i, ctx);
                    let text = &line[i..i + len];
                    let kind = match kind {
                        Kind::Link(_) => Kind::Link(text),
                        other => other,
                    };
                    out.push(Span {
                        text,
                        style: base,
                        kind,
                        spoiler: None,
                    });
                    ctx.formatted = true;
                    i += len;
                    start = i;
                } else {
                    i += 1;
                }
            }
            b'[' => {
                if let Some((label_end, url, close)) = masked(line, i) {
                    push_text(out, start, i, ctx);
                    let from = out.len();
                    let label = &line[i + 1..label_end];
                    if depth < MAX_DEPTH {
                        inline(label, base, depth + 1, out, ctx);
                    } else {
                        out.push(Span {
                            text: label,
                            style: base,
                            kind: Kind::Text,
                            spoiler: None,
                        });
                    }
                    // Every part of the label, formatted or not, opens the
                    // target; links and mentions inside it are plain label text.
                    for span in &mut out[from..] {
                        span.kind = Kind::Link(&line[url.clone()]);
                    }
                    ctx.formatted = true;
                    i = close + 1;
                    start = i;
                } else {
                    i += 1;
                }
            }
            b'*' | b'_' | b'~' | b'|' if depth < MAX_DEPTH => {
                let run = run_len(bytes, i, b);
                if let Some((flag, width)) = delimiter(b, run)
                    && let Some(close) = closing(line, i, b, width)
                {
                    {
                        push_text(out, start, i, ctx);
                        let inner = &line[i + width..close];
                        let mut style = base;
                        let mut spoiler = None;
                        for f in flag {
                            style = style.with(*f);
                            if *f == Style::SPOILER {
                                spoiler = Some(ctx.spoilers);
                                ctx.spoilers = ctx.spoilers.saturating_add(1);
                            }
                        }
                        let from = out.len();
                        inline(inner, style, depth + 1, out, ctx);
                        if let Some(id) = spoiler {
                            for span in &mut out[from..] {
                                span.spoiler.get_or_insert(id);
                            }
                        }
                        ctx.formatted = true;
                        i = close + width;
                        start = i;
                        continue;
                    }
                }
                i += run;
            }
            _ => i += 1,
        }
    }
    push_text(out, start, bytes.len(), ctx);
}

fn run_len(bytes: &[u8], at: usize, byte: u8) -> usize {
    bytes[at..].iter().take_while(|&&b| b == byte).count()
}

/// Position of the next run of exactly `len` `byte`s at or after `from`.
fn find_run(bytes: &[u8], from: usize, byte: u8, len: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == byte {
            let run = run_len(bytes, i, byte);
            if run == len {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

fn boundary(line: &str, at: usize) -> bool {
    line[..at]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_alphanumeric())
}

/// The style flags and delimiter width for a run of `run` `byte`s.
fn delimiter(byte: u8, run: usize) -> Option<(&'static [Style], usize)> {
    Some(match (byte, run) {
        (b'*', 1) => (&[Style::ITALIC], 1),
        (b'*', 2) => (&[Style::BOLD], 2),
        (b'*', 3..) => (&[Style::BOLD, Style::ITALIC], 3),
        (b'_', 1) => (&[Style::ITALIC], 1),
        (b'_', 2) => (&[Style::UNDERLINE], 2),
        (b'_', 3..) => (&[Style::UNDERLINE, Style::ITALIC], 3),
        (b'~', 2..) => (&[Style::STRIKE], 2),
        (b'|', 2..) => (&[Style::SPOILER], 2),
        _ => return None,
    })
}

/// Where the emphasis opened at `at` (`width` bytes of `byte`) closes.
fn closing(line: &str, at: usize, byte: u8, width: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let inner_start = at + width;
    let opener_ok = line[inner_start..]
        .chars()
        .next()
        .is_some_and(|c| !c.is_whitespace());
    let intraword_sensitive = byte == b'_';
    if !opener_ok || (intraword_sensitive && !boundary(line, at)) {
        return None;
    }
    let mut i = inner_start;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'`' {
            // Emphasis markers inside inline code do not count.
            let run = run_len(bytes, i, b'`');
            match find_run(bytes, i + run, b'`', run) {
                Some(close) => i = close + run,
                None => i += run,
            }
            continue;
        }
        if bytes[i] == byte {
            let run = run_len(bytes, i, byte);
            if run >= width && i > inner_start {
                let close = i + run - width;
                let before_ok = line[..close]
                    .chars()
                    .next_back()
                    .is_some_and(|c| !c.is_whitespace() || byte == b'|' || byte == b'~');
                let after_ok = !intraword_sensitive
                    || line[close + width..]
                        .chars()
                        .next()
                        .is_none_or(|c| !c.is_alphanumeric());
                if before_ok && after_ok && close > inner_start {
                    return Some(close);
                }
            }
            i += run;
            continue;
        }
        i += 1;
    }
    None
}

/// `<@id>`, `<@!id>`, `<@&id>`, `<#id>`, `<http…>` and custom emoji tokens at
/// the start of `tail`. A `None` kind is a token that stays plain text.
fn angle(tail: &str) -> Option<(usize, Option<Kind<'_>>, &str)> {
    let end = tail.bytes().take(MAX_URL + 2).position(|b| b == b'>')?;
    let inner = &tail[1..end];
    let token = &tail[..=end];
    let id = |digits: &str| {
        (!digits.is_empty() && digits.len() <= 20 && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse::<u64>().ok().map(Snowflake))
            .flatten()
            .filter(|id| id.0 != 0)
    };
    if let Some(rest) = inner.strip_prefix("@&") {
        return Some((end + 1, Some(Kind::Role(id(rest)?)), token));
    }
    if let Some(rest) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')) {
        return Some((end + 1, Some(Kind::User(id(rest)?)), token));
    }
    if let Some(rest) = inner.strip_prefix('#') {
        return Some((end + 1, Some(Kind::Channel(id(rest)?)), token));
    }
    if let Some(rest) = inner.strip_prefix("t:") {
        let (digits, style) = rest.split_once(':').unwrap_or((rest, "f"));
        let style = match style {
            "t" => TimeStyle::ShortTime,
            "T" => TimeStyle::LongTime,
            "d" => TimeStyle::ShortDate,
            "D" => TimeStyle::LongDate,
            "f" => TimeStyle::ShortDateTime,
            "F" => TimeStyle::LongDateTime,
            "R" => TimeStyle::Relative,
            _ => return None,
        };
        let unsigned = digits.strip_prefix('-').unwrap_or(digits);
        if unsigned.is_empty() || !unsigned.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let unix = digits.parse().ok()?;
        return Some((end + 1, Some(Kind::Timestamp { unix, style }), token));
    }
    if inner.starts_with("http://") || inner.starts_with("https://") {
        return (!inner.contains(char::is_whitespace)).then_some((
            end + 1,
            Some(Kind::Link(inner)),
            inner,
        ));
    }
    if (inner.starts_with(':') || inner.starts_with("a:")) && end <= 58 {
        return Some((end + 1, None, token));
    }
    None
}

/// A bare URL, `@everyone` or `@here` at the start of `tail`.
fn bare(tail: &str) -> Option<(usize, Kind<'_>)> {
    if tail.starts_with("@everyone") {
        return word_end(tail, 9).then_some((9, Kind::Everyone));
    }
    if tail.starts_with("@here") {
        return word_end(tail, 5).then_some((5, Kind::Here));
    }
    if !(tail.starts_with("http://") || tail.starts_with("https://")) {
        return None;
    }
    let mut end = tail
        .bytes()
        .take(MAX_URL)
        .position(|b| b.is_ascii_whitespace() || b == b'<')
        .unwrap_or_else(|| tail.len().min(MAX_URL));
    while !tail.is_char_boundary(end) {
        end -= 1;
    }
    // Sentence punctuation after a link is not part of it.
    while let Some(last) = tail[..end].chars().next_back() {
        let unbalanced_paren =
            last == ')' && tail[..end].matches('(').count() < tail[..end].matches(')').count();
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' | ']') || unbalanced_paren
        {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    let scheme = if tail.starts_with("https") { 8 } else { 7 };
    (end > scheme).then_some((end, Kind::Link(&tail[..end])))
}

fn word_end(tail: &str, len: usize) -> bool {
    tail[len..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_alphanumeric() && c != '_')
}

/// `[label](http…)` or `[label](<http…>)` at `at`: the `]` position, the
/// target's byte range and the position of the closing `)`.
fn masked(line: &str, at: usize) -> Option<(usize, std::ops::Range<usize>, usize)> {
    let label_end = at + 1 + line[at + 1..].bytes().take(512).position(|b| b == b']')?;
    if label_end == at + 1 || !line[label_end..].starts_with("](") {
        return None;
    }
    let mut url_start = label_end + 2;
    let (url_end, close) = if line[url_start..].starts_with('<') {
        url_start += 1;
        let gt = url_start
            + line[url_start..]
                .bytes()
                .take(MAX_URL + 1)
                .position(|b| b == b'>')?;
        if !line[gt + 1..].starts_with(')') {
            return None;
        }
        (gt, gt + 1)
    } else {
        let end = url_start
            + line[url_start..]
                .bytes()
                .take(MAX_URL)
                .position(|b| b == b')')?;
        (end, end)
    };
    let url = &line[url_start..url_end];
    ((url.starts_with("https://") || url.starts_with("http://"))
        && !url.contains(char::is_whitespace))
    .then_some((label_end, url_start..url_end, close))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(source: &str) -> Vec<Span<'_>> {
        parse(source)
            .blocks
            .into_iter()
            .flat_map(|b| b.spans)
            .collect()
    }

    fn texts(source: &str, style: Style) -> Vec<&str> {
        spans(source)
            .into_iter()
            .filter(|s| s.style.has(style))
            .map(|s| s.text)
            .collect()
    }

    #[test]
    fn plain_lines_are_not_formatted_and_keep_every_byte() {
        for source in [
            "",
            "hello world",
            "a\nb\n\nc",
            "2 * 3 * 4 and snake_case_name",
            "a | b",
        ] {
            let parsed = parse(source);
            assert!(!parsed.formatted, "{source:?}");
            let joined: Vec<String> = parsed
                .blocks
                .iter()
                .map(|b| b.spans.iter().map(|s| s.text).collect())
                .collect();
            assert_eq!(joined.join("\n"), source);
        }
    }

    #[test]
    fn emphasis_nests_and_combines() {
        assert_eq!(texts("a **bold** b", Style::BOLD), ["bold"]);
        assert_eq!(texts("*it* and _it2_", Style::ITALIC), ["it", "it2"]);
        assert_eq!(texts("__under__ ~~gone~~", Style::UNDERLINE), ["under"]);
        assert_eq!(texts("~~gone~~", Style::STRIKE), ["gone"]);
        let nested = spans("**bold *both***");
        assert!(
            nested.iter().any(|s| s.text == "both"
                && s.style.has(Style::BOLD)
                && s.style.has(Style::ITALIC))
        );
        assert_eq!(texts("***both***", Style::BOLD), ["both"]);
    }

    #[test]
    fn unmatched_or_spaced_delimiters_stay_text() {
        assert!(!parse("**open only").formatted);
        assert!(!parse("a * b * c").formatted);
        assert!(!parse("snake_case_here").formatted);
        assert!(!parse("**").formatted);
    }

    #[test]
    fn code_is_literal_and_escapes_apply() {
        assert_eq!(texts("`a **b** c`", Style::CODE), ["a **b** c"]);
        assert_eq!(texts("``a ` b``", Style::CODE), ["a ` b"]);
        let escaped = spans(r"\*not italic\*");
        assert_eq!(
            escaped.iter().map(|s| s.text).collect::<String>(),
            "*not italic*"
        );
        assert!(escaped.iter().all(|s| s.style.is_plain()));
        // Markers inside code do not close emphasis around it.
        assert_eq!(texts("**a `**` b**", Style::BOLD).concat(), "a ** b");
    }

    #[test]
    fn fenced_blocks_carry_language_and_raw_text() {
        let parsed = parse("before\n```rust\nlet a = **1**;\nlet b;\n```\nafter");
        assert_eq!(parsed.blocks.len(), 3);
        assert_eq!(parsed.blocks[1].kind, BlockKind::Code(Some("rust")));
        assert_eq!(parsed.blocks[1].spans[0].text, "let a = **1**;\nlet b;");
        assert_eq!(parsed.blocks[2].spans[0].text, "after");
        // No closing fence: ordinary lines.
        let open = parse("```\nx");
        assert!(open.blocks.iter().all(|b| b.kind == BlockKind::Paragraph));
        // One-line triple backticks are inline code.
        assert!(
            parse("```x```").blocks[0].spans[0].style.has(Style::CODE)
                || !parse("```x```").formatted
        );
    }

    #[test]
    fn quotes_headings_lists_and_subtext() {
        let parsed = parse("> quoted\n# Title\n-# small\n- item\nplain");
        let kinds: Vec<_> = parsed.blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            [
                BlockKind::Quote,
                BlockKind::Heading(1),
                BlockKind::Subtext,
                BlockKind::Paragraph,
                BlockKind::Paragraph
            ]
        );
        assert_eq!(parsed.blocks[3].spans[0].text, "\u{2022} ");
        let all = parse(">>> one\ntwo");
        assert!(all.blocks.iter().all(|b| b.kind == BlockKind::Quote));
        assert_eq!(all.blocks.len(), 2);
        assert!(!parse("#hashtag and >not quote").formatted);
    }

    #[test]
    fn spoilers_get_distinct_numbers() {
        let found = spans("||one|| and ||two **b**||");
        let ids: Vec<_> = found.iter().filter_map(|s| s.spoiler).collect();
        assert_eq!(ids, [0, 1, 1]);
        assert!(
            found
                .iter()
                .filter(|s| s.spoiler.is_some())
                .all(|s| s.style.has(Style::SPOILER))
        );
        assert!(!parse("a || b").formatted);
    }

    #[test]
    fn links_and_mentions_are_recognized() {
        let found =
            spans("see https://example.com/a?b=1. and <https://x.test> [site](https://y.test/z)");
        let links: Vec<_> = found
            .iter()
            .filter_map(|s| match s.kind {
                Kind::Link(url) => Some((s.text, url)),
                _ => None,
            })
            .collect();
        assert_eq!(
            links,
            [
                ("https://example.com/a?b=1", "https://example.com/a?b=1"),
                ("https://x.test", "https://x.test"),
                ("site", "https://y.test/z"),
            ]
        );
        let found = spans("<@1> <@!2> <@&3> <#4> @everyone @here @heresy");
        let kinds: Vec<_> = found
            .iter()
            .map(|s| s.kind)
            .filter(|k| *k != Kind::Text)
            .collect();
        assert_eq!(
            kinds,
            [
                Kind::User(Snowflake(1)),
                Kind::User(Snowflake(2)),
                Kind::Role(Snowflake(3)),
                Kind::Channel(Snowflake(4)),
                Kind::Everyone,
                Kind::Here,
            ]
        );
        let found = spans("<t:1618935630> <t:-5:R> <t:7:t><t:8:T><t:9:d><t:10:D><t:11:F>");
        let stamps: Vec<_> = found
            .iter()
            .filter_map(|s| match s.kind {
                Kind::Timestamp { unix, style } => Some((unix, style)),
                _ => None,
            })
            .collect();
        assert_eq!(
            stamps,
            [
                (1_618_935_630, TimeStyle::ShortDateTime),
                (-5, TimeStyle::Relative),
                (7, TimeStyle::ShortTime),
                (8, TimeStyle::LongTime),
                (9, TimeStyle::ShortDate),
                (10, TimeStyle::LongDate),
                (11, TimeStyle::LongDateTime),
            ]
        );
        assert_eq!(found[0].text, "<t:1618935630>");
        for bad in [
            "<t:abc>",
            "<t:1:x>",
            "<t:>",
            "<t:->",
            "<t:1:>",
            "<t:1:f:f>",
            "<t:99999999999999999999>",
        ] {
            assert!(spans(bad).iter().all(|s| s.kind == Kind::Text), "{bad}");
        }
        // Malformed ids, non-HTTP masked links and javascript: targets are text.
        for bad in [
            "<@abc>",
            "<@0>",
            "[x](javascript:alert(1))",
            "[x](ftp://a)",
            "<@99999999999999999999999>",
        ] {
            assert!(spans(bad).iter().all(|s| s.kind == Kind::Text), "{bad}");
        }
    }

    fn pairs(source: &str) -> Vec<(&str, Option<&str>, bool)> {
        spans(source)
            .into_iter()
            .map(|s| {
                let url = match s.kind {
                    Kind::Link(u) => Some(u),
                    _ => None,
                };
                (s.text, url, s.style.has(Style::BOLD))
            })
            .collect()
    }

    #[test]
    fn masked_links_accept_angle_targets_and_formatted_labels() {
        assert_eq!(
            pairs("[a](<https://x.test/y>)"),
            [("a", Some("https://x.test/y"), false)]
        );
        assert_eq!(
            pairs("([change log](<https://d.test/c#s>))"),
            [
                ("(", None, false),
                ("change log", Some("https://d.test/c#s"), false),
                (")", None, false),
            ]
        );
        assert_eq!(
            pairs("[x **b** y](<https://x.test>)"),
            [
                ("x ", Some("https://x.test"), false),
                ("b", Some("https://x.test"), true),
                (" y", Some("https://x.test"), false),
            ]
        );
        // Mentions and links inside a label are plain label text.
        assert_eq!(
            pairs("[<@1>](https://x.test)"),
            [("<@1>", Some("https://x.test"), false)]
        );
        for bad in ["[x](<javascript:alert(1)>)", "[x](<ftp://a>)"] {
            assert!(spans(bad).iter().all(|s| s.kind == Kind::Text), "{bad}");
        }
        // Unterminated: the label is not a link (a bare URL may still be).
        let open = spans("[x](<https://a");
        assert_eq!((open[0].text, open[0].kind), ("[x](<", Kind::Text));
    }

    #[test]
    fn custom_emoji_names_do_not_start_emphasis() {
        let source = "<:my_emoji_name:123> text <a:_x_:9>";
        assert!(!parse(source).formatted);
    }

    #[test]
    fn bounds_hold_for_hostile_input() {
        let stars = "*".repeat(MAX_INLINE_LINE + 10);
        assert!(!parse(&stars).formatted);
        let many = "> q\n".repeat(MAX_BLOCKS * 2);
        let parsed = parse(&many);
        assert_eq!(parsed.blocks.len(), MAX_BLOCKS + 1);
        let deep = "**".repeat(100) + "x" + &"**".repeat(100);
        let _ = parse(&deep);
        let weird = "\u{1F600}**é**\u{1F600}__|\u{e9}|__";
        let _ = parse(weird);
    }
}
