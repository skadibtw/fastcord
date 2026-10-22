//! The main screen's layout, after the web client: a rail of servers, a
//! sidebar of channels (or direct messages) with the user's own panel at its
//! foot, and the conversation. Presentation of compact reducer-owned
//! navigation windows, never full state.
use fastcord_discord::state::navigation::{ChannelKind, NavigationSnapshot, Occupant, StageSeat};
use iced::font::Weight;
use iced::widget::{
    button, column, container, image, mouse_area, row, space, text, text_input, tooltip,
};
use iced::{Element, Font, Length, Padding, alignment};
use std::collections::HashMap;

use crate::Message;
use crate::avatars::{self, AvatarKey};
use crate::composer::{self, Composer};
use crate::icons::{self, Icon};
use crate::style;
use crate::timeline;
use crate::virtual_list::{self, ROW_HEIGHT};

const BOLD: Font = Font {
    weight: Weight::Bold,
    ..Font::DEFAULT
};
/// Height of one server icon row in the rail.
pub const RAIL_ROW: f32 = 56.0;
const RAIL_WIDTH: f32 = 72.0;
pub(crate) const SIDEBAR_WIDTH: f32 = 240.0;

pub struct PrivateConversation<'a> {
    pub recipients: &'a str,
    pub notice: Option<&'a str>,
}

/// What the screen shows around the conversation.
pub struct Chrome<'a> {
    /// Direct messages instead of a server's channels in the sidebar.
    pub directs: bool,
    /// The user and voice panel at the foot of the sidebar.
    pub footer: Element<'a, Message>,
    /// Connection and notice lines above the conversation.
    pub banner: Option<Element<'a, Message>>,
    /// People speaking in the call now (their occupant avatars get a ring).
    pub speaking: Vec<fastcord_model::Snowflake>,
    /// The Friends screen, which replaces the conversation in direct messages.
    pub friends: Option<Element<'a, Message>>,
    /// The unified inbox, which replaces the conversation while open.
    pub inbox: Option<Element<'a, Message>>,
    /// The private call this account is connected to, if any.
    pub voice_call: Option<fastcord_model::Snowflake>,
    /// The post list of the open forum, when the open channel is one.
    pub forum: Option<Element<'a, Message>>,
}

#[allow(clippy::too_many_arguments)]
pub fn view<'a>(
    snapshot: &'a NavigationSnapshot,
    history: &'a timeline::Snapshot,
    composer: &'a Composer,
    interaction: timeline::Interaction,
    private: PrivateConversation<'a>,
    attachments: timeline::Attachments<'a>,
    chrome: Chrome<'a>,
) -> Element<'a, Message> {
    let Chrome {
        directs,
        footer,
        banner,
        speaking,
        friends,
        inbox,
        voice_call,
        forum,
    } = chrome;
    let rail = rail(snapshot, directs, inbox.is_some(), attachments.avatars);
    let sidebar = sidebar(
        snapshot,
        private,
        directs,
        footer,
        attachments.avatars,
        speaking,
    );
    let main = match inbox.or(friends.filter(|_| directs)) {
        Some(friends) => friends,
        None => main_pane(
            snapshot,
            history,
            composer,
            interaction,
            attachments,
            directs,
            banner,
            voice_call,
            forum,
        ),
    };
    row![rail, sidebar, main].height(Length::Fill).into()
}

/// The rail button's label: the unread conversations and mentions it holds.
fn inbox_label(inbox: &fastcord_discord::state::navigation::Inbox) -> String {
    match (inbox.total, inbox.mentions) {
        (0, _) => "Inbox".to_owned(),
        (total, 0) => format!("Inbox\n{total}"),
        (total, mentions) => format!("Inbox\n{total} @{mentions}"),
    }
}

/// Up to two initials of a server name.
fn initials(name: &str) -> String {
    let letters: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() {
        "?".to_owned()
    } else {
        letters
    }
}

fn centered_label<'a>(label: impl text::IntoFragment<'a>, size: f32) -> Element<'a, Message> {
    container(text(label).size(size).font(BOLD))
        .center(Length::Fill)
        .into()
}

fn rail<'a>(
    snapshot: &'a NavigationSnapshot,
    directs: bool,
    inbox_open: bool,
    avatars: &HashMap<AvatarKey, image::Handle>,
) -> Element<'a, Message> {
    let icon = |label: String,
                picture: Option<&AvatarKey>,
                selected: bool,
                intent: Option<Message>|
     -> Element<'a, Message> {
        let face = match picture {
            Some(key) => avatars::view(avatars, key, 48.0, || centered_label(label.clone(), 15.0)),
            None => centered_label(label, 15.0),
        };
        container(
            button(face)
                .on_press_maybe(intent)
                .width(48)
                .height(48)
                .padding(0)
                .style(style::server_button(selected)),
        )
        .center_x(Length::Fill)
        .center_y(RAIL_ROW)
        .into()
    };
    let guilds = snapshot.guilds.rows.iter().map(|guild| {
        let selected = guild.selected && !directs;
        let mut cell = row![]
            .align_y(alignment::Vertical::Center)
            .width(Length::Fill);
        let pill = if selected {
            20.0
        } else if guild.unread {
            8.0
        } else {
            0.0
        };
        cell = cell.push(
            container(space())
                .width(4)
                .height(pill)
                .style(|_| container::Style {
                    background: Some(style::BRIGHT.into()),
                    border: iced::Border {
                        radius: 2.0.into(),
                        ..iced::Border::default()
                    },
                    ..container::Style::default()
                }),
        );
        let picture = AvatarKey::guild(guild.id, guild.icon.as_deref());
        cell = cell.push(icon(
            initials(&guild.name),
            picture.as_ref(),
            selected,
            guild.available.then_some(Message::SelectGuild(guild.id)),
        ));
        // Right click opens the server menu (settings, create channel, leave).
        mouse_area(container(cell).height(RAIL_ROW))
            .on_right_press(Message::GuildAdmin(crate::guild_admin::Event::Menu(
                guild.id,
            )))
            .into()
    });
    let rail_tool = |kind, label: String, selected, message, count: Option<String>| {
        let mut face = column![icons::sized(kind, style::TEXT, 24.0)]
            .spacing(2)
            .align_x(alignment::Horizontal::Center);
        if let Some(count) = count {
            face = face.push(text(count).size(10));
        }
        container(tooltip(
            button(container(face).center(48))
                .on_press(message)
                .padding(0)
                .style(style::server_button(selected)),
            container(text(label).size(12))
                .padding(6)
                .style(style::card),
            tooltip::Position::Right,
        ))
        .center_x(Length::Fill)
        .center_y(RAIL_ROW)
    };
    let inbox_count = (snapshot.inbox.total > 0).then(|| {
        let total = snapshot.inbox.total.min(999);
        if snapshot.inbox.mentions > 0 {
            format!("{total} @{}", snapshot.inbox.mentions.min(99))
        } else {
            total.to_string()
        }
    });
    container(
        column![
            rail_tool(
                Icon::Brand,
                "Direct Messages".to_owned(),
                directs,
                Message::ShowDirects(true),
                None,
            ),
            rail_tool(
                Icon::Inbox,
                inbox_label(&snapshot.inbox),
                inbox_open,
                Message::Inbox(crate::inbox::Event::Toggle),
                inbox_count,
            ),
            rail_tool(
                Icon::Discover,
                "Discover servers".to_owned(),
                false,
                Message::Discover(crate::discover::Event::Toggle),
                None,
            ),
            icon(
                "+".to_owned(),
                None,
                false,
                Some(Message::GuildAdmin(crate::guild_admin::Event::NewServer))
            ),
            container(space())
                .width(32)
                .height(2)
                .style(|_| container::Style {
                    background: Some(style::SIDEBAR.into()),
                    ..container::Style::default()
                }),
            virtual_list::view_rows(
                "guild-list",
                snapshot.guilds.offset,
                snapshot.guilds.total,
                guilds,
                Message::GuildViewport,
                RAIL_ROW,
            ),
        ]
        .align_x(alignment::Horizontal::Center)
        .padding(Padding::new(0.0).top(8.0)),
    )
    .width(RAIL_WIDTH)
    .height(Length::Fill)
    .style(style::rail)
    .into()
}

/// A row of the sidebar: a label with an optional red mention count.
#[allow(clippy::too_many_arguments)]
fn sidebar_row<'a>(
    kind: Icon,
    label: String,
    selected: bool,
    unread: bool,
    mention_count: u32,
    intent: Option<Message>,
    presence: Option<fastcord_model::Status>,
    chat: Option<Message>,
) -> Element<'a, Message> {
    // Clipped so a long name never draws under the badge or the Chat button.
    let mut line = row![
        icons::sized(kind, if unread { style::TEXT } else { style::MUTED }, 18.0),
        container(
            text(label)
                .size(15)
                .font(if unread { BOLD } else { Font::DEFAULT })
                .wrapping(text::Wrapping::None)
        )
        .width(Length::Fill)
        .clip(true)
    ]
    .spacing(6)
    .align_y(alignment::Vertical::Center);
    if presence.is_some() {
        line = line.push(crate::status::dot(presence));
    }
    if mention_count > 0 {
        line = line.push(
            container(
                text(if mention_count > 99 {
                    "99+".to_owned()
                } else {
                    mention_count.to_string()
                })
                .size(11)
                .font(BOLD),
            )
            .padding([1, 6])
            .style(style::badge),
        );
    }
    let main = button(line)
        .on_press_maybe(intent)
        .width(Length::Fill)
        .height(ROW_HEIGHT - 2.0)
        .padding([4, 8])
        .style(style::row_button(selected, unread));
    // A voice or Stage row: the row joins the call, this opens its text chat.
    let content: Element<'a, Message> = match chat {
        Some(open) => row![main, icons::tool(Icon::Chat, "Open channel chat", open),]
            .spacing(4)
            .into(),
        None => main.into(),
    };
    container(content)
        .height(ROW_HEIGHT)
        .padding([1, 8])
        .clip(true)
        .into()
}

/// One person (or the "+N" rest) under a voice row, at the fixed row height.
/// A ring marks someone speaking in the call this account is in; the tags show
/// Go Live, camera, mute and deafen. Left click watches; right click opens actions.
pub(crate) fn occupant_row<'a>(
    occupant: &'a Occupant,
    speaking: bool,
    avatar_views: &HashMap<AvatarKey, image::Handle>,
) -> Element<'a, Message> {
    const TAG: f32 = 14.0;
    let line: Element<'a, Message> = match occupant {
        Occupant::More(more) => text(format!("+{more}"))
            .size(12)
            .style(text::secondary)
            .into(),
        Occupant::Heading(label) => text(label.to_uppercase())
            .size(10)
            .font(BOLD)
            .style(text::secondary)
            .into(),
        Occupant::Member {
            user_id,
            name,
            avatar,
            muted,
            deafened,
            streaming,
            video,
            seat,
        } => {
            let face = container(avatars::view(
                avatar_views,
                &AvatarKey::user(*user_id, avatar.as_deref()),
                22.0,
                || {
                    container(text(style::initial(name)).size(11).font(BOLD))
                        .center(22)
                        .style(style::avatar(style::avatar_color(user_id.0)))
                        .into()
                },
            ))
            .padding(2)
            .style(style::avatar_ring(speaking));
            let mut line = row![
                face,
                container(text(name.as_str()).size(13).wrapping(text::Wrapping::None))
                    .width(Length::Fill)
                    .clip(true),
            ]
            .spacing(6)
            .align_y(alignment::Vertical::Center);
            if *seat == Some(StageSeat::Hand) {
                line = line.push(
                    container(text("HAND").size(9).font(BOLD))
                        .padding([1, 4])
                        .style(style::live_badge),
                );
            }
            if *streaming {
                line = line.push(
                    container(text("LIVE").size(9).font(BOLD))
                        .padding([1, 4])
                        .style(style::live_badge),
                );
            }
            if *video {
                line = line.push(icons::sized(Icon::Camera { on: true }, style::MUTED, TAG));
            }
            if *muted {
                line = line.push(icons::sized(Icon::Mic { muted: true }, style::MUTED, TAG));
            }
            if *deafened {
                line = line.push(icons::sized(
                    Icon::Headphones { deafened: true },
                    style::MUTED,
                    TAG,
                ));
            }
            let user = fastcord_model::User {
                id: *user_id,
                username: name.clone(),
                global_name: None,
                avatar: avatar.clone(),
                bot: false,
            };
            crate::member_menu::area(
                user,
                button(line)
                    .on_press(Message::ActivateParticipant(*user_id))
                    .width(Length::Fill)
                    .padding(0)
                    .style(style::plain_button),
            )
        }
    };
    container(line)
        .height(ROW_HEIGHT)
        .padding(Padding::new(0.0).left(26.0).right(8.0))
        .align_y(alignment::Vertical::Center)
        .clip(true)
        .into()
}

fn sidebar<'a>(
    snapshot: &'a NavigationSnapshot,
    private: PrivateConversation<'a>,
    directs: bool,
    footer: Element<'a, Message>,
    avatar_views: &'a HashMap<AvatarKey, image::Handle>,
    speaking: Vec<fastcord_model::Snowflake>,
) -> Element<'a, Message> {
    let PrivateConversation {
        recipients,
        notice: private_notice,
    } = private;
    let title = if directs {
        "Direct Messages".to_owned()
    } else if snapshot.selected_guild_name.is_empty() {
        "fastcord".to_owned()
    } else {
        snapshot.selected_guild_name.clone()
    };
    let can_report = !directs && snapshot.selected_guild.is_some();
    let header = container(
        row![
            text(title)
                .size(15)
                .font(BOLD)
                .wrapping(text::Wrapping::None)
                .width(Length::Fill),
            button(text("Report").size(11))
                .on_press_maybe(
                    can_report.then_some(Message::Report(crate::report::Event::OpenServer))
                )
                .padding([2, 6])
                .style(style::tool_button(false, false)),
        ]
        .spacing(6)
        .align_y(alignment::Vertical::Center),
    )
    .padding([13, 16])
    .width(Length::Fill)
    .clip(true)
    .style(|_| container::Style {
        border: iced::Border {
            color: style::RAIL,
            width: 1.0,
            ..iced::Border::default()
        },
        ..container::Style::default()
    });
    let list: Element<'a, Message> = if directs {
        let rows = snapshot.private_channels.rows.iter().map(|channel| {
            sidebar_row(
                if channel.recipient.is_some() {
                    Icon::Profile
                } else {
                    Icon::Group
                },
                channel.label.clone(),
                channel.selected,
                channel.unread,
                channel.mentions,
                Some(Message::SelectPrivateChannel(channel.id)),
                channel
                    .recipient
                    .and_then(|id| snapshot.presences.get(&id))
                    .map(|p| p.status),
                None,
            )
        });
        column![
            sidebar_row(
                Icon::Friends,
                "Friends".to_owned(),
                false,
                false,
                0,
                Some(Message::Friends(crate::friends::Event::Open)),
                None,
                None,
            ),
            column![
                text_input("Recipient IDs, comma separated", recipients)
                    .on_input(Message::PrivateRecipients)
                    .on_submit(Message::OpenPrivate)
                    .size(13)
                    .padding(6),
                text(private_notice.unwrap_or("Enter 1–9 IDs to start a conversation."))
                    .size(11)
                    .style(text::secondary),
            ]
            .spacing(4)
            .padding([8, 10]),
            virtual_list::view(
                "private-list",
                snapshot.private_channels.offset,
                snapshot.private_channels.total,
                rows,
                Message::PrivateViewport
            ),
        ]
        .into()
    } else {
        let rows = snapshot.channels.rows.iter().map(move |channel| {
            if let Some(occupant) = &channel.occupant {
                let talking = matches!(
                    occupant,
                    Occupant::Member { user_id, .. } if speaking.contains(user_id)
                );
                return occupant_row(occupant, talking, avatar_views);
            }
            if channel.kind == ChannelKind::Category {
                return container(
                    text(channel.name.to_uppercase())
                        .size(11)
                        .font(BOLD)
                        .wrapping(text::Wrapping::None)
                        .style(text::secondary),
                )
                .height(ROW_HEIGHT)
                .padding(Padding::new(0.0).left(16.0).top(14.0))
                .clip(true)
                .into();
            }
            let kind = match channel.kind {
                ChannelKind::Text => Icon::Channel,
                ChannelKind::Announcement => Icon::Announcement,
                ChannelKind::Voice if channel.stage => Icon::Stage,
                ChannelKind::Voice => Icon::Audio,
                ChannelKind::Thread => Icon::Thread,
                ChannelKind::Forum => Icon::Forum,
                ChannelKind::Category => Icon::ChevronDown,
            };
            let label = format!(
                "{}{}",
                channel.name,
                if channel.kind == ChannelKind::Voice && !channel.permissions.connect {
                    " (no access)"
                } else if channel.thread.is_some_and(|t| t.archived) {
                    " (archived)"
                } else {
                    ""
                }
            );
            let voice = channel.kind == ChannelKind::Voice;
            let guild = snapshot.selected_guild.filter(|_| channel.enabled);
            // Voice rows join the call; their chat opens separately, so the
            // selected chat and the connection never move each other.
            let intent = guild
                .filter(|_| !voice || channel.permissions.connect)
                .map(|guild| {
                    if voice {
                        Message::JoinVoice(guild, channel.id)
                    } else {
                        Message::SelectChannel(guild, channel.id)
                    }
                });
            let chat = guild
                .filter(|_| voice)
                .map(|guild| Message::SelectChannel(guild, channel.id));
            sidebar_row(
                kind,
                label,
                channel.selected,
                channel.unread,
                channel.mentions,
                intent,
                None,
                chat,
            )
        });
        virtual_list::view(
            "channel-list",
            snapshot.channels.offset,
            snapshot.channels.total,
            rows,
            Message::ChannelViewport,
        )
    };
    container(column![header, list, footer])
        .width(SIDEBAR_WIDTH)
        .height(Length::Fill)
        .style(style::sidebar)
        .into()
}

/// The strip over the conversation: its name and what the account can do there.
fn header<'a>(
    title: String,
    note: &'static str,
    emoji: bool,
    pins: bool,
    call: Option<(&'static str, Message)>,
) -> Element<'a, Message> {
    header_with(title, note, emoji, pins, call, Vec::new())
}

/// [`header`] with extra tool buttons (thread controls) before the usual ones.
fn header_with<'a>(
    title: String,
    note: &'static str,
    emoji: bool,
    pins: bool,
    call: Option<(&'static str, Message)>,
    tools: Vec<(&'static str, Message)>,
) -> Element<'a, Message> {
    let mut line = row![
        text(title).size(16).font(BOLD),
        text(note).size(12).style(text::secondary),
        space::horizontal(),
    ]
    .spacing(12)
    .align_y(alignment::Vertical::Center);
    if let Some((label, message)) = call {
        line = line.push(
            button(icons::labeled(Icon::Call, label, 13))
                .on_press(message)
                .style(style::action_button(false)),
        );
    }
    for (label, intent) in tools {
        line = line.push(
            button(text(label).size(13))
                .on_press(intent)
                .style(style::tool_button(true, false)),
        );
    }
    if pins {
        line = line.push(icons::tool(
            Icon::Pin,
            "Pinned messages",
            Message::Pins(crate::pins::Event::Toggle),
        ));
    }
    if emoji {
        line = line.push(icons::tool(
            Icon::Emoji,
            "Emoji",
            Message::EmojiPicker(crate::emoji_picker::Event::Toggle),
        ));
        line = line.push(icons::tool(
            Icon::Gif,
            "GIF",
            Message::Gif(crate::gif::Event::Toggle),
        ));
        line = line.push(icons::tool(
            Icon::Sticker,
            "Sticker",
            Message::Sticker(crate::sticker::Event::Toggle),
        ));
        line = line.push(icons::tool(
            Icon::Poll,
            "Create poll",
            Message::Poll(crate::poll_create::Event::Toggle),
        ));
    }
    container(line)
        .padding([10, 16])
        .width(Length::Fill)
        .style(|_| container::Style {
            border: iced::Border {
                color: style::RAIL,
                width: 1.0,
                ..iced::Border::default()
            },
            ..container::Style::default()
        })
        .into()
}

#[allow(clippy::too_many_arguments)]
fn main_pane<'a>(
    snapshot: &'a NavigationSnapshot,
    history: &'a timeline::Snapshot,
    composer: &'a Composer,
    interaction: timeline::Interaction,
    attachments: timeline::Attachments<'a>,
    directs: bool,
    banner: Option<Element<'a, Message>>,
    voice_call: Option<fastcord_model::Snowflake>,
    forum: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut body = column![].width(Length::Fill).height(Length::Fill);
    let conversation = |body: iced::widget::Column<'a, Message>,
                        channel_id: fastcord_model::Snowflake,
                        bar: Option<Element<'a, Message>>| {
        let mut body = body;
        if let Some(bar) = bar {
            body = body.push(container(bar).padding([6, 16]));
        }
        if history.channel_id == Some(channel_id) {
            body.push(timeline::view(history, interaction, attachments).map(Message::Timeline))
                .push(
                    container(composer::view(composer, history).map(Message::Composer))
                        .padding([0, 16]),
                )
        } else {
            body.push(
                container(text("Loading messages…").size(14).style(text::secondary))
                    .center(Length::Fill),
            )
        }
    };
    if let (true, Some(selection)) = (directs, &snapshot.private_selection) {
        let live = snapshot
            .calls
            .iter()
            .any(|call| call.channel_id == selection.channel_id && !call.participants.is_empty());
        let call = match (voice_call == Some(selection.channel_id), live) {
            (true, _) => None,
            (false, true) => Some(("Join call", Message::AcceptCall(selection.channel_id))),
            (false, false) => Some(("Call", Message::StartCall(selection.channel_id))),
        };
        body = body.push(header_with(
            format!("@ {}", selection.name),
            "",
            true,
            true,
            call,
            vec![(
                if selection.group {
                    "Members"
                } else {
                    "Conversation"
                },
                Message::Group(crate::group_dm::Event::Toggle),
            )],
        ));
        if let Some(banner) = banner {
            body = body.push(container(banner).padding([4, 16]));
        }
        let bar = unread_bar(
            selection.unread,
            selection.mentions,
            selection.acked,
            None,
            selection.channel_id,
        );
        body = conversation(body, selection.channel_id, bar);
    } else if directs {
        body = body.push(header("Direct Messages".to_owned(), "", false, false, None));
        if let Some(banner) = banner {
            body = body.push(container(banner).padding([4, 16]));
        }
        body = body.push(
            container(
                text("Pick a conversation, or enter recipient IDs on the left to start one.")
                    .size(14)
                    .style(text::secondary),
            )
            .center(Length::Fill),
        );
    } else if snapshot.permissions_pending {
        body = body.push(header("Loading".to_owned(), "", false, false, None)).push(
            container(
                text("Waiting for your membership and permissions. Channels stay closed until Discord supplies them.")
                    .size(14)
                    .style(text::secondary),
            )
            .center(Length::Fill),
        );
    } else if let Some(selection) = &snapshot.selection {
        let permissions = selection.permissions;
        match selection.kind {
            // Voice and Stage channels open their text chat here; whether the
            // account is in the call is shown by the voice bar, not this pane.
            ChannelKind::Text
            | ChannelKind::Announcement
            | ChannelKind::Thread
            | ChannelKind::Voice => {
                let thread = selection.thread;
                let note = match (
                    permissions.read_history,
                    permissions.send_messages,
                    thread.map(|t| (t.archived, t.locked)),
                ) {
                    (false, _, _) => "You can view this channel but not read its history.",
                    (true, false, Some((_, true))) => "Locked: only thread managers can write",
                    (true, true, Some((true, _))) => "Archived: sending a message reopens it",
                    (true, false, Some((true, _))) => "Archived",
                    (true, true, _) => "",
                    (true, false, _) if snapshot.own_pending => {
                        "Accept the server's rules to write or react"
                    }
                    (true, false, _) => "Read-only",
                };
                let mut tools = Vec::new();
                let title = match thread {
                    Some(state) => {
                        if state.forum_post {
                            tools.push((
                                "Posts",
                                Message::SelectChannel(selection.guild_id, state.parent_id),
                            ));
                        }
                        tools.push(if state.joined {
                            (
                                "Leave",
                                Message::Threads(crate::threads::Event::Leave(
                                    selection.channel_id,
                                )),
                            )
                        } else {
                            (
                                "Join",
                                Message::Threads(crate::threads::Event::Join(selection.channel_id)),
                            )
                        });
                        format!("\u{21B3} {}", selection.name)
                    }
                    None => {
                        if permissions.read_history && selection.kind != ChannelKind::Voice {
                            tools.push((
                                "Threads",
                                Message::Threads(crate::threads::Event::ToggleArchived(
                                    selection.guild_id,
                                    selection.channel_id,
                                )),
                            ));
                        }
                        if permissions.create_invite
                            || permissions.manage_guild
                            || permissions.manage_channel
                        {
                            tools.push((
                                "Invite",
                                Message::Invite(crate::invite::Event::Create(
                                    selection.guild_id,
                                    selection.channel_id,
                                )),
                            ));
                        }
                        if permissions.manage_channel {
                            tools.push((
                                "Edit channel",
                                Message::GuildAdmin(crate::guild_admin::Event::EditChannel(
                                    selection.guild_id,
                                    selection.channel_id,
                                    selection.kind == ChannelKind::Voice,
                                )),
                            ));
                        }
                        tools.push((
                            "Events",
                            Message::Events(crate::scheduled_events::Event::Open(
                                selection.guild_id,
                            )),
                        ));
                        tools.push((
                            "Guide",
                            Message::Onboarding(crate::onboarding::Event::OpenGuide(
                                selection.guild_id,
                            )),
                        ));
                        tools.push((
                            "Channels & Roles",
                            Message::Onboarding(crate::onboarding::Event::OpenQuestions(
                                selection.guild_id,
                                true,
                            )),
                        ));
                        let prefix = if selection.kind == ChannelKind::Voice {
                            "\u{266A}"
                        } else {
                            "#"
                        };
                        format!("{prefix} {}", selection.name)
                    }
                };
                body = body.push(header_with(
                    title,
                    note,
                    history.can_send || composer.editing().is_some(),
                    permissions.read_history,
                    None,
                    tools,
                ));
                if let Some(banner) = banner {
                    body = body.push(container(banner).padding([4, 16]));
                }
                if permissions.read_history {
                    let bar = unread_bar(
                        selection.unread,
                        selection.mentions,
                        selection.acked,
                        Some(selection.guild_id),
                        selection.channel_id,
                    );
                    body = conversation(body, selection.channel_id, bar);
                }
            }
            ChannelKind::Forum => {
                let can_post = snapshot.forum.as_ref().is_some_and(|f| f.can_post);
                let note = match (permissions.read_history, can_post) {
                    (false, _) => "You can view this forum but not read its posts.",
                    (true, false) => "You cannot post here",
                    (true, true) => "",
                };
                let mut tools = if can_post {
                    vec![("New post", Message::Forum(crate::forum::Event::New))]
                } else {
                    Vec::new()
                };
                if permissions.manage_channel {
                    tools.push((
                        "Edit channel",
                        Message::GuildAdmin(crate::guild_admin::Event::EditChannel(
                            selection.guild_id,
                            selection.channel_id,
                            false,
                        )),
                    ));
                }
                body = body.push(header_with(
                    format!("\u{2630} {}", selection.name),
                    note,
                    false,
                    false,
                    None,
                    tools,
                ));
                if let Some(banner) = banner {
                    body = body.push(container(banner).padding([4, 16]));
                }
                if permissions.read_history
                    && let Some(forum) = forum
                {
                    body = body.push(forum);
                }
            }
            ChannelKind::Category => {}
        }
    } else {
        body = body.push(header("fastcord".to_owned(), "", false, false, None));
        if let Some(banner) = banner {
            body = body.push(container(banner).padding([4, 16]));
        }
        body = body.push(
            container(
                text("Select a channel. Permission loss closes it; your selection survives server updates by ID.")
                    .size(14)
                    .style(text::secondary),
            )
            .center(Length::Fill),
        );
    }
    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(style::chat)
        .into()
}

/// The bar above an open conversation that has unread messages: jump to where
/// the reader left off, or mark it all read. Only conversations with a read
/// cursor have one.
fn unread_bar<'a>(
    unread: bool,
    mention_count: u32,
    acked: Option<fastcord_model::Snowflake>,
    guild: Option<fastcord_model::Snowflake>,
    channel: fastcord_model::Snowflake,
) -> Option<Element<'a, Message>> {
    let acked = acked.filter(|_| unread || mention_count > 0)?;
    let summary = match mention_count {
        0 => "You have unread messages here.".to_owned(),
        1 => "You have unread messages here, 1 mention.".to_owned(),
        n => format!("You have unread messages here, {n} mentions."),
    };
    Some(
        row![
            text(summary).size(13),
            button(text("Jump to first unread").size(12))
                .on_press(Message::FirstUnread(channel, acked))
                .style(style::action_button(false)),
            button(text("Mark as read").size(12))
                .on_press(Message::MarkRead(guild, channel))
                .style(style::tool_button(true, false)),
        ]
        .spacing(8)
        .align_y(alignment::Vertical::Center)
        .into(),
    )
}

#[cfg(test)]
mod occupant_tests {
    use super::*;
    use crate::variable_list::harness::Ui;
    use fastcord_model::Snowflake;

    fn member(streaming: bool, video: bool) -> Occupant {
        Occupant::Member {
            user_id: Snowflake(7),
            name: "A person with an extremely long display name indeed".into(),
            avatar: None,
            muted: true,
            deafened: true,
            streaming,
            video,
            seat: None,
        }
    }

    #[test]
    fn occupant_rows_watch_on_left_click_and_open_actions_only_on_right_click() {
        let views = HashMap::new();
        for (speaking, streaming, video) in [(false, false, false), (true, true, true)] {
            let occupant = member(streaming, video);
            let mut ui = Ui::new(
                SIDEBAR_WIDTH,
                ROW_HEIGHT,
                occupant_row(&occupant, speaking, &views),
            );
            ui.draw();
            let opened = ui.click(100.0, ROW_HEIGHT / 2.0);
            assert!(
                matches!(
                    opened.as_slice(),
                    [Message::ActivateParticipant(user)] if *user == Snowflake(7)
                ),
                "{opened:?}"
            );
            let opened = ui.right_click(100.0, ROW_HEIGHT / 2.0);
            assert!(
                matches!(opened.as_slice(), [Message::MemberMenu(crate::member_menu::Event::Open(user, _))] if user.id == Snowflake(7)),
                "{opened:?}"
            );
        }
        let more = Occupant::More(3);
        let mut ui = Ui::new(
            SIDEBAR_WIDTH,
            ROW_HEIGHT,
            occupant_row(&more, false, &views),
        );
        ui.draw();
        assert!(ui.click(100.0, ROW_HEIGHT / 2.0).is_empty());
    }
}
