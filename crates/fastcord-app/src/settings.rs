//! The one settings screen behind the gear: a list of sections on the left,
//! the chosen section on the right. It covers the whole window, like
//! Discord's, and closes with the cross or Esc. Before signing in only the
//! sections that need no account are offered.

use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Alignment, Element, Length, Padding};

use crate::i18n::tr;
use crate::icons::{self, Icon};
use crate::{Message, style, ui};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Profile,
    Accounts,
    Privacy,
    Voice,
    Chat,
    Notifications,
    App,
}

#[derive(Clone, Debug)]
pub enum Event {
    Open(Section),
    Close,
}

impl Section {
    const USER: [Self; 3] = [Self::Profile, Self::Accounts, Self::Privacy];
    const APP: [Self; 4] = [Self::Voice, Self::Chat, Self::Notifications, Self::App];

    pub fn title(self) -> &'static str {
        tr(match self {
            Self::Profile => "My profile",
            Self::Accounts => "Accounts and security",
            Self::Privacy => "Privacy",
            Self::Voice => "Voice and video",
            Self::Chat => "Chat and appearance",
            Self::Notifications => "Notifications",
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
            Self::App => Icon::Settings,
        }
    }

    /// Needs a signed-in account.
    pub fn needs_account(self) -> bool {
        Self::USER.contains(&self)
    }
}

fn nav_row<'a>(section: Section, current: Section) -> Element<'a, Message> {
    let selected = section == current;
    button(
        row![
            icons::sized(
                section.icon(),
                if selected { style::BRIGHT } else { style::MUTED },
                18.0
            ),
            text(section.title()).size(14),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .on_press(Message::Settings(Event::Open(section)))
    .width(Length::Fill)
    .padding([7, 10])
    .style(style::row_button(selected, false))
    .into()
}

/// The screen around one section's `content`.
pub fn view<'a>(
    current: Section,
    signed_in: bool,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    let mut nav = column![].spacing(2).width(Length::Fill);
    if signed_in {
        nav = nav.push(ui::heading(tr("USER")));
        for section in Section::USER {
            nav = nav.push(nav_row(section, current));
        }
        nav = nav.push(space().height(10));
    }
    nav = nav.push(ui::heading(tr("APPLICATION")));
    for section in Section::APP {
        nav = nav.push(nav_row(section, current));
    }
    nav = nav.push(space().height(10)).push(ui::note(format!(
        "fastcord {}",
        env!("CARGO_PKG_VERSION")
    )));
    let sidebar = container(scrollable(nav).height(Length::Fill))
        .width(240)
        .height(Length::Fill)
        .padding(Padding::new(16.0).top(40.0))
        .style(style::sidebar);
    let close = column![
        icons::action(icons::Action {
            icon: Icon::Close,
            on_press: Some(Message::Settings(Event::Close)),
            active: true,
            danger: false,
            tip: tr("Close").to_owned(),
            badge: 0,
        }),
        text("ESC").size(11).color(style::MUTED),
    ]
    .spacing(2)
    .align_x(Alignment::Center);
    let page = container(scrollable(
        container(
            column![
                text(current.title()).size(22).font(ui::BOLD),
                content,
            ]
            .spacing(24)
            .max_width(720),
        )
        .padding(Padding::new(40.0).top(40.0).bottom(60.0)),
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .style(style::chat);
    row![
        sidebar,
        page,
        container(close).padding(Padding::new(0.0).top(40.0).right(24.0).left(8.0)).style(style::chat),
    ]
    .height(Length::Fill)
    .into()
}
