//! Presentation of compact reducer-owned navigation windows, never full state.
use fastcord_discord::state::navigation::{ChannelKind, NavigationSnapshot};
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, alignment};

use crate::Message;
use crate::composer::{self, Composer};
use crate::gateway::VoiceAudioState;
use crate::timeline;
use crate::virtual_list::{self, ROW_HEIGHT};

pub struct PrivateConversation<'a> {
    pub recipients: &'a str,
    pub notice: Option<&'a str>,
}

pub fn view<'a>(
    snapshot: &'a NavigationSnapshot,
    history: &'a timeline::Snapshot,
    composer: &'a Composer,
    interaction: timeline::Interaction,
    private: PrivateConversation<'a>,
    attachments: timeline::Attachments<'a>,
    voice_audio: VoiceAudioState,
) -> Element<'a, Message> {
    let PrivateConversation {
        recipients,
        notice: private_notice,
    } = private;
    let guilds = snapshot.guilds.rows.iter().map(|guild| {
        let label = format!(
            "{}{}{}{}{}",
            if guild.selected { "› " } else { "" },
            marker(guild.unread),
            guild.name,
            mentions(guild.mentions),
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
            "{}{}{}{}{}{}",
            if channel.selected { "› " } else { "" },
            marker(channel.unread),
            prefix,
            channel.name,
            mentions(channel.mentions),
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
                "{}{}{}{}",
                if channel.selected { "› " } else { "" },
                marker(channel.unread),
                channel.label,
                mentions(channel.mentions)
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
        if let Some(bar) = unread_bar(
            selection.unread,
            selection.mentions,
            selection.acked,
            None,
            selection.channel_id,
        ) {
            detail = detail.push(bar);
        }
        if history.channel_id == Some(selection.channel_id) {
            detail = detail
                .push(timeline::view(history, interaction, attachments).map(Message::Timeline))
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
                    if let Some(bar) = unread_bar(
                        selection.unread,
                        selection.mentions,
                        selection.acked,
                        Some(selection.guild_id),
                        selection.channel_id,
                    ) {
                        detail = detail.push(bar);
                    }
                    if history.channel_id == Some(selection.channel_id) {
                        // The timeline takes the free height; the composer sits below it.
                        detail = detail
                            .push(
                                timeline::view(history, interaction, attachments)
                                    .map(Message::Timeline),
                            )
                            .push(composer::view(composer, history).map(Message::Composer));
                    } else {
                        detail = detail.push(text("Loading messages…").size(14));
                    }
                }
            }
            ChannelKind::Voice => {
                detail = detail
                    .push(text(
                        "Voice channel selected. Selecting this channel joins the call.",
                    ))
                    .push(text(if permissions.speak {
                        "Connect and Speak permissions available."
                    } else {
                        "Connect permission available; you cannot speak in this channel."
                    }))
                    .push(text(match voice_audio {
                        VoiceAudioState::Idle => "Not connected to voice.",
                        VoiceAudioState::Connecting => "Connecting to voice…",
                        VoiceAudioState::Running => "Connected to voice.",
                        VoiceAudioState::EngineFailed => {
                            "Audio could not start; the voice connection was left."
                        }
                    }));
            }
            ChannelKind::Category => {}
        }
    } else {
        detail = detail.push(text("Select an accessible text or voice channel. Server updates preserve your selection by ID; permission loss closes it."));
    }
    if history.can_send || composer.editing().is_some() {
        detail = detail.push(
            button(text("Emoji").size(13))
                .on_press(Message::EmojiPicker(crate::emoji_picker::Event::Toggle)),
        );
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

/// A dot in front of a name that has unread messages.
fn marker(unread: bool) -> &'static str {
    if unread { "\u{2022} " } else { "" }
}

/// ` (3)` after a name with mentions.
fn mentions(count: u32) -> String {
    match count {
        0 => String::new(),
        1..=99 => format!(" ({count})"),
        _ => " (99+)".to_owned(),
    }
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
                .on_press(Message::FirstUnread(channel, acked)),
            button(text("Mark as read").size(12)).on_press(Message::MarkRead(guild, channel)),
        ]
        .spacing(8)
        .align_y(alignment::Vertical::Center)
        .into(),
    )
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
