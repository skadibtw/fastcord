//! Bounded, worker-owned picker pages. Search never clones a full guild catalog.
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;

use fastcord_discord::state::Store;
use fastcord_discord::state::navigation::NavigationSnapshot;
use fastcord_model::{GuildEmoji, Snowflake};
use iced::widget::{button, column, container, pick_list, row, text, text_input};
use iced::{Element, Length};

use crate::Message;
use crate::emoji::{self, Asset, Occurrence, Origin};
use crate::virtual_list::{self, ROW_HEIGHT, Window};

const MAX_QUERY_BYTES: usize = 128;
const MAX_RECENT: usize = 32;
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
            window: Window::viewport(0.0, 288.0),
            recent: VecDeque::new(),
        }
    }
}

#[derive(Default)]
pub struct Picker {
    pub request: Request,
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
}

impl Picker {
    pub fn close(&mut self) {
        if self.request.open {
            self.request.open = false;
            self.request.revision = self.request.revision.wrapping_add(1);
        }
    }

    pub fn update(
        &mut self,
        event: Event,
        channel: Option<Snowflake>,
        snapshot: &Snapshot,
    ) -> Option<Choice> {
        match event {
            Event::Toggle => {
                self.request.open = !self.request.open;
                self.request.channel = channel;
                self.request.window = Window::viewport(0.0, 288.0);
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
                self.request.window = Window::viewport(0.0, 288.0);
            }
            Event::Category(category) => {
                self.request.category = category;
                self.request.window = Window::viewport(0.0, 288.0);
            }
            Event::Window(window) => {
                self.request.window = Window {
                    start: window.start,
                    count: window.count.min(virtual_list::MAX_ROWS),
                };
            }
            Event::Variants(base) => {
                self.request.category = Category::Variants(base);
                self.request.window = Window::viewport(0.0, 288.0);
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
    let start = request.window.start;
    let count = request.window.count.min(virtual_list::MAX_ROWS);
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
    let cells = snapshot.cells.iter().map(|cell| {
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
                24.0,
            ))
            .width(32)
            .height(ROW_HEIGHT)
            .center_x(32)
            .center_y(ROW_HEIGHT)
            .into(),
            occurrence,
            Message::EmojiVisibility,
        );
        let name = match &cell.guild_name {
            Some(guild) => format!(
                "{} · {guild}{}",
                choice.name(),
                if matches!(choice, Choice::Custom { animated: true, .. }) {
                    " (animated)"
                } else {
                    ""
                }
            ),
            None => choice.name().to_owned(),
        };
        let mut content = row![
            button(row![icon, text(name).size(12).wrapping(text::Wrapping::None)].spacing(4))
                .on_press_maybe(
                    current.then(|| Message::EmojiPicker(Event::Select(choice.clone())))
                )
                .height(ROW_HEIGHT)
                .width(Length::Fill)
                .padding(0),
        ]
        .spacing(4);
        if let Some(base) = cell.variants {
            content = content.push(
                button(text("Variants").size(11))
                    .on_press(Message::EmojiPicker(Event::Variants(base)))
                    .height(ROW_HEIGHT),
            );
        }
        container(content).height(ROW_HEIGHT).clip(true).into()
    });
    let body = column![
        row![text("Emoji").size(20), button("Close").on_press(Message::EmojiPicker(Event::Close))].spacing(12),
        text_input("Search names, aliases or emoji", &picker.request.query)
            .id("emoji-search").on_input(|query| Message::EmojiPicker(Event::Search(query))),
        pick_list(categories, Some(picker.request.category), |category| Message::EmojiPicker(Event::Category(category))),
        container(virtual_list::view("emoji-list", snapshot.offset, snapshot.total, cells,
            |window| Message::EmojiPicker(Event::Window(window)))).max_height(288),
        text(if snapshot.total == 0 && current { "No available emoji match this search." } else { "" }).size(12),
        text("Animated and external emoji may require Nitro. Account entitlements are not assumed; Discord rejections appear beside unsent messages.").size(11),
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
            members: vec![GuildMember {
                user_id: Snowflake(1),
                nick: None,
                roles: vec![Snowflake(101)],
                roles_known: true,
                communication_disabled_until: None,
            }],
        };
        let mut store = Store::new();
        store.apply(GatewayEvent::Dispatch {
            sequence: 1,
            event: Dispatch::Ready(Box::new(Ready {
                session_id: SessionId::new("emoji-fixture-session".into()),
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
                guild_id: Snowflake(100),
                channel_id: Snowflake(500),
                name: "general".into(),
                kind: ChannelKind::Text,
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
        let mut picker = Picker { request };
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
        assert!(all.total > 1_000 && all.cells.len() <= virtual_list::MAX_ROWS);
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
            start: 1_000,
            count: usize::MAX,
        };
        let page = snapshot(&request, &store, &navigation);
        assert_eq!(page.offset, 1_000);
        assert_eq!(page.cells.len(), virtual_list::MAX_ROWS);
    }

    #[test]
    fn recent_order_query_bytes_and_stale_selections_are_bounded() {
        let (store, navigation, request) = fixture();
        let mut picker = Picker { request };
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
        let picker = Picker { request };
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
}
