//! The one settings screen behind the gear: a list of sections on the left,
//! the chosen section on the right. It covers the whole window, like
//! Discord's, and closes with the cross or Esc. Before signing in only the
//! sections that need no account are offered. [`screen`] is the same frame
//! for the server and channel settings.

use crate::ui::button;
use iced::widget::{Id, column, container, row, scrollable, space, text};
use iced::{Alignment, Element, Length, Padding};

use crate::i18n::tr;
use crate::icons::{self, Icon};
use crate::{Message, style, ui};

/// The scrolled page of every settings screen; a new section starts at its top.
pub const PAGE: &str = "settings-page";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Profile,
    Accounts,
    Privacy,
    Voice,
    Chat,
    Notifications,
    Network,
    App,
}

#[derive(Clone, Debug)]
pub enum Event {
    Open(Section),
    Close,
}

impl Section {
    const USER: [Self; 3] = [Self::Profile, Self::Accounts, Self::Privacy];
    const APP: [Self; 5] = [
        Self::Voice,
        Self::Chat,
        Self::Notifications,
        Self::Network,
        Self::App,
    ];

    pub fn title(self) -> &'static str {
        tr(match self {
            Self::Profile => "My profile",
            Self::Accounts => "Accounts and security",
            Self::Privacy => "Privacy",
            Self::Voice => "Voice and video",
            Self::Chat => "Chat and appearance",
            Self::Notifications => "Notifications",
            Self::Network => "Network",
            Self::App => "Application",
        })
    }

    fn icon(self) -> Icon {
        match self {
            Self::Profile => Icon::Profile,
            Self::Accounts => Icon::Lock,
            Self::Privacy => Icon::Safety,
            Self::Voice => Icon::Mic { muted: false },
            Self::Chat => Icon::Chat,
            Self::Notifications => Icon::Bell,
            Self::Network => Icon::Signal,
            Self::App => Icon::Settings,
        }
    }

    /// Needs a signed-in account.
    pub fn needs_account(self) -> bool {
        Self::USER.contains(&self)
    }
}

/// One entry of a settings list; `None` greys it out.
pub fn nav_row<'a>(
    icon: Icon,
    title: &'a str,
    selected: bool,
    on: Option<Message>,
) -> Element<'a, Message> {
    let enabled = on.is_some();
    let color = if selected {
        style::BRIGHT
    } else if enabled {
        style::MUTED
    } else {
        iced::Color {
            a: 0.45,
            ..style::MUTED
        }
    };
    button(
        row![
            icons::sized(icon, color, 18.0),
            text(title)
                .size(14)
                .color(if enabled { style::TEXT } else { color }),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .on_press_maybe(on.filter(|_| !selected))
    .width(Length::Fill)
    .padding([7, 10])
    .style(style::row_button(selected, false))
    .into()
}

/// A full-window settings frame: `nav` on the left, the page `title` and
/// `content` scrolled on the right, one close cross (Esc does the same).
pub fn screen<'a>(
    nav: Element<'a, Message>,
    title: impl text::IntoFragment<'a>,
    content: Element<'a, Message>,
    close: Message,
) -> Element<'a, Message> {
    let sidebar = container(scrollable(nav).height(Length::Fill))
        .width(232)
        .height(Length::Fill)
        .padding(Padding::new(12.0).top(40.0))
        .style(style::sidebar);
    let close = column![
        icons::action(icons::Action {
            icon: Icon::Close,
            on_press: Some(close),
            active: true,
            danger: false,
            tip: tr("Close").to_owned(),
            badge: 0,
        }),
        text("ESC").size(11).color(style::MUTED),
    ]
    .spacing(2)
    .align_x(Alignment::Center);
    let page = container(
        scrollable(
            container(
                column![
                    text(title).size(22).font(ui::BOLD),
                    // Long rows wrap inside the page instead of running past it.
                    container(content).width(Length::Fill),
                ]
                .spacing(24)
                .max_width(760),
            )
            .padding(Padding::new(32.0).top(40.0).bottom(60.0)),
        )
        .id(Id::new(PAGE))
        .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(style::chat);
    row![
        sidebar,
        page,
        container(close)
            .padding(Padding::new(0.0).top(40.0).right(16.0).left(4.0))
            .height(Length::Fill)
            .style(style::chat),
    ]
    .height(Length::Fill)
    .into()
}

/// The user settings around one section's `content`.
pub fn view<'a>(
    current: Section,
    signed_in: bool,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    let entry = |section: Section| {
        nav_row(
            section.icon(),
            section.title(),
            section == current,
            Some(Message::Settings(Event::Open(section))),
        )
    };
    let mut nav = column![].spacing(2).width(Length::Fill);
    if signed_in {
        nav = nav.push(ui::heading(tr("USER")));
        for section in Section::USER {
            nav = nav.push(entry(section));
        }
        nav = nav.push(space().height(10));
    }
    nav = nav.push(ui::heading(tr("APPLICATION")));
    for section in Section::APP {
        nav = nav.push(entry(section));
    }
    nav = nav
        .push(space().height(10))
        .push(ui::note(format!("fastcord {}", env!("CARGO_PKG_VERSION"))));
    screen(
        nav.into(),
        current.title(),
        content,
        Message::Settings(Event::Close),
    )
}
