//! The server and channel settings screen: the user settings' frame
//! ([`crate::settings::screen`]) with the server's sections on the left. Each
//! section is one of the existing management panels; the panel that is open
//! decides the section, and choosing another closes it and opens that one.
//! Big administrative forms never sit above the conversation.

use fastcord_discord::state::navigation::GuildAdmin;
use fastcord_model::{Permissions, Snowflake};
use iced::Element;
use iced::widget::{column, space};

use crate::i18n::tr;
use crate::icons::Icon;
use crate::settings::{nav_row, screen};
use crate::{Message, ui};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Overview,
    Roles,
    Expressions,
    Community,
    AutoMod,
    Integrations,
    Bans,
    Audit,
    /// One channel's own settings.
    Channel(Snowflake),
    /// The incoming webhooks of one channel.
    Webhooks(Snowflake),
}

impl Section {
    const SERVER: [Self; 8] = [
        Self::Overview,
        Self::Roles,
        Self::Expressions,
        Self::Community,
        Self::AutoMod,
        Self::Integrations,
        Self::Bans,
        Self::Audit,
    ];

    pub fn title(self) -> &'static str {
        tr(match self {
            Self::Overview => "Overview",
            Self::Roles => "Roles",
            Self::Expressions => "Emoji, stickers and sounds",
            Self::Community => "Community",
            Self::AutoMod => "AutoMod",
            Self::Integrations => "Apps and integrations",
            Self::Bans => "Bans",
            Self::Audit => "Audit log",
            Self::Channel(_) => "Overview",
            Self::Webhooks(_) => "Webhooks",
        })
    }

    fn icon(self) -> Icon {
        match self {
            Self::Overview | Self::Channel(_) => Icon::Settings,
            Self::Roles => Icon::Group,
            Self::Expressions => Icon::Emoji,
            Self::Community => Icon::Friends,
            Self::AutoMod => Icon::Safety,
            Self::Integrations | Self::Webhooks(_) => Icon::Link,
            Self::Bans => Icon::Block,
            Self::Audit => Icon::File,
        }
    }

    /// Whether this account may open the section, by the server's read model.
    pub fn allowed(self, admin: Option<&GuildAdmin>) -> bool {
        let Some(admin) = admin else {
            return false;
        };
        match self {
            Self::Overview | Self::Community | Self::AutoMod | Self::Integrations => {
                admin.can_manage_guild
            }
            Self::Roles => admin.can_manage_roles,
            Self::Expressions => {
                admin.mine.contains(Permissions::CREATE_GUILD_EXPRESSIONS)
                    || admin.mine.contains(Permissions::MANAGE_GUILD_EXPRESSIONS)
            }
            Self::Bans => admin.mine.contains(Permissions::BAN_MEMBERS),
            Self::Audit => admin.mine.contains(Permissions::VIEW_AUDIT_LOG),
            Self::Channel(_) | Self::Webhooks(_) => admin.can_manage_channels,
        }
    }

    /// The first server page this account may open.
    pub fn first(admin: Option<&GuildAdmin>) -> Option<Self> {
        Self::SERVER.into_iter().find(|each| each.allowed(admin))
    }

    /// A channel's page, not the server's.
    pub fn channel(self) -> Option<Snowflake> {
        match self {
            Self::Channel(channel) | Self::Webhooks(channel) => Some(channel),
            _ => None,
        }
    }
}

/// What the screen shows: the server, its name, the open section and, for a
/// channel page, the channel's name and whether it has webhooks.
pub struct Page<'a> {
    pub guild: Snowflake,
    pub guild_name: &'a str,
    pub section: Section,
    pub channel: Option<(&'a str, bool)>,
    pub admin: Option<&'a GuildAdmin>,
}

pub fn view<'a>(page: Page<'a>, content: Element<'a, Message>) -> Element<'a, Message> {
    let Page {
        guild,
        guild_name,
        section,
        channel,
        admin,
    } = page;
    let entry = |each: Section| {
        nav_row(
            each.icon(),
            each.title(),
            each == section,
            each.allowed(admin)
                .then_some(Message::ServerSettings(guild, each)),
        )
    };
    let mut nav = column![].spacing(2);
    let title = match (section.channel(), channel) {
        (Some(id), Some((name, webhooks))) => {
            nav = nav
                .push(ui::heading(name.to_uppercase()))
                .push(entry(Section::Channel(id)));
            if webhooks {
                nav = nav.push(entry(Section::Webhooks(id)));
            }
            nav = nav
                .push(space().height(10))
                .push(ui::heading(guild_name.to_uppercase()));
            // Back to the server's own pages.
            nav = nav.push(entry(Section::Overview));
            format!("{name} · {}", section.title())
        }
        _ => {
            nav = nav.push(ui::heading(guild_name.to_uppercase()));
            for each in Section::SERVER {
                // Moderation is its own group, as in Discord.
                if each == Section::Bans {
                    nav = nav
                        .push(space().height(10))
                        .push(ui::heading(tr("MODERATION")));
                }
                nav = nav.push(entry(each));
            }
            section.title().to_owned()
        }
    };
    screen(nav.into(), title, content, Message::CloseServerSettings)
}
