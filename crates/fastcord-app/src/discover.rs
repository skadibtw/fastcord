//! Server discovery (SPEC §16, issue #90): a read-only list of public servers
//! (`GET /discoverable-guilds`, an unverified user-client route). One page of
//! [`fastcord_discord::DISCOVERY_PAGE`] servers is asked for at a time, on
//! open and on an explicit "Show more", at most [`MAX_PAGES`] pages are kept,
//! and a page is never asked twice. There is no join route here: a server is
//! joined with an invite link. A bounded local query filters only loaded pages;
//! global discovery search is not implemented.

use crate::ui::button;
use fastcord_discord::state::navigation::NavigationSnapshot;
use fastcord_discord::{DISCOVERY_PAGE, DiscoveredGuild, RestClient, RestError};
use iced::widget::{column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Task};

use crate::avatars::{self, AvatarKey};
use crate::i18n::{tr, trf};
use crate::panel_kit::auth_rejected;

/// Pages kept, so at most `MAX_PAGES * DISCOVERY_PAGE` servers.
pub const MAX_PAGES: usize = 4;
/// At most 400 UTF-8 bytes retained, independent of pasted input size.
const QUERY_CHARS: usize = 100;

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    More,
    Query(String),
    /// A page answered (generation, offset).
    Page(u64, usize, Result<Vec<DiscoveredGuild>, RestError>),
}

#[derive(Debug, Default)]
pub struct State {
    open: bool,
    guilds: Vec<DiscoveredGuild>,
    query: String,
    /// A request is in flight.
    loading: bool,
    /// The last page was full, so another may exist.
    more: bool,
    error: Option<String>,
    generation: u64,
}

impl State {
    /// Logout or account change.
    pub fn clear(&mut self) {
        *self = Self {
            generation: self.generation.wrapping_add(1),
            ..Self::default()
        };
    }

    /// Offset of the next page, or none at the page cap or while loading.
    fn next_offset(&self) -> Option<usize> {
        let offset = self.guilds.len();
        (!self.loading && offset < MAX_PAGES * DISCOVERY_PAGE).then_some(offset)
    }

    fn finish(&mut self, offset: usize, result: Result<Vec<DiscoveredGuild>, RestError>) {
        if !self.loading || offset != self.guilds.len() {
            return;
        }
        self.loading = false;
        match result {
            Ok(page) => {
                self.error = None;
                self.more = page.len() >= DISCOVERY_PAGE;
                self.guilds.extend(page);
            }
            Err(error) => self.error = Some(crate::invite::describe(error)),
        }
    }

    pub fn update(&mut self, event: Event, client: &RestClient) -> Task<Event> {
        let offset = match event {
            Event::Query(query) => {
                self.query = query.chars().take(QUERY_CHARS).collect();
                return Task::none();
            }
            Event::Toggle => {
                self.open = !self.open;
                if !self.open || !self.guilds.is_empty() {
                    return Task::none();
                }
                self.next_offset()
            }
            Event::More => self.more.then(|| self.next_offset()).flatten(),
            Event::Page(generation, offset, result) => {
                if generation == self.generation {
                    self.finish(offset, result);
                }
                return Task::none();
            }
        };
        let Some(offset) = offset else {
            return Task::none();
        };
        self.loading = true;
        self.error = None;
        let generation = self.generation;
        let client = client.clone();
        Task::perform(
            async move { client.discoverable_guilds(offset).await },
            move |result| Event::Page(generation, offset, result),
        )
    }

    /// A 401 stops the account like every other request.
    pub fn is_rejected(&self, event: &Event) -> bool {
        // A cancelled or retired request must not reject the current account.
        let current = match event {
            Event::Page(serial, ..) => *serial == self.generation,
            _ => false,
        };
        if !current {
            return false;
        }
        match event {
            Event::Page(_, _, r) => auth_rejected(r),
            _ => false,
        }
    }

    /// Filter only retained pages, preserving their order and pagination cursor.
    fn matching(&self) -> impl Iterator<Item = &DiscoveredGuild> {
        let query = self.query.trim().to_lowercase();
        self.guilds.iter().filter(move |guild| {
            query.is_empty()
                || guild.name.to_lowercase().contains(&query)
                || guild.description.to_lowercase().contains(&query)
        })
    }

    /// Icons of the filtered servers, while the list is open.
    pub fn icon_keys(&self) -> Vec<AvatarKey> {
        if !self.open {
            return Vec::new();
        }
        self.matching()
            .filter_map(|g| AvatarKey::guild(g.id, g.icon.as_deref()))
            .collect()
    }
}

fn counts_line(guild: &DiscoveredGuild) -> String {
    let mut parts = Vec::new();
    if let Some(online) = guild.online {
        parts.push(trf("{} online", &[&crate::invite::count(online)]));
    }
    if let Some(members) = guild.members {
        parts.push(crate::invite::members_line(members));
    }
    parts.join(" \u{b7} ")
}

/// The discovery panel above the conversation; `None` while closed.
pub fn view<'a>(
    state: &'a State,
    navigation: &NavigationSnapshot,
    pictures: &crate::avatars::Views,
) -> Option<Element<'a, Event>> {
    if !state.open {
        return None;
    }
    let mut list = column![].spacing(8);
    let mut matches = 0;
    for guild in state.matching() {
        matches += 1;
        let name = guild.name.clone();
        let fallback = move || -> Element<'static, Event> {
            let initial = name
                .chars()
                .find(|c| c.is_alphanumeric())
                .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect());
            container(text(initial).size(16))
                .center(40)
                .style(container::rounded_box)
                .into()
        };
        let face = match AvatarKey::guild(guild.id, guild.icon.as_deref()) {
            Some(key) => avatars::view(pictures, &key, 40.0, fallback),
            None => fallback(),
        };
        let joined = navigation.guilds.rows.iter().any(|g| g.id == guild.id);
        let mut info = column![text(guild.name.clone()).size(14)];
        if !guild.description.is_empty() {
            info = info.push(
                text(guild.description.clone())
                    .size(12)
                    .color(crate::style::muted()),
            );
        }
        info = info.push(
            text(counts_line(guild))
                .size(12)
                .color(crate::style::muted()),
        );
        let mut line = row![face, info.width(Length::Fill)]
            .spacing(10)
            .align_y(iced::Alignment::Center);
        if joined {
            line = line.push(text(tr("Joined")).size(12).color(crate::style::muted()));
        }
        list = list.push(line);
    }
    if state.loading {
        list = list.push(text(tr("Loading\u{2026}")).size(12));
    }
    if let Some(error) = &state.error {
        list = list.push(text(error.clone()).size(12).style(text::danger));
    }
    if state.guilds.is_empty() && !state.loading && state.error.is_none() {
        list = list.push(text(tr("No public servers were listed.")).size(12));
    }
    if matches == 0 && !state.query.trim().is_empty() && !state.guilds.is_empty() {
        list = list.push(text(tr("No loaded servers match this filter.")).size(12));
    }
    if state.more && !state.loading && state.next_offset().is_some() {
        list = list.push(button(text(tr("Show more")).size(13)).on_press(Event::More));
    } else if state.guilds.len() >= MAX_PAGES * DISCOVERY_PAGE {
        list = list.push(
            text(trf("Showing the first {} servers.", &[&state.guilds.len()]))
                .size(12)
                .color(crate::style::muted()),
        );
    }
    Some(
        column![
            row![
                text(tr("Explore public servers"))
                    .size(13)
                    .width(Length::Fill),
                crate::icons::close(Some(Event::Toggle)),
            ],
            text(tr(
                "Filter only the servers loaded here. Show more loads another page. To join, open an invite link."
            ))
            .size(12)
            .color(crate::style::muted()),
            text_input(tr("Filter loaded servers"), &state.query)
                .on_input(Event::Query)
                .size(14)
                .padding(6),
            scrollable(list).height(Length::Fixed(280.0)),
        ]
        .spacing(6)
        .max_width(720)
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(from: u64, n: usize) -> Vec<DiscoveredGuild> {
        (0..n as u64)
            .map(|i| DiscoveredGuild {
                id: fastcord_model::Snowflake(from + i),
                name: format!("S{}", from + i),
                description: String::new(),
                icon: None,
                members: Some(1200),
                online: None,
            })
            .collect()
    }

    fn client() -> RestClient {
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
            fastcord_discord::gateway::ClientProperties::bundled(
                fastcord_discord::gateway::HostOs::Linux,
                "en-US",
            ),
        )
        .unwrap()
    }

    #[test]
    fn pages_are_one_at_a_time_in_order_and_capped() {
        let mut state = State {
            open: true,
            ..State::default()
        };
        assert_eq!(state.next_offset(), Some(0));
        state.loading = true;
        assert_eq!(state.next_offset(), None, "one request at a time");
        // An answer for another offset is ignored.
        state.finish(24, Ok(page(100, DISCOVERY_PAGE)));
        assert!(state.guilds.is_empty() && state.loading);
        for n in 0..MAX_PAGES {
            let offset = n * DISCOVERY_PAGE;
            state.finish(offset, Ok(page(offset as u64, DISCOVERY_PAGE)));
            assert!(!state.loading && state.more);
            if n + 1 < MAX_PAGES {
                assert_eq!(state.next_offset(), Some(offset + DISCOVERY_PAGE));
                state.loading = true;
            }
        }
        assert_eq!(state.guilds.len(), MAX_PAGES * DISCOVERY_PAGE);
        assert_eq!(state.next_offset(), None, "the page cap");
        // A late duplicate of the last page changes nothing.
        state.finish(0, Ok(page(0, DISCOVERY_PAGE)));
        assert_eq!(state.guilds.len(), MAX_PAGES * DISCOVERY_PAGE);
    }

    #[test]
    fn a_short_page_ends_the_list_and_a_failure_keeps_what_is_shown() {
        let mut state = State {
            loading: true,
            ..State::default()
        };
        state.finish(0, Ok(page(0, 5)));
        assert!(!state.more);
        assert_eq!(state.guilds.len(), 5);
        state.loading = true;
        state.finish(5, Err(RestError::ResourceGone));
        assert_eq!(state.guilds.len(), 5);
        assert!(state.error.is_some() && !state.loading);
    }

    #[test]
    fn opening_asks_once_and_a_stale_answer_after_logout_is_ignored() {
        let client = client();
        let mut state = State::default();
        let _ = state.update(Event::Toggle, &client);
        assert!(state.open && state.loading);
        // Closing and reopening while loading does not start a second request.
        let _ = state.update(Event::Toggle, &client);
        let _ = state.update(Event::Toggle, &client);
        assert!(state.loading && state.guilds.is_empty());
        let before = state.generation;
        state.clear();
        let _ = state.update(Event::Page(before, 0, Ok(page(0, 3))), &client);
        assert!(state.guilds.is_empty());
        assert!(!state.is_rejected(&Event::Page(
            before,
            0,
            Err(RestError::AuthenticationRequired)
        )));
        assert!(state.is_rejected(&Event::Page(
            state.generation,
            0,
            Err(RestError::AuthenticationRequired)
        )));
    }

    #[test]
    fn local_filter_matches_unicode_names_and_descriptions_and_limits_icon_demand() {
        let client = client();
        let mut guilds = page(1, 3);
        guilds[0].name = "Музыка".into();
        guilds[1].description = "ÉTUDES et musique".into();
        guilds[2].name = "Games".into();
        for guild in &mut guilds {
            guild.icon = Some("0123456789abcdef0123456789abcdef".into());
        }
        let mut state = State {
            open: true,
            guilds,
            ..State::default()
        };
        let ids = |state: &State| state.matching().map(|guild| guild.id.0).collect::<Vec<_>>();
        assert_eq!(ids(&state), [1, 2, 3]);
        assert_eq!(state.icon_keys().len(), 3);
        for (query, expected) in [
            ("  МУЗЫКА ", vec![1]),
            ("études", vec![2]),
            ("absent", vec![]),
        ] {
            assert_eq!(state.update(Event::Query(query.into()), &client).units(), 0);
            assert_eq!(ids(&state), expected);
            let icons = state.icon_keys();
            assert_eq!(icons.len(), expected.len());
            for (icon, id) in icons.iter().zip(expected) {
                assert!(matches!(icon, AvatarKey::Guild { id: found, .. } if found.0 == id));
            }
        }
        assert_eq!(state.update(Event::Query(" ".into()), &client).units(), 0);
        assert_eq!(ids(&state), [1, 2, 3], "clearing restores original order");
        state.open = false;
        assert!(state.icon_keys().is_empty());
    }

    #[test]
    fn typing_never_fetches_and_more_uses_loaded_count_even_with_no_matches() {
        let client = client();
        let mut state = State::default();
        assert_eq!(
            state.update(Event::Query("wanted".into()), &client).units(),
            0
        );
        assert!(!state.loading, "typing cannot start the initial request");
        assert_eq!(state.update(Event::Toggle, &client).units(), 1);
        assert_eq!(
            state.update(Event::Query("WANTED".into()), &client).units(),
            0
        );
        assert_eq!(
            state
                .update(Event::Page(0, 0, Ok(page(1, DISCOVERY_PAGE))), &client)
                .units(),
            0
        );
        assert_eq!(state.matching().count(), 0);
        assert_eq!(state.next_offset(), Some(DISCOVERY_PAGE));
        assert_eq!(
            state.update(Event::Query("wanted".into()), &client).units(),
            0
        );
        assert!(
            !state.loading,
            "no automatic page fetch for an empty filtered list"
        );
        assert_eq!(state.update(Event::More, &client).units(), 1);
        assert_eq!(
            state.update(Event::More, &client).units(),
            0,
            "one request at a time"
        );
        let mut next = page(100, 1);
        next[0].description = "The wanted server".into();
        assert_eq!(
            state
                .update(Event::Page(0, DISCOVERY_PAGE, Ok(next)), &client)
                .units(),
            0
        );
        assert_eq!(state.matching().map(|g| g.id.0).collect::<Vec<_>>(), [100]);
        assert_eq!(state.guilds.len(), DISCOVERY_PAGE + 1);
        assert_eq!(state.query, "wanted");
        assert!(!state.more);
    }

    #[test]
    fn query_is_utf8_bounded_and_account_reset_forgets_it_and_old_pages() {
        let client = client();
        let mut state = State::default();
        assert_eq!(
            state
                .update(Event::Query("😀".repeat(10_000)), &client)
                .units(),
            0
        );
        assert_eq!(state.query.chars().count(), QUERY_CHARS);
        assert_eq!(state.query.len(), QUERY_CHARS * 4);
        let _ = state.update(Event::Toggle, &client);
        let old = state.generation;
        state.clear();
        assert!(state.query.is_empty() && state.guilds.is_empty() && !state.open);
        let _ = state.update(Event::Page(old, 0, Ok(page(1, DISCOVERY_PAGE))), &client);
        assert!(state.guilds.is_empty() && state.icon_keys().is_empty());
    }

    #[test]
    fn an_empty_filtered_page_keeps_manual_pagination_clickable() {
        use crate::variable_list::harness::Ui;
        let state = State {
            open: true,
            guilds: page(1, DISCOVERY_PAGE),
            query: "no match".into(),
            more: true,
            ..State::default()
        };
        let nav = NavigationSnapshot::default();
        let pictures = crate::avatars::Views::default();
        let mut ui = Ui::new(420.0, 500.0, view(&state, &nav, &pictures).unwrap());
        ui.redraw();
        ui.draw();
        let mut more = false;
        for y in (100..400).step_by(4) {
            for x in (8..150).step_by(8) {
                more |= ui
                    .click(x as f32, y as f32)
                    .iter()
                    .any(|event| matches!(event, Event::More));
            }
        }
        assert!(more, "zero matching servers must not hide Show more");
    }

    #[test]
    fn counts_read_like_invite_cards() {
        let mut guild = page(1, 1).remove(0);
        assert_eq!(counts_line(&guild), "1,200 members");
        guild.online = Some(34);
        assert_eq!(counts_line(&guild), "34 online \u{b7} 1,200 members");
    }
}
