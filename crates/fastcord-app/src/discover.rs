//! Server discovery (SPEC §16, issue #90): a read-only list of public servers
//! (`GET /discoverable-guilds`, an unverified user-client route). One page of
//! [`fastcord_discord::DISCOVERY_PAGE`] servers is asked for at a time, on
//! open and on an explicit "Show more", at most [`MAX_PAGES`] pages are kept,
//! and a page is never asked twice. There is no join route here: a server is
//! joined with an invite link. Free-text discovery search is not implemented.

use std::collections::HashMap;

use crate::ui::button;
use fastcord_discord::state::navigation::NavigationSnapshot;
use fastcord_discord::{DISCOVERY_PAGE, DiscoveredGuild, RestClient, RestError};
use iced::widget::{column, container, image, row, scrollable, text};
use iced::{Element, Length, Task};

use crate::avatars::{self, AvatarKey};
use crate::i18n::{tr, trf};

/// Pages kept, so at most `MAX_PAGES * DISCOVERY_PAGE` servers.
pub const MAX_PAGES: usize = 4;

#[derive(Clone, Debug)]
pub enum Event {
    Toggle,
    More,
    /// A page answered (generation, offset).
    Page(u64, usize, Result<Vec<DiscoveredGuild>, RestError>),
}

#[derive(Debug, Default)]
pub struct State {
    open: bool,
    guilds: Vec<DiscoveredGuild>,
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
    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Page(_, _, Err(RestError::AuthenticationRequired))
        )
    }

    /// Icons of the listed servers, while the list is open.
    pub fn icon_keys(&self) -> Vec<AvatarKey> {
        if !self.open {
            return Vec::new();
        }
        self.guilds
            .iter()
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
    pictures: &HashMap<AvatarKey, image::Handle>,
) -> Option<Element<'a, Event>> {
    if !state.open {
        return None;
    }
    let mut list = column![].spacing(8);
    for guild in &state.guilds {
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
                "To join a server, open an invite link to it. Searching the list is not available."
            ))
            .size(12)
            .color(crate::style::muted()),
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
        assert!(State::is_rejected(&Event::Page(
            0,
            0,
            Err(RestError::AuthenticationRequired)
        )));
    }

    #[test]
    fn counts_read_like_invite_cards() {
        let mut guild = page(1, 1).remove(0);
        assert_eq!(counts_line(&guild), "1,200 members");
        guild.online = Some(34);
        assert_eq!(counts_line(&guild), "34 online \u{b7} 1,200 members");
    }
}
