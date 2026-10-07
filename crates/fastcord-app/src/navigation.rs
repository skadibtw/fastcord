//! Presentation of compact reducer-owned navigation windows, never full state.
use fastcord_discord::state::navigation::{ChannelKind, NavigationSnapshot};
use iced::widget::{button, column, container, row, text};
use iced::{Element, Length, alignment};

use crate::Message;
use crate::timeline;
use crate::virtual_list::{self, ROW_HEIGHT};

pub fn view<'a>(
    snapshot: &'a NavigationSnapshot,
    history: &'a timeline::Snapshot,
) -> Element<'a, Message> {
    let guilds = snapshot.guilds.rows.iter().map(|guild| {
        let label = format!(
            "{}{}{}",
            if guild.selected { "› " } else { "" },
            guild.name,
            if guild.available {
                ""
            } else {
                " (unavailable)"
            }
        );
        fixed_button(
            label,
            guild.available.then_some(Message::SelectGuild(guild.id)),
        )
    });
    let channels = snapshot.channels.rows.iter().map(|channel| {
        let prefix = match channel.kind {
            ChannelKind::Category => "",
            ChannelKind::Text | ChannelKind::Announcement => "# ",
            ChannelKind::Voice => "Voice · ",
        };
        let label = format!(
            "{}{}{}{}",
            if channel.selected { "› " } else { "" },
            prefix,
            channel.name,
            if channel.kind == ChannelKind::Voice && !channel.enabled {
                " (no Connect permission)"
            } else {
                ""
            }
        );
        let intent = snapshot
            .selected_guild
            .filter(|_| channel.enabled)
            .map(|guild| Message::SelectChannel(guild, channel.id));
        fixed_button(label, intent)
    });
    let mut detail = column![text("Choose a channel").size(24)];
    if snapshot.permissions_pending {
        detail = detail.push(text("Waiting for your server membership and permissions. Channels remain closed until Discord supplies them."));
    } else if let Some(selection) = &snapshot.selection {
        detail = column![text(&selection.name).size(24)];
        let permissions = selection.permissions;
        match selection.kind {
            ChannelKind::Text | ChannelKind::Announcement => {
                detail = detail.push(
                    text(
                        match (permissions.read_history, permissions.send_messages) {
                            (false, _) => "You can view this channel, but cannot read its history.",
                            (true, true) => {
                                "Message history. You have permission to send messages."
                            }
                            (true, false) => {
                                "Message history. This channel is read-only for your account."
                            }
                        },
                    )
                    .size(13),
                );
                if permissions.read_history {
                    detail = detail.push(if history.channel_id == Some(selection.channel_id) {
                        timeline::view(history).map(Message::Timeline)
                    } else {
                        text("Loading messages…").size(14).into()
                    });
                }
            }
            ChannelKind::Voice => {
                detail = detail
                    .push(text(
                        "Voice channel selected. Selecting a channel does not join a call.",
                    ))
                    .push(text(if permissions.speak {
                        "Connect and Speak permissions available."
                    } else {
                        "Connect permission available; you cannot speak in this channel."
                    }));
            }
            ChannelKind::Category => {}
        }
    } else {
        detail = detail.push(text("Select an accessible text or voice channel. Server updates preserve your selection by ID; permission loss closes it."));
    }
    row![
        column![
            text("Servers"),
            virtual_list::view(
                "guild-list",
                snapshot.guilds.offset,
                snapshot.guilds.total,
                guilds,
                Message::GuildViewport
            )
        ]
        .width(190),
        column![
            text("Channels"),
            virtual_list::view(
                "channel-list",
                snapshot.channels.offset,
                snapshot.channels.total,
                channels,
                Message::ChannelViewport
            )
        ]
        .width(250),
        container(detail.spacing(8).height(Length::Fill))
            .padding(12)
            .width(Length::Fill)
            .height(Length::Fill),
    ]
    .spacing(12)
    .height(Length::Fill)
    .into()
}

fn fixed_button<'a>(label: String, intent: Option<Message>) -> Element<'a, Message> {
    let content = text(label)
        .size(14)
        .wrapping(iced::widget::text::Wrapping::None);
    container(
        button(content)
            .on_press_maybe(intent)
            .width(Length::Fill)
            .height(ROW_HEIGHT)
            .padding([4, 8]),
    )
    .height(ROW_HEIGHT)
    .width(Length::Fill)
    .align_y(alignment::Vertical::Center)
    .clip(true)
    .into()
}
