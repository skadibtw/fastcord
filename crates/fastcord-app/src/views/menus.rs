//! Menus, pickers and secondary screens drawn over the account screen.

use crate::*;
use iced::widget::{column, row};

impl App {
    pub(crate) fn share_picker_view<'a>(
        picker: &'a gateway::SharePicker,
        share_audio: bool,
        audio_sources: &'a [fastcord_platform::loopback::AudioSource],
        audio_target: Option<fastcord_platform::loopback::AudioTarget>,
    ) -> Element<'a, Message> {
        use fastcord_platform::capture::SourceKind;
        let mut content = column![
            row![
                text(i18n::tr("Share your screen")).size(22).width(Length::Fill),
                button(text(i18n::tr("Cancel"))).on_press(Message::CloseSharePicker),
            ]
            .spacing(12),
            text(i18n::tr(if cfg!(windows) {
                "Choose a screen or a window. Windows draws a border around what is shared, and your voice stays connected."
            } else if cfg!(target_os = "linux") {
                "Choose a screen or a window; your desktop then asks which one in its own dialog. Your voice stays connected."
            } else {
                "Choose a screen or a window. Your voice stays connected."
            }))
            .size(12),
        ]
        .spacing(12);
        match picker {
            gateway::SharePicker::Closed => {}
            gateway::SharePicker::Loading => {
                content = content.push(text(i18n::tr("Looking for screens and windows…")));
            }
            gateway::SharePicker::Failed(error) => {
                content = content.push(text(i18n::tr(error).to_owned()));
            }
            gateway::SharePicker::Listed(sources) => {
                let mut screens = column![text(i18n::tr("Screens")).size(16)].spacing(6);
                let mut windows = column![text(i18n::tr("Windows")).size(16)].spacing(6);
                let mut any_window = false;
                for source in sources {
                    let entry = button(text(source.title.clone()))
                        .on_press(Message::StartShare(source.id))
                        .width(Length::Fill);
                    match source.id.kind() {
                        SourceKind::Display => screens = screens.push(entry),
                        SourceKind::Window => {
                            any_window = true;
                            windows = windows.push(entry);
                        }
                    }
                }
                if !any_window {
                    windows =
                        windows.push(text(i18n::tr("No shareable windows are open.")).size(12));
                }
                let offer = gateway::AudioOffer::current();
                if offer != gateway::AudioOffer::Unavailable {
                    let mut audio = column![
                        checkbox(share_audio)
                            .label(i18n::tr("Share audio"))
                            .on_toggle(Message::ToggleShareAudio)
                    ]
                    .spacing(4);
                    audio = audio.push(text(offer.note()).size(12));
                    if offer == gateway::AudioOffer::Choose && share_audio {
                        if audio_sources.is_empty() {
                            audio = audio.push(
                                text(i18n::tr("No PipeWire outputs or inputs were found."))
                                    .size(12),
                            );
                        }
                        for source in audio_sources {
                            audio = audio.push(iced::widget::radio(
                                source.title.as_str(),
                                source.target,
                                audio_target,
                                Message::ChooseShareAudio,
                            ));
                        }
                    }
                    let chosen_carries_call =
                        audio_target.is_some_and(|target| target.whole_output);
                    if offer.warns() || (share_audio && chosen_carries_call) {
                        audio = audio.push(
                            text(i18n::tr(
                                "Warning: viewers may hear your call while this is on.",
                            ))
                            .size(12)
                            .color(iced::Color::from_rgb(0.9, 0.55, 0.1)),
                        );
                    }
                    content = content.push(audio);
                } else {
                    content = content.push(text(offer.note()).size(12));
                }
                content = content
                    .push(scrollable(column![screens, windows].spacing(16)).height(Length::Fill));
            }
        }
        container(content)
            .padding(20)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(Color::from_rgb8(24, 25, 30).into()),
                ..Default::default()
            })
            .into()
    }

    /// The menu under the server's name: invite and manage first, then the
    /// server's pages and alerts, then report and leave. Management rows are
    /// greyed out until the server's permissions are known and when missing.
    pub(crate) fn server_menu<'a>(
        &'a self,
        gateway: &'a GatewayPanel,
        guild: Snowflake,
    ) -> Element<'a, Message> {
        let pick = |message| Some(Message::MenuPick(Box::new(message)));
        let admin = gateway
            .navigation
            .admin
            .as_ref()
            .filter(|admin| admin.id == guild);
        let can = |allowed: fn(&fastcord_discord::state::navigation::GuildAdmin) -> bool| {
            admin.is_some_and(allowed)
        };
        let invite_channel = gateway
            .navigation
            .selection
            .as_ref()
            .filter(|s| s.guild_id == guild)
            .filter(|s| {
                s.permissions.create_invite
                    || s.permissions.manage_guild
                    || s.permissions.manage_channel
            })
            .map(|s| s.channel_id);
        let admin_event = |event| Message::GuildAdmin(event);
        let mut rows = column![ui::item(
            Some(icons::Icon::Invite),
            i18n::tr("Invite people"),
            invite_channel
                .and_then(|channel| pick(Message::Invite(invite::Event::Create(guild, channel)))),
        ),]
        .spacing(1);
        // Only what this account may do is listed; every management page
        // is a section of the one settings screen.
        let settings = server_settings::Section::first(admin);
        if settings.is_some() || can(|a| a.can_manage_channels) {
            rows = rows.push(ui::separator());
        }
        if let Some(section) = settings {
            rows = rows.push(ui::item(
                Some(icons::Icon::Settings),
                i18n::tr("Server settings"),
                pick(Message::ServerSettings(guild, section)),
            ));
        }
        if can(|a| a.can_manage_channels) {
            rows = rows.push(ui::item(
                Some(icons::Icon::Plus),
                i18n::tr("Create channel"),
                pick(admin_event(guild_admin::Event::CreateChannel(guild, None))),
            ));
        }
        rows = rows
            .push(ui::separator())
            .push(ui::item(
                Some(icons::Icon::Calendar),
                i18n::tr("Events"),
                pick(Message::Events(scheduled_events::Event::Open(guild))),
            ))
            .push(ui::item(
                Some(icons::Icon::Info),
                i18n::tr("Server guide"),
                pick(Message::Onboarding(onboarding::Event::OpenGuide(guild))),
            ))
            .push(ui::item(
                Some(icons::Icon::Grid),
                i18n::tr("Channels and roles"),
                pick(Message::Onboarding(onboarding::Event::OpenQuestions(
                    guild, true,
                ))),
            ))
            .push(ui::item(
                Some(icons::Icon::Bell),
                i18n::tr("Notification settings"),
                pick(Message::NotifySettings(notify_settings::Event::Toggle)),
            ))
            .push(ui::separator())
            .push(ui::item(
                Some(icons::Icon::Report),
                i18n::tr("Report server"),
                pick(Message::Report(report::Event::OpenServer)),
            ))
            .push(ui::danger_item(
                Some(icons::Icon::Exit),
                i18n::tr("Leave server"),
                pick(admin_event(guild_admin::Event::Leave(guild))),
            ));
        ui::popover(
            ui::menu_card(rows, 232.0),
            ui::Anchor::TopLeft(navigation::RAIL_WIDTH + 4.0, 50.0),
            Message::OpenMenu(None),
        )
    }

    /// The `⋯` menu of the open conversation, under the header's right end.
    pub(crate) fn conversation_menu<'a>(
        &'a self,
        gateway: &'a GatewayPanel,
    ) -> Element<'a, Message> {
        let mut rows = column![].spacing(1);
        for (label, icon, message) in
            navigation::conversation_menu(&gateway.navigation, gateway.directs, self.wide())
        {
            rows = rows.push(ui::item(
                Some(icon),
                i18n::tr(label),
                Some(Message::MenuPick(Box::new(message))),
            ));
        }
        ui::popover(
            ui::menu_card(rows, 232.0),
            ui::Anchor::TopRight(12.0, 46.0),
            Message::OpenMenu(None),
        )
    }

    /// The server or channel settings screen around the open page's panel.
    pub(crate) fn server_screen<'a>(
        &'a self,
        gateway: &'a GatewayPanel,
        guild: Snowflake,
        section: server_settings::Section,
    ) -> Element<'a, Message> {
        use server_settings::Section;
        let navigation = &gateway.navigation;
        let empty = || -> Element<'a, Message> { text("").into() };
        let content: Element<'a, Message> = match section {
            Section::Overview | Section::Roles | Section::Channel(_) => {
                guild_admin::view(&self.ui.guild_admin, navigation)
                    .map_or_else(empty, |panel| panel.map(Message::GuildAdmin))
            }
            Section::Expressions => assets::view(&self.ui.assets, navigation)
                .map_or_else(empty, |panel| panel.map(Message::Assets)),
            Section::Community => community::view(&self.ui.community, navigation)
                .map_or_else(empty, |panel| panel.map(Message::Community)),
            Section::AutoMod => automod::view(&self.ui.automod, navigation)
                .map_or_else(empty, |panel| panel.map(Message::AutoMod)),
            Section::Integrations | Section::Webhooks(_) => apps::view(&self.ui.apps, navigation)
                .map_or_else(empty, |panel| panel.map(Message::Apps)),
            Section::Bans | Section::Audit => moderation::view(&self.ui.moderation, navigation)
                .map_or_else(empty, |panel| panel.map(Message::Moderation)),
        };
        let admin = navigation.admin.as_ref().filter(|admin| admin.id == guild);
        let guild_name = admin.map(|admin| admin.name.as_str()).or_else(|| {
            navigation
                .guilds
                .rows
                .iter()
                .find(|row| row.id == guild)
                .map(|row| row.name.as_str())
        });
        let channel = section.channel().and_then(|id| {
            admin?.channels.iter().find(|c| c.id == id).map(|c| {
                (
                    c.name.as_str(),
                    matches!(c.kind, AdminKind::Text | AdminKind::Announcement),
                )
            })
        });
        server_settings::view(
            server_settings::Page {
                guild,
                guild_name: guild_name.unwrap_or(""),
                section,
                channel,
                admin,
            },
            content,
        )
    }

    /// Emoji, GIF and stickers in one window above the message box. A
    /// reaction or an edit offers emoji only.
    pub(crate) fn expressions_view<'a>(
        &'a self,
        gateway: &'a GatewayPanel,
    ) -> Element<'a, Message> {
        let tab = self.expressions_tab(gateway).unwrap_or(Expressions::Emoji);
        let body: Element<'a, Message> = match tab {
            Expressions::Emoji => emoji_picker::view(
                &gateway.emoji_picker,
                &gateway.emoji_snapshot,
                &gateway.emoji.views,
            ),
            Expressions::Gif => gif::view(&self.ui.gif)
                .map_or_else(|| text("").into(), |panel| panel.map(Message::Gif)),
            Expressions::Sticker => sticker::view(&self.ui.sticker)
                .map_or_else(|| text("").into(), |panel| panel.map(Message::Sticker)),
        };
        let full = gateway.emoji_picker.reaction().is_none()
            && gateway.composer.editing().is_none()
            && gateway.timeline.can_send;
        // One window of one size whatever the tab: the tabs and the cross
        // never move under the pointer. A reaction or an edit offers emoji
        // only, under a title instead of the tabs.
        let head: Element<'a, Message> = if full {
            ui::tabs(
                &[
                    (Expressions::Emoji, i18n::tr("Emoji")),
                    (Expressions::Gif, "GIF"),
                    (Expressions::Sticker, i18n::tr("Stickers")),
                ],
                tab,
                Message::Expressions,
            )
        } else {
            text(if gateway.emoji_picker.reaction().is_some() {
                i18n::tr("React with emoji")
            } else {
                i18n::tr("Emoji")
            })
            .size(16)
            .font(ui::BOLD)
            .into()
        };
        let close = match tab {
            Expressions::Emoji => Message::EmojiPicker(emoji_picker::Event::Close),
            Expressions::Gif => Message::Gif(gif::Event::Toggle),
            Expressions::Sticker => Message::Sticker(sticker::Event::Toggle),
        };
        container(
            column![
                row![
                    container(head).width(Length::Fill),
                    icons::close(Some(close)),
                ]
                .spacing(8)
                .height(36)
                .align_y(iced::Alignment::Center),
                container(body).height(Length::Fill),
            ]
            .spacing(10),
        )
        .padding(12)
        .width(EXPRESSIONS_SIZE.width)
        .height(EXPRESSIONS_SIZE.height)
        .style(style::card)
        .into()
    }

    /// The "+" menu of the message box.
    pub(crate) fn composer_menu<'a>(&'a self, gateway: &'a GatewayPanel) -> Element<'a, Message> {
        let pick = |message| Some(Message::MenuPick(Box::new(message)));
        let can_send = gateway.timeline.can_send;
        let mut rows = column![
            ui::item(
                Some(icons::Icon::Attach),
                i18n::tr("Attach a file"),
                can_send
                    .then(|| pick(Message::Composer(composer::Event::Attach)))
                    .flatten(),
            ),
            ui::item(
                Some(icons::Icon::Paste),
                i18n::tr("Paste image"),
                can_send
                    .then(|| pick(Message::Composer(composer::Event::PasteImage)))
                    .flatten(),
            ),
            ui::item(
                Some(icons::Icon::Poll),
                i18n::tr("Create poll"),
                can_send
                    .then(|| pick(Message::Composer(composer::Event::Toggle(
                        composer::Picker::Poll
                    ))))
                    .flatten(),
            ),
        ]
        .spacing(1);
        if self.audio_preferences.voice_recording {
            rows = rows.push(ui::item(
                Some(icons::Icon::Record),
                i18n::tr("Voice message"),
                can_send
                    .then(|| pick(Message::Voice(voice_record::Event::Start)))
                    .flatten(),
            ));
        }
        ui::popover(
            ui::menu_card(rows, 232.0),
            ui::Anchor::BottomLeft(
                navigation::RAIL_WIDTH + navigation::SIDEBAR_WIDTH + 12.0,
                64.0,
            ),
            Message::OpenMenu(None),
        )
    }
}
