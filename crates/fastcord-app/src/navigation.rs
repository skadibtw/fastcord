//! Presentation of compact reducer-owned navigation windows, never full state.
use fastcord_discord::state::navigation::{ChannelKind, NavigationSnapshot};
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, alignment};

use crate::Message;
use crate::composer::{self, Composer};
use crate::timeline;
use crate::virtual_list::{self, ROW_HEIGHT};

pub fn view<'a>(
    snapshot: &'a NavigationSnapshot,
    history: &'a timeline::Snapshot,
    composer: &'a Composer,
    interaction: timeline::Interaction,
    recipients: &'a str,
    private_notice: Option<&'a str>,
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
    let private_channels = snapshot.private_channels.rows.iter().map(|channel| {
        fixed_button(
            format!(
                "{}{}",
                if channel.selected { "› " } else { "" },
                channel.label
            ),
            Some(Message::SelectPrivateChannel(channel.id)),
        )
    });
    let mut detail = column![text("Choose a channel").size(24)];
    if let Some(selection) = &snapshot.private_selection {
        detail = column![
            text(&selection.name).size(24),
            text(
                "Private conversation. Discord remains authoritative for message access and writes."
            )
            .size(13),
        ];
        if history.channel_id == Some(selection.channel_id) {
            detail = detail
                .push(timeline::view(history, interaction).map(Message::Timeline))
                .push(composer::view(composer, history).map(Message::Composer));
        } else {
            detail = detail.push(text("Loading messages…").size(14));
        }
    } else if snapshot.permissions_pending {
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
                    if history.channel_id == Some(selection.channel_id) {
                        // The timeline takes the free height; the composer sits below it.
                        detail = detail
                            .push(timeline::view(history, interaction).map(Message::Timeline))
                            .push(composer::view(composer, history).map(Message::Composer));
                    } else {
                        detail = detail.push(text("Loading messages…").size(14));
                    }
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
            scrollable(
                column![
                    text("Servers"),
                    virtual_list::view(
                        "guild-list",
                        snapshot.guilds.offset,
                        snapshot.guilds.total,
                        guilds,
                        Message::GuildViewport
                    ),
                    text("Direct conversations"),
                    virtual_list::view(
                        "private-list",
                        snapshot.private_channels.offset,
                        snapshot.private_channels.total,
                        private_channels,
                        Message::PrivateViewport
                    ),
                ]
                .spacing(4)
            )
            .height(Length::Fill),
            column![
                text_input("Recipient IDs (comma separated)", recipients)
                    .on_input(Message::PrivateRecipients)
                    .size(12),
                button("Open selected recipients").on_press(Message::OpenPrivate),
                text(
                    private_notice.unwrap_or("Select 1–9 recipient IDs to open a DM or group DM.")
                )
                .size(12),
            ]
            .spacing(4),
        ]
        .spacing(4)
        .height(Length::Fill)
        .width(210),
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

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::state::navigation::{PrivateChannelRow, PrivateChannelSelection, Window};

    #[test]
    fn fixture_private_channel_builds_the_full_timeline_and_composer_view() {
        let channel = fastcord_model::Snowflake(300_000_000_000_000_001);
        let snapshot = NavigationSnapshot {
            private_channels: Window {
                offset: 0,
                total: 1,
                rows: vec![PrivateChannelRow {
                    id: channel,
                    label: "Nelly".to_owned(),
                    selected: true,
                }],
            },
            private_selection: Some(PrivateChannelSelection {
                channel_id: channel,
                name: "Nelly".to_owned(),
            }),
            ..NavigationSnapshot::default()
        };
        let timeline = timeline::Snapshot {
            channel_id: Some(channel),
            ..timeline::Snapshot::default()
        };
        let composer = Composer::default();
        let _view = view(
            &snapshot,
            &timeline,
            &composer,
            timeline::Interaction::default(),
            "80351110224678912",
            None,
        );
    }
}
