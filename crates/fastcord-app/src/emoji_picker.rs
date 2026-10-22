//! Bounded, worker-owned picker pages. Search never clones a full guild catalog.
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;

use fastcord_discord::state::Store;
use fastcord_discord::state::navigation::NavigationSnapshot;
use fastcord_model::{Emoji, GuildEmoji, Snowflake};
use iced::Element;
use iced::widget::{button, column, container, mouse_area, pick_list, row, text, text_input};

use crate::Message;
use crate::emoji::{self, Asset, Occurrence, Origin};
use crate::virtual_list::{self, Window};

const MAX_QUERY_BYTES: usize = 128;
const MAX_RECENT: usize = 32;
/// Emoji per grid row and the square size of one cell. Window rows are grid
/// rows; the worker turns them into at most `MAX_CELLS` cells.
pub const COLUMNS: usize = 9;
const CELL: f32 = 38.0;
const MAX_CELLS: usize = 256;
const VIEW_HEIGHT: f32 = 288.0;

fn first_window() -> Window {
    Window::viewport_rows(0.0, VIEW_HEIGHT, CELL)
}
const CATEGORIES: &[&str] = &[
    "Smileys & Emotion",
    "People & Body",
    "Animals & Nature",
    "Food & Drink",
    "Travel & Places",
    "Activities",
    "Objects",
    "Symbols",
    "Flags",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Category {
    #[default]
    All,
    Recent,
    Custom,
    Unicode(u8),
    Variants(u16),
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::All => "All emoji",
            Self::Recent => "Recent",
            Self::Custom => "Server emoji",
            Self::Unicode(id) => CATEGORIES
                .get(usize::from(*id))
                .copied()
                .unwrap_or("All emoji"),
            Self::Variants(_) => "Unicode variants",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    Unicode(u16),
    Custom {
        guild: Snowflake,
        id: Snowflake,
        animated: bool,
        name: Arc<str>,
    },
}

impl Choice {
    pub fn asset(&self) -> Asset {
        match self {
            Self::Unicode(id) => Asset::Unicode(*id),
            Self::Custom { id, animated, .. } => Asset::Custom {
                id: *id,
                animated: *animated,
            },
        }
    }

    pub fn source(&self) -> String {
        match self {
            Self::Unicode(id) => emoji::entries()[usize::from(*id)].text.to_owned(),
            Self::Custom {
                id, animated, name, ..
            } => {
                format!("<{}:{name}:{id}>", if *animated { "a" } else { "" })
            }
        }
    }

    /// The emoji as a reaction to a message names it.
    pub fn reaction_emoji(&self) -> Emoji {
        match self {
            Self::Unicode(id) => Emoji {
                id: None,
                name: Some(emoji::entries()[usize::from(*id)].text.to_owned()),
                animated: false,
            },
            Self::Custom {
                id, animated, name, ..
            } => Emoji {
                id: Some(*id),
                name: Some(name.to_string()),
                animated: *animated,
            },
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Unicode(id) => emoji::entries()[usize::from(*id)].name,
            Self::Custom { name, .. } => name,
        }
    }

    fn fallback(&self) -> &str {
        match self {
            Self::Unicode(id) => emoji::entries()[usize::from(*id)].text,
            Self::Custom { name, .. } => name,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub choice: Choice,
    pub guild_name: Option<Arc<str>>,
    pub variants: Option<u16>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub revision: u64,
    pub channel: Option<Snowflake>,
    pub offset: usize,
    pub total: usize,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub open: bool,
    pub revision: u64,
    pub channel: Option<Snowflake>,
    pub query: String,
    pub category: Category,
    pub window: Window,
    pub recent: VecDeque<Choice>,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            open: false,
            revision: 0,
            channel: None,
            query: String::new(),
            category: Category::All,
            window: first_window(),
            recent: VecDeque::new(),
        }
    }
}

#[derive(Default)]
pub struct Picker {
    pub request: Request,
    /// The message the picker was opened to react to; the pick is then a
    /// reaction rather than text for the composer.
    reaction: Option<Snowflake>,
    /// The last hovered cell: its name and skin-tone control fill the footer.
    hover: Option<Cell>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    Close,
    Search(String),
    Category(Category),
    Window(Window),
    Variants(u16),
    Select(Choice),
    /// The pointer entered a cell; shown in the footer, never re-queries.
    Hover(Choice),
}

impl Picker {
    pub fn close(&mut self) {
        self.reaction = None;
        if self.request.open {
            self.request.open = false;
            self.request.revision = self.request.revision.wrapping_add(1);
        }
    }

    /// Opens the picker to choose a reaction to `message` of `channel`.
    pub fn open_for_reaction(&mut self, channel: Snowflake, message: Snowflake) {
        self.reaction = Some(message);
        self.request.open = true;
        self.request.channel = Some(channel);
        self.request.window = first_window();
        self.request.revision = self.request.revision.wrapping_add(1);
    }

    /// The message a pick reacts to, when the picker was opened for one.
    pub fn reaction(&self) -> Option<Snowflake> {
        self.reaction
    }

    pub fn update(
        &mut self,
        event: Event,
        channel: Option<Snowflake>,
        snapshot: &Snapshot,
    ) -> Option<Choice> {
        match event {
            Event::Hover(choice) => {
                self.hover = snapshot
                    .cells
                    .iter()
                    .find(|cell| cell.choice == choice)
                    .cloned();
                return None;
            }
            Event::Toggle => {
                // The composer's button: a pick is text, never a reaction.
                self.reaction = None;
                self.request.open = !self.request.open;
                self.request.channel = channel;
                self.request.window = first_window();
            }
            Event::Close => self.close(),
            Event::Search(mut query) => {
                let mut end = query.len().min(MAX_QUERY_BYTES);
                while !query.is_char_boundary(end) {
                    end -= 1;
                }
                query.truncate(end);
                query.shrink_to_fit();
                self.request.query = query;
                self.request.window = first_window();
            }
            Event::Category(category) => {
                self.request.category = category;
                self.request.window = first_window();
            }
            Event::Window(window) => {
                self.request.window = Window {
                    start: window.start,
                    count: window.count.min(virtual_list::MAX_ROWS),
                };
            }
            Event::Variants(base) => {
                self.request.category = Category::Variants(base);
                self.request.window = first_window();
            }
            Event::Select(choice) => {
                if self.request.open
                    && snapshot.revision == self.request.revision
                    && snapshot.channel == channel
                    && self.request.channel == channel
                    && snapshot.cells.iter().any(|cell| cell.choice == choice)
                {
                    return Some(choice);
                }
                return None;
            }
        }
        self.hover = None;
        self.request.revision = self.request.revision.wrapping_add(1);
        None
    }

    pub fn inserted(&mut self, choice: Choice) {
        self.request
            .recent
            .retain(|item| item.asset() != choice.asset());
        self.request.recent.push_front(choice);
        self.request.recent.truncate(MAX_RECENT);
        self.close();
    }
}

fn name_valid(emoji: &GuildEmoji) -> Option<&str> {
    let name = emoji.name.as_deref()?;
    (emoji.id.0 != 0
        && (2..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
    .then_some(name)
}

fn custom_usable<'a>(
    store: &Store,
    navigation: &NavigationSnapshot,
    guild: Snowflake,
    emoji: &'a GuildEmoji,
) -> Option<&'a str> {
    let user = store.current_user()?.id;
    let source = store.guild(guild)?;
    let selected = navigation.selection.as_ref();
    let external_allowed = navigation.private_selection.is_some()
        || selected.is_some_and(|selection| selection.permissions.use_external_emojis);
    if !source.emoji_usable(emoji, user)
        || (!external_allowed && selected.is_none_or(|selection| selection.guild_id != guild))
    {
        return None;
    }
    name_valid(emoji)
}

fn custom_cell(store: &Store, guild: Snowflake, emoji: &GuildEmoji, name: &str) -> Cell {
    let guild_name = store
        .guild(guild)
        .map_or("", |source| source.name())
        .chars()
        .take(64)
        .collect::<String>();
    Cell {
        choice: Choice::Custom {
            guild,
            id: emoji.id,
            animated: emoji.animated,
            name: Arc::from(name),
        },
        guild_name: Some(Arc::from(guild_name)),
        variants: None,
    }
}

fn add(snapshot: &mut Snapshot, start: usize, count: usize, cell: impl FnOnce() -> Cell) {
    if snapshot.total >= start && snapshot.cells.len() < count {
        snapshot.cells.push(cell());
    }
    snapshot.total += 1;
}

fn matches_name(name: &str, query: &str) -> bool {
    name.as_bytes()
        .windows(query.len().max(1))
        .any(|part| part.eq_ignore_ascii_case(query.as_bytes()))
        || query.is_empty()
}

fn unicode_cell(id: u16) -> Cell {
    let entry = &emoji::entries()[usize::from(id)];
    let variants = emoji::entries()
        .get(usize::from(id) + 1)
        .is_some_and(|next| next.base == entry.base);
    Cell {
        choice: Choice::Unicode(id),
        guild_name: None,
        variants: variants.then_some(entry.base),
    }
}

/// Executed by the account worker on its normalized store, not from `view`.
/// Only the window's cells (at most 128) cross the latest-slot bridge.
pub fn snapshot(request: &Request, store: &Store, navigation: &NavigationSnapshot) -> Snapshot {
    let channel = navigation
        .private_selection
        .as_ref()
        .map(|selection| selection.channel_id)
        .or_else(|| {
            navigation
                .selection
                .as_ref()
                .map(|selection| selection.channel_id)
        });
    let mut result = Snapshot {
        revision: request.revision,
        channel,
        ..Snapshot::default()
    };
    if !request.open || channel.is_none() || channel != request.channel {
        return result;
    }
    let query = request.query.trim().trim_matches(':').to_ascii_lowercase();
    let start = request.window.start.saturating_mul(COLUMNS);
    let count = request
        .window
        .count
        .min(virtual_list::MAX_ROWS)
        .saturating_mul(COLUMNS)
        .min(MAX_CELLS);
    let matches = |text: &str| query.is_empty() || text.contains(&query);
    if request.category == Category::Recent {
        for choice in request.recent.iter().take(MAX_RECENT) {
            match choice {
                Choice::Unicode(id) => {
                    if emoji::entries()
                        .get(usize::from(*id))
                        .is_some_and(|entry| matches(entry.aliases) || matches(entry.text))
                    {
                        add(&mut result, start, count, || unicode_cell(*id));
                    }
                }
                Choice::Custom { guild, id, .. } => {
                    if let Some(emoji) = store
                        .guild(*guild)
                        .and_then(|source| source.emojis().iter().find(|emoji| emoji.id == *id))
                        && let Some(name) = custom_usable(store, navigation, *guild, emoji)
                        && matches_name(name, &query)
                    {
                        add(&mut result, start, count, || {
                            custom_cell(store, *guild, emoji, name)
                        });
                    }
                }
            }
        }
    } else {
        if request.category != Category::Custom {
            for (id, entry) in emoji::entries().iter().enumerate() {
                let category_matches = match request.category {
                    Category::Unicode(category) => {
                        CATEGORIES.get(usize::from(category)) == Some(&entry.category)
                    }
                    Category::Variants(base) => entry.base == base,
                    _ => true,
                };
                let variants =
                    matches!(request.category, Category::Variants(_)) || !query.is_empty();
                if category_matches
                    && (variants || usize::from(entry.base) == id)
                    && (matches(entry.aliases) || matches(entry.text))
                {
                    add(&mut result, start, count, || unicode_cell(id as u16));
                }
            }
        }
        if matches!(request.category, Category::All | Category::Custom) {
            for &guild in store.guild_ids() {
                let Some(source) = store.guild(guild) else {
                    continue;
                };
                for emoji in source.emojis() {
                    if let Some(name) = custom_usable(store, navigation, guild, emoji)
                        && matches_name(name, &query)
                    {
                        add(&mut result, start, count, || {
                            custom_cell(store, guild, emoji, name)
                        });
                    }
                }
            }
        }
    }
    result.offset = start.min(result.total);
    if start > result.total {
        let mut reset = request.clone();
        reset.window.start = 0;
        return snapshot(&reset, store, navigation);
    }
    result
}

fn cell_style(theme: &iced::Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    button::Style {
        background: matches!(status, button::Status::Hovered | button::Status::Pressed)
            .then(|| palette.background.strong.color.into()),
        text_color: palette.background.base.text,
        border: iced::border::rounded(6),
        ..button::Style::default()
    }
}

pub fn view<'a>(
    picker: &'a Picker,
    snapshot: &'a Snapshot,
    views: &HashMap<Asset, emoji::View>,
) -> Element<'a, Message> {
    let categories = [Category::All, Category::Recent, Category::Custom]
        .into_iter()
        .chain((0..CATEGORIES.len()).map(|id| Category::Unicode(id as u8)))
        .collect::<Vec<_>>();
    let current = snapshot.revision == picker.request.revision;
    let cells = snapshot.cells.chunks(COLUMNS).map(|chunk| {
        let mut line = row![].height(CELL);
        for cell in chunk {
            let choice = &cell.choice;
            let occurrence = Occurrence {
                origin: Origin::Picker {
                    revision: snapshot.revision,
                },
                asset: choice.asset(),
            };
            let icon = emoji::sensed(
                container(emoji::asset_view(
                    choice.asset(),
                    choice.fallback(),
                    views,
                    26.0,
                ))
                .center_x(CELL)
                .center_y(CELL)
                .into(),
                occurrence,
                Message::EmojiVisibility,
            );
            line = line.push(
                mouse_area(
                    button(icon)
                        .on_press_maybe(
                            current.then(|| Message::EmojiPicker(Event::Select(choice.clone()))),
                        )
                        .style(cell_style)
                        .padding(0)
                        .width(CELL)
                        .height(CELL),
                )
                .on_enter(Message::EmojiPicker(Event::Hover(choice.clone()))),
            );
        }
        Element::from(line)
    });
    let footer: Element<'_, Message> = match &picker.hover {
        Some(cell) => {
            let name = match &cell.guild_name {
                Some(guild) => format!(":{}: · {guild}", cell.choice.name()),
                None => format!(":{}:", cell.choice.name()),
            };
            let mut line = row![text(name).size(13).wrapping(text::Wrapping::None)]
                .spacing(8)
                .height(28)
                .align_y(iced::Alignment::Center);
            if let Some(base) = cell.variants {
                line = line.push(
                    button(text("Skin tones").size(11))
                        .on_press(Message::EmojiPicker(Event::Variants(base)))
                        .padding([2, 6]),
                );
            }
            line.into()
        }
        None => text("Hover an emoji for its name.")
            .size(13)
            .height(28)
            .into(),
    };
    let body = column![
        row![text(if picker.reaction.is_some() { "React with emoji" } else { "Emoji" }).size(20), button("Close").on_press(Message::EmojiPicker(Event::Close))].spacing(12),
        text_input("Search names, aliases or emoji", &picker.request.query)
            .id("emoji-search").on_input(|query| Message::EmojiPicker(Event::Search(query))),
        pick_list(categories, Some(picker.request.category), |category| Message::EmojiPicker(Event::Category(category))),
        container(virtual_list::view_rows("emoji-list", snapshot.offset / COLUMNS, snapshot.total.div_ceil(COLUMNS), cells,
            |window| Message::EmojiPicker(Event::Window(window)), CELL)).height(VIEW_HEIGHT),
        footer,
        text(if snapshot.total == 0 && current { "No available emoji match this search." } else { "" }).size(12),
        text(if picker.reaction.is_some() {
            "Animated and external emoji may require Nitro. Account entitlements are not assumed; a reaction Discord rejects is reported above the message box."
        } else {
            "Animated and external emoji may require Nitro. Account entitlements are not assumed; Discord rejections appear beside unsent messages."
        }).size(11),
        iced::widget::tooltip(
            text("Twemoji 17.0.3 · Unicode 16.0 · Attribution (hover)").size(10),
            container(text(include_str!("../../../assets/emoji/NOTICE.txt")).size(10)).width(420).padding(8),
            iced::widget::tooltip::Position::Top,
        ),
    ].spacing(8);
    container(body)
        .width(440)
        .padding(12)
        .style(container::rounded_box)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::gateway::{Dispatch, GatewayEvent, Ready, SessionId};
    use fastcord_discord::state::navigation::{ChannelKind, ChannelSelection, PermissionSummary};
    use fastcord_model::{Guild, GuildMember, Permissions, Role};
    use iced_renderer::core::widget::Tree;

    fn custom(
        id: u64,
        name: Option<&str>,
        animated: bool,
        roles: &[u64],
        available: bool,
    ) -> GuildEmoji {
        GuildEmoji {
            id: Snowflake(id),
            name: name.map(str::to_owned),
            animated,
            roles: roles.iter().map(|&id| Snowflake(id)).collect(),
            available,
        }
    }

    fn fixture() -> (Store, NavigationSnapshot, Request) {
        let user =
            serde_json::from_value(serde_json::json!({"id":"1","username":"emoji_alt"})).unwrap();
        let guild = |id: u64, emojis| Guild {
            id: Snowflake(id),
            name: format!("Guild {id}"),
            icon: None,
            owner_id: Some(Snowflake(2)),
            roles: vec![
                Role {
                    id: Snowflake(id),
                    name: "@everyone".into(),
                    permissions: Permissions::NONE,
                    position: 0,
                },
                Role {
                    id: Snowflake(101),
                    name: "Allowed".into(),
                    permissions: Permissions::NONE,
                    position: 1,
                },
            ],
            channels: Vec::new(),
            emojis,
            member_count: 1,
            stage_instances: Vec::new(),
            scheduled_events: Vec::new(),
            members: vec![GuildMember {
                user_id: Snowflake(1),
                nick: None,
                roles: vec![Snowflake(101)],
                roles_known: true,
                communication_disabled_until: None,
                pending: None,
                flags: None,
            }],
        };
        let mut store = Store::new();
        store.apply(GatewayEvent::Dispatch {
            sequence: 1,
            event: Dispatch::Ready(Box::new(Ready {
                session_id: SessionId::new("emoji-fixture-session".into()),
                read: Default::default(),
                relationships: Vec::new(),
                ignored: Vec::new(),
                presences: Vec::new(),
                user,
                users: Vec::new(),
                guilds: vec![
                    guild(
                        100,
                        vec![
                            custom(501, Some("wave"), false, &[], true),
                            custom(502, Some("party"), true, &[], true),
                            custom(503, Some("role_ok"), false, &[101], true),
                            custom(504, Some("role_no"), false, &[102], true),
                            custom(505, Some("gone"), false, &[], false),
                            custom(506, None, false, &[], true),
                            custom(507, Some("bad-name"), false, &[], true),
                        ],
                    ),
                    guild(200, vec![custom(601, Some("external"), true, &[], true)]),
                ],
                unavailable_guilds: Vec::new(),
                private_channels: Vec::new(),
            })),
        });
        let navigation = NavigationSnapshot {
            selection: Some(ChannelSelection {
                acked: None,
                unread: false,
                mentions: 0,
                guild_id: Snowflake(100),
                channel_id: Snowflake(500),
                name: "general".into(),
                kind: ChannelKind::Text,
                thread: None,
                permissions: PermissionSummary {
                    send_messages: true,
                    ..PermissionSummary::default()
                },
            }),
            ..NavigationSnapshot::default()
        };
        let request = Request {
            open: true,
            channel: Some(Snowflake(500)),
            revision: 1,
            ..Request::default()
        };
        (store, navigation, request)
    }

    #[test]
    fn custom_catalog_filters_roles_availability_names_and_external_permission() {
        let (store, mut navigation, mut request) = fixture();
        request.category = Category::Custom;
        let page = snapshot(&request, &store, &navigation);
        assert_eq!(
            page.cells
                .iter()
                .map(|cell| cell.choice.source())
                .collect::<Vec<_>>(),
            ["<:wave:501>", "<a:party:502>", "<:role_ok:503>"]
        );
        navigation.selection.as_mut().unwrap().permissions =
            PermissionSummary::from(Permissions::USE_EXTERNAL_EMOJIS);
        let page = snapshot(&request, &store, &navigation);
        assert_eq!(page.total, 4);
        assert_eq!(
            page.cells.last().unwrap().choice.source(),
            "<a:external:601>"
        );
        let (_, navigation, _) = fixture();
        let mut picker = Picker {
            request,
            reaction: None,
            hover: None,
        };
        picker.inserted(Choice::Custom {
            guild: Snowflake(200),
            id: Snowflake(601),
            animated: true,
            name: Arc::from("external"),
        });
        picker.request.open = true;
        picker.request.category = Category::Recent;
        assert_eq!(
            snapshot(&picker.request, &store, &navigation).total,
            0,
            "recent does not bypass current permissions"
        );
    }

    #[test]
    fn emoji_role_restrictions_include_the_implicit_everyone_role() {
        let (store, _, _) = fixture();
        let guild = store.guild(Snowflake(100)).unwrap();
        assert!(guild.emoji_usable(
            &custom(508, Some("everyone"), false, &[100], true),
            Snowflake(1)
        ));
        assert!(!guild.emoji_usable(
            &custom(508, Some("everyone"), false, &[100], false),
            Snowflake(1)
        ));
        assert!(!guild.emoji_usable(
            &custom(508, Some("restricted"), false, &[102], true),
            Snowflake(1)
        ));
    }

    #[test]
    fn search_aliases_categories_and_variants_have_only_bounded_pages() {
        let (store, navigation, mut request) = fixture();
        let all = snapshot(&request, &store, &navigation);
        assert!(all.total > 1_000 && all.cells.len() <= MAX_CELLS);
        request.query = ":THUMBSUP:".into();
        let thumbs = snapshot(&request, &store, &navigation);
        assert_eq!(thumbs.total, 6);
        assert!(thumbs.cells.iter().any(|cell| cell.choice.source() == "👍🏽"));
        request.query.clear();
        request.category = Category::Variants(emoji::unicode("👍").unwrap());
        assert_eq!(snapshot(&request, &store, &navigation).total, 6);
        request.category = Category::Unicode(8);
        let flags = snapshot(&request, &store, &navigation);
        assert!(flags.cells.iter().all(|cell| match cell.choice {
            Choice::Unicode(id) => emoji::entries()[usize::from(id)].category == "Flags",
            _ => false,
        }));
        request.category = Category::All;
        request.window = Window {
            start: 20,
            count: usize::MAX,
        };
        let page = snapshot(&request, &store, &navigation);
        assert_eq!(page.offset, 20 * COLUMNS);
        assert_eq!(page.cells.len(), MAX_CELLS);
    }

    #[test]
    fn recent_order_query_bytes_and_stale_selections_are_bounded() {
        let (store, navigation, request) = fixture();
        let mut picker = Picker {
            request,
            reaction: None,
            hover: None,
        };
        let page = snapshot(&picker.request, &store, &navigation);
        let selected = page.cells[0].choice.clone();
        assert_eq!(
            picker.update(Event::Select(selected.clone()), Some(Snowflake(501)), &page),
            None
        );
        assert_eq!(
            picker.update(Event::Select(selected.clone()), Some(Snowflake(500)), &page),
            Some(selected)
        );
        picker.update(Event::Search("👍".repeat(200)), Some(Snowflake(500)), &page);
        assert!(picker.request.query.len() <= MAX_QUERY_BYTES);
        assert!(
            picker
                .request
                .query
                .is_char_boundary(picker.request.query.len())
        );
        assert_eq!(
            picker.update(
                Event::Select(page.cells[0].choice.clone()),
                Some(Snowflake(500)),
                &page
            ),
            None
        );
        for id in 0..100 {
            picker.inserted(Choice::Unicode(id));
        }
        picker.inserted(Choice::Unicode(50));
        assert_eq!(picker.request.recent.len(), MAX_RECENT);
        assert_eq!(picker.request.recent[0], Choice::Unicode(50));
        assert!(!picker.request.open);
        assert_eq!(
            picker
                .request
                .recent
                .iter()
                .filter(|item| **item == Choice::Unicode(50))
                .count(),
            1
        );
    }

    fn nodes(tree: &Tree) -> usize {
        1 + tree.children.iter().map(nodes).sum::<usize>()
    }

    #[test]
    fn picker_widgets_track_cells_not_the_total_catalog_and_are_clickable() {
        let (store, navigation, mut request) = fixture();
        request.query = "grinning face".into();
        let picker = Picker {
            request,
            reaction: None,
            hover: None,
        };
        let page = snapshot(&picker.request, &store, &navigation);
        let empty = HashMap::new();
        let widgets = |total: usize| {
            let mut catalog = page.clone();
            catalog.total = total;
            nodes(&Tree::new(view(&picker, &catalog, &empty)))
        };
        assert_eq!(
            widgets(1_000),
            widgets(1_000_000),
            "widget count does not depend on catalog size"
        );
        assert!(widgets(1_000_000) < 1_000);
        let mut ui =
            crate::variable_list::harness::Ui::new(440.0, 640.0, view(&picker, &page, &empty));
        ui.redraw();
        let mut selected = false;
        for y in (0..440).step_by(16) {
            for x in (0..400).step_by(32) {
                selected |= ui
                    .click(x as f32, y as f32)
                    .iter()
                    .any(|message| matches!(message, Message::EmojiPicker(Event::Select(_))));
            }
        }
        assert!(selected, "actual picker buttons emit insertion intents");
    }

    #[test]
    fn results_form_a_grid_and_a_cell_click_picks_that_emoji() {
        let (store, navigation, request) = fixture();
        let mut picker = Picker {
            request,
            reaction: None,
            hover: None,
        };
        let page = snapshot(&picker.request, &store, &navigation);
        assert_eq!(
            page.cells.len(),
            first_window().count * COLUMNS,
            "a page is whole grid rows"
        );
        let empty = HashMap::new();
        let mut ui =
            crate::variable_list::harness::Ui::new(440.0, 640.0, view(&picker, &page, &empty));
        ui.redraw();
        // Find the cell at grid row 1, column 2 by scanning for its message.
        let want = page.cells[COLUMNS + 2].choice.clone();
        // Stay clear of the scrollbar on the right edge: pressing it scrolls.
        let mut hit = false;
        for y in (0..300).step_by(4) {
            for x in (0..380).step_by(4) {
                hit |= ui.click(x as f32, y as f32).iter().any(|message| {
                    matches!(message, Message::EmojiPicker(Event::Select(c)) if *c == want)
                });
            }
        }
        assert!(hit, "clicking a grid cell publishes its emoji");
        drop(ui);
        picker.update(Event::Hover(want.clone()), Some(Snowflake(500)), &page);
        assert_eq!(picker.hover.as_ref().map(|c| &c.choice), Some(&want));
        assert_eq!(picker.request.revision, 1, "hover never re-queries");
    }

    #[test]
    fn a_picker_opened_for_a_message_picks_a_reaction_not_composer_text() {
        let (store, navigation, request) = fixture();
        let mut picker = Picker {
            request: Request {
                open: false,
                ..request
            },
            reaction: None,
            hover: None,
        };
        picker.open_for_reaction(Snowflake(500), Snowflake(42));
        assert!(picker.request.open);
        assert_eq!(picker.reaction(), Some(Snowflake(42)));
        let page = snapshot(&picker.request, &store, &navigation);
        assert_eq!(page.revision, picker.request.revision);
        let choice = page.cells[0].choice.clone();
        // Only a pick from the current page of the open channel counts.
        assert_eq!(
            picker.update(Event::Select(choice.clone()), Some(Snowflake(501)), &page),
            None
        );
        assert_eq!(
            picker.update(Event::Select(choice.clone()), Some(Snowflake(500)), &page),
            Some(choice.clone())
        );
        assert_eq!(
            picker.reaction(),
            Some(Snowflake(42)),
            "the caller reads the message before the pick closes the picker"
        );
        picker.inserted(choice);
        assert!(!picker.request.open);
        assert_eq!(picker.reaction(), None);

        // The composer's button toggles a text picker; it also closes a reaction one.
        picker.open_for_reaction(Snowflake(500), Snowflake(42));
        picker.update(Event::Toggle, Some(Snowflake(500)), &page);
        assert!(!picker.request.open && picker.reaction().is_none());
        picker.update(Event::Toggle, Some(Snowflake(500)), &page);
        assert!(picker.request.open && picker.reaction().is_none());
        // Closing, as when the channel changes, forgets the message too.
        picker.open_for_reaction(Snowflake(500), Snowflake(43));
        picker.close();
        assert!(!picker.request.open && picker.reaction().is_none());
    }

    #[test]
    fn choices_name_the_emoji_a_reaction_is_made_with() {
        let thumbs = emoji::unicode("\u{1f44d}").unwrap();
        assert_eq!(
            Choice::Unicode(thumbs).reaction_emoji(),
            Emoji {
                id: None,
                name: Some("\u{1f44d}".to_owned()),
                animated: false
            }
        );
        let party = Choice::Custom {
            guild: Snowflake(100),
            id: Snowflake(502),
            animated: true,
            name: Arc::from("party"),
        };
        assert_eq!(
            party.reaction_emoji(),
            Emoji {
                id: Some(Snowflake(502)),
                name: Some("party".to_owned()),
                animated: true
            }
        );
        // Every bundled emoji can be named in a reaction request.
        for id in 0..emoji::entries().len() {
            let emoji = Choice::Unicode(id as u16).reaction_emoji();
            assert!(
                fastcord_discord::ReactionEmoji::new(&emoji).is_ok(),
                "{id}: {emoji:?}"
            );
        }
    }
}
