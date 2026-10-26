//! Source-preserving inline emoji, using the versioned Unicode index and
//! attributed Twemoji PNGs. Only visible assets are decoded; handles share pixels.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fastcord_model::{Emoji, Snowflake};
use iced::task::Handle;
use iced::widget::{column, image, row, sensor, text};
use iced::{Element, Length, Task};
use unicode_segmentation::UnicodeSegmentation;

use crate::Message;
use crate::attachments::{AttachmentCache, DecodedAnimation};

#[path = "emoji_index.rs"]
mod index;

pub struct Entry {
    pub text: &'static str,
    pub name: &'static str,
    pub aliases: &'static str,
    pub category: &'static str,
    pub base: u16,
    pub offset: usize,
    pub len: usize,
}

pub fn entries() -> &'static [Entry] {
    index::ENTRIES
}

pub fn unicode(sequence: &str) -> Option<u16> {
    index::LOOKUP
        .binary_search_by(|(text, _)| text.cmp(&sequence))
        .ok()
        .map(|at| index::LOOKUP[at].1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Asset {
    Unicode(u16),
    Custom { id: Snowflake, animated: bool },
}

impl Asset {
    async fn load(self, cache: AttachmentCache) -> Result<DecodedAnimation, &'static str> {
        let bytes = match self {
            Self::Unicode(id) => {
                let entry = index::ENTRIES
                    .get(usize::from(id))
                    .ok_or_else(|| crate::i18n::tr("Emoji is unavailable."))?;
                bytes::Bytes::from_static(&index::PNGS[entry.offset..entry.offset + entry.len])
            }
            Self::Custom { id, animated } => {
                cache.custom_emoji(id, animated).await.map_err(|_| {
                    crate::i18n::tr(
                        "Custom emoji could not be loaded; its original token is preserved.",
                    )
                })?
            }
        };
        cache
            .decode_emoji(bytes)
            .await
            .map_err(|_| crate::i18n::tr("Emoji could not be decoded; original text is preserved."))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Run<'a> {
    Text(&'a str),
    Emoji { source: &'a str, asset: Asset },
}

fn custom(source: &str) -> Option<(usize, Asset)> {
    let (prefix, animated) = if source.starts_with("<a:") {
        (3, true)
    } else if source.starts_with("<:") {
        (2, false)
    } else {
        return None;
    };
    // Bound even malformed-token scanning: 32 name bytes, 20 ID digits.
    let end = source.as_bytes().iter().take(58).position(|&b| b == b'>')?;
    let (name, id) = source.get(prefix..end)?.split_once(':')?;
    if !(2..=32).contains(&name.len())
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    let id = id.parse::<Snowflake>().ok()?;
    (id.0 != 0).then_some((end + 1, Asset::Custom { id, animated }))
}

/// Source-preserving, allocation-free grapheme runs. Unknown/new sequences,
/// text variation selectors and malformed custom tokens remain complete text.
pub fn runs(source: &str) -> Runs<'_> {
    Runs { source, at: 0 }
}

pub struct Runs<'a> {
    source: &'a str,
    at: usize,
}

impl<'a> Iterator for Runs<'a> {
    type Item = Run<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let plain = self.at;
        while self.at < self.source.len() {
            let tail = &self.source[self.at..];
            let grapheme = tail.graphemes(true).next()?;
            let recognized = custom(tail)
                .or_else(|| unicode(grapheme).map(|id| (grapheme.len(), Asset::Unicode(id))));
            if let Some((len, asset)) = recognized {
                if plain < self.at {
                    return Some(Run::Text(&self.source[plain..self.at]));
                }
                let source = &self.source[self.at..self.at + len];
                self.at += len;
                return Some(Run::Emoji { source, asset });
            }
            self.at += grapheme.len();
        }
        (plain < self.at).then(|| Run::Text(&self.source[plain..self.at]))
    }
}

/// A sensor belongs to a specific occurrence, not just its shared asset.
/// Two rows using the same animation cannot hide each other's frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    Timeline {
        channel: Snowflake,
        message: Snowflake,
        revision: u64,
        run: u16,
    },
    /// The `chip`th reaction chip of a message's row.
    Reaction {
        channel: Snowflake,
        message: Snowflake,
        revision: u64,
        chip: u8,
    },
    Picker {
        revision: u64,
    },
}

/// The image a reaction's emoji is drawn from: a custom emoji by its ID, a
/// Unicode emoji by the bundled index. Unicode text the index does not know
/// (a sequence newer than the bundle) has none and is drawn as text.
pub fn reaction_asset(emoji: &Emoji) -> Option<Asset> {
    match emoji.id {
        Some(id) => Some(Asset::Custom {
            id,
            animated: emoji.animated,
        }),
        None => emoji.name.as_deref().and_then(unicode).map(Asset::Unicode),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Occurrence {
    pub origin: Origin,
    pub asset: Asset,
}

pub const MAX_INLINE_ASSETS: usize = 128;
const MAX_OCCURRENCES: usize = 512;
const MAX_ASSETS: usize = 128;
/// Decoded pixels, including retained animation frames, for timeline emoji.
const VISIBLE_BYTES: usize = 2 * 1024 * 1024;
/// Picker cells are budgeted in addition, so an open picker neither evicts
/// still-visible timeline emoji nor is starved by a full timeline cache.
const PICKER_BYTES: usize = 2 * 1024 * 1024;
const WORKERS: usize = 4;

pub struct View {
    frames: Arc<[image::Handle]>,
    delays: Arc<[Duration]>,
    bytes: usize,
    /// Which budget admitted this view; fixed so each budget stays bounded.
    picker: bool,
    /// Every occurrence plays from here, so copies of one emoji stay in step.
    start: Instant,
}

impl View {
    pub(crate) fn from_decoded(animation: DecodedAnimation) -> Self {
        let bytes = animation.frames.iter().map(|frame| frame.rgba.len()).sum();
        let frames = animation
            .frames
            .into_iter()
            .map(|frame| image::Handle::from_rgba(frame.width, frame.height, frame.rgba))
            .collect();
        Self {
            frames,
            delays: animation.delays.into(),
            bytes,
            picker: false,
            start: Instant::now(),
        }
    }
}

/// The frame shown `elapsed` into the loop and how long it stays; `None`
/// for a still image.
fn frame_at(delays: &[Duration], elapsed: Duration) -> (usize, Option<Duration>) {
    let cycle: Duration = delays.iter().sum();
    if delays.len() < 2 || cycle.is_zero() {
        return (0, None);
    }
    let mut at = Duration::from_nanos((elapsed.as_nanos() % cycle.as_nanos()) as u64);
    for (index, delay) in delays.iter().enumerate() {
        if at < *delay {
            return (index, Some(*delay - at));
        }
        at -= *delay;
    }
    (0, Some(delays[0]))
}

/// Draws one emoji and animates it by itself: on each redraw it picks the
/// due frame and asks for the next redraw when that frame ends. A frame
/// change costs one redraw, with no message, `update` or `view` (#155).
struct Animated {
    frames: Arc<[image::Handle]>,
    delays: Arc<[Duration]>,
    start: Instant,
    size: f32,
}

impl<M> iced::advanced::Widget<M, iced::Theme, iced::Renderer> for Animated {
    fn size(&self) -> iced::Size<Length> {
        iced::Size::new(Length::Fixed(self.size), Length::Fixed(self.size))
    }

    fn tag(&self) -> iced::advanced::widget::tree::Tag {
        iced::advanced::widget::tree::Tag::of::<usize>()
    }

    fn state(&self) -> iced::advanced::widget::tree::State {
        iced::advanced::widget::tree::State::new(0_usize)
    }

    fn layout(
        &mut self,
        _tree: &mut iced::advanced::widget::Tree,
        _renderer: &iced::Renderer,
        limits: &iced::advanced::layout::Limits,
    ) -> iced::advanced::layout::Node {
        iced::advanced::layout::atomic(limits, self.size, self.size)
    }

    fn update(
        &mut self,
        tree: &mut iced::advanced::widget::Tree,
        event: &iced::Event,
        layout: iced::advanced::Layout<'_>,
        _cursor: iced::mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn iced::advanced::Clipboard,
        shell: &mut iced::advanced::Shell<'_, M>,
        viewport: &iced::Rectangle,
    ) {
        let iced::Event::Window(iced::window::Event::RedrawRequested(now)) = event else {
            return;
        };
        // Reduced motion (or autoplay off) shows the first frame.
        if !crate::chat_prefs::current().animated() {
            *tree.state.downcast_mut::<usize>() = 0;
            return;
        }
        let (frame, left) = frame_at(&self.delays, now.saturating_duration_since(self.start));
        *tree.state.downcast_mut::<usize>() = frame;
        // Off screen (overscan rows) it keeps its frame but wakes nothing.
        if let Some(left) = left
            && layout.bounds().intersects(viewport)
        {
            shell.request_redraw_at(*now + left);
        }
    }

    fn draw(
        &self,
        tree: &iced::advanced::widget::Tree,
        renderer: &mut iced::Renderer,
        _theme: &iced::Theme,
        _style: &iced::advanced::renderer::Style,
        layout: iced::advanced::Layout<'_>,
        _cursor: iced::mouse::Cursor,
        _viewport: &iced::Rectangle,
    ) {
        let Some(handle) = self.frames.get(*tree.state.downcast_ref::<usize>()) else {
            return;
        };
        iced::widget::image::draw(
            renderer,
            layout,
            handle,
            None,
            iced::border::Radius::default(),
            iced::ContentFit::Contain,
            image::FilterMethod::default(),
            iced::Rotation::default(),
            1.0,
            1.0,
        );
    }
}

#[derive(Default)]
pub struct State {
    pub views: HashMap<Asset, View>,
    workers: HashMap<Asset, (u64, Handle)>,
    failed: HashSet<Asset>,
    // Timeline and picker assets wanted right now, indexed like `visible`.
    desired: [Vec<Asset>; 2],
    active: bool,
    // Separate bounded interests let the picker displace a full timeline cache
    // without losing the timeline's visibility when the picker closes.
    visible: [HashSet<Occurrence>; 2],
    next_worker: u64,
}

impl State {
    pub fn visibility(&mut self, occurrence: Occurrence, visible: bool) {
        let picker = matches!(occurrence.origin, Origin::Picker { .. });
        let interests = &mut self.visible[usize::from(picker)];
        let limit = if picker { MAX_ASSETS } else { MAX_OCCURRENCES };
        if visible && interests.len() < limit {
            interests.insert(occurrence);
        } else if !visible {
            interests.remove(&occurrence);
        }
    }

    /// Widget removal does not produce a hide event; prune removed/replaced
    /// timeline rows and picker pages when their bounded snapshots change.
    pub fn retain(&mut self, mut valid: impl FnMut(&Occurrence) -> bool) {
        for interests in &mut self.visible {
            interests.retain(|occurrence| valid(occurrence));
        }
    }

    pub fn synchronize(
        &mut self,
        active: bool,
        picker: bool,
        account: u64,
        cache: Option<&AttachmentCache>,
    ) -> Task<Message> {
        self.active = active;
        for (pool, desired) in self.desired.iter_mut().enumerate() {
            desired.clear();
            if !active || (pool == 1 && !picker) {
                continue;
            }
            let mut seen = HashSet::new();
            for occurrence in &self.visible[pool] {
                if desired.len() == MAX_ASSETS {
                    break;
                }
                if seen.insert(occurrence.asset) {
                    desired.push(occurrence.asset);
                }
            }
        }
        let desired = &self.desired;
        let wanted = |asset: &Asset| desired.iter().any(|pool| pool.contains(asset));
        self.views.retain(|key, _| wanted(key));
        self.workers.retain(|key, _| wanted(key));
        self.failed.retain(wanted);
        self.start(account, cache)
    }

    fn wants(&self, asset: &Asset) -> bool {
        self.desired.iter().any(|pool| pool.contains(asset))
    }

    fn start(&mut self, account: u64, cache: Option<&AttachmentCache>) -> Task<Message> {
        let Some(cache) = cache else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        for &asset in self.desired[1].iter().chain(&self.desired[0]) {
            if self.workers.len() == WORKERS {
                break;
            }
            if self.views.contains_key(&asset)
                || self.workers.contains_key(&asset)
                || self.failed.contains(&asset)
            {
                continue;
            }
            let cache = cache.clone();
            self.next_worker = self.next_worker.wrapping_add(1);
            let ticket = self.next_worker;
            let (task, handle) = Task::perform(asset.load(cache), move |result| {
                Message::EmojiLoaded(account, asset, ticket, result)
            })
            .abortable();
            self.workers.insert(asset, (ticket, handle.abort_on_drop()));
            tasks.push(task);
        }
        Task::batch(tasks)
    }

    pub fn loaded(
        &mut self,
        account: u64,
        asset: Asset,
        ticket: u64,
        result: Result<DecodedAnimation, &'static str>,
        cache: Option<&AttachmentCache>,
    ) -> Task<Message> {
        if self
            .workers
            .get(&asset)
            .is_none_or(|(current, _)| *current != ticket)
            || !self.wants(&asset)
        {
            return Task::none();
        }
        self.workers.remove(&asset);
        match result {
            Ok(animation) => {
                let mut view = View::from_decoded(animation);
                view.picker = !self.desired[0].contains(&asset);
                let limit = if view.picker {
                    PICKER_BYTES
                } else {
                    VISIBLE_BYTES
                };
                let used: usize = self
                    .views
                    .values()
                    .filter(|other| other.picker == view.picker)
                    .map(|other| other.bytes)
                    .sum();
                if used + view.bytes <= limit {
                    self.views.insert(asset, view);
                } else {
                    self.failed.insert(asset);
                }
            }
            Err(_) => {
                self.failed.insert(asset);
            }
        }
        self.start(account, cache)
    }

    #[cfg(test)]
    pub fn animating(&self) -> bool {
        self.active
            && crate::chat_prefs::current().animated()
            && self.views.values().any(|view| view.frames.len() > 1)
    }
}

pub fn asset_view<'a, M: 'a>(
    asset: Asset,
    fallback: &'a str,
    views: &HashMap<Asset, View>,
    size: f32,
) -> Element<'a, M> {
    if let Some(view) = views.get(&asset) {
        Element::new(Animated {
            frames: view.frames.clone(),
            delays: view.delays.clone(),
            start: view.start,
            size,
        })
    } else {
        text(fallback).size(size.min(16.0)).into()
    }
}

pub fn sensed<'a, M: Clone + 'a>(
    content: Element<'a, M>,
    occurrence: Occurrence,
    on_visibility: impl Fn(Occurrence, bool) -> M + 'a,
) -> Element<'a, M> {
    let hidden = on_visibility(occurrence, false);
    sensor(content)
        .key(occurrence)
        .on_show(move |_| on_visibility(occurrence, true))
        .on_hide(hidden)
        .into()
}

/// Native wrapping text/image runs. Sensors report actual clipped visibility,
/// not the timeline's overscan window. Oversized rows retain a text tail.
pub fn inline<'a, M: Clone + 'a>(
    source: &'a str,
    views: &HashMap<Asset, View>,
    origin: impl Fn(u16) -> Origin,
    on_visibility: impl Fn(Occurrence, bool) -> M + Clone + 'a,
) -> Element<'a, M> {
    if matches!(runs(source).next(), Some(Run::Text(plain)) if plain.len() == source.len()) {
        return text(source)
            .size(14)
            .width(Length::Fill)
            .wrapping(text::Wrapping::WordOrGlyph)
            .into();
    }
    let mut emoji_count = 0;
    let jumbo = runs(source).all(|run| match run {
        Run::Text(s) => s.trim().is_empty(),
        Run::Emoji { .. } => {
            emoji_count += 1;
            emoji_count <= 30
        }
    }) && emoji_count > 0;
    let size = if jumbo { 32.0 } else { 22.0 };
    let mut lines = column![].spacing(0);
    let new_line = || {
        row![]
            .spacing(0)
            .align_y(iced::alignment::Vertical::Center)
            .width(Length::Fill)
    };
    let mut line = new_line();
    let mut parsed = runs(source);
    let mut assets = 0;
    while let Some(run) = parsed.next() {
        match run {
            Run::Emoji {
                source: token,
                asset,
            } => {
                if assets == MAX_INLINE_ASSETS {
                    line = line.push(
                        text(&source[parsed.at - token.len()..])
                            .size(14)
                            .wrapping(text::Wrapping::WordOrGlyph),
                    );
                    break;
                }
                let occurrence = Occurrence {
                    origin: origin(assets as u16),
                    asset,
                };
                line = line.push(sensed(
                    asset_view(asset, token, views, size),
                    occurrence,
                    on_visibility.clone(),
                ));
                assets += 1;
            }
            Run::Text(source) => {
                for (n, piece) in source.split('\n').enumerate() {
                    if n > 0 {
                        lines = lines.push(line.wrap());
                        line = new_line();
                    }
                    for word in piece.split_inclusive(char::is_whitespace) {
                        line = line.push(text(word).size(14).wrapping(text::Wrapping::WordOrGlyph));
                    }
                }
            }
        }
    }
    lines.push(line.wrap()).width(Length::Fill).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skin_zwj_flags_and_selectors_preserve_source() {
        let source = "a 👩🏽‍💻 👍🏻 🇺🇦 ❤️ #️⃣ end";
        let parsed: Vec<_> = runs(source).collect();
        let restored: String = parsed
            .iter()
            .map(|run| match run {
                Run::Text(s) | Run::Emoji { source: s, .. } => *s,
            })
            .collect();
        assert_eq!(restored, source);
        assert_eq!(
            parsed
                .iter()
                .filter(|r| matches!(r, Run::Emoji { .. }))
                .count(),
            5
        );
        assert!(matches!(
            runs("❤\u{fe0e}").collect::<Vec<_>>().as_slice(),
            [Run::Text(_)]
        ));
        // GB11 joins only extended pictographs through ZWJ. The ASCII prefix
        // is text, while the independent fox grapheme remains recognizable.
        assert!(matches!(
            runs("x\u{200d}🦊").collect::<Vec<_>>().as_slice(),
            [Run::Text("x\u{200d}"), Run::Emoji { source: "🦊", .. }]
        ));
    }
    #[test]
    fn custom_tokens_keep_static_and_animated_identity() {
        let parsed: Vec<_> = runs("<:sample:12345> <a:party:45678> <:bad-name:12345>").collect();
        assert!(matches!(
            parsed[0],
            Run::Emoji {
                asset: Asset::Custom {
                    animated: false,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            parsed[2],
            Run::Emoji {
                asset: Asset::Custom { animated: true, .. },
                ..
            }
        ));
        assert!(matches!(parsed[3], Run::Text(_)));
    }
    #[test]
    fn inactive_state_admits_no_frames_and_does_not_animate() {
        let mut state = State::default();
        let asset = custom_asset(1);
        state.visibility(occurrence(asset, 1), true);
        let _ = state.synchronize(false, false, 1, None);
        load(&mut state, asset, 1, 2);
        assert!(state.views.is_empty());
        assert!(!state.animating());
    }

    #[test]
    fn the_versioned_index_is_complete_sorted_and_asset_ranges_are_valid() {
        assert!(index::LOOKUP.windows(2).all(|pair| pair[0].0 < pair[1].0));
        for (id, entry) in entries().iter().enumerate() {
            assert_eq!(unicode(entry.text), Some(id as u16));
            assert!(matches!(
                runs(entry.text).collect::<Vec<_>>().as_slice(),
                [Run::Emoji { .. }]
            ));
            assert!(usize::from(entry.base) <= id);
            let png = &index::PNGS[entry.offset..entry.offset + entry.len];
            assert_eq!(
                ::image::guess_format(png).unwrap(),
                ::image::ImageFormat::Png
            );
        }
        for source in [
            "❤\u{fe0e}",
            "🛸‍🪩",
            "<a:x:1>",
            "<:name:0>",
            "<:bad-name:12345>",
            "<:name:18446744073709551616>",
        ] {
            assert_eq!(runs(source).collect::<Vec<_>>(), [Run::Text(source)]);
        }
    }

    fn occurrence(asset: Asset, message: u64) -> Occurrence {
        Occurrence {
            asset,
            origin: Origin::Timeline {
                channel: Snowflake(1),
                message: Snowflake(message),
                revision: 1,
                run: 0,
            },
        }
    }

    fn animation(frames: usize) -> DecodedAnimation {
        let frame = crate::attachments::DecodedImage {
            width: 32,
            height: 32,
            rgba: vec![0x80; 32 * 32 * 4].into(),
        };
        DecodedAnimation {
            frames: vec![frame; frames],
            delays: vec![Duration::from_millis(20); frames],
        }
    }

    fn custom_asset(id: u64) -> Asset {
        Asset::Custom {
            id: Snowflake(id),
            animated: true,
        }
    }

    fn picker_occurrence(asset: Asset) -> Occurrence {
        Occurrence {
            asset,
            origin: Origin::Picker { revision: 1 },
        }
    }

    /// Delivers a decoded animation as the worker for `ticket` would.
    fn load(state: &mut State, asset: Asset, ticket: u64, frames: usize) {
        state.workers.insert(
            asset,
            (
                ticket,
                Task::<Message>::none().abortable().1.abort_on_drop(),
            ),
        );
        let _ = state.loaded(1, asset, ticket, Ok(animation(frames)), None);
    }

    fn decoded_bytes(state: &State) -> usize {
        state.views.values().map(|view| view.bytes).sum()
    }

    #[test]
    fn the_frame_follows_elapsed_time_and_wakes_at_its_end() {
        let delays = [Duration::from_millis(100), Duration::from_millis(50)];
        let ms = Duration::from_millis;
        assert_eq!(frame_at(&delays, ms(0)), (0, Some(ms(100))));
        assert_eq!(frame_at(&delays, ms(99)), (0, Some(ms(1))));
        assert_eq!(frame_at(&delays, ms(100)), (1, Some(ms(50))));
        assert_eq!(frame_at(&delays, ms(170)), (0, Some(ms(80))), "loops");
        assert_eq!(
            frame_at(&delays[..1], ms(170)),
            (0, None),
            "a still image never wakes"
        );
        assert_eq!(frame_at(&[Duration::ZERO; 2], ms(5)), (0, None));
    }

    #[test]
    fn animations_share_occurrences_and_stop_when_hidden_or_inactive() {
        let asset = custom_asset(5);
        let mut state = State::default();
        let first = occurrence(asset, 1);
        let second = occurrence(asset, 2);
        state.visibility(first, true);
        state.visibility(second, true);
        let _ = state.synchronize(true, false, 1, None);
        state.views.insert(asset, View::from_decoded(animation(2)));
        assert!(state.animating());
        state.visibility(first, false);
        let _ = state.synchronize(true, false, 1, None);
        assert!(
            state.animating(),
            "the second occurrence still owns the shared asset"
        );
        let _ = state.synchronize(false, false, 1, None);
        assert!(!state.animating());
        assert!(state.views.is_empty());
        let _ = state.synchronize(true, false, 1, None);
        load(&mut state, asset, 1, 2);
        assert!(
            state.animating(),
            "focus/overlay restoration does not need another sensor event"
        );
        state.visibility(second, false);
        let _ = state.synchronize(true, false, 1, None);
        assert!(!state.animating());
        assert!(state.views.is_empty());
    }

    #[test]
    fn opening_the_picker_keeps_visible_timeline_views_and_budgets_cells_in_addition() {
        let mut state = State::default();
        let timeline: Vec<_> = (1..=4).map(custom_asset).collect();
        for (message, &asset) in (1..).zip(&timeline) {
            state.visibility(occurrence(asset, message), true);
        }
        let _ = state.synchronize(true, false, 1, None);
        for (ticket, &asset) in (1..).zip(&timeline) {
            load(&mut state, asset, ticket, 120);
        }
        let timeline_bytes = decoded_bytes(&state);
        assert!(timeline_bytes <= VISIBLE_BYTES);
        assert!(
            timeline_bytes + 120 * 32 * 32 * 4 > VISIBLE_BYTES,
            "the timeline budget is full"
        );

        let cells: Vec<_> = (11..=15).map(custom_asset).collect();
        for &asset in &cells {
            state.visibility(picker_occurrence(asset), true);
        }
        let _ = state.synchronize(true, true, 1, None);
        assert!(timeline.iter().all(|asset| state.views.contains_key(asset)));
        assert!(state.animating());
        for (ticket, &asset) in (10..).zip(&cells) {
            load(&mut state, asset, ticket, 120);
        }
        assert!(
            cells[..4]
                .iter()
                .all(|asset| state.views.contains_key(asset)),
            "a full timeline cache does not starve the picker"
        );
        assert!(
            !state.views.contains_key(&cells[4]),
            "picker cells are bounded"
        );
        assert!(timeline.iter().all(|asset| state.views.contains_key(asset)));
        assert!(decoded_bytes(&state) <= VISIBLE_BYTES + PICKER_BYTES);

        let _ = state.synchronize(true, false, 1, None);
        assert!(timeline.iter().all(|asset| state.views.contains_key(asset)));
        assert!(cells.iter().all(|asset| !state.views.contains_key(asset)));
        assert_eq!(decoded_bytes(&state), timeline_bytes);
    }

    #[test]
    fn a_saturated_timeline_visibility_window_does_not_starve_the_picker() {
        let mut state = State::default();
        let timeline_asset = custom_asset(1);
        for message in 0..MAX_OCCURRENCES as u64 + 1 {
            state.visibility(occurrence(timeline_asset, message), true);
        }
        let picker_asset = custom_asset(2);
        state.visibility(picker_occurrence(picker_asset), true);
        let _ = state.synchronize(true, true, 1, None);
        load(&mut state, timeline_asset, 1, 2);
        load(&mut state, picker_asset, 2, 2);
        assert!(state.views.contains_key(&timeline_asset));
        assert!(state.views.contains_key(&picker_asset));
        state.retain(|occurrence| matches!(occurrence.origin, Origin::Picker { .. }));
        let _ = state.synchronize(true, true, 1, None);
        assert!(!state.views.contains_key(&timeline_asset));
        assert!(state.views.contains_key(&picker_asset));
        let _ = state.synchronize(true, false, 1, None);
        assert!(state.views.is_empty());
        assert!(!state.animating());
    }

    #[test]
    fn decoded_frames_are_byte_bounded_and_old_worker_completions_cannot_replace_new_work() {
        let mut state = State::default();
        for id in 1..=5 {
            state.visibility(occurrence(custom_asset(id), id), true);
        }
        let _ = state.synchronize(true, false, 1, None);
        for id in 1..=5 {
            load(&mut state, custom_asset(id), id, 120);
        }
        let one = 120 * 32 * 32 * 4;
        let total = decoded_bytes(&state);
        assert!(total <= VISIBLE_BYTES);
        assert!(
            total + one > VISIBLE_BYTES,
            "the budget is used, not wasted"
        );

        let asset = Asset::Unicode(0);
        state.visibility(occurrence(asset, 99), true);
        let _ = state.synchronize(true, false, 1, None);
        state.workers.insert(
            asset,
            (99, Task::<Message>::none().abortable().1.abort_on_drop()),
        );
        let _ = state.loaded(1, asset, 98, Ok(animation(1)), None);
        assert!(
            !state.views.contains_key(&asset),
            "a superseded worker's result is discarded"
        );
        let _ = state.loaded(1, asset, 99, Ok(animation(1)), None);
        assert!(state.views.contains_key(&asset));
    }
}
